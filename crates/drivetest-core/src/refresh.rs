//! File-level SSD refresh: rewrite files in place with their own bytes.
//!
//! For each file: open it with other writers blocked, read a chunk, write the
//! identical bytes back at the same offset (write-through), read the chunk back
//! without the OS cache and compare. A mismatch is rewritten once from the copy
//! in memory; if it still differs, the file is reported as failing.
//!
//! Why this is safe: the bytes written are identical, so a power cut mid-write
//! leaves either the old or the new data, which are the same. The file keeps
//! its permissions, links, streams and ID; its modified and accessed times and
//! its attributes (including Archive) are restored afterwards. The rewrite is
//! still visible to tools that watch the NTFS change journal or change time.
//!
//! Every path is checked twice: it must be a plain relative path before it is
//! opened, and the opened file's real location (links, junctions and short
//! names resolved) must be on the authorized volume and pass the exclusions.
//!
//! Skipped: files in use, links, cloud placeholders, EFS-encrypted, compressed,
//! sparse, read-only and system files, the pagefile / hibernation / swap files,
//! and — on the system disk — the Windows, Program Files and ProgramData folders.

use std::collections::HashMap;
use std::fs::FileTimes;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::aligned::{AlignedBuf, read_until_eof};
use crate::events::{EngineEvent, Progress};
use crate::fsinfo;
use crate::platform::Platform;
use crate::policy::FileRefreshAuthorization;
use crate::stale::display_path;

/// Which files to refresh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefreshTargets {
    /// Specific files (relative to the volume root), e.g. the slow files found
    /// by the stale-data check. The only option allowed on the system disk.
    Files { paths: Vec<String> },
    /// Every file under a folder (relative to the volume root).
    Folder { path: String },
    /// Every file on the volume.
    WholeVolume,
}

impl RefreshTargets {
    /// Number of listed files for a targeted refresh; `None` for folder / volume.
    pub fn file_count(&self) -> Option<usize> {
        match self {
            RefreshTargets::Files { paths } => Some(paths.len()),
            _ => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            RefreshTargets::Files { paths } => format!("{} selected files", paths.len()),
            RefreshTargets::Folder { path } => format!("folder {path}"),
            RefreshTargets::WholeVolume => "whole volume".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRefreshConfig {
    pub targets: RefreshTargets,
    pub chunk_size: u32,
    /// Re-time each refreshed file afterwards (proof of improvement).
    pub measure_after: bool,
    /// Read speeds before the refresh (from a stale-data check), by relative path.
    #[serde(default)]
    pub before_mb_s: HashMap<String, f64>,
}

impl FileRefreshConfig {
    pub fn new(targets: RefreshTargets) -> Self {
        Self { targets, chunk_size: 1 << 20, measure_after: true, before_mb_s: HashMap::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileNote {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeforeAfter {
    pub path: String,
    pub before_mb_s: Option<f64>,
    pub after_mb_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRefreshOutcome {
    pub mount: String,
    pub targets: String,
    pub on_system_disk: bool,
    pub files_considered: u64,
    pub files_refreshed: u64,
    pub bytes_rewritten: u64,
    pub skipped_count: u64,
    /// First 500 skipped files with reasons.
    pub skipped: Vec<FileNote>,
    /// Files that could not be refreshed or did not read back identically.
    pub failed: Vec<FileNote>,
    /// Chunks that read back wrong once and were rewritten successfully.
    pub chunks_retried: u64,
    pub before_after: Vec<BeforeAfter>,
    pub completed: bool,
    pub cancelled: bool,
}

impl FileRefreshOutcome {
    pub fn problems_found(&self) -> bool {
        !self.failed.is_empty() || self.chunks_retried > 0
    }

    /// Median speed-up for files measured before and after.
    pub fn median_speedup(&self) -> Option<f64> {
        let mut v: Vec<f64> = self
            .before_after
            .iter()
            .filter_map(|b| b.before_mb_s.filter(|x| *x > 0.0).map(|x| b.after_mb_s / x))
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        Some(v[v.len() / 2])
    }
}

/// Collects the files to refresh as validated relative paths. Entries that
/// are not plain relative paths (drive letters, roots, `..`) are rejected.
fn collect(root: &Path, targets: &RefreshTargets, cancel: &AtomicBool) -> (Vec<PathBuf>, Vec<FileNote>) {
    let reject = |p: &str| FileNote { path: p.to_string(), reason: "not a path inside the selected volume".into() };
    match targets {
        RefreshTargets::Files { paths } => {
            let mut ok = Vec::new();
            let mut bad = Vec::new();
            for p in paths {
                match fsinfo::safe_relative(p) {
                    Some(rel) => ok.push(rel),
                    None => bad.push(reject(p)),
                }
            }
            (ok, bad)
        }
        RefreshTargets::Folder { path } => match fsinfo::safe_relative(path) {
            Some(rel) => (walk(root, &rel, cancel), Vec::new()),
            None => (Vec::new(), vec![reject(path)]),
        },
        RefreshTargets::WholeVolume => (walk(root, Path::new(""), cancel), Vec::new()),
    }
}

fn walk(root: &Path, start: &Path, cancel: &AtomicBool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(entries) = std::fs::read_dir(root.join(&dir)) else { continue };
        for e in entries.flatten() {
            let rel = dir.join(e.file_name());
            let Ok(meta) = e.metadata() else { continue };
            if meta.file_type().is_symlink() || fsinfo::attributes(&meta) & fsinfo::ATTR_REPARSE_POINT != 0 {
                continue;
            }
            if meta.is_dir() {
                if !fsinfo::skip_dir(&rel) {
                    stack.push(rel);
                }
            } else if meta.is_file() {
                out.push(rel);
            }
        }
    }
    out
}

enum FileResult {
    Refreshed { bytes: u64, retried: u64 },
    Skipped(String),
    Failed(String),
}

enum ChunkLoop {
    Done,
    Cancelled,
    Mismatch(u64),
    Io(u64, std::io::Error),
}

/// Refreshes one file. `rel` has already been validated as a plain relative path.
fn refresh_one(
    platform: &dyn Platform,
    root: &Path,
    volume_id: &str,
    rel: &Path,
    on_system_disk: bool,
    chunk: usize,
    cancel: &AtomicBool,
) -> FileResult {
    let path = root.join(rel);
    let mut file = match platform.open_for_refresh(&path) {
        Ok(f) => f,
        Err(e) => return FileResult::Skipped(format!("in use or not writable ({e})")),
    };
    // Everything below is judged on the opened handle, not on the name.
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(e) => return FileResult::Skipped(format!("cannot read attributes ({e})")),
    };
    let attrs = fsinfo::attributes(&meta);
    if !meta.is_file() || meta.file_type().is_symlink() || attrs & fsinfo::ATTR_REPARSE_POINT != 0 {
        return FileResult::Skipped("link or reparse point".into());
    }
    let real_rel = match platform.resolve_on_volume(&file, &path, root, volume_id) {
        Ok(r) => r,
        Err(e) => return FileResult::Skipped(format!("could not confirm the file is on the selected volume ({e})")),
    };
    if let Some(reason) = fsinfo::refresh_exclusion(&real_rel, attrs, on_system_disk) {
        return FileResult::Skipped(reason.into());
    }
    let len = meta.len();
    if len == 0 {
        return FileResult::Skipped("empty file".into());
    }
    let modified = meta.modified().ok();
    let accessed = meta.accessed().ok();
    let mut verify = match platform.open_unbuffered_read(&path) {
        Ok(f) => f,
        Err(e) => return FileResult::Skipped(format!("cannot open for verification ({e})")),
    };

    let mut data = vec![0u8; chunk];
    let mut back = AlignedBuf::new(chunk);
    let mut offset = 0u64;
    let mut retried = 0u64;
    let mut wrote = false;
    let outcome = loop {
        if offset >= len {
            break ChunkLoop::Done;
        }
        if cancel.load(Ordering::Relaxed) {
            break ChunkLoop::Cancelled;
        }
        let n = (len - offset).min(chunk as u64) as usize;
        let io = (|| -> std::io::Result<bool> {
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut data[..n])?;
            for attempt in 0..2 {
                file.seek(SeekFrom::Start(offset))?;
                wrote = true;
                file.write_all(&data[..n])?;
                file.flush()?;
                verify.seek(SeekFrom::Start(offset))?;
                let got = read_until_eof(&mut verify, &mut back)?;
                if got >= n && back[..n] == data[..n] {
                    if attempt == 1 {
                        retried += 1;
                    }
                    return Ok(true);
                }
            }
            Ok(false)
        })();
        match io {
            Ok(true) => offset += n as u64,
            Ok(false) => break ChunkLoop::Mismatch(offset),
            Err(e) => break ChunkLoop::Io(offset, e),
        }
    };

    // Check all finalization steps even after a partial rewrite.
    let mut finalization_errors = Vec::new();
    if wrote {
        if let Err(error) = file.sync_all() {
            finalization_errors.push(format!("could not flush data: {error}"));
        }
        let mut times = FileTimes::new();
        if let Some(m) = modified {
            times = times.set_modified(m);
        }
        if let Some(a) = accessed {
            times = times.set_accessed(a);
        }
        if let Err(error) = file.set_times(times) {
            finalization_errors.push(format!("could not restore timestamps: {error}"));
        }
        drop(file);
        if let Err(error) = platform.restore_attributes(&path, attrs) {
            finalization_errors.push(format!("could not restore attributes: {error}"));
        }
    }
    if !finalization_errors.is_empty() {
        return FileResult::Failed(format!("Refresh finalization failed: {}", finalization_errors.join(", ")));
    }
    match outcome {
        ChunkLoop::Done => FileResult::Refreshed { bytes: offset, retried },
        ChunkLoop::Cancelled => FileResult::Skipped("stopped part-way (contents unchanged)".into()),
        ChunkLoop::Mismatch(at) => FileResult::Failed(format!(
            "data at offset {at} did not read back identically after rewriting — the drive may be failing"
        )),
        ChunkLoop::Io(at, e) => FileResult::Failed(format!("I/O error at offset {at}: {e}")),
    }
}

/// Runs a file-level refresh on an authorized volume.
pub fn run_file_refresh(
    platform: &dyn Platform,
    auth: &FileRefreshAuthorization,
    cfg: &FileRefreshConfig,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> FileRefreshOutcome {
    let root = platform.volume_root(auth.volume());
    let mount = auth.mount().to_string();
    let system = auth.on_system_disk();
    sink(EngineEvent::Pass { name: "Finding files".into(), total_blocks: 0, block_size: 0 });
    let (files, rejected) = collect(&root, &cfg.targets, cancel);
    let mut out = FileRefreshOutcome {
        mount: mount.clone(),
        targets: cfg.targets.label(),
        on_system_disk: system,
        files_considered: files.len() as u64,
        files_refreshed: 0,
        bytes_rewritten: 0,
        skipped_count: 0,
        skipped: Vec::new(),
        failed: Vec::new(),
        chunks_retried: 0,
        before_after: Vec::new(),
        completed: false,
        cancelled: false,
    };
    out.skipped_count += rejected.len() as u64;
    out.skipped.extend(rejected);
    sink(EngineEvent::Pass { name: "Refreshing files".into(), total_blocks: 0, block_size: 0 });
    let start = Instant::now();
    let chunk = (cfg.chunk_size.max(4096) / 4096 * 4096) as usize;
    for (i, rel) in files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            break;
        }
        let display = display_path(&mount, rel);
        let abs = root.join(rel);
        let result = refresh_one(platform, &root, &auth.volume().id, rel, system, chunk, cancel);
        match result {
            FileResult::Refreshed { bytes, retried } => {
                out.files_refreshed += 1;
                out.bytes_rewritten += bytes;
                out.chunks_retried += retried;
                if cfg.measure_after
                    && let Ok((b, took)) = platform.time_file_read(&abs, 32 << 20)
                    && took.as_secs_f64() > 0.0
                {
                    let key = rel.to_string_lossy().to_string();
                    out.before_after.push(BeforeAfter {
                        before_mb_s: cfg.before_mb_s.get(&key).copied(),
                        after_mb_s: b as f64 / 1e6 / took.as_secs_f64(),
                        path: display,
                    });
                }
            }
            FileResult::Skipped(reason) => {
                out.skipped_count += 1;
                if out.skipped.len() < 500 {
                    out.skipped.push(FileNote { path: display, reason });
                }
            }
            FileResult::Failed(reason) => {
                sink(EngineEvent::Log(format!("{display}: {reason}")));
                out.failed.push(FileNote { path: display, reason });
            }
        }
        sink(EngineEvent::Progress(Progress {
            done: i as u64 + 1,
            total: files.len() as u64,
            bytes: out.bytes_rewritten,
            elapsed: start.elapsed(),
            recent_mb_s: 0.0,
        }));
    }
    out.cancelled |= cancel.load(Ordering::Relaxed);
    out.completed = !out.cancelled;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::DemoPlatform;
    use crate::model::BusType;
    use crate::policy::{Mode, Policy};

    fn files_on(p: &DemoPlatform, bus: BusType) -> (crate::model::DriveInfo, String) {
        let d = p.enumerate().unwrap().into_iter().find(|d| d.bus == bus).unwrap();
        let m = d.volumes[0].mount_points[0].clone();
        (d, m)
    }

    #[test]
    fn refresh_preserves_content_and_mtime() {
        let p = DemoPlatform::new_fast();
        let (hdd, mount) = files_on(&p, BusType::Sata);
        let auth = Policy::new(Mode::ReadWrite).authorize_file_refresh(&p, &hdd, &mount, false, None).unwrap();
        let root = p.volume_root(auth.volume());
        let path = root.join("probe.bin");
        let content: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(&path, &content).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();

        let cfg = FileRefreshConfig::new(RefreshTargets::Files { paths: vec!["probe.bin".into()] });
        let out = run_file_refresh(&p, &auth, &cfg, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(out.files_refreshed, 1, "{out:?}");
        assert_eq!(out.bytes_rewritten, content.len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), content);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), old);
    }

    #[test]
    fn paths_outside_the_volume_are_refused() {
        let p = DemoPlatform::new_fast();
        let (hdd, mount) = files_on(&p, BusType::Sata);
        let auth = Policy::new(Mode::ReadWrite).authorize_file_refresh(&p, &hdd, &mount, false, None).unwrap();
        let outside = std::env::temp_dir().join(format!("drivetest-outside-{}.bin", std::process::id()));
        std::fs::write(&outside, b"must not be touched").unwrap();
        let before = std::fs::metadata(&outside).unwrap().modified().unwrap();
        let escape = format!("..{0}..{0}..{0}..{0}..{0}..{0}..{0}..{1}", std::path::MAIN_SEPARATOR, outside.display());
        for targets in [
            RefreshTargets::Files { paths: vec![outside.display().to_string(), escape.clone()] },
            RefreshTargets::Folder { path: outside.parent().unwrap().display().to_string() },
        ] {
            let out =
                run_file_refresh(&p, &auth, &FileRefreshConfig::new(targets), &AtomicBool::new(false), &mut |_| {});
            assert_eq!(out.files_refreshed, 0, "{out:?}");
        }
        assert_eq!(std::fs::metadata(&outside).unwrap().modified().unwrap(), before);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn odd_sized_files_refresh() {
        let p = DemoPlatform::new_fast();
        let (hdd, mount) = files_on(&p, BusType::Sata);
        let auth = Policy::new(Mode::ReadWrite).authorize_file_refresh(&p, &hdd, &mount, false, None).unwrap();
        let root = p.volume_root(auth.volume());
        let content: Vec<u8> = (0..1_234_567u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(root.join("odd.bin"), &content).unwrap();
        let cfg = FileRefreshConfig {
            chunk_size: 64 << 10,
            ..FileRefreshConfig::new(RefreshTargets::Files { paths: vec!["odd.bin".into()] })
        };
        let out = run_file_refresh(&p, &auth, &cfg, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(out.files_refreshed, 1, "{out:?}");
        assert_eq!(std::fs::read(root.join("odd.bin")).unwrap(), content);
    }

    #[test]
    fn system_disk_needs_opt_in_and_targets() {
        let p = DemoPlatform::new_fast();
        let (nvme, mount) = files_on(&p, BusType::Nvme);
        let policy = Policy::new(Mode::ReadWrite);
        assert!(policy.authorize_file_refresh(&p, &nvme, &mount, false, Some(1)).is_err());
        assert!(policy.authorize_file_refresh(&p, &nvme, &mount, true, None).is_err());
        assert!(policy.authorize_file_refresh(&p, &nvme, &mount, true, Some(5000)).is_err());
        let auth = policy.authorize_file_refresh(&p, &nvme, &mount, true, Some(1)).unwrap();
        assert!(auth.on_system_disk());
        // Windows folder files are skipped even when listed.
        let cfg = FileRefreshConfig::new(RefreshTargets::Files {
            paths: vec![Path::new("Windows").join("System32").join("demo-driver.sys").to_string_lossy().to_string()],
        });
        let out = run_file_refresh(&p, &auth, &cfg, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(out.files_refreshed, 0);
        assert_eq!(out.skipped_count, 1);
    }
}
