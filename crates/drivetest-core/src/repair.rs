//! Raw-level maintenance: block refresh and weak-sector remap.
//!
//! Both run inside a [`crate::policy::DestructiveSession`] (volumes locked and
//! dismounted, serial confirmed, never the system disk) because they write raw
//! sectors, but both keep the drive's data:
//!
//! * **Block refresh** reads each block and writes the same bytes back, then
//!   reads them again to confirm. All-zero blocks are skipped (on SSDs these
//!   are usually trimmed, and rewriting them would waste endurance and undo
//!   the TRIM). Optionally only blocks slower than a threshold are rewritten.
//! * **Remap** finds sectors that cannot be read and overwrites them with
//!   zeros, which makes the drive swap in a spare. Their contents were already
//!   lost. Listed sectors that turn out to be readable are rewritten with their
//!   own data instead.
//!
//! Identical-data writes mean a power cut leaves old or new data, which match.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::aligned::AlignedBuf;
use crate::events::{Batcher, BlockUpdate, EngineEvent};
use crate::platform::RawWriter;
use crate::scan::{BlockMap, BlockStatus, duration_us};
use crate::writetest::{WriteTestConfig, WriteTestOutcome, aligned_block_size};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RepairSummary {
    pub blocks_checked: u64,
    pub blocks_rewritten: u64,
    pub bytes_rewritten: u64,
    /// All-zero blocks left alone (trimmed or empty).
    pub zero_blocks_skipped: u64,
    /// Blocks below the slowness threshold, left alone.
    pub fast_blocks_skipped: u64,
    pub unreadable_blocks: u64,
    /// Blocks or sectors that still read back wrong after a retry.
    pub verify_failures: u64,
    #[serde(default)]
    pub aborted: Option<String>,
    /// Unreadable sectors that read back the replacement bytes.
    pub remapped_sectors: Vec<u64>,
    /// Sectors that stayed unreadable even after being overwritten.
    pub still_bad_sectors: Vec<u64>,
    /// Listed sectors that were readable and were rewritten with their own data.
    pub refreshed_sectors: Vec<u64>,
}

fn timed_read(dev: &mut dyn RawWriter, offset: u64, buf: &mut [u8]) -> (std::io::Result<()>, u32) {
    let t = Instant::now();
    let r = dev.read_at(offset, buf);
    (r, duration_us(dev.simulated_latency().unwrap_or_else(|| t.elapsed())))
}

fn is_zero(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0)
}

/// Writes `data` at `offset` and confirms it reads back; retries once.
fn write_verified(dev: &mut dyn RawWriter, offset: u64, data: &[u8], scratch: &mut [u8]) -> bool {
    for _ in 0..2 {
        if dev.write_at(offset, data).is_err() {
            continue;
        }
        let _ = dev.flush();
        if dev.read_at(offset, &mut scratch[..data.len()]).is_ok() && scratch[..data.len()] == *data {
            return true;
        }
    }
    false
}

pub fn run_block_refresh(
    dev: &mut dyn RawWriter,
    cfg: &WriteTestConfig,
    slower_than_us: Option<u32>,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> WriteTestOutcome {
    let start = Instant::now();
    let size = dev.size_bytes();
    let bs = aligned_block_size(cfg.block_size, dev.logical_sector_size());
    let mut out = WriteTestOutcome::empty(cfg.kind.clone(), size, bs);
    let mut sum = RepairSummary::default();
    let n = out.write_map.total_blocks();
    let mut buf = AlignedBuf::new(bs as usize);
    let mut scratch = AlignedBuf::new(bs as usize);
    let mut batcher = Batcher::new(sink);
    batcher.pass("Block refresh", n, n, bs);
    for index in 0..n {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            break;
        }
        let offset = out.write_map.offset(index);
        let len = out.write_map.block_len(index) as usize;
        sum.blocks_checked += 1;
        let (r, lat) = timed_read(dev, offset, &mut buf[..len]);
        let status = if r.is_err() {
            sum.unreadable_blocks += 1;
            out.unreadable_units += (len / 512) as u64;
            out.note_bad(offset);
            if sum.unreadable_blocks <= 20 {
                batcher.log(format!("Unreadable block at byte {offset}; left as is (the remap tool handles these)."));
            }
            BlockStatus::Error
        } else if is_zero(&buf[..len]) {
            sum.zero_blocks_skipped += 1;
            BlockStatus::OkZero
        } else if slower_than_us.is_some_and(|t| lat < t) {
            sum.fast_blocks_skipped += 1;
            BlockStatus::Ok
        } else if write_verified(dev, offset, &buf[..len], &mut scratch) {
            sum.blocks_rewritten += 1;
            sum.bytes_rewritten += len as u64;
            out.bytes_written += len as u64;
            BlockStatus::Ok
        } else {
            sum.verify_failures += 1;
            out.corrupt_units += (len / 512) as u64;
            out.note_bad(offset);
            batcher.log(format!("Block at byte {offset} did not read back identically after rewriting."));
            BlockStatus::Error
        };
        out.write_map.set(index, lat, status);
        batcher.push(Some(BlockUpdate { index, latency_us: lat, status }), len as u64);
    }
    batcher.flush();
    out.repair = Some(sum);
    out.completed = !out.cancelled;
    out.elapsed = start.elapsed();
    out
}

fn read_for_remap(dev: &mut dyn RawWriter, offset: u64, buf: &mut [u8]) -> std::io::Result<bool> {
    for _ in 0..2 {
        match dev.read_at(offset, buf) {
            Ok(()) => return Ok(true),
            Err(e) if crate::platform::is_media_read_error(&e) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

pub fn run_remap(
    dev: &mut dyn RawWriter,
    cfg: &WriteTestConfig,
    lbas: &[u64],
    refresh_slower_than_us: Option<u32>,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> WriteTestOutcome {
    let start = Instant::now();
    let size = dev.size_bytes();
    let sector = u64::from(dev.logical_sector_size().max(512));
    let bs = aligned_block_size(cfg.block_size, dev.logical_sector_size());
    let mut out = WriteTestOutcome::empty(cfg.kind.clone(), size, bs);
    let mut sum = RepairSummary::default();
    let mut batcher = Batcher::new(sink);
    // Drop listed sectors that do not fit this drive (including overflowing values).
    let mut candidates: Vec<u64> = lbas
        .iter()
        .copied()
        .filter(|&l| l.checked_add(1).and_then(|e| e.checked_mul(sector)).is_some_and(|end| end <= size))
        .collect();
    let mut sector_buf = AlignedBuf::new(sector as usize);
    let mut sector_scratch = AlignedBuf::new(sector as usize);

    // Find unreadable sectors when none were given (or when also refreshing slow blocks).
    if candidates.is_empty() || refresh_slower_than_us.is_some() {
        let mut buf = AlignedBuf::new(bs as usize);
        let mut scratch = AlignedBuf::new(bs as usize);
        let n = out.write_map.total_blocks();
        batcher.pass("Scanning for weak sectors", n, n, bs);
        'scan: for index in 0..n {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                break;
            }
            let offset = out.write_map.offset(index);
            let len = out.write_map.block_len(index) as usize;
            sum.blocks_checked += 1;
            let (r, lat) = timed_read(dev, offset, &mut buf[..len]);
            let status = match r {
                Err(error) if !crate::platform::is_media_read_error(&error) => {
                    sum.aborted = Some(format!("Stopped at byte {offset}: {error}"));
                    break;
                }
                Err(_) => {
                    sum.unreadable_blocks += 1;
                    let mut pos = offset;
                    while pos < offset + len as u64 {
                        if cancel.load(Ordering::Relaxed) {
                            out.cancelled = true;
                            break 'scan;
                        }
                        match read_for_remap(dev, pos, &mut sector_buf) {
                            Ok(false) => candidates.push(pos / sector),
                            Ok(true) => {}
                            Err(error) => {
                                sum.aborted = Some(format!("Stopped at byte {pos}: {error}"));
                                break 'scan;
                            }
                        }
                        pos += sector;
                    }
                    BlockStatus::Error
                }
                Ok(()) => {
                    if refresh_slower_than_us.is_some_and(|t| lat >= t) && !is_zero(&buf[..len]) {
                        if write_verified(dev, offset, &buf[..len], &mut scratch) {
                            sum.blocks_rewritten += 1;
                            sum.bytes_rewritten += len as u64;
                            out.bytes_written += len as u64;
                        } else {
                            sum.verify_failures += 1;
                        }
                    }
                    BlockStatus::Ok
                }
            };
            out.write_map.set(index, lat, status);
            batcher.push(Some(BlockUpdate { index, latency_us: lat, status }), len as u64);
        }
    }
    candidates.sort_unstable();
    candidates.dedup();

    if !out.cancelled && sum.aborted.is_none() && !candidates.is_empty() {
        let mut vmap = BlockMap::new(size, bs);
        batcher.pass("Remapping sectors", candidates.len() as u64, vmap.total_blocks(), bs);
        batcher.log(format!("{} sectors to treat", candidates.len()));
        let zeros = AlignedBuf::new(sector as usize);
        for &lba in &candidates {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                break;
            }
            let offset = lba * sector;
            let readable = match read_for_remap(dev, offset, &mut sector_buf) {
                Ok(readable) => readable,
                Err(error) => {
                    sum.aborted = Some(format!("Stopped at LBA {lba}: {error}"));
                    break;
                }
            };
            let status = if readable {
                // Still readable: keep its data, just rewrite it.
                if write_verified(dev, offset, &sector_buf, &mut sector_scratch) {
                    sum.refreshed_sectors.push(lba);
                    BlockStatus::Ok
                } else {
                    sum.verify_failures += 1;
                    sum.still_bad_sectors.push(lba);
                    BlockStatus::Error
                }
            } else if write_verified(dev, offset, &zeros, &mut sector_scratch) {
                out.bytes_written += sector;
                sum.remapped_sectors.push(lba);
                BlockStatus::Ok
            } else {
                sum.still_bad_sectors.push(lba);
                out.note_bad(offset);
                BlockStatus::Error
            };
            let index = offset / u64::from(bs);
            vmap.set(index, 0, status);
            batcher.push(Some(BlockUpdate { index, latency_us: 0, status }), sector);
        }
        out.verify_map = Some(vmap);
    } else if candidates.is_empty() && !out.cancelled && sum.aborted.is_none() {
        batcher.log("No unreadable sectors found; nothing to remap.");
    }
    batcher.flush();
    out.unreadable_units = sum.still_bad_sectors.len() as u64;
    if let Some(error) = &sum.aborted {
        batcher.log(error);
    }
    out.completed = !out.cancelled && sum.aborted.is_none();
    out.repair = Some(sum);
    out.elapsed = start.elapsed();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::{DemoDisk, DemoProfile};
    use crate::platform::RawReader;
    use crate::writetest::{WriteTestKind, run_write_test};

    fn cfg(kind: WriteTestKind) -> WriteTestConfig {
        WriteTestConfig { kind, block_size: 1 << 20, seed: 1 }
    }

    #[test]
    fn block_refresh_keeps_data_and_skips_zero_blocks() {
        let mut p = DemoProfile::test_flash(16 << 20, None);
        p.trimmed_fraction = 0.5;
        let mut disk = DemoDisk::new(p, false);
        let mut before = vec![0u8; 16 << 20];
        disk.read_at(0, &mut before).unwrap();
        // 64 KiB blocks match the demo's trim granularity, so some blocks are all zeros.
        let c = WriteTestConfig { block_size: 64 << 10, ..cfg(WriteTestKind::BlockRefresh { slower_than_us: None }) };
        let out = run_write_test(&mut disk, &c, &AtomicBool::new(false), &mut |_| {});
        let sum = out.repair.unwrap();
        assert!(sum.blocks_rewritten > 0 && sum.zero_blocks_skipped > 0, "{sum:?}");
        assert_eq!(sum.verify_failures, 0);
        let mut after = vec![0u8; 16 << 20];
        disk.read_at(0, &mut after).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn remap_fixes_bad_sectors_from_a_scan() {
        let mut p = DemoProfile::test_hdd(64 << 20);
        p.remap_on_write = true;
        let bad = p.bad_sectors.clone();
        let mut disk = DemoDisk::new(p, false);
        let out = run_write_test(
            &mut disk,
            &cfg(WriteTestKind::Remap { lbas: Vec::new(), refresh_slower_than_us: None }),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        let sum = out.repair.unwrap();
        assert_eq!(sum.remapped_sectors, bad);
        assert!(sum.still_bad_sectors.is_empty());
        let mut s = vec![0u8; 512];
        assert!(disk.read_at(bad[0] * 512, &mut s).is_ok());
    }

    #[test]
    fn remap_of_listed_readable_sector_keeps_data() {
        let mut disk = DemoDisk::new(DemoProfile::test_flash(8 << 20, None), false);
        let mut before = vec![0u8; 512];
        disk.read_at(100 * 512, &mut before).unwrap();
        let out = run_write_test(
            &mut disk,
            &cfg(WriteTestKind::Remap { lbas: vec![100], refresh_slower_than_us: None }),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(out.repair.unwrap().refreshed_sectors, vec![100]);
        let mut after = vec![0u8; 512];
        disk.read_at(100 * 512, &mut after).unwrap();
        assert_eq!(before, after);
    }
}
