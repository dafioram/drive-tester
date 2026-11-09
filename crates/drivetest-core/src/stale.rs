//! Stale-data check (read-only).
//!
//! On TLC and QLC SSDs, data that has sat unrewritten for a long time can read
//! much slower than fresh data, because the cells' charge drifts and the
//! controller needs more error correction. This check samples files of
//! different ages on a mounted volume, reads them without the OS cache, and
//! compares their speed. Slow old files are candidates for a targeted refresh.
//!
//! Nothing is written, so the check is allowed on the system disk. Cloud
//! placeholders are skipped because reading them would trigger a download.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::events::{EngineEvent, Progress};
use crate::fsinfo;
use crate::model::{MediaKind, VolumeInfo, format_bytes};
use crate::platform::Platform;
use crate::signature::SplitMix64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaleConfig {
    /// Files read in total.
    pub max_files: usize,
    /// Bytes read from the start of each file.
    pub bytes_per_file: u64,
    /// Smaller files are ignored (too short to time reliably).
    pub min_file_size: u64,
    /// Upper bound on files examined while walking the volume.
    pub max_candidates: usize,
    pub seed: u64,
}

impl Default for StaleConfig {
    fn default() -> Self {
        Self {
            max_files: 240,
            bytes_per_file: 32 << 20,
            min_file_size: 4 << 20,
            max_candidates: 200_000,
            seed: 0x57A1E,
        }
    }
}

/// Read speed of one sampled file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileSpeed {
    /// Path relative to the volume root.
    pub path: String,
    /// Mount point + path, for display.
    pub display: String,
    pub size: u64,
    /// Days since the file was last written.
    pub age_days: u32,
    pub mb_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgeBucket {
    pub label: String,
    pub files: u32,
    pub median_mb_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaleOutcome {
    pub mount: String,
    pub files_examined: u64,
    pub files_sampled: u32,
    pub bytes_read: u64,
    /// Typical speed of data that reads well (75th percentile of the sample).
    pub baseline_mb_s: f64,
    pub buckets: Vec<AgeBucket>,
    /// Files reading at under half the baseline, slowest first.
    pub slow_files: Vec<FileSpeed>,
    pub sampled: Vec<FileSpeed>,
    pub skipped: u64,
    /// True when old data reads markedly slower than new data.
    pub stale_detected: bool,
    pub verdict: String,
    pub media_note: Option<String>,
    pub completed: bool,
    pub cancelled: bool,
}

impl StaleOutcome {
    /// Relative paths → MB/s, for before/after comparisons.
    pub fn speeds(&self) -> HashMap<String, f64> {
        self.sampled.iter().map(|f| (f.path.clone(), f.mb_s)).collect()
    }
}

const BUCKETS: [(&str, u32); 5] =
    [("Under 30 days", 0), ("1–6 months", 30), ("6–12 months", 180), ("1–2 years", 365), ("Over 2 years", 730)];

fn bucket_of(age_days: u32) -> usize {
    BUCKETS.iter().rposition(|(_, min)| age_days >= *min).unwrap_or(0)
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

struct Candidate {
    rel: PathBuf,
    size: u64,
    age_days: u32,
}

/// Joins a mount point and a relative path for display.
pub fn display_path(mount: &str, rel: &Path) -> String {
    let rel = rel.to_string_lossy().replace('/', "\\");
    format!("{}\\{}", mount.trim_end_matches('\\'), rel)
}

fn walk(
    root: &Path,
    cfg: &StaleConfig,
    cancel: &AtomicBool,
    rng: &mut SplitMix64,
    skipped: &mut u64,
) -> (Vec<Candidate>, u64) {
    let now = SystemTime::now();
    let mut stack = vec![PathBuf::new()];
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen = 0u64;
    while let Some(rel_dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(entries) = std::fs::read_dir(root.join(&rel_dir)) else {
            *skipped += 1;
            continue;
        };
        for entry in entries.flatten() {
            let rel = rel_dir.join(entry.file_name());
            let Ok(meta) = entry.metadata() else {
                *skipped += 1;
                continue;
            };
            let attrs = fsinfo::attributes(&meta);
            if attrs & fsinfo::ATTR_REPARSE_POINT != 0 || meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if !fsinfo::skip_dir(&rel) {
                    stack.push(rel);
                }
                continue;
            }
            if !meta.is_file() || meta.len() < cfg.min_file_size || fsinfo::is_cloud_placeholder(attrs) {
                continue;
            }
            let age_days = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .map_or(0, |d| (d.as_secs() / 86_400) as u32);
            seen += 1;
            let c = Candidate { rel, size: meta.len(), age_days };
            // Reservoir sampling keeps the candidate set fair on huge volumes.
            if out.len() < cfg.max_candidates {
                out.push(c);
            } else {
                let j = rng.below(seen) as usize;
                if j < cfg.max_candidates {
                    out[j] = c;
                }
            }
        }
    }
    (out, seen)
}

/// Runs the stale-data check on a mounted volume.
pub fn run_stale_check(
    platform: &dyn Platform,
    volume: &VolumeInfo,
    mount: &str,
    media: MediaKind,
    cfg: &StaleConfig,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> Result<StaleOutcome, String> {
    let root = platform.volume_root(volume);
    let mut rng = SplitMix64::new(cfg.seed);
    let mut skipped = 0u64;
    sink(EngineEvent::Pass { name: "Finding files".into(), total_blocks: 0, block_size: 0 });
    sink(EngineEvent::Log(format!("Looking for files of {} or more on {mount}", format_bytes(cfg.min_file_size))));
    let (candidates, examined) = walk(&root, cfg, cancel, &mut rng, &mut skipped);
    if candidates.is_empty() && !cancel.load(Ordering::Relaxed) {
        return Err(format!("No files of {} or more found on {mount}.", format_bytes(cfg.min_file_size)));
    }

    // Pick evenly across age buckets so old data is represented.
    let mut by_bucket: Vec<Vec<Candidate>> = (0..BUCKETS.len()).map(|_| Vec::new()).collect();
    for c in candidates {
        by_bucket[bucket_of(c.age_days)].push(c);
    }
    for b in &mut by_bucket {
        rng.shuffle(b);
    }
    let mut picks: Vec<Candidate> = Vec::new();
    while picks.len() < cfg.max_files && by_bucket.iter().any(|b| !b.is_empty()) {
        for b in &mut by_bucket {
            if let Some(c) = b.pop() {
                picks.push(c);
            }
        }
    }
    picks.truncate(cfg.max_files);

    sink(EngineEvent::Pass { name: "Timing reads".into(), total_blocks: 0, block_size: 0 });
    let start = Instant::now();
    let mut sampled: Vec<FileSpeed> = Vec::new();
    let mut bytes_read = 0u64;
    let mut cancelled = false;
    for (i, c) in picks.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        match platform.time_file_read(&root.join(&c.rel), cfg.bytes_per_file) {
            Ok((bytes, took)) if bytes > 0 && took.as_secs_f64() > 0.0 => {
                bytes_read += bytes;
                sampled.push(FileSpeed {
                    path: c.rel.to_string_lossy().to_string(),
                    display: display_path(mount, &c.rel),
                    size: c.size,
                    age_days: c.age_days,
                    mb_s: bytes as f64 / 1e6 / took.as_secs_f64(),
                });
            }
            _ => skipped += 1,
        }
        sink(EngineEvent::Progress(Progress {
            done: i as u64 + 1,
            total: picks.len() as u64,
            bytes: bytes_read,
            elapsed: start.elapsed(),
            recent_mb_s: 0.0,
        }));
    }

    let mut speeds: Vec<f64> = sampled.iter().map(|f| f.mb_s).collect();
    speeds.sort_by(|a, b| a.total_cmp(b));
    let baseline = speeds.get(speeds.len() * 3 / 4).copied().unwrap_or(0.0);
    let buckets: Vec<AgeBucket> = BUCKETS
        .iter()
        .enumerate()
        .map(|(i, (label, _))| {
            let v: Vec<f64> = sampled.iter().filter(|f| bucket_of(f.age_days) == i).map(|f| f.mb_s).collect();
            AgeBucket { label: (*label).into(), files: v.len() as u32, median_mb_s: median(v) }
        })
        .collect();
    let mut slow_files: Vec<FileSpeed> = sampled.iter().filter(|f| f.mb_s < 0.5 * baseline).cloned().collect();
    slow_files.sort_by(|a, b| a.mb_s.total_cmp(&b.mb_s));

    let recent = buckets.iter().take(2).filter(|b| b.files >= 3).map(|b| b.median_mb_s).fold(0.0, f64::max);
    let old: Vec<&AgeBucket> = buckets.iter().skip(2).filter(|b| b.files >= 3).collect();
    let old_median = median(old.iter().map(|b| b.median_mb_s).collect());
    let stale_detected = recent > 0.0 && !old.is_empty() && old_median < 0.6 * recent;
    let verdict = if sampled.len() < 10 {
        "Too few files to judge; results are indicative only.".to_string()
    } else if stale_detected {
        format!(
            "Old data reads markedly slower: about {old_median:.0} MB/s versus {recent:.0} MB/s for recent files. \
             A targeted refresh of the {} slow files is likely to help.",
            slow_files.len()
        )
    } else if recent == 0.0 || old.is_empty() {
        "Not enough recent and old files to compare ages; see the per-file speeds.".to_string()
    } else {
        "No significant slowdown of old data.".to_string()
    };
    let media_note = match media {
        MediaKind::Hdd => Some(
            "On hard drives, speed differences mostly reflect where files sit on the platter and \
             fragmentation, not data age. Refresh is not useful for this."
                .into(),
        ),
        MediaKind::Unknown => Some("Media type unknown; interpret with care.".into()),
        MediaKind::Ssd => None,
    };
    Ok(StaleOutcome {
        mount: mount.to_string(),
        files_examined: examined,
        files_sampled: sampled.len() as u32,
        bytes_read,
        baseline_mb_s: baseline,
        buckets,
        slow_files,
        sampled,
        skipped,
        stale_detected,
        verdict,
        media_note,
        completed: !cancelled,
        cancelled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::DemoPlatform;

    #[test]
    fn demo_nvme_shows_stale_old_files() {
        let p = DemoPlatform::new_fast();
        let nvme = p.enumerate().unwrap().into_iter().find(|d| d.bus == crate::model::BusType::Nvme).unwrap();
        let vol = nvme.volumes[0].clone();
        let mount = vol.mount_points[0].clone();
        let out = run_stale_check(
            &p,
            &vol,
            &mount,
            MediaKind::Ssd,
            &StaleConfig::default(),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert!(out.files_sampled >= 10, "{out:?}");
        assert!(out.stale_detected, "{}", out.verdict);
        assert!(!out.slow_files.is_empty());
        assert!(out.slow_files.iter().all(|f| f.age_days >= 180));
    }

    #[test]
    fn buckets_by_age() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(45), 1);
        assert_eq!(bucket_of(400), 3);
        assert_eq!(bucket_of(5000), 4);
    }
}
