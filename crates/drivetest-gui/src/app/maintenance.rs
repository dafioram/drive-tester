//! Maintenance tab (stale-data check, SSD refresh, block refresh, remap),
//! the speed-class section of the Write tests tab, SD register display and
//! report details for these tests.

use std::collections::BTreeSet;

use drivetest_core::refresh::{FileRefreshConfig, RefreshTargets};
use drivetest_core::report::Report;
use drivetest_core::sd::SdInfo;
use drivetest_core::speedclass::{CLASSES, SpeedClassConfig};
use drivetest_core::stale::StaleConfig;

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RefreshChoice {
    SlowFiles,
    Folder,
    WholeVolume,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RawChoice {
    RefreshAll,
    RefreshSlow,
    Remap,
}

pub struct MaintForm {
    refresh_choice: RefreshChoice,
    folder: String,
    system_opt_in: bool,
    raw_choice: RawChoice,
    slower_ms: f64,
    remap_use_scan: bool,
    typed_serial: String,
    understood: bool,
}

impl Default for MaintForm {
    fn default() -> Self {
        Self {
            refresh_choice: RefreshChoice::SlowFiles,
            folder: String::new(),
            system_opt_in: false,
            raw_choice: RawChoice::RefreshSlow,
            slower_ms: 50.0,
            remap_use_scan: true,
            typed_serial: String::new(),
            understood: false,
        }
    }
}

pub struct SpeedForm {
    claimed: BTreeSet<String>,
    test_gb: f64,
    prefilled_for: Option<u32>,
}

impl Default for SpeedForm {
    fn default() -> Self {
        Self { claimed: BTreeSet::new(), test_gb: 2.0, prefilled_for: None }
    }
}

/// SD card identity and ratings from its registers.
pub fn show_sd(ui: &mut egui::Ui, sd: &SdInfo) {
    ui.add_space(6.0);
    ui.strong("SD card registers");
    if !sd.sources.is_empty() {
        ui.weak(format!("From: {}", sd.sources.join(", ")));
    }
    egui::Grid::new("sd_registers").num_columns(2).striped(true).show(ui, |ui| {
        let rows: [(&str, Option<String>); 7] = [
            (
                "Manufacturer",
                sd.manufacturer_id.map(|m| format!("0x{m:02X} {}", sd.manufacturer.clone().unwrap_or_default())),
            ),
            ("OEM ID", sd.oem_id.clone()),
            ("Product name", sd.product_name.clone()),
            ("Revision", sd.revision.clone()),
            ("Serial", sd.serial.map(|s| format!("0x{s:08X}"))),
            ("Manufactured", sd.manufactured.clone()),
            ("Capacity (CSD)", sd.csd_capacity_bytes.map(format_bytes)),
        ];
        for (k, v) in rows {
            if let Some(v) = v {
                ui.weak(k);
                ui.label(v);
                ui.end_row();
            }
        }
        if !sd.claims.is_empty() {
            ui.weak("Speed ratings");
            ui.label(sd.claims.labels().join(" · "));
            ui.end_row();
        }
    });
    for w in &sd.warnings {
        ui.colored_label(ORANGE, format!("⚠ {w}"));
    }
    for n in &sd.notes {
        ui.weak(n);
    }
}

fn mounts_of(d: &DriveInfo) -> Vec<String> {
    d.volumes.iter().flat_map(|v| v.mount_points.clone()).collect()
}

impl DriveTestApp {
    /// Volume picker shared by the Write tests and Maintenance tabs.
    fn volume_picker(&mut self, ui: &mut egui::Ui, d: &DriveInfo, id: &str) -> Option<String> {
        let mounts = mounts_of(d);
        if mounts.is_empty() {
            ui.weak("This drive has no mounted volume.");
            return None;
        }
        let f = &mut self.write_form;
        if !mounts.contains(&f.volume) {
            f.volume = mounts[0].clone();
        }
        ui.horizontal(|ui| {
            ui.label("Volume");
            egui::ComboBox::from_id_salt(id).selected_text(&f.volume).show_ui(ui, |ui| {
                for m in &mounts {
                    ui.selectable_value(&mut f.volume, m.clone(), m);
                }
            });
        });
        Some(f.volume.clone())
    }

    pub(super) fn speed_class_section(&mut self, ui: &mut egui::Ui, d: &DriveInfo, busy: bool) {
        ui.add_space(14.0);
        ui.heading("Speed-class check (non-destructive)");
        ui.label(
            "Writes a temporary file, measures sustained write, read and random 4K speed, compares them with the \
             card's ratings, then deletes the file. Results are indicative: official tests use a freshly erased card.",
        );
        if mounts_of(d).is_empty() {
            ui.weak("This drive has no mounted volume.");
            return;
        }
        if self.speed_form.prefilled_for != Some(d.number) {
            self.speed_form.claimed = self
                .health
                .get(&d.number)
                .and_then(|h| h.sd.as_ref())
                .map(|sd| sd.claims.labels())
                .unwrap_or_default()
                .into_iter()
                .collect();
            self.speed_form.prefilled_for = Some(d.number);
        }
        let mount = self.write_form.volume.clone();
        let form = &mut self.speed_form;
        ui.horizontal_wrapped(|ui| {
            ui.label("Ratings to check:");
            for c in CLASSES {
                let mut on = form.claimed.contains(c);
                if ui.checkbox(&mut on, c).changed() {
                    if on {
                        form.claimed.insert(c.to_string());
                    } else {
                        form.claimed.remove(c);
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Test file");
            ui.add(egui::DragValue::new(&mut form.test_gb).range(0.5..=64.0).suffix(" GB").speed(0.1));
            ui.weak(format!("on {mount}"));
        });
        let check = Policy::new(self.mode).check(d, Tier::NonDestructive);
        let reason = check.as_ref().err().map(|e| e.to_string()).unwrap_or_else(|| "A test is already running.".into());
        let start = ui
            .add_enabled(check.is_ok() && !busy, egui::Button::new("▶ Start speed-class check"))
            .on_disabled_hover_text(reason);
        if start.clicked() {
            let config = SpeedClassConfig {
                test_bytes: (self.speed_form.test_gb * 1e9) as u64,
                claimed: self.speed_form.claimed.iter().cloned().collect(),
                ..Default::default()
            };
            self.start(d, JobKind::SpeedClass { mount, config }, None);
        }
    }

    pub(super) fn maintenance_tab(&mut self, ui: &mut egui::Ui, d: &DriveInfo) {
        let busy = self.jobs.contains_key(&d.number);
        if busy {
            ui.weak("A test is running on this drive — use Stop above.");
            self.live_view(ui, d);
            return;
        }

        // ---- stale-data check
        ui.heading("Stale-data check (read-only)");
        ui.label(
            "Times reads of old and new files. On TLC / QLC SSDs, data left unrewritten for a long time can read \
             much slower. Nothing is written, so this is allowed on every drive.",
        );
        let mount = self.volume_picker(ui, d, "maint_volume");
        if let Some(m) = &mount
            && ui.button("▶ Start stale-data check").clicked()
        {
            self.start(d, JobKind::StaleCheck { mount: m.clone(), config: StaleConfig::default() }, None);
            return;
        }
        // Only results made for this exact drive (serial) and volume are used.
        let stale = self
            .stale
            .get(&d.number)
            .filter(|(drive, s)| d.matches_report_device(drive) && Some(&s.mount) == mount.as_ref())
            .map(|(_, s)| s.clone());
        if let Some(st) = &stale {
            ui.add_space(4.0);
            ui.colored_label(if st.stale_detected { ORANGE } else { GREEN }, &st.verdict);
            if let Some(n) = &st.media_note {
                ui.weak(n);
            }
            egui::Grid::new("stale_buckets").num_columns(3).striped(true).show(ui, |ui| {
                ui.strong("Last written");
                ui.strong("Files");
                ui.strong("Median read");
                ui.end_row();
                for b in &st.buckets {
                    ui.label(&b.label);
                    ui.label(b.files.to_string());
                    ui.label(if b.files > 0 { format!("{:.0} MB/s", b.median_mb_s) } else { "—".into() });
                    ui.end_row();
                }
            });
            if !st.slow_files.is_empty() {
                egui::CollapsingHeader::new(format!("Slow files ({})", st.slow_files.len()))
                    .id_salt("slow_files")
                    .show(ui, |ui| {
                        for f in st.slow_files.iter().take(100) {
                            ui.label(format!("{:.0} MB/s · {} days · {}", f.mb_s, f.age_days, f.display));
                        }
                    });
            }
        }

        // ---- SSD refresh
        ui.add_space(14.0);
        ui.heading("SSD refresh (rewrite files in place)");
        ui.label(
            "Rewrites each file with its own bytes so the flash gets a fresh charge, verifies it, and restores the \
             file's timestamps and attributes. In-use, cloud, encrypted, compressed, linked and system files are \
             skipped. Backup tools that watch the NTFS change journal will still see the files as rewritten.",
        );
        if self.mode == Mode::ReadOnly {
            ui.weak("Needs Read-write mode (top bar).");
        } else if let Some(mount) = mount.clone() {
            self.refresh_section(ui, d, &mount, stale.as_ref());
        }

        // ---- raw maintenance
        ui.add_space(14.0);
        ui.heading("Block refresh and sector remap");
        if d.is_protected() {
            ui.colored_label(RED, "Blocked on the system disk: these rewrite raw sectors.");
        } else if self.mode == Mode::ReadOnly {
            ui.weak("Needs Read-write mode (top bar).");
        } else {
            self.raw_section(ui, d);
        }
        ui.add_space(10.0);
        self.live_view(ui, d);
    }

    fn refresh_section(
        &mut self,
        ui: &mut egui::Ui,
        d: &DriveInfo,
        mount: &str,
        stale: Option<&drivetest_core::stale::StaleOutcome>,
    ) {
        let protected = d.is_protected();
        let slow: Vec<&drivetest_core::stale::FileSpeed> =
            stale.map(|s| s.slow_files.iter().collect()).unwrap_or_default();
        let f = &mut self.maint_form;
        if protected && f.refresh_choice != RefreshChoice::SlowFiles {
            f.refresh_choice = RefreshChoice::SlowFiles;
        }
        ui.add_enabled_ui(!slow.is_empty(), |ui| {
            ui.radio_value(
                &mut f.refresh_choice,
                RefreshChoice::SlowFiles,
                format!("The {} slow files found by the stale-data check", slow.len()),
            )
        })
        .response
        .on_disabled_hover_text("Run the stale-data check on this volume first.");
        ui.add_enabled_ui(!protected, |ui| {
            ui.horizontal(|ui| {
                ui.radio_value(&mut f.refresh_choice, RefreshChoice::Folder, "Every file under folder");
                ui.text_edit_singleline(&mut f.folder);
            });
            ui.radio_value(&mut f.refresh_choice, RefreshChoice::WholeVolume, "Every file on the volume");
        })
        .response
        .on_disabled_hover_text("On the system disk only the slow files can be refreshed.");

        if protected {
            egui::Frame::group(ui.style()).stroke(Stroke::new(1.0, ORANGE)).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.colored_label(ORANGE, "System disk: refresh is an opt-in exception");
                ui.label("• Only the selected slow files are rewritten, through the file system, never raw sectors.");
                ui.label(
                    "• Windows, Program Files, ProgramData, system files, the pagefile and files in use are skipped.",
                );
                ui.label("• Each file is verified and its timestamps restored; everything is listed in the report.");
                ui.checkbox(&mut f.system_opt_in, "Allow this refresh on the system disk");
            });
        }

        let bytes: u64 = match f.refresh_choice {
            RefreshChoice::SlowFiles => slow.iter().map(|s| s.size).sum(),
            _ => d.volume_by_mount(mount).map(|v| v.size_bytes.saturating_sub(v.free_bytes)).unwrap_or(0),
        };
        let mut note = format!("Writes about {}", format_bytes(bytes));
        if let Some(n) = self.health.get(&d.number).and_then(|h| h.nvme.as_ref())
            && n.bytes_written() > 0
        {
            note.push_str(&format!(" (the drive has written {} in its life so far)", format_bytes(n.bytes_written())));
        }
        ui.weak(note);

        let ready = match f.refresh_choice {
            RefreshChoice::SlowFiles => !slow.is_empty() && (!protected || f.system_opt_in),
            RefreshChoice::Folder => !protected && !f.folder.trim().is_empty(),
            RefreshChoice::WholeVolume => !protected,
        } && !d.write_protected;
        let start = ui
            .add_enabled(ready, egui::Button::new("▶ Start refresh"))
            .on_disabled_hover_text("Choose what to refresh (and tick the opt-in on the system disk).");
        if start.clicked() {
            let targets = match f.refresh_choice {
                RefreshChoice::SlowFiles => {
                    RefreshTargets::Files { paths: slow.iter().map(|s| s.path.clone()).collect() }
                }
                RefreshChoice::Folder => {
                    // Accept "E:\Photos" as well as "Photos" for the selected volume E:\.
                    // Anything else that is not a plain relative path is refused by the engine.
                    let typed = f.folder.trim();
                    let rel = match typed.get(..mount.len()) {
                        Some(head) if head.eq_ignore_ascii_case(mount) => &typed[mount.len()..],
                        _ => typed,
                    };
                    RefreshTargets::Folder { path: rel.trim_matches('\\').to_string() }
                }
                RefreshChoice::WholeVolume => RefreshTargets::WholeVolume,
            };
            let mut config = FileRefreshConfig::new(targets);
            if let Some(st) = stale {
                config.before_mb_s = st.speeds();
            }
            let system_opt_in = f.system_opt_in;
            f.system_opt_in = false;
            self.start(d, JobKind::FileRefresh { mount: mount.to_string(), config, system_opt_in }, None);
        }
    }

    fn raw_section(&mut self, ui: &mut egui::Ui, d: &DriveInfo) {
        let bad = self
            .last_bad
            .get(&d.number)
            .filter(|(drive, _)| d.matches_report_device(drive))
            .map(|(_, b)| b.clone())
            .unwrap_or_default();
        let f = &mut self.maint_form;
        ui.label("These work below the file system, so the drive's volumes are dismounted while they run.");
        ui.radio_value(&mut f.raw_choice, RawChoice::RefreshSlow, "Block refresh — only blocks that read slowly");
        ui.radio_value(&mut f.raw_choice, RawChoice::RefreshAll, "Block refresh — every block with data");
        ui.radio_value(&mut f.raw_choice, RawChoice::Remap, "Remap unreadable sectors");
        match f.raw_choice {
            RawChoice::RefreshSlow => {
                ui.horizontal(|ui| {
                    ui.label("Slower than");
                    ui.add(egui::DragValue::new(&mut f.slower_ms).range(1.0..=2000.0).suffix(" ms"));
                });
            }
            RawChoice::Remap => {
                if bad.is_empty() {
                    ui.weak("No bad sectors from a scan in this session; the drive will be scanned first.");
                } else {
                    ui.checkbox(&mut f.remap_use_scan, format!("Use the {} bad sectors from the last scan", bad.len()));
                }
            }
            RawChoice::RefreshAll => {}
        }
        let kind = match f.raw_choice {
            RawChoice::RefreshAll => WriteTestKind::BlockRefresh { slower_than_us: None },
            RawChoice::RefreshSlow => {
                WriteTestKind::BlockRefresh { slower_than_us: Some((f.slower_ms * 1000.0) as u32) }
            }
            RawChoice::Remap => WriteTestKind::Remap {
                lbas: if f.remap_use_scan { bad.clone() } else { Vec::new() },
                refresh_slower_than_us: None,
            },
        };
        egui::Frame::group(ui.style()).stroke(Stroke::new(1.5, ORANGE)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.colored_label(ORANGE, RichText::new("Keeps data, but back it up first.").strong());
            ui.label("A failing drive can lose more data while it is being rewritten.");
            if matches!(kind, WriteTestKind::Remap { .. }) {
                ui.label("Confirmed unreadable sectors will be overwritten with zeros, destroying potentially recoverable data.");
            }
            ui.label(format!("{} · serial {} · {}", d.title(), d.serial, format_bytes(d.size_bytes)));
            for v in &d.volumes {
                ui.label(format!("Volume {} will be dismounted while this runs.", v.display_name()));
            }
            if d.media == MediaKind::Ssd && matches!(kind, WriteTestKind::BlockRefresh { slower_than_us: None }) {
                ui.weak("Rewrites every block that holds data: about one full drive write of wear.");
            }
            ui.horizontal(|ui| {
                ui.label("Type the serial number to enable Start:");
                ui.text_edit_singleline(&mut f.typed_serial);
            });
            let confirmation = if matches!(kind, WriteTestKind::Remap { .. }) {
                "I understand that this can overwrite recoverable data"
            } else {
                "I have a backup and understand the volumes will be dismounted"
            };
            ui.checkbox(&mut f.understood, confirmation);
        });
        let check = Policy::new(self.mode).check(d, Tier::Destructive);
        let ready = check.is_ok() && d.serial_matches(&f.typed_serial) && f.understood;
        let reason = match &check {
            Err(e) => e.to_string(),
            Ok(()) if !d.serial_matches(&f.typed_serial) => "Type the drive's serial number exactly.".into(),
            Ok(()) => "Tick the confirmation box.".into(),
        };
        let start =
            ui.add_enabled(ready, egui::Button::new(RichText::new("▶ Start").strong())).on_disabled_hover_text(reason);
        if start.clicked() {
            let serial = f.typed_serial.clone();
            f.typed_serial.clear();
            f.understood = false;
            self.start(d, JobKind::WriteTest(WriteTestConfig::new(kind)), Some(serial));
        }
    }
}

/// Report details for the maintenance and speed-class tests.
pub fn report_extras(ui: &mut egui::Ui, r: &Report) {
    if let Some(st) = &r.stale {
        ui.add_space(8.0);
        ui.strong(format!("Stale-data check on {}", st.mount));
        ui.label(format!(
            "{} files timed of {} examined; baseline {:.0} MB/s; {} slow.",
            st.files_sampled,
            st.files_examined,
            st.baseline_mb_s,
            st.slow_files.len()
        ));
    }
    if let Some(fr) = &r.file_refresh {
        ui.add_space(8.0);
        ui.strong(format!("SSD refresh on {} ({})", fr.mount, fr.targets));
        if !fr.failed.is_empty() {
            ui.colored_label(RED, format!("{} files failed:", fr.failed.len()));
            for n in fr.failed.iter().take(50) {
                ui.label(format!("  {} — {}", n.path, n.reason));
            }
        }
        if !fr.before_after.is_empty() {
            egui::CollapsingHeader::new(format!("Read speed before / after ({})", fr.before_after.len()))
                .id_salt("before_after")
                .show(ui, |ui| {
                    for b in fr.before_after.iter().take(200) {
                        let before = b.before_mb_s.map(|x| format!("{x:.0}")).unwrap_or_else(|| "—".into());
                        ui.label(format!("{before} → {:.0} MB/s · {}", b.after_mb_s, b.path));
                    }
                });
        }
        if !fr.skipped.is_empty() {
            egui::CollapsingHeader::new(format!("Skipped ({})", fr.skipped_count)).id_salt("skipped").show(ui, |ui| {
                for n in fr.skipped.iter().take(200) {
                    ui.label(format!("{} — {}", n.path, n.reason));
                }
            });
        }
    }
    if let Some(sc) = &r.speed_class {
        ui.add_space(8.0);
        ui.strong(format!("Speed-class check on {}", sc.mount));
        egui::Grid::new("speed_results").num_columns(4).striped(true).show(ui, |ui| {
            for c in &sc.results {
                ui.label(&c.class);
                ui.colored_label(if c.pass { GREEN } else { RED }, if c.pass { "meets" } else { "fails" });
                ui.label(&c.requirement);
                ui.label(&c.measured);
                ui.end_row();
            }
        });
        ui.weak(&sc.note);
    }
    if let Some(rep) = r.write_test.as_ref().and_then(|w| w.repair.as_ref()) {
        ui.add_space(8.0);
        ui.strong("Block refresh / remap");
        ui.label(format!(
            "{} blocks checked · {} rewritten · {} empty skipped · {} fast skipped · {} unreadable",
            rep.blocks_checked,
            rep.blocks_rewritten,
            rep.zero_blocks_skipped,
            rep.fast_blocks_skipped,
            rep.unreadable_blocks
        ));
        if !rep.remapped_sectors.is_empty() {
            ui.colored_label(GREEN, format!("Zeroed and verified: {} sectors", rep.remapped_sectors.len()));
        }
        if !rep.still_bad_sectors.is_empty() {
            ui.colored_label(RED, format!("Still unreadable: {} sectors", rep.still_bad_sectors.len()));
        }
    }
}
