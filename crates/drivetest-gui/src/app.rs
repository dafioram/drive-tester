//! Application state and layout: drive list on the left, the selected drive's
//! tests in tabs on the right, mode switch in the top bar.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use drivetest_core::events::EngineEvent;
use drivetest_core::freespace::FreeSpaceConfig;
use drivetest_core::health::{HealthSnapshot, RecordingVerdict, VerdictLevel, assess_smr};
use drivetest_core::model::{DriveInfo, HealthSource, MediaKind, format_bytes};
use drivetest_core::platform::{Platform, SelfTestKind};
use drivetest_core::policy::{Mode, Policy, Tier};
use drivetest_core::report::{Report, default_report_dir};
use drivetest_core::runner::{JobEvent, JobHandle, JobKind, JobRequest, start_job};
use drivetest_core::scan::{ScanConfig, ScanMode};
use drivetest_core::stale::StaleOutcome;
use drivetest_core::writetest::{WriteTestConfig, WriteTestKind};
use eframe::egui::{self, Color32, RichText, Sense, Stroke};

use crate::view::{self, LiveView};

mod maintenance;

const AUTO_REFRESH: Duration = Duration::from_secs(15);

const RED: Color32 = Color32::from_rgb(210, 60, 50);
const ORANGE: Color32 = Color32::from_rgb(230, 150, 40);
const GREEN: Color32 = Color32::from_rgb(60, 170, 80);

fn verdict_color(level: VerdictLevel) -> Color32 {
    match level {
        VerdictLevel::Good => GREEN,
        VerdictLevel::Caution => ORANGE,
        VerdictLevel::Replace => RED,
        VerdictLevel::Unknown => Color32::GRAY,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Health,
    Scan,
    WriteTests,
    Maintenance,
    Report,
}

impl Tab {
    const ALL: [Tab; 6] = [Tab::Overview, Tab::Health, Tab::Scan, Tab::WriteTests, Tab::Maintenance, Tab::Report];

    fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Health => "Health",
            Tab::Scan => "Scan",
            Tab::WriteTests => "Write tests",
            Tab::Maintenance => "Maintenance",
            Tab::Report => "Report",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanChoice {
    Sequential,
    Random,
    Butterfly,
    Range,
}

impl ScanChoice {
    fn label(self) -> &'static str {
        match self {
            ScanChoice::Sequential => "Full sequential",
            ScanChoice::Random => "Random sample",
            ScanChoice::Butterfly => "Butterfly (start ↔ end)",
            ScanChoice::Range => "Range",
        }
    }
}

struct ScanForm {
    choice: ScanChoice,
    percent: f64,
    start_gb: f64,
    end_gb: f64,
    block_kib: u32,
    retries: u8,
    pinpoint: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteChoice {
    Full,
    Probe,
    Smr,
}

struct WriteForm {
    choice: WriteChoice,
    samples: u32,
    smr_gib: u32,
    smr_minutes: u32,
    typed_serial: String,
    understood: bool,
    volume: String,
    max_gb: f64,
}

struct RunningJob {
    handle: JobHandle,
    name: String,
}

/// Finished report with where it was saved, or the failure message.
type JobOutcome = Result<(Box<Report>, Option<PathBuf>, Option<String>), String>;

struct LastReport {
    report: Report,
    saved_to: Option<PathBuf>,
    save_error: Option<String>,
}

pub struct DriveTestApp {
    platform: Arc<dyn Platform>,
    demo: bool,
    elevated: bool,
    mode: Mode,
    confirm_read_write: bool,
    drives: Vec<DriveInfo>,
    enum_error: Option<String>,
    last_refresh: Instant,
    selected: Option<u32>,
    tab: Tab,
    health: HashMap<u32, HealthSnapshot>,
    health_pending: HashSet<u32>,
    health_tx: Sender<(DriveInfo, HealthSnapshot)>,
    health_rx: Receiver<(DriveInfo, HealthSnapshot)>,
    jobs: HashMap<u32, RunningJob>,
    views: HashMap<u32, LiveView>,
    reports: HashMap<u32, LastReport>,
    errors: HashMap<u32, String>,
    scan_form: ScanForm,
    write_form: WriteForm,
    maint_form: maintenance::MaintForm,
    speed_form: maintenance::SpeedForm,
    /// Latest stale-data check per drive (feeds the targeted refresh).
    /// Keyed by disk number, with the drive serial it was made for.
    stale: HashMap<u32, (DriveInfo, StaleOutcome)>,
    /// Bad sectors from the latest read scan per drive (feeds remap).
    last_bad: HashMap<u32, (DriveInfo, Vec<u64>)>,
    report_dir: PathBuf,
    recheck_path: String,
}

impl DriveTestApp {
    pub fn new(platform: Arc<dyn Platform>, demo: bool) -> Self {
        let (health_tx, health_rx) = mpsc::channel();
        let mut app = Self {
            elevated: platform.is_elevated(),
            platform,
            demo,
            mode: Mode::ReadOnly,
            confirm_read_write: false,
            drives: Vec::new(),
            enum_error: None,
            last_refresh: Instant::now(),
            selected: None,
            tab: Tab::Overview,
            health: HashMap::new(),
            health_pending: HashSet::new(),
            health_tx,
            health_rx,
            jobs: HashMap::new(),
            views: HashMap::new(),
            reports: HashMap::new(),
            errors: HashMap::new(),
            scan_form: ScanForm {
                choice: ScanChoice::Sequential,
                percent: 5.0,
                start_gb: 0.0,
                end_gb: 10.0,
                block_kib: 1024,
                retries: 2,
                pinpoint: true,
            },
            write_form: WriteForm {
                choice: WriteChoice::Probe,
                samples: 1024,
                smr_gib: 40,
                smr_minutes: 20,
                typed_serial: String::new(),
                understood: false,
                volume: String::new(),
                max_gb: 0.0,
            },
            maint_form: maintenance::MaintForm::default(),
            speed_form: maintenance::SpeedForm::default(),
            stale: HashMap::new(),
            last_bad: HashMap::new(),
            report_dir: default_report_dir(),
            recheck_path: String::new(),
        };
        app.refresh();
        let numbers: Vec<u32> = app.drives.iter().map(|d| d.number).collect();
        for n in numbers {
            app.request_health(n, None);
        }
        if let Some(first) = app.drives.iter().find(|d| !d.is_protected()).or(app.drives.first()) {
            let n = first.number;
            app.select(n, None);
        }
        app
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        match self.platform.enumerate() {
            Ok(drives) => {
                for old in &self.drives {
                    let disk = old.number;
                    if !drives.iter().any(|new| new.number == disk && old.same_device(new)) {
                        self.health.remove(&disk);
                        self.reports.remove(&disk);
                        self.views.remove(&disk);
                        self.errors.remove(&disk);
                        self.last_bad.remove(&disk);
                        self.stale.remove(&disk);
                        if self.selected == Some(disk) {
                            self.selected = None;
                            self.write_form.typed_serial.clear();
                            self.write_form.understood = false;
                            self.maint_form = maintenance::MaintForm::default();
                        }
                    }
                }
                self.drives = drives;
                self.enum_error = None;
            }
            Err(e) => {
                self.enum_error = Some(if self.elevated {
                    format!("Could not list drives: {e}")
                } else {
                    format!("Could not list drives: {e}. Start Drive Test as administrator.")
                });
            }
        }
        if let Some(sel) = self.selected
            && !self.drives.iter().any(|d| d.number == sel)
        {
            self.selected = None;
        }
    }

    fn drive(&self, n: u32) -> Option<&DriveInfo> {
        self.drives.iter().find(|d| d.number == n)
    }

    fn select(&mut self, n: u32, ctx: Option<&egui::Context>) {
        if self.selected != Some(n) {
            self.write_form.typed_serial.clear();
            self.write_form.understood = false;
            self.write_form.volume = self
                .drive(n)
                .and_then(|d| d.volumes.iter().find_map(|v| v.mount_points.first().cloned()))
                .unwrap_or_default();
        }
        self.selected = Some(n);
        if !self.health.contains_key(&n) {
            self.request_health(n, ctx);
        }
    }

    fn request_health(&mut self, disk: u32, ctx: Option<&egui::Context>) {
        let Some(drive) = self.drive(disk).cloned() else { return };
        if !self.health_pending.insert(disk) {
            return;
        }
        let platform = self.platform.clone();
        let tx = self.health_tx.clone();
        let ctx = ctx.cloned();
        std::thread::spawn(move || {
            let snap = platform.read_health(disk).unwrap_or_else(|e| HealthSnapshot::unavailable(e.to_string()));
            let _ = tx.send((drive, snap));
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
    }

    fn start(&mut self, drive: &DriveInfo, kind: JobKind, serial: Option<String>) {
        if self.jobs.contains_key(&drive.number) {
            return;
        }
        let name = kind.name();
        let handle = start_job(
            self.platform.clone(),
            JobRequest {
                drive: drive.clone(),
                kind,
                mode: self.mode,
                confirm_serial: serial,
                report_dir: Some(self.report_dir.clone()),
            },
        );
        self.errors.remove(&drive.number);
        let mut view = LiveView::default();
        view.phase = "Starting".into();
        self.views.insert(drive.number, view);
        self.jobs.insert(drive.number, RunningJob { handle, name });
    }

    /// Moves events from worker threads into the views.
    fn pump(&mut self) {
        while let Ok((drive, snap)) = self.health_rx.try_recv() {
            let disk = drive.number;
            self.health_pending.remove(&disk);
            if self.drive(disk).is_some_and(|current| current.same_device(&drive)) {
                self.health.insert(disk, snap);
            } else {
                self.request_health(disk, None);
            }
        }
        let mut done: Vec<(u32, JobOutcome)> = Vec::new();
        for (disk, job) in &self.jobs {
            let view = self.views.entry(*disk).or_default();
            for event in job.handle.poll() {
                match event {
                    JobEvent::Phase(p) => view.phase = p,
                    JobEvent::Engine(EngineEvent::Pass { name, total_blocks, block_size }) => {
                        view.start_pass(&name, total_blocks, block_size)
                    }
                    JobEvent::Engine(EngineEvent::Blocks(b)) => view.apply(&b),
                    JobEvent::Engine(EngineEvent::Progress(p)) => view.progress = p,
                    JobEvent::Engine(EngineEvent::Log(m)) => view.push_log(m),
                    JobEvent::Temperature { elapsed_s, celsius } => view.temps.push((elapsed_s, celsius)),
                    JobEvent::Finished { report, saved_to, save_error } => {
                        done.push((*disk, Ok((report, saved_to, save_error))))
                    }
                    JobEvent::Failed(e) => done.push((*disk, Err(e.message))),
                }
            }
        }
        for (disk, outcome) in done {
            self.jobs.remove(&disk);
            match outcome {
                Ok((report, saved_to, save_error)) => {
                    if !self.drive(disk).is_some_and(|d| d.same_device(&report.drive)) {
                        continue;
                    }
                    let view = self.views.entry(disk).or_default();
                    view.phase = if report.test.cancelled { "Stopped".into() } else { "Finished".into() };
                    if let Some(s) = &report.scan {
                        view.exact = Some(s.latency.clone());
                        view.reported_curve = Some(s.speed_curve_mb_s.clone());
                        self.last_bad.insert(disk, (report.drive.clone(), s.bad_sectors.clone()));
                    }
                    if let Some(st) = &report.stale {
                        self.stale.insert(disk, (report.drive.clone(), st.clone()));
                    }
                    if let Some(after) = report.health.after.clone()
                        && after.is_available()
                    {
                        self.health.insert(disk, after);
                    }
                    self.reports.insert(disk, LastReport { report: *report, saved_to, save_error });
                }
                Err(message) => {
                    self.views.entry(disk).or_default().phase = "Failed".into();
                    self.errors.insert(disk, message);
                }
            }
        }
    }

    // -------------------------------------------------------------- layout

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Drive Test");
                if self.demo {
                    ui.label(RichText::new("DEMO DRIVES").color(ORANGE).strong())
                        .on_hover_text("Simulated drives. Run on Windows without --demo to test real hardware.");
                }
                ui.separator();
                ui.label("Mode:");
                if ui.selectable_label(self.mode == Mode::ReadOnly, "Read-only").clicked() {
                    self.mode = Mode::ReadOnly;
                }
                let rw = ui
                    .selectable_label(self.mode == Mode::ReadWrite, "Read-write")
                    .on_hover_text("Enables tests that write. The system disk stays blocked.");
                if rw.clicked() && self.mode != Mode::ReadWrite {
                    self.confirm_read_write = true;
                }
                ui.separator();
                if ui.button("⟳ Refresh drives").clicked() {
                    self.refresh();
                }
            });
            ui.add_space(4.0);
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.demo {
                    ui.label("Demo platform");
                } else if self.elevated {
                    ui.label("Running as administrator");
                } else {
                    ui.colored_label(RED, "Not running as administrator — raw disk access will fail");
                }
                ui.separator();
                ui.label("🔒 System disk is write-locked");
                ui.separator();
                ui.label(format!("Reports: {}", self.report_dir.display()));
                if ui.small_button("Open folder").clicked() {
                    open_folder(&self.report_dir);
                }
            });
        });
    }

    fn read_write_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm_read_write {
            return;
        }
        let response = egui::Modal::new(egui::Id::unique("confirm_read_write")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.heading("Enable Read-write mode?");
            ui.add_space(6.0);
            ui.label("Read-write mode enables tests that write to drives:");
            ui.label("• Free-space verify writes test files into free space, then deletes them.");
            ui.label("• Destructive tests erase the whole drive and need its serial number to start.");
            ui.add_space(6.0);
            ui.label(RichText::new("The system disk stays write-locked in every mode.").strong());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Enable Read-write").clicked() {
                    self.mode = Mode::ReadWrite;
                    self.confirm_read_write = false;
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_read_write = false;
                }
            });
        });
        if response.should_close() {
            self.confirm_read_write = false;
        }
    }

    fn drive_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.heading("Drives");
        if let Some(e) = &self.enum_error {
            ui.colored_label(RED, e);
        }
        if self.drives.is_empty() && self.enum_error.is_none() {
            ui.label("No drives found.");
        }
        let drives = self.drives.clone();
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for d in &drives {
                if self.drive_card(ui, d) {
                    self.select(d.number, Some(&ctx));
                }
                ui.add_space(4.0);
            }
        });
    }

    /// Returns true when clicked.
    fn drive_card(&self, ui: &mut egui::Ui, d: &DriveInfo) -> bool {
        let selected = self.selected == Some(d.number);
        let visuals = ui.visuals().clone();
        let frame = egui::Frame::group(ui.style())
            .stroke(if selected {
                Stroke::new(2.0, visuals.selection.stroke.color)
            } else {
                visuals.widgets.noninteractive.bg_stroke
            })
            .fill(if selected { visuals.selection.bg_fill.gamma_multiply(0.25) } else { Color32::TRANSPARENT });
        let inner = frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(d.title()).strong());
            ui.weak(d.summary_line());
            if d.is_protected() {
                ui.colored_label(RED, "🔒 System disk — write-locked");
            } else if d.write_protected {
                ui.colored_label(ORANGE, "Write-protected");
            }
            if let Some(h) = self.health.get(&d.number) {
                if h.is_available() {
                    let v = h.verdict();
                    ui.colored_label(verdict_color(v.level), format!("● Health: {}", v.level.label()));
                } else if let Some(sd) = &h.sd {
                    if sd.warnings.is_empty() {
                        ui.weak("SD card registers read");
                    } else {
                        ui.colored_label(ORANGE, format!("⚠ SD card: {} warning(s)", sd.warnings.len()));
                    }
                } else {
                    ui.weak("No health data from this drive");
                }
            } else if self.health_pending.contains(&d.number) {
                ui.weak("Reading health…");
            }
            if let Some(job) = self.jobs.get(&d.number) {
                let pct = self.views.get(&d.number).map_or(0.0, |v| v.progress.fraction() * 100.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("{} {pct:.0}%", job.name));
                });
            }
        });
        let response = ui.interact(inner.response.rect, egui::Id::unique(("drive_card", d.number)), Sense::click());
        response.clicked()
    }

    fn main_panel(&mut self, ui: &mut egui::Ui) {
        let Some(d) = self.selected.and_then(|n| self.drive(n)).cloned() else {
            ui.centered_and_justified(|ui| ui.label("Select a drive on the left."));
            return;
        };
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading(d.title());
            ui.weak(d.summary_line());
        });
        if d.is_protected() {
            egui::Frame::group(ui.style()).stroke(Stroke::new(1.0, RED)).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.colored_label(RED, format!("🔒 {}", d.role.describe()));
                ui.label(
                    "Raw writes and write tests are never allowed here. The one exception is an opt-in refresh \
                     of selected files (Maintenance tab).",
                );
            });
        }
        self.job_banner(ui, &d);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for t in Tab::ALL {
                ui.selectable_value(&mut self.tab, t, t.label());
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
            Tab::Overview => self.overview_tab(ui, &d),
            Tab::Health => self.health_tab(ui, &d),
            Tab::Scan => self.scan_tab(ui, &d),
            Tab::WriteTests => self.write_tab(ui, &d),
            Tab::Maintenance => self.maintenance_tab(ui, &d),
            Tab::Report => self.report_tab(ui, &d),
        });
    }

    fn job_banner(&self, ui: &mut egui::Ui, d: &DriveInfo) {
        let Some(job) = self.jobs.get(&d.number) else {
            if let Some(e) = self.errors.get(&d.number) {
                ui.colored_label(RED, format!("Last test failed: {e}"));
            }
            return;
        };
        let view = self.views.get(&d.number);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.strong(&job.name);
            if let Some(v) = view {
                if v.phase != job.name {
                    ui.label(&v.phase);
                }
                if let Some((_, t)) = v.temps.last() {
                    ui.label(format!("· {t} °C"));
                }
            }
            if job.handle.cancel_requested() {
                ui.weak("Stopping…");
            } else if ui.button("⏹ Stop").clicked() {
                job.handle.cancel();
            }
        });
        if let Some(v) = view {
            let p = v.progress;
            let eta = p.eta().map(fmt_duration).unwrap_or_else(|| "--:--".into());
            let text = if p.recent_mb_s > 0.0 {
                format!("{} — {:.1}% · {:.1} MB/s · ETA {eta}", v.pass, p.fraction() * 100.0, p.recent_mb_s)
            } else {
                format!("{} — {:.1}%", v.pass, p.fraction() * 100.0)
            };
            ui.add(egui::ProgressBar::new(p.fraction()).text(text));
        }
    }

    // -------------------------------------------------------------- tabs

    fn overview_tab(&mut self, ui: &mut egui::Ui, d: &DriveInfo) {
        egui::Grid::new("identity").num_columns(2).striped(true).show(ui, |ui| {
            let rows: [(&str, String); 12] = [
                ("Model", d.model.clone()),
                ("Vendor", d.vendor.clone()),
                ("Serial", d.serial.clone()),
                ("Firmware", d.firmware.clone()),
                ("Bus", d.bus.label().into()),
                ("Media", d.media.label().into()),
                ("Capacity", format!("{} ({} bytes)", format_bytes(d.size_bytes), d.size_bytes)),
                ("Sector size", format!("{} logical / {} physical", d.logical_sector_size, d.physical_sector_size)),
                ("Removable", yes_no(d.removable).into()),
                ("Write-protected", yes_no(d.write_protected).into()),
                ("Device", d.device_path.clone()),
                ("Adapter", d.adapter.clone().unwrap_or_default()),
            ];
            for (k, v) in rows {
                ui.weak(k);
                ui.label(if v.is_empty() { "—".to_string() } else { v });
                ui.end_row();
            }
        });

        ui.add_space(10.0);
        ui.strong("Volumes");
        if d.volumes.is_empty() {
            ui.weak("No volumes.");
        } else {
            egui::Grid::new("volumes").num_columns(4).striped(true).show(ui, |ui| {
                for v in &d.volumes {
                    ui.label(v.display_name());
                    ui.label(&v.file_system);
                    ui.label(format!("{} free", format_bytes(v.free_bytes)));
                    ui.label(format!("of {}", format_bytes(v.size_bytes)));
                    ui.end_row();
                }
            });
        }

        ui.add_space(10.0);
        ui.strong("What can run on this drive");
        let policy = Policy::new(self.mode);
        egui::Grid::new("capabilities").num_columns(2).show(ui, |ui| {
            ui.weak("Health data");
            let health_text = match self.health.get(&d.number) {
                Some(h) if h.is_available() => h.source.label().to_string(),
                Some(h) => h.unavailable_reason.clone().unwrap_or_else(|| "Not available".into()),
                None => format!("{} (not read yet)", d.health_source.label()),
            };
            ui.label(health_text);
            ui.end_row();
            ui.weak("Read-only tests");
            ui.colored_label(GREEN, "Allowed");
            ui.end_row();
            if d.is_protected() {
                ui.weak("SSD file refresh");
                ui.label("Opt-in only, selected files (Maintenance tab)");
                ui.end_row();
            }
            for (label, tier) in
                [("Non-destructive writes", Tier::NonDestructive), ("Destructive tests", Tier::Destructive)]
            {
                ui.weak(label);
                match policy.check(d, tier) {
                    Ok(()) => ui.colored_label(GREEN, "Allowed"),
                    Err(e) => ui.colored_label(ORANGE, e.to_string()),
                };
                ui.end_row();
            }
        });

        ui.add_space(10.0);
        match self.health.get(&d.number) {
            Some(h) => {
                let v = h.verdict();
                ui.horizontal(|ui| {
                    ui.strong("Health:");
                    ui.colored_label(verdict_color(v.level), RichText::new(v.level.label()).strong());
                    if let Some(life) = v.remaining_life_percent {
                        ui.label(format!("· {life}% life remaining"));
                    }
                });
                for r in &v.reasons {
                    ui.label(format!("• {r}"));
                }
                let smr = assess_smr(d.media, &d.model, h.identify(), None);
                if smr.verdict != RecordingVerdict::NotApplicable {
                    ui.horizontal(|ui| {
                        ui.strong("Recording:");
                        ui.label(format!("{} — {}", smr.headline(), smr.evidence));
                    });
                }
            }
            None => {
                ui.horizontal(|ui| {
                    if self.health_pending.contains(&d.number) {
                        ui.spinner();
                        ui.label("Reading health data…");
                    } else if ui.button("Read health").clicked() {
                        self.request_health(d.number, Some(ui.ctx()));
                    }
                });
            }
        }

        if let Some(last) = self.reports.get(&d.number) {
            ui.add_space(10.0);
            ui.strong(format!("Last test: {}", last.report.test.kind));
            for f in last.report.findings.iter().take(6) {
                ui.label(format!("• {f}"));
            }
        }
    }

    fn health_tab(&mut self, ui: &mut egui::Ui, d: &DriveInfo) {
        let busy = self.jobs.contains_key(&d.number);
        ui.horizontal(|ui| {
            if self.health_pending.contains(&d.number) {
                ui.spinner();
                ui.label("Reading…");
            } else if ui.button("⟳ Read health").clicked() {
                self.request_health(d.number, Some(ui.ctx()));
            }
            ui.separator();
            let can_self_test = match d.health_source {
                HealthSource::AtaSmart | HealthSource::SatSmart => {
                    self.health.get(&d.number).is_some_and(|h| h.ata.is_some())
                }
                HealthSource::NvmeLog => self.health.get(&d.number).is_some_and(|h| h.nvme.is_some()),
                HealthSource::None => false,
            };
            for (label, kind) in
                [("Short self-test", SelfTestKind::Short), ("Extended self-test", SelfTestKind::Extended)]
            {
                let r = ui
                    .add_enabled(!busy && can_self_test, egui::Button::new(label))
                    .on_hover_text("The drive tests itself; nothing is written. Allowed on every drive.")
                    .on_disabled_hover_text(if busy {
                        "A test is already running on this drive."
                    } else {
                        "Available on SATA and NVMe drives that report health data."
                    });
                if r.clicked() {
                    self.start(d, JobKind::SelfTest(kind), None);
                }
            }
        });
        ui.add_space(6.0);
        let Some(h) = self.health.get(&d.number) else {
            ui.weak("No health data read yet.");
            return;
        };
        let v = h.verdict();
        ui.horizontal(|ui| {
            ui.label(RichText::new(v.level.label()).size(22.0).strong().color(verdict_color(v.level)));
            if let Some(life) = v.remaining_life_percent {
                ui.label(format!("{life}% life remaining (drive's own estimate)"));
            }
            if let Some(t) = h.temperature_c {
                ui.label(format!("· {t} °C"));
            }
        });
        if let Some(score) = h.health_score(d.media) {
            ui.collapsing(format!("Custom health score: {}% ({})", score.percent, score.status.label()), |ui| {
                ui.label(format!("Base {}% ({})", score.base_percent, score.base_source));
                for reason in score.adjustments {
                    ui.label(reason);
                }
            });
        } else {
            ui.weak("Custom health score unavailable");
        }
        for r in &v.reasons {
            ui.label(format!("• {r}"));
        }
        ui.weak(format!(
            "Source: {} · read {}",
            h.source.label(),
            h.taken_at.with_timezone(&chrono_local()).format("%H:%M:%S")
        ));
        ui.add_space(8.0);

        if let Some(n) = &h.nvme {
            egui::Grid::new("nvme").num_columns(2).striped(true).show(ui, |ui| {
                let rows: [(&str, String); 11] = [
                    ("Percentage used", format!("{}%", n.percentage_used)),
                    (
                        "Available spare",
                        format!("{}% (threshold {}%)", n.available_spare_percent, n.available_spare_threshold_percent),
                    ),
                    ("Data written", format_bytes(n.bytes_written())),
                    ("Data read", format_bytes(n.bytes_read())),
                    ("Power-on hours", n.power_on_hours.to_string()),
                    ("Power cycles", n.power_cycles.to_string()),
                    ("Unsafe shutdowns", n.unsafe_shutdowns.to_string()),
                    ("Media / integrity errors", n.media_errors.to_string()),
                    ("Error log entries", n.error_log_entries.to_string()),
                    ("Critical warning flags", format!("0x{:02X}", n.critical_warning)),
                    ("Temperature", format!("{} °C", n.temperature_c)),
                ];
                for (k, val) in rows {
                    ui.weak(k);
                    ui.label(val);
                    ui.end_row();
                }
            });
        }
        if h.ata.is_none()
            && let Some(st) = &h.nvme_self_test
        {
            ui.label(format!(
                "Self-test: {}{}",
                st.description,
                st.percent_remaining.map(|p| format!(" ({p}% remaining)")).unwrap_or_default()
            ));
        }
        if let Some(sd) = &h.sd {
            maintenance::show_sd(ui, sd);
        }
        if let Some(a) = &h.ata {
            if let Some(st) = &a.self_test {
                ui.label(format!(
                    "Self-test: {}{}",
                    st.description,
                    st.percent_remaining.map(|p| format!(" ({p}% remaining)")).unwrap_or_default()
                ));
            }
            if let Some(id) = &a.identify {
                ui.weak(format!(
                    "Rotation: {} · TRIM: {} · Zoned: {:?}",
                    if id.non_rotating {
                        "solid state".to_string()
                    } else {
                        id.rotation_rate_rpm.map_or("not reported".into(), |r| format!("{r} rpm"))
                    },
                    yes_no(id.trim_supported),
                    id.zoned
                ));
            }
            ui.add_space(6.0);
            egui::Grid::new("smart").num_columns(6).striped(true).show(ui, |ui| {
                for head in ["ID", "Attribute", "Value", "Worst", "Threshold", "Raw"] {
                    ui.strong(head);
                }
                ui.end_row();
                for x in &a.attributes {
                    let color = if x.failing_now { RED } else { ui.visuals().text_color() };
                    ui.colored_label(color, x.id.to_string());
                    ui.colored_label(color, &x.name);
                    ui.label(x.current.to_string());
                    ui.label(x.worst.to_string());
                    ui.label(x.threshold.to_string());
                    ui.label(x.raw.to_string());
                    ui.end_row();
                }
            });
        }
    }

    fn scan_tab(&mut self, ui: &mut egui::Ui, d: &DriveInfo) {
        let busy = self.jobs.contains_key(&d.number);
        ui.label("Times every block it reads. Read-only, so it is allowed on every drive, including the system disk.");
        ui.add_space(4.0);
        ui.add_enabled_ui(!busy, |ui| {
            let f = &mut self.scan_form;
            egui::Grid::new("scan_form").num_columns(2).show(ui, |ui| {
                ui.label("Mode");
                egui::ComboBox::from_id_salt("scan_mode").selected_text(f.choice.label()).show_ui(ui, |ui| {
                    for c in [ScanChoice::Sequential, ScanChoice::Random, ScanChoice::Butterfly, ScanChoice::Range] {
                        ui.selectable_value(&mut f.choice, c, c.label());
                    }
                });
                ui.end_row();
                match f.choice {
                    ScanChoice::Random => {
                        ui.label("Sample");
                        ui.add(egui::Slider::new(&mut f.percent, 0.1..=50.0).suffix(" %").logarithmic(true));
                        ui.end_row();
                    }
                    ScanChoice::Range => {
                        let max_gb = d.size_bytes as f64 / 1e9;
                        ui.label("From");
                        ui.add(egui::DragValue::new(&mut f.start_gb).range(0.0..=max_gb).suffix(" GB").speed(1.0));
                        ui.end_row();
                        ui.label("To");
                        ui.add(egui::DragValue::new(&mut f.end_gb).range(0.0..=max_gb).suffix(" GB").speed(1.0));
                        ui.end_row();
                    }
                    _ => {}
                }
                ui.label("Block size");
                egui::ComboBox::from_id_salt("scan_block").selected_text(format!("{} KiB", f.block_kib)).show_ui(
                    ui,
                    |ui| {
                        for kib in [256u32, 1024, 4096] {
                            ui.selectable_value(&mut f.block_kib, kib, format!("{kib} KiB"));
                        }
                    },
                );
                ui.end_row();
                ui.label("Retries");
                ui.add(egui::DragValue::new(&mut f.retries).range(0..=5));
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut f.pinpoint, "Pinpoint failing blocks to single sectors");
                ui.end_row();
            });
        });
        ui.horizontal(|ui| {
            if busy {
                ui.weak("A test is running on this drive — use Stop above.");
            } else if ui.button(RichText::new("▶ Start read scan").strong()).clicked() {
                let f = &self.scan_form;
                let mode = match f.choice {
                    ScanChoice::Sequential => ScanMode::Sequential,
                    ScanChoice::Random => ScanMode::RandomSample { percent: f.percent },
                    ScanChoice::Butterfly => ScanMode::Butterfly,
                    ScanChoice::Range => ScanMode::Range {
                        start_byte: (f.start_gb.min(f.end_gb) * 1e9) as u64,
                        end_byte: (f.end_gb.max(f.start_gb) * 1e9) as u64,
                    },
                };
                let cfg = ScanConfig {
                    mode,
                    block_size: f.block_kib * 1024,
                    retries: f.retries,
                    pinpoint_errors: f.pinpoint,
                    ..Default::default()
                };
                self.start(d, JobKind::ReadScan(cfg), None);
            }
        });
        ui.add_space(8.0);
        ui.add_enabled_ui(!busy, |ui| {
            let last_scan = self.reports.get(&d.number).filter(|last| {
                last.report.scan.as_ref().is_some_and(|s| {
                    !s.bad_sectors.is_empty() || !s.slow_offsets.is_empty() || !s.unresolved_blocks.is_empty()
                })
            });
            if ui.add_enabled(last_scan.is_some(), egui::Button::new("Recheck last scan")).clicked()
                && let Some(last) = last_scan
            {
                match drivetest_core::recheck::RecheckPlan::from_report(&last.report, self.platform.name()) {
                    Ok(plan) => self.start(d, JobKind::Recheck(plan), None),
                    Err(error) => {
                        self.errors.insert(d.number, error);
                    }
                }
            }
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut self.recheck_path).on_hover_text("Path to a saved JSON scan report");
                if ui.button("Recheck saved report").clicked() {
                    let plan = Report::load(std::path::Path::new(self.recheck_path.trim()))
                        .map_err(|error| error.to_string())
                        .and_then(|report| {
                            drivetest_core::recheck::RecheckPlan::from_report(&report, self.platform.name())
                        });
                    match plan {
                        Ok(plan) => self.start(d, JobKind::Recheck(plan), None),
                        Err(error) => {
                            self.errors.insert(d.number, error);
                        }
                    }
                }
            });
            if let Some(error) = self.errors.get(&d.number) {
                ui.colored_label(RED, error);
            }
        });
        self.live_view(ui, d);
    }

    fn live_view(&self, ui: &mut egui::Ui, d: &DriveInfo) {
        let Some(v) = self.views.get(&d.number) else {
            if let Some(last) = self.reports.get(&d.number) {
                let replay = LiveView::from_report(&last.report);
                show_view(ui, &replay, d);
            }
            return;
        };
        show_view(ui, v, d);
    }

    fn write_tab(&mut self, ui: &mut egui::Ui, d: &DriveInfo) {
        let policy = Policy::new(self.mode);
        let busy = self.jobs.contains_key(&d.number);
        ui.label("Free-space verification writes temporary files. On fake-capacity media, those writes can damage existing files.");

        if d.is_protected() {
            ui.colored_label(RED, "Write tests are blocked on this drive.");
            ui.label(d.role.describe());
            return;
        }
        if busy {
            ui.weak("A test is running on this drive — use Stop above.");
            self.live_view(ui, d);
            return;
        }
        if self.mode == Mode::ReadOnly {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label("Write tests need Read-write mode (switch it in the top bar).");
                ui.weak("Destructive tests are hidden in Read-only mode.");
            });
            ui.add_space(6.0);
        }

        // ---- free-space verify
        ui.heading("Free-space verify (non-destructive)");
        ui.label("Fills free space with test files, reads them back and deletes them. Existing files are not touched.");
        let mounts: Vec<String> = d.volumes.iter().flat_map(|v| v.mount_points.clone()).collect();
        if mounts.is_empty() {
            ui.weak("This drive has no mounted volume.");
        } else {
            let f = &mut self.write_form;
            if !mounts.contains(&f.volume) {
                f.volume = mounts[0].clone();
            }
            ui.horizontal(|ui| {
                ui.label("Volume");
                egui::ComboBox::from_id_salt("fs_volume").selected_text(&f.volume).show_ui(ui, |ui| {
                    for m in &mounts {
                        ui.selectable_value(&mut f.volume, m.clone(), m);
                    }
                });
                ui.label("Write at most");
                ui.add(egui::DragValue::new(&mut f.max_gb).range(0.0..=100_000.0).suffix(" GB").speed(1.0));
                ui.weak("(0 = all free space)");
            });
            let check = policy.check(d, Tier::NonDestructive);
            let reason = check
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "A test is already running on this drive.".into());
            let start = ui
                .add_enabled(check.is_ok() && !busy, egui::Button::new("▶ Start free-space verify"))
                .on_disabled_hover_text(reason);
            if start.clicked() {
                let config = FreeSpaceConfig {
                    max_bytes: (self.write_form.max_gb > 0.0).then_some((self.write_form.max_gb * 1e9) as u64),
                    ..Default::default()
                };
                let mount = self.write_form.volume.clone();
                self.start(d, JobKind::FreeSpaceVerify { mount, config }, None);
            }
        }

        self.speed_class_section(ui, d, busy);

        // ---- destructive
        if self.mode == Mode::ReadWrite {
            ui.add_space(14.0);
            ui.heading(RichText::new("Destructive tests").color(RED));
            let f = &mut self.write_form;
            ui.radio_value(
                &mut f.choice,
                WriteChoice::Probe,
                "Fake-capacity quick probe — writes sampled positions, fast",
            );
            ui.radio_value(
                &mut f.choice,
                WriteChoice::Full,
                "Full write + verify — every block; finds bad sectors, fakes and the cache cliff",
            );
            ui.radio_value(&mut f.choice, WriteChoice::Smr, "SMR write test (HDDs) — sustained random writes");
            match f.choice {
                WriteChoice::Probe => {
                    ui.horizontal(|ui| {
                        ui.label("Positions");
                        ui.add(egui::DragValue::new(&mut f.samples).range(64..=65_536));
                    });
                }
                WriteChoice::Smr => {
                    ui.horizontal(|ui| {
                        ui.label("Up to");
                        ui.add(egui::DragValue::new(&mut f.smr_gib).range(1..=1000).suffix(" GiB"));
                        ui.label("or");
                        ui.add(egui::DragValue::new(&mut f.smr_minutes).range(1..=240).suffix(" min"));
                    });
                }
                WriteChoice::Full => {}
            }
            let kind = match f.choice {
                WriteChoice::Full => WriteTestKind::FullWriteVerify,
                WriteChoice::Probe => WriteTestKind::FakeCapacityProbe { samples: f.samples },
                WriteChoice::Smr => WriteTestKind::SmrProbe { max_gib: f.smr_gib, max_minutes: f.smr_minutes },
            };
            let bytes = kind.estimated_bytes_written(d.size_bytes, 1 << 20);
            egui::Frame::group(ui.style()).stroke(Stroke::new(1.5, RED)).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.colored_label(RED, RichText::new("Everything on this drive will be erased.").strong());
                ui.label(format!("{} · serial {} · {}", d.title(), d.serial, format_bytes(d.size_bytes)));
                for v in &d.volumes {
                    ui.label(format!("Volume {} will be dismounted and overwritten.", v.display_name()));
                }
                ui.label(format!("Writes up to {}.", format_bytes(bytes)));
                if d.media != MediaKind::Hdd {
                    ui.weak("Flash media wears with every full write; a full pass uses one write cycle.");
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Type the serial number to enable Start:");
                    ui.text_edit_singleline(&mut f.typed_serial);
                });
                ui.checkbox(&mut f.understood, "I understand all data on this drive will be destroyed");
            });
            let check = policy.check(d, Tier::Destructive);
            let serial_ok = d.serial_matches(&f.typed_serial);
            let ready = check.is_ok() && serial_ok && f.understood && !busy;
            let reason = match (&check, serial_ok, f.understood, busy) {
                (Err(e), ..) => e.to_string(),
                (_, false, ..) => "Type the drive's serial number exactly.".into(),
                (_, _, false, _) => "Tick the confirmation box.".into(),
                _ => "A test is already running on this drive.".into(),
            };
            let start = ui
                .add_enabled(ready, egui::Button::new(RichText::new("▶ Start destructive test").color(RED).strong()))
                .on_disabled_hover_text(reason);
            if start.clicked() {
                let serial = f.typed_serial.clone();
                f.typed_serial.clear();
                f.understood = false;
                self.start(d, JobKind::WriteTest(WriteTestConfig::new(kind)), Some(serial));
            }
        }

        ui.add_space(10.0);
        self.live_view(ui, d);
    }

    fn report_tab(&mut self, ui: &mut egui::Ui, d: &DriveInfo) {
        if let Some(e) = self.errors.get(&d.number) {
            ui.colored_label(RED, format!("Last test failed: {e}"));
        }
        let Some(last) = self.reports.get(&d.number) else {
            ui.label("No report for this drive yet in this session.");
            ui.weak(format!("Reports are saved as JSON in {}", self.report_dir.display()));
            return;
        };
        let r = &last.report;
        ui.horizontal(|ui| {
            ui.heading(&r.test.kind);
            use drivetest_core::report::JobStatus;
            let status = r.status();
            let color = match status {
                JobStatus::Passed => GREEN,
                JobStatus::Problems | JobStatus::Error => RED,
                JobStatus::Incomplete | JobStatus::Unsupported => ORANGE,
            };
            ui.colored_label(color, RichText::new(status.label()).strong());
        });
        ui.weak(format!(
            "Started {} · {} test · run in {} mode",
            r.test.started_at.with_timezone(&chrono_local()).format("%Y-%m-%d %H:%M:%S"),
            r.test.tier.label(),
            r.test.mode.label()
        ));
        match (&last.saved_to, &last.save_error) {
            (Some(p), _) => {
                ui.horizontal(|ui| {
                    ui.label(format!("Saved: {}", p.display()));
                    if ui.small_button("Open folder").clicked() {
                        open_folder(p.parent().unwrap_or(&self.report_dir));
                    }
                });
            }
            (None, Some(e)) => {
                ui.colored_label(RED, e);
            }
            _ => {}
        }
        ui.add_space(8.0);
        ui.strong("Findings");
        for f in &r.findings {
            ui.label(format!("• {f}"));
        }
        maintenance::report_extras(ui, r);
        if !r.health.changes.is_empty() {
            ui.add_space(8.0);
            ui.strong("Health counters that rose during the test");
            egui::Grid::new("changes").num_columns(3).striped(true).show(ui, |ui| {
                for c in &r.health.changes {
                    ui.label(&c.metric);
                    ui.label(format!("{} → {}", c.before, c.after));
                    ui.colored_label(RED, "rose during test");
                    ui.end_row();
                }
            });
        }
        if let Some(s) = &r.scan {
            ui.add_space(8.0);
            ui.strong("Scan");
            ui.label(format!(
                "{} · {} of {} blocks · {:.1} MB/s average · {} slow blocks · {} weak regions",
                s.mode.label(),
                s.scanned_blocks,
                s.total_blocks,
                s.average_mb_s,
                s.slow_blocks,
                s.weak_regions.len()
            ));
            for w in &s.weak_regions {
                ui.label(format!(
                    "  Weak region {} – {}: {:.0} MB/s (neighbours {:.0} MB/s)",
                    format_bytes(w.start_byte),
                    format_bytes(w.end_byte),
                    w.mb_s,
                    w.expected_mb_s
                ));
            }
            if !s.bad_sectors.is_empty() {
                ui.add_space(6.0);
                ui.strong(format!("Bad sectors ({})", s.bad_sectors.len()));
                egui::Grid::new("bad").num_columns(2).striped(true).show(ui, |ui| {
                    ui.strong("LBA");
                    ui.strong("Occupied by");
                    ui.end_row();
                    if s.bad_sector_files.is_empty() {
                        for lba in s.bad_sectors.iter().take(500) {
                            ui.label(lba.to_string());
                            ui.weak("(not mapped)");
                            ui.end_row();
                        }
                    } else {
                        for sf in s.bad_sector_files.iter().take(500) {
                            ui.label(sf.lba.to_string());
                            ui.label(&sf.location);
                            ui.end_row();
                        }
                    }
                });
            }
        }
        if let Some(w) = &r.write_test {
            ui.add_space(8.0);
            ui.strong(w.kind.label());
            ui.label(format!(
                "Written {} · write errors {} · corrupt {} · misplaced {} · unreadable {}",
                format_bytes(w.bytes_written),
                w.write_errors,
                w.corrupt_units,
                w.aliased_units,
                w.unreadable_units
            ));
            if let Some(c) = &w.capacity {
                ui.colored_label(if c.genuine && !c.defects_found { GREEN } else { RED }, &c.summary);
            }
            if let Some(smr) = &w.smr {
                let rates: Vec<String> = smr.windows_mb_s.iter().map(|r| format!("{r:.0}")).collect();
                ui.label(format!("Random-write MB/s per {:.0} s window: {}", smr.window_seconds, rates.join(", ")));
            }
        }
        if let Some(fsr) = &r.free_space {
            ui.add_space(8.0);
            ui.strong("Free-space verify");
            ui.label(format!(
                "{} · wrote {} at {:.1} MB/s · verified {} at {:.1} MB/s · test files deleted: {}",
                fsr.mount,
                format_bytes(fsr.bytes_written),
                fsr.write_mb_s,
                format_bytes(fsr.bytes_verified),
                fsr.read_mb_s,
                yes_no(fsr.cleanup_ok)
            ));
        }
    }
}

fn show_view(ui: &mut egui::Ui, v: &LiveView, d: &DriveInfo) {
    if v.pass.is_empty() && !v.has_grid() {
        return;
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.strong(&v.pass);
        if !v.phase.is_empty() {
            ui.weak(format!("· {}", v.phase));
        }
        if let Some((_, t)) = v.temps.last() {
            ui.weak(format!("· {t} °C"));
        }
    });
    if v.has_grid() {
        view::draw_grid(ui, v);
        view::draw_legend(ui, v);
        ui.add_space(6.0);
        let title = if v.pass.to_lowercase().contains("writ") {
            "Write speed across the drive"
        } else {
            "Read speed across the drive"
        };
        view::draw_curve(ui, v, title);
        if d.media == MediaKind::Ssd && v.zero_blocks > 0 {
            ui.weak("Grey-blue cells read as zeros (trimmed or never written); SSD speed there is not meaningful.");
        }
        ui.add_space(6.0);
        view::draw_stats(ui, v);
    } else {
        ui.add(egui::ProgressBar::new(v.progress.fraction()).show_percentage());
    }
    if !v.log.is_empty() {
        egui::CollapsingHeader::new(format!("Log ({})", v.log.len())).id_salt(("log", d.number)).show(ui, |ui| {
            for line in v.log.iter().rev().take(100) {
                ui.monospace(line);
            }
        });
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "Yes" } else { "No" }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

fn chrono_local() -> chrono::Local {
    chrono::Local
}

fn open_folder(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

impl eframe::App for DriveTestApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump();
        self.top_bar(ui);
        self.status_bar(ui);
        egui::Panel::left("drive_list")
            .resizable(true)
            .default_size(300.0)
            .min_size(240.0)
            .show(ui, |ui| self.drive_list(ui));
        egui::CentralPanel::default().show(ui, |ui| self.main_panel(ui));
        self.read_write_modal(&ctx);

        if !self.jobs.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            if self.last_refresh.elapsed() >= AUTO_REFRESH {
                self.refresh();
            }
            ctx.request_repaint_after(Duration::from_secs(2));
        }
    }
}
