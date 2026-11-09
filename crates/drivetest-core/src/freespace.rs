//! Non-destructive free-space test (in the style of H2testw).
//!
//! Fills a mounted volume's free space with signed test files, reads them back
//! and deletes them. Existing files are never touched. Catches fake-capacity
//! media and bad flash without wiping the drive.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::aligned::AlignedBuf;
use crate::events::{Batcher, EngineEvent};
use crate::model::format_bytes;
use crate::platform::Platform;
use crate::policy::{FreeSpaceAuthorization, free_space_root};
use crate::signature::{UNIT, check_signed, fill_signed, fresh_seed};
use crate::writetest::CapacityVerdict;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FreeSpaceConfig {
    pub file_size: u64,
    /// Free space left untouched so the volume never fills completely.
    pub reserve_bytes: u64,
    pub block_size: u32,
    pub seed: u64,
    /// Optional cap on how much to write.
    pub max_bytes: Option<u64>,
}

impl Default for FreeSpaceConfig {
    fn default() -> Self {
        Self { file_size: 1 << 30, reserve_bytes: 256 << 20, block_size: 1 << 20, seed: fresh_seed(), max_bytes: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FreeSpaceOutcome {
    pub mount: String,
    pub directory: String,
    pub free_bytes_before: u64,
    pub files_written: u32,
    pub bytes_written: u64,
    pub bytes_verified: u64,
    pub write_mb_s: f64,
    pub read_mb_s: f64,
    pub write_error: Option<String>,
    pub corrupt_units: u64,
    pub aliased_units: u64,
    pub unreadable_bytes: u64,
    pub first_bad: Option<String>,
    pub completed: bool,
    pub cancelled: bool,
    pub cleanup_ok: bool,
    pub capacity: Option<CapacityVerdict>,
}

impl FreeSpaceOutcome {
    pub fn problems_found(&self) -> bool {
        self.corrupt_units + self.aliased_units > 0 || self.unreadable_bytes > 0 || self.write_error.is_some()
    }
}

/// Size of the files' I/O blocks is rounded to this, which suits unbuffered
/// I/O on 512-byte and 4K-sector volumes alike.
const FILE_ALIGN: u64 = 4096;

fn chunk_name(index: u64) -> String {
    format!("chunk-{index:05}.bin")
}

/// Deletes only the files this test created, then the (now empty) folder.
/// Anything else that appeared in the folder is left alone.
fn cleanup(dir: &std::path::Path, files: u64) -> bool {
    let mut ok = true;
    for f in 0..files {
        let path = dir.join(chunk_name(f));
        if path.exists() && fs::remove_file(&path).is_err() {
            ok = false;
        }
    }
    ok && fs::remove_dir(dir).is_ok()
}

/// Cleans up even if the test panics.
struct DirGuard {
    dir: PathBuf,
    files: u64,
    armed: bool,
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        if self.armed {
            cleanup(&self.dir, self.files);
        }
    }
}

pub fn run_free_space(
    platform: &dyn Platform,
    auth: &FreeSpaceAuthorization,
    cfg: &FreeSpaceConfig,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> Result<FreeSpaceOutcome, String> {
    let root = free_space_root(platform, auth);
    let free =
        platform.volume_free_bytes(&root).map_err(|e| format!("Could not read free space on {}: {e}", auth.mount()))?;
    let bs = (u64::from(cfg.block_size) / FILE_ALIGN).max(1) * FILE_ALIGN;
    let mut budget = free.saturating_sub(cfg.reserve_bytes);
    if let Some(cap) = cfg.max_bytes {
        budget = budget.min(cap);
    }
    budget = budget / bs * bs;
    if budget == 0 {
        return Err(format!("Not enough free space on {} to test.", auth.mount()));
    }

    let dir = root.join(format!("DriveTest-verify-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    fs::create_dir(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    // Confirm the folder really landed on the authorized volume (guards against
    // a mount point that silently fell through to its parent volume).
    match platform.path_is_on_volume(&dir, &auth.volume().id) {
        Ok(true) => {}
        Ok(false) => {
            let _ = fs::remove_dir(&dir);
            return Err(format!("Refusing to write: {} is not on the selected volume.", dir.display()));
        }
        Err(e) => {
            let _ = fs::remove_dir(&dir);
            return Err(format!("Could not confirm which volume {} is on: {e}", dir.display()));
        }
    }
    let file_size = (cfg.file_size / bs).max(1) * bs;
    let file_count = budget.div_ceil(file_size);
    let mut guard = DirGuard { dir: dir.clone(), files: file_count, armed: true };
    let mut out = FreeSpaceOutcome {
        mount: auth.mount().to_string(),
        directory: dir.display().to_string(),
        free_bytes_before: free,
        files_written: 0,
        bytes_written: 0,
        bytes_verified: 0,
        write_mb_s: 0.0,
        read_mb_s: 0.0,
        write_error: None,
        corrupt_units: 0,
        aliased_units: 0,
        unreadable_bytes: 0,
        first_bad: None,
        completed: false,
        cancelled: false,
        cleanup_ok: false,
        capacity: None,
    };

    let mut buf = AlignedBuf::new(bs as usize);
    let mut batcher = Batcher::new(sink);
    batcher.log(format!("Writing {} of test files to {}", format_bytes(budget), dir.display()));
    batcher.pass("Writing test files", budget / bs, 0, bs as u32);

    // ---- write pass
    let write_start = Instant::now();
    let mut written_sizes: Vec<u64> = Vec::new();
    'files: for f in 0..file_count {
        let this_size = file_size.min(budget - f * file_size);
        let path = dir.join(chunk_name(f));
        let mut file = match platform.open_test_file(&path, true) {
            Ok(file) => file,
            Err(e) => {
                out.write_error = Some(format!("Could not create {}: {e}", path.display()));
                break;
            }
        };
        let mut file_bytes = 0u64;
        for b in 0..this_size / bs {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                written_sizes.push(file_bytes);
                break 'files;
            }
            let unit = (f * file_size + b * bs) / UNIT as u64;
            fill_signed(&mut buf, cfg.seed, unit);
            if let Err(e) = file.write_all(&buf) {
                out.write_error =
                    Some(format!("Write failed in {} after {}: {e}", path.display(), format_bytes(out.bytes_written)));
                written_sizes.push(file_bytes);
                break 'files;
            }
            file_bytes += bs;
            out.bytes_written += bs;
            batcher.push(None, bs);
        }
        if let Err(e) = file.sync_all() {
            out.write_error = Some(format!("Flush failed for {}: {e}", path.display()));
        }
        written_sizes.push(file_bytes);
        out.files_written += 1;
    }
    let write_secs = write_start.elapsed().as_secs_f64();
    out.write_mb_s = if write_secs > 0.0 { out.bytes_written as f64 / 1e6 / write_secs } else { 0.0 };
    if let Some(e) = &out.write_error {
        batcher.log(e.clone());
    }

    // ---- verify pass
    if !out.cancelled {
        let read_start = Instant::now();
        batcher.pass("Verifying test files", out.bytes_written / bs, 0, bs as u32);
        'verify: for (f, &size) in written_sizes.iter().enumerate() {
            let f = f as u64;
            let path = dir.join(chunk_name(f));
            let mut file = match platform.open_test_file(&path, false) {
                Ok(file) => file,
                Err(e) => {
                    out.unreadable_bytes += size;
                    out.first_bad.get_or_insert_with(|| format!("{}: {e}", path.display()));
                    continue;
                }
            };
            for b in 0..size / bs {
                if cancel.load(Ordering::Relaxed) {
                    out.cancelled = true;
                    break 'verify;
                }
                let unit = (f * file_size + b * bs) / UNIT as u64;
                match file.read_exact(&mut buf) {
                    Ok(()) => {
                        let check = check_signed(&buf, cfg.seed, unit);
                        out.corrupt_units += check.corrupt;
                        out.aliased_units += check.aliased;
                        if let Some(first) = check.first_bad {
                            out.first_bad.get_or_insert_with(|| {
                                format!("{} at offset {}", path.display(), b * bs + first * UNIT as u64)
                            });
                        }
                        out.bytes_verified += bs;
                    }
                    Err(e) => {
                        out.unreadable_bytes += size - b * bs;
                        out.first_bad.get_or_insert_with(|| format!("{} at offset {}: {e}", path.display(), b * bs));
                        break;
                    }
                }
                batcher.push(None, bs);
            }
        }
        let read_secs = read_start.elapsed().as_secs_f64();
        out.read_mb_s = if read_secs > 0.0 { out.bytes_verified as f64 / 1e6 / read_secs } else { 0.0 };
        batcher.flush();
    }

    // ---- cleanup
    out.cleanup_ok = cleanup(&dir, file_count);
    guard.armed = !out.cleanup_ok;
    if !out.cleanup_ok {
        batcher.log(format!("Could not delete {}; please delete it manually.", dir.display()));
    }

    out.completed = !out.cancelled;
    if out.completed {
        let bad = out.corrupt_units + out.aliased_units;
        out.capacity = Some(if bad == 0 && out.unreadable_bytes == 0 {
            CapacityVerdict {
                genuine: out.write_error.is_none(),
                defects_found: out.write_error.is_some(),
                usable_bytes_estimate: None,
                summary: match &out.write_error {
                    None => {
                        format!("Wrote and verified {} of free space; no errors.", format_bytes(out.bytes_verified))
                    }
                    Some(e) => format!("Verified {} but writing stopped early: {e}", format_bytes(out.bytes_verified)),
                },
            }
        } else {
            CapacityVerdict {
                genuine: out.aliased_units == 0,
                defects_found: true,
                usable_bytes_estimate: None,
                summary: format!(
                    "{bad} sectors of test data read back wrong{}. {}",
                    if out.unreadable_bytes > 0 {
                        format!(" and {} was unreadable", format_bytes(out.unreadable_bytes))
                    } else {
                        String::new()
                    },
                    if out.aliased_units > 0 {
                        "Data landed in the wrong place: typical of fake-capacity media."
                    } else {
                        "The drive is corrupting data."
                    }
                ),
            }
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::DemoPlatform;
    use crate::policy::{Mode, Policy};

    #[test]
    fn free_space_round_trip_and_cleanup() {
        let platform = DemoPlatform::new_fast();
        let drives = platform.enumerate().unwrap();
        let usb = drives.iter().find(|d| !d.volumes.is_empty() && !d.is_protected()).unwrap();
        let mount = usb.volumes[0].mount_points[0].clone();
        let auth = Policy::new(Mode::ReadWrite).authorize_free_space(&platform, usb, &mount).unwrap();
        let cfg = FreeSpaceConfig {
            file_size: 4 << 20,
            reserve_bytes: 0,
            block_size: 1 << 20,
            seed: 5,
            max_bytes: Some(10 << 20),
        };
        let out = run_free_space(&platform, &auth, &cfg, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(out.completed);
        assert_eq!(out.bytes_written, 10 << 20);
        assert_eq!(out.bytes_verified, 10 << 20);
        assert_eq!(out.files_written, 3);
        assert!(!out.problems_found());
        assert!(out.cleanup_ok);
        assert!(!std::path::Path::new(&out.directory).exists());
    }
}
