//! Speed-class check (non-destructive).
//!
//! Writes a temporary test file into free space and measures what speed-class
//! ratings depend on:
//!
//! * sustained sequential write, in windows, so dips count (C / U / V classes);
//! * sequential read;
//! * random 4 KiB read and write IOPS (A1 / A2 application classes).
//!
//! The official tests run on a freshly erased card with its own allocation
//! units, so results here are indicative: a card that misses its class by a
//! wide margin is suspect; one that misses by a few percent may be fine.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::aligned::AlignedBuf;
use crate::events::{EngineEvent, Progress};
use crate::platform::Platform;
use crate::policy::{FreeSpaceAuthorization, free_space_root};
use crate::signature::{SplitMix64, fill_signed};

/// Every rating the check knows, strongest-requirement order within each family.
pub const CLASSES: [&str; 13] = ["V90", "V60", "V30", "V10", "V6", "U3", "U1", "C10", "C6", "C4", "C2", "A2", "A1"];

/// Minimum sustained sequential write for a speed / UHS / video class, MB/s.
pub fn min_write_mb_s(class: &str) -> Option<f64> {
    Some(match class.to_ascii_uppercase().as_str() {
        "C2" => 2.0,
        "C4" => 4.0,
        "C6" | "V6" => 6.0,
        "C10" | "U1" | "V10" => 10.0,
        "U3" | "V30" => 30.0,
        "V60" => 60.0,
        "V90" => 90.0,
        _ => return None,
    })
}

/// Minimum random 4 KiB (read, write) IOPS for an application class.
pub fn min_iops(class: &str) -> Option<(f64, f64)> {
    match class.to_ascii_uppercase().as_str() {
        "A1" => Some((1500.0, 500.0)),
        "A2" => Some((4000.0, 2000.0)),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeedClassConfig {
    /// Size of the sequential test file.
    pub test_bytes: u64,
    /// Ratings printed on the card (or read from its registers).
    pub claimed: Vec<String>,
    /// Time budget for each random-I/O phase.
    pub random_seconds: u32,
    /// Free space left untouched.
    pub reserve_bytes: u64,
}

impl Default for SpeedClassConfig {
    fn default() -> Self {
        Self { test_bytes: 2 << 30, claimed: Vec::new(), random_seconds: 8, reserve_bytes: 256 << 20 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassResult {
    pub class: String,
    pub requirement: String,
    pub measured: String,
    pub pass: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeedClassOutcome {
    pub mount: String,
    pub bytes_written: u64,
    pub seq_write_avg_mb_s: f64,
    /// Slowest 64 MiB window of the sequential write.
    pub seq_write_min_mb_s: f64,
    pub seq_read_mb_s: f64,
    pub random_read_iops: f64,
    pub random_write_iops: f64,
    pub results: Vec<ClassResult>,
    /// Highest video / UHS / speed class the measurements meet.
    pub best_class_met: Option<String>,
    pub cleanup_ok: bool,
    pub completed: bool,
    pub cancelled: bool,
    pub note: String,
}

impl SpeedClassOutcome {
    pub fn problems_found(&self) -> bool {
        self.results.iter().any(|r| !r.pass)
    }
}

const WINDOW: u64 = 64 << 20;
const CHUNK: usize = 4 << 20;

pub fn run_speed_class(
    platform: &dyn Platform,
    auth: &FreeSpaceAuthorization,
    cfg: &SpeedClassConfig,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> Result<SpeedClassOutcome, String> {
    let root = free_space_root(platform, auth);
    let free = platform.volume_free_bytes(&root).map_err(|e| format!("Could not read free space: {e}"))?;
    let size = cfg.test_bytes.min(free.saturating_sub(cfg.reserve_bytes)) / CHUNK as u64 * CHUNK as u64;
    if size < 4 * WINDOW {
        return Err(format!("Not enough free space on {} for a meaningful test (need about 512 MB).", auth.mount()));
    }
    let dir = root.join(format!("DriveTest-speed-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    fs::create_dir(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    match platform.path_is_on_volume(&dir, &auth.volume().id) {
        Ok(true) => {}
        other => {
            let _ = fs::remove_dir(&dir);
            return Err(format!(
                "Refusing to write: could not confirm {} is on the selected volume ({other:?}).",
                dir.display()
            ));
        }
    }
    let path = dir.join("speedtest.bin");
    let result = measure(platform, &path, size, cfg, cancel, sink);
    let cleanup_ok = (!path.exists() || fs::remove_file(&path).is_ok()) && fs::remove_dir(&dir).is_ok();
    let mut out = result?;
    out.mount = auth.mount().to_string();
    out.cleanup_ok = cleanup_ok;
    Ok(out)
}

fn measure(
    platform: &dyn Platform,
    path: &std::path::Path,
    size: u64,
    cfg: &SpeedClassConfig,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> Result<SpeedClassOutcome, String> {
    let mut out = SpeedClassOutcome {
        mount: String::new(),
        bytes_written: 0,
        seq_write_avg_mb_s: 0.0,
        seq_write_min_mb_s: 0.0,
        seq_read_mb_s: 0.0,
        random_read_iops: 0.0,
        random_write_iops: 0.0,
        results: Vec::new(),
        best_class_met: None,
        cleanup_ok: false,
        completed: false,
        cancelled: false,
        note: "Indicative: official ratings are measured on a freshly erased card.".into(),
    };
    let mut buf = AlignedBuf::new(CHUNK);
    let total_chunks = size / CHUNK as u64;

    // ---- sequential write
    sink(EngineEvent::Pass { name: "Sequential write".into(), total_blocks: 0, block_size: 0 });
    let mut file = platform.open_test_file(path, true).map_err(|e| format!("Could not create test file: {e}"))?;
    let start = Instant::now();
    let (mut win_bytes, mut win_start) = (0u64, Instant::now());
    let mut windows: Vec<f64> = Vec::new();
    for i in 0..total_chunks {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            return Ok(out);
        }
        fill_signed(&mut buf, 0x5BEED, i * (CHUNK as u64 / 512));
        file.write_all(&buf).map_err(|e| format!("Write failed after {} bytes: {e}", out.bytes_written))?;
        out.bytes_written += CHUNK as u64;
        win_bytes += CHUNK as u64;
        if win_bytes >= WINDOW {
            windows.push(win_bytes as f64 / 1e6 / win_start.elapsed().as_secs_f64().max(1e-6));
            win_bytes = 0;
            win_start = Instant::now();
        }
        sink(EngineEvent::Progress(Progress {
            done: i + 1,
            total: total_chunks,
            bytes: out.bytes_written,
            elapsed: start.elapsed(),
            recent_mb_s: windows.last().copied().unwrap_or(0.0),
        }));
    }
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    out.seq_write_avg_mb_s = out.bytes_written as f64 / 1e6 / start.elapsed().as_secs_f64().max(1e-6);
    out.seq_write_min_mb_s = windows.iter().copied().fold(f64::INFINITY, f64::min);
    if !out.seq_write_min_mb_s.is_finite() {
        out.seq_write_min_mb_s = out.seq_write_avg_mb_s;
    }

    // ---- sequential read
    sink(EngineEvent::Pass { name: "Sequential read".into(), total_blocks: 0, block_size: 0 });
    let mut file = platform.open_unbuffered_read(path).map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut read = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            return Ok(out);
        }
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        read += n as u64;
    }
    out.seq_read_mb_s = read as f64 / 1e6 / start.elapsed().as_secs_f64().max(1e-6);

    // ---- random 4 KiB
    let blocks4k = size / 4096;
    let mut rng = SplitMix64::new(42);
    let mut small = AlignedBuf::new(4096);
    let budget = Duration::from_secs(u64::from(cfg.random_seconds.max(1)));
    sink(EngineEvent::Pass { name: "Random 4K reads".into(), total_blocks: 0, block_size: 0 });
    let start = Instant::now();
    let mut ops = 0u64;
    while start.elapsed() < budget && !cancel.load(Ordering::Relaxed) {
        file.seek(SeekFrom::Start(rng.below(blocks4k) * 4096)).map_err(|e| e.to_string())?;
        file.read_exact(&mut small).map_err(|e| e.to_string())?;
        ops += 1;
    }
    out.random_read_iops = ops as f64 / start.elapsed().as_secs_f64().max(1e-6);
    drop(file);

    sink(EngineEvent::Pass { name: "Random 4K writes".into(), total_blocks: 0, block_size: 0 });
    let mut file = platform.open_for_refresh(path).map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut ops = 0u64;
    while start.elapsed() < budget && !cancel.load(Ordering::Relaxed) {
        file.seek(SeekFrom::Start(rng.below(blocks4k) * 4096)).map_err(|e| e.to_string())?;
        file.write_all(&small).map_err(|e| e.to_string())?;
        ops += 1;
    }
    let _ = file.sync_all();
    out.random_write_iops = ops as f64 / start.elapsed().as_secs_f64().max(1e-6);
    if cancel.load(Ordering::Relaxed) {
        out.cancelled = true;
        return Ok(out);
    }

    // ---- verdicts
    for class in &cfg.claimed {
        let c = class.to_ascii_uppercase();
        if let Some(min) = min_write_mb_s(&c) {
            out.results.push(ClassResult {
                class: c.clone(),
                requirement: format!("≥ {min:.0} MB/s sustained write"),
                measured: format!("{:.1} MB/s slowest window", out.seq_write_min_mb_s),
                pass: out.seq_write_min_mb_s >= min,
            });
        } else if let Some((r, w)) = min_iops(&c) {
            out.results.push(ClassResult {
                class: c.clone(),
                requirement: format!("≥ {r:.0} read / {w:.0} write IOPS (4K random)"),
                measured: format!("{:.0} / {:.0} IOPS", out.random_read_iops, out.random_write_iops),
                pass: out.random_read_iops >= r && out.random_write_iops >= w,
            });
        }
    }
    out.best_class_met = CLASSES
        .iter()
        .filter(|c| !c.starts_with('A'))
        .find(|c| min_write_mb_s(c).is_some_and(|m| out.seq_write_min_mb_s >= m))
        .map(|c| c.to_string());
    out.completed = true;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_tables() {
        assert_eq!(min_write_mb_s("v30"), Some(30.0));
        assert_eq!(min_write_mb_s("U1"), Some(10.0));
        assert_eq!(min_iops("A2"), Some((4000.0, 2000.0)));
        assert_eq!(min_write_mb_s("A1"), None);
    }

    #[test]
    fn runs_on_demo_volume() {
        use crate::demo::DemoPlatform;
        use crate::policy::{Mode, Policy};
        let p = DemoPlatform::new_fast();
        let sd = p.enumerate().unwrap().into_iter().find(|d| d.bus == crate::model::BusType::Sd).unwrap();
        let mount = sd.volumes[0].mount_points[0].clone();
        let auth = Policy::new(Mode::ReadWrite).authorize_free_space(&p, &sd, &mount).unwrap();
        let cfg = SpeedClassConfig {
            test_bytes: 256 << 20,
            claimed: vec!["C10".into(), "A1".into()],
            random_seconds: 1,
            reserve_bytes: 0,
        };
        let out = run_speed_class(&p, &auth, &cfg, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(out.completed && out.cleanup_ok, "{out:?}");
        assert_eq!(out.results.len(), 2);
        assert!(out.seq_write_avg_mb_s > 0.0);
    }
}
