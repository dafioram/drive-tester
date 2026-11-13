//! Runs one test on a worker thread and streams events to the front end.
//!
//! Every job: checks the policy, keeps the PC awake, snapshots health before
//! and after, samples temperature while it runs, builds the report and saves
//! it as JSON.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::analysis::{speed_curve, weak_regions};
use crate::events::{EngineEvent, Progress};
use crate::freespace::{FreeSpaceConfig, run_free_space};
use crate::health::{HealthSnapshot, RecordingVerdict, VerdictLevel, assess_smr, health_changes};
use crate::model::{DriveInfo, MediaKind, format_bytes};
use crate::platform::{Platform, SelfTestKind};
use crate::policy::{Mode, Policy, PolicyError, Tier};
use crate::refresh::{FileRefreshConfig, run_file_refresh};
use crate::report::{Report, ScanReport, SelfTestReport, TemperatureSample, WriteTestReport};
use crate::scan::{BUCKET_COUNT, ScanConfig, ScanOutcome, run_read_scan};
use crate::speedclass::{SpeedClassConfig, run_speed_class};
use crate::stale::{StaleConfig, run_stale_check};
use crate::stats::{format_latency, latency_stats};
use crate::writetest::{WriteTestConfig, WriteTestKind, WriteTestOutcome, run_write_test};

const TEMPERATURE_INTERVAL: Duration = Duration::from_secs(30);

/// What to run.
#[derive(Debug, Clone)]
pub enum JobKind {
    HealthCheck,
    SelfTest(SelfTestKind),
    ReadScan(ScanConfig),
    Recheck(crate::recheck::RecheckPlan),
    FreeSpaceVerify {
        mount: String,
        config: FreeSpaceConfig,
    },
    WriteTest(WriteTestConfig),
    /// Read-only: compares read speed of old and new files on a volume.
    StaleCheck {
        mount: String,
        config: StaleConfig,
    },
    /// In-place file refresh. `system_opt_in` is the explicit opt-in needed on the system disk.
    FileRefresh {
        mount: String,
        config: FileRefreshConfig,
        system_opt_in: bool,
    },
    /// Non-destructive speed-class check using a temporary file.
    SpeedClass {
        mount: String,
        config: SpeedClassConfig,
    },
}

impl JobKind {
    pub fn tier(&self) -> Tier {
        match self {
            JobKind::HealthCheck
            | JobKind::SelfTest(_)
            | JobKind::ReadScan(_)
            | JobKind::Recheck(_)
            | JobKind::StaleCheck { .. } => Tier::ReadOnly,
            JobKind::FreeSpaceVerify { .. } | JobKind::FileRefresh { .. } | JobKind::SpeedClass { .. } => {
                Tier::NonDestructive
            }
            JobKind::WriteTest(_) => Tier::Destructive,
        }
    }

    pub fn name(&self) -> String {
        match self {
            JobKind::HealthCheck => "Health check".into(),
            JobKind::SelfTest(k) => format!("Self-test ({})", k.label()),
            JobKind::ReadScan(_) => "Read scan".into(),
            JobKind::Recheck(_) => "Read-only recheck".into(),
            JobKind::FreeSpaceVerify { .. } => "Free-space verify".into(),
            JobKind::WriteTest(cfg) => cfg.kind.label().into(),
            JobKind::StaleCheck { .. } => "Stale-data check".into(),
            JobKind::FileRefresh { .. } => "SSD file refresh".into(),
            JobKind::SpeedClass { .. } => "Speed-class check".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobRequest {
    pub drive: DriveInfo,
    pub kind: JobKind,
    pub mode: Mode,
    /// Serial typed by the user; required for destructive tests.
    pub confirm_serial: Option<String>,
    /// Where to save the JSON report; `None` skips saving.
    pub report_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    pub message: String,
    /// The safety policy refused the job (as opposed to an I/O failure).
    pub refused_by_policy: bool,
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn refused(e: PolicyError) -> JobError {
    let refused_by_policy = !matches!(e, PolicyError::Open(_) | PolicyError::Platform(_) | PolicyError::LockFailed(_));
    JobError { message: e.to_string(), refused_by_policy }
}

fn failed(message: impl Into<String>) -> JobError {
    JobError { message: message.into(), refused_by_policy: false }
}

#[derive(Debug, Clone)]
pub enum JobEvent {
    Phase(String),
    Engine(EngineEvent),
    Temperature { elapsed_s: f64, celsius: i32 },
    Finished { report: Box<Report>, saved_to: Option<PathBuf>, save_error: Option<String> },
    Failed(JobError),
}

/// A running job. Dropping the handle asks the job to stop.
pub struct JobHandle {
    events: Receiver<JobEvent>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    read_canceller: Option<Arc<dyn Platform>>,
    pub disk: u32,
    pub name: String,
}

impl JobHandle {
    /// Requests cancellation, including best-effort interruption of raw read I/O.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let (Some(platform), Some(thread)) = (&self.read_canceller, &self.thread) {
            platform.cancel_read_io(thread);
        }
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Events received so far, without blocking.
    pub fn poll(&self) -> Vec<JobEvent> {
        self.events.try_iter().collect()
    }

    /// Waits up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<JobEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        if !self.is_finished() {
            self.cancel();
        }
    }
}

/// Starts a job on its own thread.
pub fn start_job(platform: Arc<dyn Platform>, request: JobRequest) -> JobHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let job_cancel = cancel.clone();
    let disk = request.drive.number;
    let name = request.kind.name();
    let read_canceller = matches!(&request.kind, JobKind::ReadScan(_) | JobKind::Recheck(_)).then(|| platform.clone());
    let thread = thread::Builder::new()
        .name(format!("drivetest-disk{disk}"))
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_job(&*platform, &request, &job_cancel, &tx)
            }));
            let event = match outcome {
                Ok(Ok(report)) => {
                    let (saved_to, save_error) = match &request.report_dir {
                        None => (None, None),
                        Some(dir) => {
                            let path = dir.join(report.default_file_name());
                            match report.save(&path) {
                                Ok(()) => (Some(path), None),
                                Err(e) => (None, Some(format!("Could not save report to {}: {e}", path.display()))),
                            }
                        }
                    };
                    JobEvent::Finished { report: Box::new(report), saved_to, save_error }
                }
                Ok(Err(e)) => JobEvent::Failed(e),
                Err(_) => JobEvent::Failed(failed("Internal error: the test thread panicked.")),
            };
            let _ = tx.send(event);
        })
        .expect("spawn job thread");
    JobHandle { events: rx, cancel, thread: Some(thread), read_canceller, disk, name }
}

fn read_health(platform: &dyn Platform, disk: u32) -> HealthSnapshot {
    platform.read_health(disk).unwrap_or_else(|e| HealthSnapshot::unavailable(e.to_string()))
}

/// Runs a job synchronously on the current thread (used by `start_job` and tests).
pub fn run_job(
    platform: &dyn Platform,
    req: &JobRequest,
    cancel: &AtomicBool,
    tx: &Sender<JobEvent>,
) -> Result<Report, JobError> {
    let _awake = platform.keep_awake();
    let policy = Policy::new(req.mode);
    let tier = req.kind.tier();
    // File refresh has its own rules (the system-disk opt-in); everything else
    // goes through the generic tier check first.
    if !matches!(req.kind, JobKind::FileRefresh { .. }) {
        policy.check(&req.drive, tier).map_err(refused)?;
    }

    let disk = req.drive.number;
    let started = Instant::now();
    let mut report = Report::new(platform.name(), &req.drive, &req.kind.name(), tier, req.mode);
    let send = |e: JobEvent| {
        let _ = tx.send(e);
    };

    send(JobEvent::Phase("Reading health data".into()));
    let before = read_health(platform, disk);
    let mut temps: Vec<TemperatureSample> = Vec::new();
    if let Some(c) = before.temperature_c {
        temps.push(TemperatureSample { elapsed_s: 0.0, celsius: c });
        send(JobEvent::Temperature { elapsed_s: 0.0, celsius: c });
    }
    let can_sample = before.temperature_c.is_some();
    report.health.score_before = before.health_score(req.drive.media);
    report.health.before = Some(before.clone());

    let mut log: Vec<String> = Vec::new();
    let mut post_log: Vec<String> = Vec::new();
    let mut last_sample = Instant::now();
    let mut write_probe = None;
    {
        let mut sink = |ev: EngineEvent| {
            if let EngineEvent::Log(m) = &ev
                && log.len() < 500
            {
                log.push(m.clone());
            }
            let _ = tx.send(JobEvent::Engine(ev));
            if can_sample && last_sample.elapsed() >= TEMPERATURE_INTERVAL {
                last_sample = Instant::now();
                if let Ok(h) = platform.read_health(disk)
                    && let Some(c) = h.temperature_c
                {
                    let elapsed_s = started.elapsed().as_secs_f64();
                    temps.push(TemperatureSample { elapsed_s, celsius: c });
                    let _ = tx.send(JobEvent::Temperature { elapsed_s, celsius: c });
                }
            }
        };

        match &req.kind {
            JobKind::HealthCheck => {
                report.test.completed = before.is_available() || before.sd.is_some();
            }
            JobKind::SelfTest(kind) => {
                send(JobEvent::Phase(format!("Running {} self-test", kind.label())));
                let (st, start_error) = run_self_test(platform, disk, *kind, before.self_test(), cancel, &mut sink);
                if let Some(error) = start_error {
                    report.test.unsupported = matches!(error, crate::platform::PlatformError::Unsupported(_));
                    if !report.test.unsupported {
                        report.test.error = Some(error.to_string());
                    }
                }
                report.test.completed = st.final_status.is_some();
                report.test.cancelled = cancel.load(Ordering::Relaxed);
                report.self_test = Some(st);
            }
            JobKind::ReadScan(cfg) => {
                send(JobEvent::Phase("Scanning".into()));
                let mut reader = policy.open_read(platform, &req.drive).map_err(refused)?;
                if reader.size_bytes() == 0
                    || matches!(cfg.mode, crate::scan::ScanMode::Range { start_byte, end_byte }
                        if start_byte >= end_byte || start_byte >= reader.size_bytes())
                {
                    return Err(failed("The scan range contains no bytes on this drive."));
                }
                let mut cfg = cfg.clone();
                cfg.exclude_zero_from_throughput = req.drive.media == MediaKind::Ssd;
                let sector = reader.logical_sector_size().max(512);
                let block = (cfg.block_size / sector).max(1) * sector;
                if cfg.dense && reader.size_bytes().div_ceil(u64::from(block)) > 20_000_000 {
                    return Err(failed(
                        "Dense scan export would exceed 20 million blocks. Use the compact default or increase the block size.",
                    ));
                }
                let outcome = run_read_scan(&mut *reader, &cfg, cancel, &mut sink);
                report.test.error = outcome.error.clone();
                let sector = reader.logical_sector_size();
                drop(reader);
                report.test.completed = outcome.completed;
                report.test.cancelled = outcome.cancelled;
                if !outcome.bad_sectors.is_empty() {
                    send(JobEvent::Phase("Mapping bad sectors to files".into()));
                }
                report.scan = Some(build_scan_report(platform, &req.drive, sector, &cfg, outcome, &mut post_log));
            }
            JobKind::Recheck(plan) => {
                let drives = platform.enumerate().map_err(|error| failed(error.to_string()))?;
                let mut matching = drives.iter().filter(|drive| drive.matches_report_device(&plan.drive));
                if matching.next().is_none_or(|drive| drive.number != disk || !drive.same_device(&req.drive))
                    || matching.next().is_some()
                {
                    return Err(failed("The report's device identity is missing, ambiguous, or changed."));
                }
                let mut reader = policy.open_read(platform, &req.drive).map_err(refused)?;
                let outcome = crate::recheck::run(&mut *reader, plan, cancel, &mut sink);
                report.test.completed = !outcome.cancelled && outcome.error.is_none();
                report.test.cancelled = outcome.cancelled;
                report.test.error = outcome.error.clone();
                report.recheck = Some(outcome);
            }
            JobKind::FreeSpaceVerify { mount, config } => {
                let auth = policy.authorize_free_space(platform, &req.drive, mount).map_err(refused)?;
                send(JobEvent::Phase(format!("Free-space verify on {mount}")));
                let out = run_free_space(platform, &auth, config, cancel, &mut sink).map_err(failed)?;
                report.test.completed = out.completed;
                report.test.cancelled = out.cancelled;
                report.free_space = Some(out);
            }
            JobKind::StaleCheck { mount, config } => {
                let volume = req
                    .drive
                    .volume_by_mount(mount)
                    .ok_or_else(|| failed(format!("{mount} is not a volume on this drive")))?
                    .clone();
                send(JobEvent::Phase(format!("Stale-data check on {mount}")));
                let out = run_stale_check(platform, &volume, mount, req.drive.media, config, cancel, &mut sink)
                    .map_err(failed)?;
                report.test.completed = out.completed;
                report.test.cancelled = out.cancelled;
                report.stale = Some(out);
            }
            JobKind::FileRefresh { mount, config, system_opt_in } => {
                let auth = policy
                    .authorize_file_refresh(platform, &req.drive, mount, *system_opt_in, config.targets.file_count())
                    .map_err(refused)?;
                send(JobEvent::Phase(format!("Refreshing {} on {mount}", config.targets.label())));
                let out = run_file_refresh(platform, &auth, config, cancel, &mut sink);
                report.test.completed = out.completed;
                report.test.cancelled = out.cancelled;
                report.file_refresh = Some(out);
            }
            JobKind::SpeedClass { mount, config } => {
                let auth = policy.authorize_free_space(platform, &req.drive, mount).map_err(refused)?;
                send(JobEvent::Phase(format!("Speed-class check on {mount}")));
                let out = run_speed_class(platform, &auth, config, cancel, &mut sink).map_err(failed)?;
                report.test.completed = out.completed;
                report.test.cancelled = out.cancelled;
                report.speed_class = Some(out);
            }
            JobKind::WriteTest(cfg) => {
                send(JobEvent::Phase("Locking and dismounting volumes".into()));
                let serial = req.confirm_serial.as_deref().unwrap_or("");
                let mut session = policy.begin_destructive(platform, &req.drive, serial).map_err(refused)?;
                report.drive = session.drive().clone();
                send(JobEvent::Phase(cfg.kind.label().into()));
                let out = run_write_test(session.writer(), cfg, cancel, &mut sink);
                drop(session);
                report.test.completed = out.completed;
                report.test.cancelled = out.cancelled;
                write_probe = out.smr.as_ref().filter(|_| out.completed && out.write_errors == 0).cloned();
                report.test.error = out
                    .flush_error
                    .as_ref()
                    .map(|e| format!("Could not flush drive writes: {e}"))
                    .or_else(|| out.repair.as_ref().and_then(|repair| repair.aborted.clone()));
                report.write_test = Some(build_write_report(out));
            }
        }
    }

    log.extend(post_log);
    let after = if report.test.cancelled || report.test.error.is_some() {
        HealthSnapshot::unavailable("Post-test health skipped after an interrupted job")
    } else if matches!(req.kind, JobKind::ReadScan(_) | JobKind::Recheck(_))
        && !platform
            .enumerate()
            .is_ok_and(|drives| drives.iter().any(|drive| drive.number == disk && drive.same_device(&report.drive)))
    {
        HealthSnapshot::unavailable("Drive identity changed before the post-test health snapshot")
    } else {
        send(JobEvent::Phase("Reading health data".into()));
        read_health(platform, disk)
    };
    report.health.changes = health_changes(&before, &after);
    report.health.verdict = Some(if after.is_available() { after.verdict() } else { before.verdict() });
    let identify = after.identify().or(before.identify()).cloned();
    report.health.score_after = after.health_score(report.drive.media);
    report.health.after = Some(after);
    report.smr = Some(assess_smr(req.drive.media, &req.drive.model, identify.as_ref(), write_probe.as_ref()));
    report.temperatures = temps;
    report.log = log;
    report.test.finished_at = Some(Utc::now());
    report.test.status = Some(report.status());
    report.findings = findings(&report);
    Ok(report)
}

fn run_self_test(
    platform: &dyn Platform,
    disk: u32,
    kind: SelfTestKind,
    previous: Option<&crate::health::SelfTestStatus>,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> (SelfTestReport, Option<crate::platform::PlatformError>) {
    let mut rep =
        SelfTestReport { kind: kind.label().into(), started: false, final_status: None, note: None, failed: false };
    if cancel.load(Ordering::Relaxed) {
        return (rep, None);
    }
    if previous.is_some_and(|status| status.in_progress) {
        rep.note = Some("The drive already has a self-test in progress.".into());
        return (rep, None);
    }
    if let Err(e) = platform.start_self_test(disk, kind) {
        rep.note = Some(e.to_string());
        return (rep, Some(e));
    }
    rep.started = true;
    sink(EngineEvent::Pass { name: format!("{} self-test", kind.label()), total_blocks: 0, block_size: 0 });
    sink(EngineEvent::Log(format!("Started the drive's {} self-test.", kind.label())));
    let limit = match kind {
        SelfTestKind::Short => Duration::from_secs(30 * 60),
        SelfTestKind::Extended => Duration::from_secs(48 * 3600),
    };
    let start = Instant::now();
    let mut saw_progress = false;
    loop {
        // Poll every 5 s, checking for cancel every 100 ms.
        for _ in 0..50 {
            if cancel.load(Ordering::Relaxed) {
                rep.note = Some("Stopped watching. The drive keeps running its self-test in the background.".into());
                return (rep, None);
            }
            thread::sleep(Duration::from_millis(100));
        }
        if start.elapsed() > limit {
            rep.note = Some("Gave up waiting for the self-test to finish.".into());
            return (rep, None);
        }
        let Ok(h) = platform.read_health(disk) else { continue };
        match h.self_test() {
            Some(st) if st.in_progress => {
                saw_progress = true;
                let remaining = u64::from(st.percent_remaining.unwrap_or(100));
                sink(EngineEvent::Progress(Progress {
                    done: 100 - remaining.min(100),
                    total: 100,
                    bytes: 0,
                    elapsed: start.elapsed(),
                    recent_mb_s: 0.0,
                }));
            }
            Some(st)
                if (saw_progress || previous.is_some_and(|old| !old.in_progress && old != st))
                    && (st.code == 0 || st.failed) =>
            {
                sink(EngineEvent::Progress(Progress {
                    done: 100,
                    total: 100,
                    bytes: 0,
                    elapsed: start.elapsed(),
                    recent_mb_s: 0.0,
                }));
                rep.final_status = Some(st.description.clone());
                rep.failed = st.failed;
                return (rep, None);
            }
            Some(_) if !saw_progress && start.elapsed() < Duration::from_secs(30) => {}
            Some(st) => {
                rep.note = Some(format!("Could not confirm a completed result for this run: {}", st.description));
                return (rep, None);
            }
            None => {
                rep.note = Some("The drive does not report self-test progress.".into());
                return (rep, None);
            }
        }
    }
}

fn build_scan_report(
    platform: &dyn Platform,
    drive: &DriveInfo,
    sector: u32,
    cfg: &ScanConfig,
    outcome: ScanOutcome,
    log: &mut Vec<String>,
) -> ScanReport {
    let positional = cfg.mode.is_positional();
    let curve = if positional { outcome.speed_curve_mb_s.clone() } else { Vec::new() };
    let weak = if positional { weak_regions(&outcome.map, &curve) } else { Vec::new() };
    let bad_sector_files = if outcome.bad_sectors.is_empty() {
        Vec::new()
    } else {
        let take = outcome.bad_sectors.len().min(1000);
        if take < outcome.bad_sectors.len() {
            log.push(format!(
                "File lookup covers the first {take} of {} unreadable sectors.",
                outcome.bad_sectors.len()
            ));
        }
        match platform.map_sectors_to_files(drive, sector, &outcome.bad_sectors[..take]) {
            Ok(v) => v,
            Err(e) => {
                log.push(format!("Bad sectors could not be mapped to files: {e}"));
                Vec::new()
            }
        }
    };
    let secs = outcome.elapsed.as_secs_f64();

    ScanReport {
        mode: cfg.mode.clone(),
        block_size: outcome.io_block_size,
        total_blocks: outcome.total_blocks,
        scanned_blocks: outcome.scanned_blocks,
        bytes_read: outcome.bytes_read,
        elapsed_s: secs,
        average_mb_s: if outcome.data_io_us > 0 { outcome.data_bytes as f64 / outcome.data_io_us as f64 } else { 0.0 },
        latency: outcome.latency,
        bucket_counts: outcome.bucket_counts,
        slow_blocks: outcome.slow_blocks,
        slow_offsets: outcome.slow_offsets,
        recovered_blocks: outcome.recovered_blocks,
        scan_config: Some(cfg.clone()),
        zero_blocks: outcome.zero_blocks,
        data_bytes: Some(outcome.data_bytes),
        bad_sectors: outcome.bad_sectors,
        bad_sector_files,
        unresolved_blocks: outcome.unresolved_blocks,
        unresolved_block_count: outcome.unresolved_block_count,
        weak_regions: weak,
        speed_curve_mb_s: curve,
        blocks: outcome.map,
    }
}

fn build_write_report(out: WriteTestOutcome) -> WriteTestReport {
    let full = matches!(out.kind, WriteTestKind::FullWriteVerify);
    WriteTestReport {
        bytes_written: out.bytes_written,
        elapsed_s: out.elapsed.as_secs_f64(),
        write_errors: out.write_errors,
        flush_error: out.flush_error,
        corrupt_units: out.corrupt_units,
        aliased_units: out.aliased_units,
        unreadable_units: out.unreadable_units,
        first_bad_offset: out.first_bad_offset,
        bad_offsets: out.bad_offsets,
        capacity: out.capacity,
        cache_cliff: out.cache_cliff,
        smr: out.smr,
        repair: out.repair,
        write_latency: latency_stats(out.write_map.ok_latencies()),
        write_speed_curve_mb_s: if full { speed_curve(&out.write_map, 200) } else { Vec::new() },
        write_blocks: out.write_map,
        verify_blocks: out.verify_map,
        kind: out.kind,
    }
}

/// Plain-language findings, most important first.
pub fn findings(r: &Report) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(error) = &r.test.error {
        f.push(error.clone());
    }
    if r.test.cancelled {
        f.push("The test was stopped before it finished; results cover only the part completed.".into());
    }
    let remapping = r.write_test.as_ref().is_some_and(|w| matches!(w.kind, WriteTestKind::Remap { .. }));
    for c in &r.health.changes {
        if remapping && c.metric.starts_with("Reallocated") {
            f.push(format!(
                "{} rose from {} to {}, as expected: the drive swapped in spares for the overwritten sectors.",
                c.metric, c.before, c.after
            ));
        } else {
            f.push(format!(
                "During the test, {} rose from {} to {}. A rise during testing is a strong failure signal.",
                c.metric, c.before, c.after
            ));
        }
    }
    if let Some(v) = &r.health.verdict
        && v.level != VerdictLevel::Unknown
    {
        f.push(format!("Health: {} — {}", v.level.label(), v.reasons.join("; ")));
    }
    if let Some(score) = r.health.score_after.as_ref().or(r.health.score_before.as_ref()) {
        f.push(format!("Custom health score: {}% ({})", score.percent, score.status.label()));
    }
    if let Some(w) = &r.write_test {
        if let Some(c) = &w.capacity {
            f.push(c.summary.clone());
        }
        if let Some(cliff) = &w.cache_cliff {
            f.push(format!(
                "Writes ran at {:.0} MB/s for the first {}, then dropped to {:.0} MB/s (write cache exhausted).",
                cliff.fast_mb_s,
                format_bytes(cliff.after_bytes),
                cliff.sustained_mb_s
            ));
        }
    }
    if let Some(st) = &r.stale {
        f.push(format!("Stale data: {}", st.verdict));
        if let Some(n) = &st.media_note {
            f.push(n.clone());
        }
    }
    if let Some(fr) = &r.file_refresh {
        let mut line = format!(
            "Refreshed {} of {} files ({}); {} skipped.",
            fr.files_refreshed,
            fr.files_considered,
            format_bytes(fr.bytes_rewritten),
            fr.skipped_count
        );
        if let Some(x) = fr.median_speedup() {
            line.push_str(&format!(" Median read speed-up: {x:.1}×."));
        }
        f.push(line);
        if !fr.failed.is_empty() {
            f.push(format!(
                "{} files did not read back identically or could not be rewritten — the drive may be failing.",
                fr.failed.len()
            ));
        }
    }
    if let Some(sc) = &r.speed_class {
        f.push(sc.note.clone());
        f.push(format!(
            "Sequential write {:.1} MB/s (slowest window {:.1}), read {:.1} MB/s, random 4K {:.0} read / {:.0} write IOPS.",
            sc.seq_write_avg_mb_s, sc.seq_write_min_mb_s, sc.seq_read_mb_s, sc.random_read_iops, sc.random_write_iops
        ));
        for c in &sc.results {
            f.push(format!(
                "{}: {} — needs {}, measured {}.",
                c.class,
                if c.pass { "meets" } else { "FAILS" },
                c.requirement,
                c.measured
            ));
        }
        if let Some(best) = &sc.best_class_met {
            f.push(format!("Highest write class threshold met on this setup: {best}."));
        }
    }
    if let Some(w) = &r.write_test
        && let Some(rep) = &w.repair
    {
        if rep.blocks_rewritten > 0 || rep.blocks_checked > 0 {
            f.push(format!(
                "Rewrote {} blocks ({}); skipped {} empty and {} fast blocks; {} unreadable.",
                rep.blocks_rewritten,
                format_bytes(rep.bytes_rewritten),
                rep.zero_blocks_skipped,
                rep.fast_blocks_skipped,
                rep.unreadable_blocks
            ));
        }
        if !rep.remapped_sectors.is_empty() {
            f.push(format!(
                "{} unreadable sectors were overwritten and now return the replacement bytes.",
                rep.remapped_sectors.len()
            ));
        }
        if !rep.still_bad_sectors.is_empty() {
            f.push(format!(
                "{} sectors are still unreadable after rewriting — replace the drive.",
                rep.still_bad_sectors.len()
            ));
        }
        if rep.verify_failures > 0 {
            f.push(format!("{} blocks did not read back identically after rewriting.", rep.verify_failures));
        }
    }
    if let Some(fs) = &r.free_space
        && !fs.cleanup_ok
    {
        f.push(format!("Temporary test files could not be fully cleaned up at {}.", fs.directory));
    }
    if let Some(fs) = &r.free_space
        && let Some(c) = &fs.capacity
    {
        f.push(c.summary.clone());
    }
    if let Some(result) = &r.recheck {
        use crate::recheck::RecheckState;
        let count = |state| result.reads.iter().filter(|read| read.state == state).count();
        f.push(format!(
            "Rechecked {} of {} locations: {} readable at normal speed, {} still slow, {} unreadable, {} I/O errors.",
            result.reads.len(),
            result.planned,
            count(RecheckState::Readable),
            count(RecheckState::StillSlow),
            count(RecheckState::Unreadable),
            count(RecheckState::IoError)
        ));
    }
    if let Some(s) = &r.scan {
        f.push(format!(
            "{}: {} of {} blocks checked ({} read).",
            s.mode.label(),
            s.scanned_blocks,
            s.total_blocks,
            format_bytes(s.bytes_read)
        ));
        if s.recovered_blocks > 0 {
            f.push(format!("{} blocks read successfully after a retry.", s.recovered_blocks));
        }
        let unresolved = s.unresolved_block_count.max(s.unresolved_blocks.len() as u64);
        if unresolved > 0 {
            f.push(format!("{unresolved} blocks could not be fully diagnosed."));
        }
        if unresolved > s.unresolved_blocks.len() as u64
            || s.slow_blocks > s.slow_offsets.len() as u64 && s.scan_config.is_some()
        {
            f.push("The report retained the first 100,000 addresses in each capped list.".into());
        }
        if !s.bad_sectors.is_empty() {
            f.push(format!("{} unreadable sectors found.", s.bad_sectors.len()));
        } else if s.bucket_counts[BUCKET_COUNT - 1] > 0 {
            f.push(format!("{} blocks could not be read.", s.bucket_counts[BUCKET_COUNT - 1]));
        }
        if s.slow_blocks > 0 {
            f.push(format!(
                "{} blocks took over 150 ms to read — usually internal retries, an early warning sign.",
                s.slow_blocks
            ));
        }
        if !s.weak_regions.is_empty() {
            f.push(format!("{} region(s) read markedly slower than their surroundings.", s.weak_regions.len()));
        }
        if r.drive.media == MediaKind::Ssd && s.zero_blocks > 0 && s.scanned_blocks > 0 {
            f.push(format!(
                "{:.0}% of checked blocks returned zeros. They are excluded from the SSD throughput summary.",
                100.0 * s.zero_blocks as f64 / s.scanned_blocks as f64
            ));
        }
        if s.latency.samples > 0 {
            f.push(format!(
                "Average {:.1} MB/s; latency p50 {}, p99 {}, max {}.",
                s.average_mb_s,
                format_latency(s.latency.p50_us),
                format_latency(s.latency.p99_us),
                format_latency(s.latency.max_us)
            ));
        }
    }
    if let Some(st) = &r.self_test {
        match (&st.final_status, &st.note) {
            (Some(s), _) => f.push(format!("Self-test: {s}.")),
            (None, Some(n)) => f.push(format!("Self-test: {n}")),
            _ => {}
        }
    }
    if let Some(smr) = &r.smr
        && !matches!(smr.verdict, RecordingVerdict::NotApplicable | RecordingVerdict::Unknown)
    {
        f.push(format!("Recording: {} — {}.", smr.headline(), smr.evidence));
    }
    if r.temperatures.len() > 1 {
        let max = r.temperatures.iter().map(|t| t.celsius).max().unwrap_or(0);
        f.push(format!("Peak temperature during the test: {max} °C."));
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::DemoPlatform;
    use crate::model::BusType;

    fn run(
        platform: &DemoPlatform,
        drive: &DriveInfo,
        kind: JobKind,
        mode: Mode,
        serial: Option<&str>,
    ) -> Result<Report, JobError> {
        let (tx, _rx) = mpsc::channel();
        let req = JobRequest {
            drive: drive.clone(),
            kind,
            mode,
            confirm_serial: serial.map(str::to_string),
            report_dir: None,
        };
        run_job(platform, &req, &AtomicBool::new(false), &tx)
    }

    fn drive(platform: &DemoPlatform, bus: BusType) -> DriveInfo {
        platform.enumerate().unwrap().into_iter().find(|d| d.bus == bus).unwrap()
    }

    #[test]
    fn read_scan_on_system_disk_is_allowed() {
        let p = DemoPlatform::new_fast();
        let nvme = drive(&p, BusType::Nvme);
        let cfg = ScanConfig { mode: crate::scan::ScanMode::RandomSample { percent: 1.0 }, ..Default::default() };
        let r = run(&p, &nvme, JobKind::ReadScan(cfg), Mode::ReadOnly, None).unwrap();
        assert!(r.test.completed);
        assert!(r.scan.unwrap().scanned_blocks > 0);
    }

    #[test]
    fn write_test_on_system_disk_is_refused() {
        let p = DemoPlatform::new_fast();
        let nvme = drive(&p, BusType::Nvme);
        let kind = JobKind::WriteTest(WriteTestConfig::new(WriteTestKind::FullWriteVerify));
        let err = run(&p, &nvme, kind, Mode::ReadWrite, Some(&nvme.serial)).unwrap_err();
        assert!(err.refused_by_policy);
    }

    #[test]
    fn hdd_scan_reports_bad_sectors_and_health_change() {
        let p = DemoPlatform::new_fast();
        let hdd = drive(&p, BusType::Sata);
        let r = run(&p, &hdd, JobKind::ReadScan(ScanConfig::default()), Mode::ReadOnly, None).unwrap();
        let scan = r.scan.as_ref().unwrap();
        assert_eq!(scan.bad_sectors.len(), 4);
        assert_eq!(scan.bad_sector_files.len(), 4);
        assert!(scan.slow_blocks >= 3);
        assert!(!scan.weak_regions.is_empty());
        assert!(!r.health.changes.is_empty(), "pending sectors should rise");
        assert!(r.problems_found());
        assert!(r.findings.iter().any(|f| f.contains("unreadable sectors")));
        // The report must serialise.
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"schema\":1"));
    }

    #[test]
    fn fake_usb_is_caught_by_probe() {
        let p = DemoPlatform::new_fast();
        let usb = drive(&p, BusType::Usb);
        let kind = JobKind::WriteTest(WriteTestConfig::new(WriteTestKind::FakeCapacityProbe { samples: 256 }));
        let r = run(&p, &usb, kind, Mode::ReadWrite, Some(&usb.serial)).unwrap();
        let cap = r.write_test.unwrap().capacity.unwrap();
        assert!(!cap.genuine, "{}", cap.summary);
    }

    #[test]
    fn sd_full_verify_shows_cache_cliff() {
        let p = DemoPlatform::new_fast();
        let sd = drive(&p, BusType::Sd);
        let kind = JobKind::WriteTest(WriteTestConfig::new(WriteTestKind::FullWriteVerify));
        let r = run(&p, &sd, kind, Mode::ReadWrite, Some(&sd.serial)).unwrap();
        let w = r.write_test.unwrap();
        assert!(w.capacity.unwrap().genuine);
        let cliff = w.cache_cliff.expect("cache cliff");
        assert!((cliff.after_bytes as i64 - (512 << 20) as i64).abs() < (40 << 20));
    }

    #[test]
    fn health_check_with_no_telemetry_is_incomplete() {
        let p = DemoPlatform::new_fast();
        for d in p.enumerate().unwrap() {
            let r = run(&p, &d, JobKind::HealthCheck, Mode::ReadOnly, None).unwrap();
            assert_eq!(r.test.completed, d.bus != BusType::Usb);
            assert!(r.health.verdict.is_some());
        }
    }
}
