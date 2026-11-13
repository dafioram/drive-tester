//! `drivetest` — command-line front end.
//!
//! Exit codes: 0 = passed, 1 = problems found, 2 = error, 3 = refused by the
//! safety policy.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use drivetest_core::demo::DemoPlatform;
use drivetest_core::events::EngineEvent;
use drivetest_core::freespace::FreeSpaceConfig;
use drivetest_core::health::{HealthSnapshot, assess_smr};
use drivetest_core::model::{DriveInfo, format_bytes};
use drivetest_core::platform::{Platform, SelfTestKind};
use drivetest_core::policy::{Mode, Policy, Tier};
use drivetest_core::refresh::{FileRefreshConfig, RefreshTargets};
use drivetest_core::report::{Report, default_report_dir};
use drivetest_core::runner::{JobEvent, JobKind, JobRequest, start_job};
use drivetest_core::scan::{BlockStatus, ScanConfig, ScanMode};
use drivetest_core::speedclass::SpeedClassConfig;
use drivetest_core::stale::StaleConfig;
use drivetest_core::writetest::{WriteTestConfig, WriteTestKind};

const EXIT_OK: u8 = 0;
const EXIT_PROBLEMS: u8 = 1;
const EXIT_ERROR: u8 = 2;
const EXIT_REFUSED: u8 = 3;

#[derive(Parser)]
#[command(
    name = "drivetest",
    version,
    about = "Test drives safely: health, read scans and write tests that can never touch the system disk.",
    after_help = "Exit codes: 0 passed, 1 problems found, 2 error, 3 refused by the safety policy."
)]
struct Cli {
    /// Use four simulated drives instead of real hardware.
    #[arg(long, global = true)]
    demo: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List drives with their role, health interface and volumes.
    List {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show SMART / NVMe health and a plain-language verdict.
    Health {
        disk: u32,
        #[arg(long)]
        json: bool,
    },
    /// Start the drive's built-in self-test and wait for the result (read-only).
    SelfTest {
        disk: u32,
        /// Run the extended self-test instead of the short one.
        #[arg(long)]
        extended: bool,
        #[command(flatten)]
        report: ReportArgs,
    },
    /// Read scan: time every block and find slow or unreadable sectors (read-only).
    Scan {
        disk: u32,
        #[arg(long, value_enum, default_value_t = ScanModeArg::Sequential)]
        mode: ScanModeArg,
        /// Share of the drive to sample in random mode.
        #[arg(long, default_value_t = 5.0)]
        percent: f64,
        /// Start of the range in GB (range mode).
        #[arg(long)]
        start_gb: Option<f64>,
        /// End of the range in GB (range mode).
        #[arg(long)]
        end_gb: Option<f64>,
        #[arg(long, default_value_t = 1024)]
        block_kib: u32,
        #[arg(long, default_value_t = 2)]
        retries: u8,
        /// Do not narrow failing blocks down to single sectors.
        #[arg(long)]
        no_pinpoint: bool,
        #[command(flatten)]
        report: ReportArgs,
    },
    /// Fill a volume's free space with test files, verify, delete (non-destructive).
    VerifyFreeSpace {
        disk: u32,
        /// Mounted volume on the drive, e.g. E:\
        #[arg(long)]
        volume: String,
        /// Required: allows tests that write.
        #[arg(long)]
        read_write: bool,
        /// Write at most this many GB.
        #[arg(long)]
        max_gb: Option<f64>,
        #[command(flatten)]
        report: ReportArgs,
    },
    /// DESTRUCTIVE: write test patterns to the raw drive. Erases everything on it.
    WriteTest {
        disk: u32,
        #[arg(long, value_enum, default_value_t = WriteKindArg::Full)]
        kind: WriteKindArg,
        /// Required: allows tests that write.
        #[arg(long)]
        read_write: bool,
        /// The drive's serial number. Prompted for when omitted.
        #[arg(long)]
        confirm_serial: Option<String>,
        /// Positions sampled by the quick probe.
        #[arg(long, default_value_t = 1024)]
        samples: u32,
        /// Data budget for the SMR test.
        #[arg(long, default_value_t = 40)]
        smr_gib: u32,
        /// Time budget for the SMR test.
        #[arg(long, default_value_t = 20)]
        smr_minutes: u32,
        /// Refresh / remap: only rewrite blocks that read slower than this.
        #[arg(long)]
        slower_than_ms: Option<f64>,
        /// Remap: take bad sectors from a scan report instead of scanning again.
        #[arg(long)]
        from_report: Option<PathBuf>,
        /// Required for remap. A zero write can destroy recoverable data.
        #[arg(long)]
        allow_zero_unreadable: bool,
        #[command(flatten)]
        report: ReportArgs,
    },
    /// Compare read speed of old and new files on a volume (read-only).
    StaleCheck {
        disk: u32,
        /// Mounted volume on the drive, e.g. C:\
        #[arg(long)]
        volume: String,
        /// Files to time.
        #[arg(long, default_value_t = 240)]
        max_files: usize,
        #[command(flatten)]
        report: ReportArgs,
    },
    /// Rewrite files in place with their own bytes (SSD refresh).
    RefreshFiles {
        disk: u32,
        #[arg(long)]
        volume: String,
        /// Required: allows tests that write.
        #[arg(long)]
        read_write: bool,
        /// Refresh the slow files listed in a stale-check report.
        #[arg(long, conflicts_with_all = ["folder", "whole_volume"])]
        from_report: Option<PathBuf>,
        /// Refresh every file under this folder (relative to the volume).
        #[arg(long, conflicts_with = "whole_volume")]
        folder: Option<String>,
        /// Refresh every file on the volume (data drives only).
        #[arg(long)]
        whole_volume: bool,
        /// Allow a targeted refresh on the system disk (Windows and program folders are still skipped).
        #[arg(long)]
        system_disk_opt_in: bool,
        #[command(flatten)]
        report: ReportArgs,
    },
    /// Measure sustained write, read and random IOPS against speed classes (non-destructive).
    SpeedClass {
        disk: u32,
        #[arg(long)]
        volume: String,
        /// Required: allows tests that write.
        #[arg(long)]
        read_write: bool,
        /// Classes printed on the card, e.g. V30,U3,A2. Defaults to what the card reports.
        #[arg(long, value_delimiter = ',')]
        claimed: Vec<String>,
        /// Size of the test file in GB.
        #[arg(long, default_value_t = 2.0)]
        test_gb: f64,
        #[command(flatten)]
        report: ReportArgs,
    },
}

#[derive(Args, Clone)]
struct ReportArgs {
    /// Folder for the JSON report (default: Documents\DriveTest\reports).
    #[arg(long)]
    report_dir: Option<PathBuf>,
    /// Do not save a report.
    #[arg(long)]
    no_report: bool,
    /// Print the full report JSON to stdout when done.
    #[arg(long)]
    json: bool,
}

impl ReportArgs {
    fn dir(&self) -> Option<PathBuf> {
        if self.no_report { None } else { Some(self.report_dir.clone().unwrap_or_else(default_report_dir)) }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ScanModeArg {
    Sequential,
    Random,
    Butterfly,
    Range,
}

#[derive(Clone, Copy, ValueEnum)]
enum WriteKindArg {
    /// Write and verify every block.
    Full,
    /// Write and verify sampled positions (fast fake-capacity check).
    Probe,
    /// Sustained random writes to detect drive-managed SMR (HDDs).
    Smr,
    /// Rewrite blocks with their own data (keeps data; drive must be unmounted).
    Refresh,
    /// Overwrite unreadable sectors so the drive reallocates them (keeps readable data).
    Remap,
}

fn platform(demo: bool) -> Result<Arc<dyn Platform>, String> {
    if demo {
        return Ok(Arc::new(DemoPlatform::new()));
    }
    #[cfg(windows)]
    {
        let p = drivetest_win::WinPlatform::new();
        if !p.is_elevated() {
            eprintln!("warning: not running as administrator. Raw disk access needs an Administrator terminal.");
        }
        Ok(Arc::new(p))
    }
    #[cfg(not(windows))]
    {
        Err("Real drives are only supported on Windows. Add --demo to try the simulated drives.".into())
    }
}

fn find_drive(p: &dyn Platform, disk: u32) -> Result<DriveInfo, String> {
    p.enumerate()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|d| d.number == disk)
        .ok_or_else(|| format!("No drive {disk}. Run `drivetest list` to see drive numbers."))
}

/// Loads a report and confirms it was made for this exact drive (serial,
/// capacity, sector size) and, when given, this exact volume.
fn load_report_for(p: &dyn Platform, disk: u32, path: &std::path::Path, mount: Option<&str>) -> Result<Report, String> {
    let r = Report::load(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    if r.schema != drivetest_core::report::REPORT_SCHEMA || r.platform != p.name() {
        return Err("The report schema or platform does not match this app.".into());
    }
    let drives = p.enumerate().map_err(|error| error.to_string())?;
    let mut matches = drives.iter().filter(|drive| drive.matches_report_device(&r.drive));
    let Some(d) = matches.next().filter(|drive| drive.number == disk) else {
        return Err(format!("{} does not identify disk {disk}.", path.display()));
    };
    if matches.next().is_some() {
        return Err("More than one attached device matches the report.".into());
    }
    if let Some(m) = mount {
        let then = r.drive.volume_by_mount(m).map(|v| v.id.clone());
        let now = d.volume_by_mount(m).map(|v| v.id.clone());
        if then.is_none() || then != now {
            return Err(format!("{} was made for a different volume than {m}.", path.display()));
        }
    }
    Ok(r)
}

fn exit_error(message: String) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(EXIT_ERROR)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let platform = match platform(cli.demo) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    let result = match cli.command {
        Command::List { json } => list(&*platform, json),
        Command::Health { disk, json } => health(&*platform, disk, json),
        Command::SelfTest { disk, extended, report } => {
            let kind = if extended { SelfTestKind::Extended } else { SelfTestKind::Short };
            job(platform, disk, JobKind::SelfTest(kind), Mode::ReadOnly, None, &report)
        }
        Command::Scan { disk, mode, percent, start_gb, end_gb, block_kib, retries, no_pinpoint, report } => {
            let mode = match mode {
                ScanModeArg::Sequential => ScanMode::Sequential,
                ScanModeArg::Random => ScanMode::RandomSample { percent },
                ScanModeArg::Butterfly => ScanMode::Butterfly,
                ScanModeArg::Range => ScanMode::Range {
                    start_byte: (start_gb.unwrap_or(0.0) * 1e9) as u64,
                    end_byte: end_gb.map_or(u64::MAX, |g| (g * 1e9) as u64),
                },
            };
            let cfg = ScanConfig {
                mode,
                block_size: block_kib.clamp(4, 65_536) * 1024,
                retries,
                pinpoint_errors: !no_pinpoint,
                ..Default::default()
            };
            job(platform, disk, JobKind::ReadScan(cfg), Mode::ReadOnly, None, &report)
        }
        Command::VerifyFreeSpace { disk, volume, read_write, max_gb, report } => {
            let config = FreeSpaceConfig { max_bytes: max_gb.map(|g| (g * 1e9) as u64), ..Default::default() };
            let mode = if read_write { Mode::ReadWrite } else { Mode::ReadOnly };
            job(platform, disk, JobKind::FreeSpaceVerify { mount: volume, config }, mode, None, &report)
        }
        Command::WriteTest {
            disk,
            kind,
            read_write,
            confirm_serial,
            samples,
            smr_gib,
            smr_minutes,
            slower_than_ms,
            from_report,
            allow_zero_unreadable,
            report,
        } => {
            let mut report_drive = None;
            let slower = slower_than_ms.map(|ms| (ms * 1000.0) as u32);
            let kind = match kind {
                WriteKindArg::Full => WriteTestKind::FullWriteVerify,
                WriteKindArg::Probe => WriteTestKind::FakeCapacityProbe { samples },
                WriteKindArg::Smr => WriteTestKind::SmrProbe { max_gib: smr_gib, max_minutes: smr_minutes },
                WriteKindArg::Refresh => WriteTestKind::BlockRefresh { slower_than_us: slower },
                WriteKindArg::Remap => {
                    if !allow_zero_unreadable {
                        eprintln!(
                            "Remap can overwrite potentially recoverable data. Pass --allow-zero-unreadable to confirm."
                        );
                        return ExitCode::from(EXIT_REFUSED);
                    }
                    let lbas = match &from_report {
                        None => Vec::new(),
                        Some(p) => match load_report_for(&*platform, disk, p, None) {
                            Ok(r) => match r.scan {
                                Some(s) if !s.bad_sectors.is_empty() => {
                                    report_drive = Some(DriveInfo { number: disk, ..r.drive });
                                    s.bad_sectors
                                }
                                Some(_) => return exit_error("The report contains no unreadable sectors.".into()),
                                None => return exit_error(format!("{} is not a scan report.", p.display())),
                            },
                            Err(e) => return exit_error(e),
                        },
                    };
                    WriteTestKind::Remap { lbas, refresh_slower_than_us: slower }
                }
            };
            let mode = if read_write { Mode::ReadWrite } else { Mode::ReadOnly };
            write_test(platform, disk, report_drive, kind, mode, confirm_serial, &report)
        }
        Command::StaleCheck { disk, volume, max_files, report } => {
            let config = StaleConfig { max_files, ..Default::default() };
            job(platform, disk, JobKind::StaleCheck { mount: volume, config }, Mode::ReadOnly, None, &report)
        }
        Command::RefreshFiles {
            disk,
            volume,
            read_write,
            from_report,
            folder,
            whole_volume,
            system_disk_opt_in,
            report,
        } => {
            let mut report_drive = None;
            let mut config = match (&from_report, folder, whole_volume) {
                (Some(p), _, _) => match load_report_for(&*platform, disk, p, Some(&volume)) {
                    Err(e) => return exit_error(e),
                    Ok(r) => match r.stale {
                        Some(st) if st.mount.eq_ignore_ascii_case(&volume) => {
                            let mut c = FileRefreshConfig::new(RefreshTargets::Files {
                                paths: st.slow_files.iter().map(|f| f.path.clone()).collect(),
                            });
                            c.before_mb_s = st.speeds();
                            report_drive = Some(DriveInfo { number: disk, ..r.drive });
                            c
                        }
                        Some(_) => return exit_error(format!("{} was made for a different volume.", p.display())),
                        None => return exit_error(format!("{} is not a stale-check report.", p.display())),
                    },
                },
                (None, Some(f), _) => FileRefreshConfig::new(RefreshTargets::Folder { path: f }),
                (None, None, true) => FileRefreshConfig::new(RefreshTargets::WholeVolume),
                (None, None, false) => {
                    return exit_error(
                        "Choose what to refresh: --from-report, --folder or --whole-volume.".to_string(),
                    );
                }
            };
            config.measure_after = true;
            let mode = if read_write { Mode::ReadWrite } else { Mode::ReadOnly };
            let kind = JobKind::FileRefresh { mount: volume, config, system_opt_in: system_disk_opt_in };
            job_for_selection(platform, disk, report_drive, kind, mode, None, &report)
        }
        Command::SpeedClass { disk, volume, read_write, claimed, test_gb, report } => {
            let claimed = if claimed.is_empty() {
                platform.read_health(disk).ok().and_then(|h| h.sd).map(|sd| sd.claims.labels()).unwrap_or_default()
            } else {
                claimed
            };
            let config = SpeedClassConfig { test_bytes: (test_gb * 1e9) as u64, claimed, ..Default::default() };
            let mode = if read_write { Mode::ReadWrite } else { Mode::ReadOnly };
            job(platform, disk, JobKind::SpeedClass { mount: volume, config }, mode, None, &report)
        }
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

// ------------------------------------------------------------------ list / health

fn list(p: &dyn Platform, json: bool) -> Result<u8, String> {
    let drives = p.enumerate().map_err(|e| e.to_string())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&drives).map_err(|e| e.to_string())?);
        return Ok(EXIT_OK);
    }
    if drives.is_empty() {
        println!("No drives found.");
        return Ok(EXIT_OK);
    }
    println!("{:<5} {:<15} {:<14} {:<10} {:<32} {:<22} WRITES", "DISK", "BUS", "MEDIA", "SIZE", "MODEL", "SERIAL");
    for d in &drives {
        let writes = if d.is_protected() {
            "blocked (system disk)"
        } else if d.write_protected {
            "blocked (write-protected)"
        } else {
            "allowed in read-write mode"
        };
        println!(
            "{:<5} {:<15} {:<14} {:<10} {:<32} {:<22} {}",
            d.number,
            d.bus.label(),
            d.media.label(),
            format_bytes(d.size_bytes),
            truncate(&d.title(), 32),
            truncate(&d.serial, 22),
            writes
        );
        if d.is_protected() {
            println!("      {}", d.role.describe());
        }
        for v in &d.volumes {
            println!(
                "      volume {} {} {} free of {}",
                v.display_name(),
                v.file_system,
                format_bytes(v.free_bytes),
                format_bytes(v.size_bytes)
            );
        }
    }
    Ok(EXIT_OK)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).chain(std::iter::once('…')).collect() }
}

fn health(p: &dyn Platform, disk: u32, json: bool) -> Result<u8, String> {
    let d = find_drive(p, disk)?;
    let snap = p.read_health(disk).unwrap_or_else(|e| HealthSnapshot::unavailable(e.to_string()));
    let v = snap.verdict();
    let exit = if v.level >= drivetest_core::health::VerdictLevel::Caution {
        EXIT_PROBLEMS
    } else if !snap.is_available() && snap.sd.is_none() {
        EXIT_ERROR
    } else {
        EXIT_OK
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&snap).map_err(|e| e.to_string())?);
        return Ok(exit);
    }
    println!("{} — {}", d.title(), d.summary_line());
    print_health(&d, &snap);
    Ok(exit)
}

fn print_health(d: &DriveInfo, snap: &HealthSnapshot) {
    let v = snap.verdict();
    println!("\nVerdict: {}", v.level.label());
    for r in &v.reasons {
        println!("  - {r}");
    }
    if let Some(life) = v.remaining_life_percent {
        println!("  Remaining life (drive's own estimate): {life}%");
    }
    if let Some(t) = snap.temperature_c {
        println!("  Temperature: {t} °C");
    }
    println!("  Source: {}", snap.source.label());
    if let Some(n) = &snap.nvme {
        println!("\nNVMe health log");
        println!("  Percentage used        {}%", n.percentage_used);
        println!(
            "  Available spare        {}% (threshold {}%)",
            n.available_spare_percent, n.available_spare_threshold_percent
        );
        println!("  Data written           {}", format_bytes(n.bytes_written()));
        println!("  Data read              {}", format_bytes(n.bytes_read()));
        println!("  Power-on hours         {}", n.power_on_hours);
        println!("  Power cycles           {}", n.power_cycles);
        println!("  Unsafe shutdowns       {}", n.unsafe_shutdowns);
        println!("  Media errors           {}", n.media_errors);
        println!("  Error log entries      {}", n.error_log_entries);
        println!("  Critical warning flags 0x{:02X}", n.critical_warning);
    }
    if let Some(a) = &snap.ata {
        println!("\n {:>3}  {:<32} {:>5} {:>5} {:>6} {:>14}", "ID", "ATTRIBUTE", "VALUE", "WORST", "THRESH", "RAW");
        for x in &a.attributes {
            println!(
                " {:>3}  {:<32} {:>5} {:>5} {:>6} {:>14}{}",
                x.id,
                truncate(&x.name, 32),
                x.current,
                x.worst,
                x.threshold,
                x.raw,
                if x.failing_now { "  FAILING" } else { "" }
            );
        }
        if let Some(st) = &a.self_test {
            println!("\nSelf-test: {}", st.description);
        }
    }
    if snap.ata.is_none()
        && let Some(st) = &snap.nvme_self_test
    {
        println!("\nSelf-test: {}", st.description);
    }
    if let Some(sd) = &snap.sd {
        println!(
            "\nSD card registers ({})",
            if sd.sources.is_empty() { "none read".into() } else { sd.sources.join(", ") }
        );
        let field = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                println!("  {k:<22} {v}");
            }
        };
        field(
            "Manufacturer",
            sd.manufacturer_id.map(|m| format!("0x{m:02X} {}", sd.manufacturer.clone().unwrap_or_default())),
        );
        field("OEM ID", sd.oem_id.clone());
        field("Product name", sd.product_name.clone());
        field("Revision", sd.revision.clone());
        field("Serial", sd.serial.map(|s| format!("0x{s:08X}")));
        field("Manufactured", sd.manufactured.clone());
        field("Capacity (CSD)", sd.csd_capacity_bytes.map(format_bytes));
        if !sd.claims.is_empty() {
            println!("  {:<22} {}", "Speed ratings", sd.claims.labels().join(" "));
        }
        for w in &sd.warnings {
            println!("  WARNING: {w}");
        }
        for n in &sd.notes {
            println!("  note: {n}");
        }
    }
    if let Some(a) = &d.adapter {
        println!("Adapter: {a}");
    }
    let smr = assess_smr(d.media, &d.model, snap.identify(), None);
    println!("Recording: {} — {}", smr.headline(), smr.evidence);
}

// ------------------------------------------------------------------ jobs

fn write_test(
    platform: Arc<dyn Platform>,
    disk: u32,
    expected: Option<DriveInfo>,
    kind: WriteTestKind,
    mode: Mode,
    confirm_serial: Option<String>,
    report: &ReportArgs,
) -> Result<u8, String> {
    let d = resolve_selection(&*platform, disk, expected.as_ref())?;
    if let Err(e) = Policy::new(mode).check(&d, Tier::Destructive) {
        eprintln!("{e}");
        if mode == Mode::ReadOnly && !d.is_protected() {
            eprintln!("Add --read-write to allow tests that write.");
        }
        return Ok(EXIT_REFUSED);
    }
    let cfg = WriteTestConfig::new(kind);
    let serial = match confirm_serial {
        Some(s) => s,
        None => {
            if cfg.kind.preserves_data() {
                eprintln!("\n*** RAW WRITE — the drive's volumes will be dismounted and its sectors rewritten ***");
                eprintln!(
                    "    Data is kept, but back it up first: a failing drive can lose more while being rewritten.\n"
                );
            } else {
                eprintln!("\n*** DESTRUCTIVE TEST — EVERYTHING ON THIS DRIVE WILL BE ERASED ***\n");
            }
            eprintln!("  Drive:   {}  ({})", d.title(), d.summary_line());
            eprintln!("  Serial:  {}", d.serial);
            for v in &d.volumes {
                let what = if cfg.kind.preserves_data() { "dismounted" } else { "dismounted and overwritten" };
                eprintln!("  Volume {} will be {what}", v.display_name());
            }
            eprintln!(
                "  Test:    {} — writes up to {}",
                cfg.kind.label(),
                format_bytes(cfg.kind.estimated_bytes_written(d.size_bytes, cfg.block_size))
            );
            if d.media != drivetest_core::model::MediaKind::Hdd {
                eprintln!("  Note:    flash media wears with every full write.");
            }
            eprint!("\nType the drive's serial number to continue: ");
            io::stderr().flush().ok();
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
            line.trim().to_string()
        }
    };
    if !d.serial_matches(&serial) {
        eprintln!("The serial number does not match. Nothing was written.");
        return Ok(EXIT_REFUSED);
    }
    job_for_selection(platform, disk, Some(d), JobKind::WriteTest(cfg), mode, Some(serial), report)
}

fn job(
    platform: Arc<dyn Platform>,
    disk: u32,
    kind: JobKind,
    mode: Mode,
    confirm_serial: Option<String>,
    report_args: &ReportArgs,
) -> Result<u8, String> {
    job_for_selection(platform, disk, None, kind, mode, confirm_serial, report_args)
}

fn resolve_selection(platform: &dyn Platform, disk: u32, expected: Option<&DriveInfo>) -> Result<DriveInfo, String> {
    let current = find_drive(platform, disk)?;
    if expected.is_some_and(|expected| !current.same_device(expected)) {
        return Err("The selected device changed before the job started.".into());
    }
    Ok(current)
}

fn job_for_selection(
    platform: Arc<dyn Platform>,
    disk: u32,
    expected: Option<DriveInfo>,
    kind: JobKind,
    mode: Mode,
    confirm_serial: Option<String>,
    report_args: &ReportArgs,
) -> Result<u8, String> {
    let drive = resolve_selection(&*platform, disk, expected.as_ref())?;
    if let (Some(expected), JobKind::FileRefresh { mount, .. }) = (&expected, &kind)
        && expected.volume_by_mount(mount).map(|volume| &volume.id)
            != drive.volume_by_mount(mount).map(|volume| &volume.id)
    {
        return Err("The selected volume changed before the job started.".into());
    }

    let precheck =
        if matches!(kind, JobKind::FileRefresh { .. }) { Ok(()) } else { Policy::new(mode).check(&drive, kind.tier()) };
    if let Err(e) = precheck {
        eprintln!("{e}");
        if mode == Mode::ReadOnly && !drive.is_protected() {
            eprintln!("Add --read-write to allow tests that write.");
        }
        return Ok(EXIT_REFUSED);
    }
    eprintln!("{} on disk {} — {}", kind.name(), drive.number, drive.title());

    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));

    let handle = start_job(platform, JobRequest { drive, kind, mode, confirm_serial, report_dir: report_args.dir() });
    let mut progress = ProgressLine::default();
    loop {
        if interrupted.load(Ordering::SeqCst) && !handle.cancel_requested() {
            progress.clear();
            eprintln!("Stopping… (a partial report will still be written)");
            handle.cancel();
        }
        let Some(event) = handle.recv_timeout(Duration::from_millis(200)) else {
            if handle.is_finished() {
                // The worker always sends Finished or Failed before exiting;
                // drain anything left.
                if let Some(ev) = handle.recv_timeout(Duration::from_millis(50)) {
                    if let Some(code) = progress.handle(ev, report_args.json) {
                        return Ok(code);
                    }
                    continue;
                }
                return Err("the test ended without a result".into());
            }
            continue;
        };
        if let Some(code) = progress.handle(event, report_args.json) {
            return Ok(code);
        }
    }
}

#[derive(Default)]
struct ProgressLine {
    pass: String,
    errors: u64,
    temp: Option<i32>,
    drawn: bool,
}

impl ProgressLine {
    fn clear(&mut self) {
        if self.drawn {
            eprint!("\r{:<100}\r", "");
            self.drawn = false;
        }
    }

    fn handle(&mut self, event: JobEvent, json: bool) -> Option<u8> {
        match event {
            JobEvent::Phase(p) => {
                self.clear();
                eprintln!("» {p}");
            }
            JobEvent::Temperature { celsius, .. } => self.temp = Some(celsius),
            JobEvent::Engine(EngineEvent::Pass { name, .. }) => {
                self.clear();
                self.pass = name;
                self.errors = 0;
            }
            JobEvent::Engine(EngineEvent::Blocks(b)) => {
                self.errors += b.iter().filter(|u| u.status == BlockStatus::Error).count() as u64;
            }
            JobEvent::Engine(EngineEvent::Log(m)) => {
                self.clear();
                eprintln!("  {m}");
            }
            JobEvent::Engine(EngineEvent::Progress(p)) => {
                let width = 30;
                let filled = ((p.fraction() * width as f32) as usize).min(width);
                let eta = p.eta().map(fmt_duration).unwrap_or_else(|| "--:--".into());
                let temp = self.temp.map(|t| format!("  {t}°C")).unwrap_or_default();
                eprint!(
                    "\r{:<22} [{}{}] {:5.1}%  {:7.1} MB/s  ETA {}  errors {}{}   ",
                    truncate(&self.pass, 22),
                    "#".repeat(filled),
                    "-".repeat(width - filled),
                    p.fraction() * 100.0,
                    p.recent_mb_s,
                    eta,
                    self.errors,
                    temp
                );
                io::stderr().flush().ok();
                self.drawn = true;
            }
            JobEvent::Failed(e) => {
                self.clear();
                eprintln!("{}", e.message);
                return Some(if e.refused_by_policy { EXIT_REFUSED } else { EXIT_ERROR });
            }
            JobEvent::Finished { report, saved_to, save_error } => {
                self.clear();
                print_summary(&report);
                if let Some(p) = saved_to {
                    eprintln!("\nReport saved: {}", p.display());
                }
                if let Some(e) = save_error {
                    eprintln!("\n{e}");
                }
                if json {
                    println!("{}", serde_json::to_string(&*report).unwrap_or_default());
                }
                use drivetest_core::report::JobStatus;
                return Some(match report.status() {
                    JobStatus::Passed => EXIT_OK,
                    JobStatus::Problems => EXIT_PROBLEMS,
                    JobStatus::Incomplete | JobStatus::Unsupported | JobStatus::Error => EXIT_ERROR,
                });
            }
        }
        None
    }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

fn print_summary(r: &Report) {
    eprintln!("\n=== {} — {} ===", r.test.kind, r.drive.title());
    for f in &r.findings {
        eprintln!("  • {f}");
    }
    if let Some(s) = &r.scan
        && !s.bad_sector_files.is_empty()
    {
        eprintln!("\n  Bad sectors and what occupies them:");
        for sf in s.bad_sector_files.iter().take(20) {
            eprintln!("    LBA {:>12}  {}", sf.lba, sf.location);
        }
        if s.bad_sector_files.len() > 20 {
            eprintln!("    … {} more in the report", s.bad_sector_files.len() - 20);
        }
    }
    eprintln!("\nResult: {}", r.status().label().to_uppercase());
}
