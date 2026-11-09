//! Destructive raw write tests. Only reachable through a
//! [`crate::policy::DestructiveSession`], so never on the system disk.
//!
//! * Full write + verify: every block written with signed data, then read
//!   back. Detects bad sectors, fake capacity and the SSD cache cliff.
//! * Fake-capacity probe: the same check at sampled positions only; fast.
//! * SMR probe: sustained random writes; a drive-managed SMR disk collapses
//!   once its internal cache fills, a CMR disk holds steady.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::aligned::AlignedBuf;
use crate::analysis::{self, CacheCliff};
use crate::events::{Batcher, BlockUpdate, EngineEvent};
use crate::model::format_bytes;
use crate::platform::RawWriter;
use crate::scan::{BlockMap, BlockStatus, duration_us};
use crate::signature::{SplitMix64, UNIT, check_signed, fill_signed, fresh_seed};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WriteTestKind {
    /// Write every block, then verify every block.
    FullWriteVerify,
    /// Write and verify only sampled positions.
    FakeCapacityProbe { samples: u32 },
    /// Random writes until the budget runs out or a collapse is confirmed.
    SmrProbe { max_gib: u32, max_minutes: u32 },
    /// Rewrites blocks with their own data, so flash gets a fresh charge and
    /// weak HDD sectors are rewritten. Keeps data. All-zero blocks (trimmed or
    /// empty) are skipped. With `slower_than_us`, only blocks that read slower
    /// than that are rewritten.
    BlockRefresh { slower_than_us: Option<u32> },
    /// Forces the drive to reallocate unreadable sectors by overwriting them
    /// (their data is already lost). Listed sectors that read fine are
    /// rewritten with their own data. With no list, the drive is scanned first.
    Remap { lbas: Vec<u64>, refresh_slower_than_us: Option<u32> },
}

impl WriteTestKind {
    pub fn label(&self) -> &'static str {
        match self {
            WriteTestKind::FullWriteVerify => "Full write + verify",
            WriteTestKind::FakeCapacityProbe { .. } => "Fake-capacity quick probe",
            WriteTestKind::SmrProbe { .. } => "SMR write test",
            WriteTestKind::BlockRefresh { .. } => "Block refresh",
            WriteTestKind::Remap { .. } => "Weak-sector remap",
        }
    }

    /// Keeps the drive's data (apart from sectors that are already unreadable).
    pub fn preserves_data(&self) -> bool {
        matches!(self, WriteTestKind::BlockRefresh { .. } | WriteTestKind::Remap { .. })
    }

    /// Upper bound on data written, shown before the user confirms.
    pub fn estimated_bytes_written(&self, drive_size: u64, block_size: u32) -> u64 {
        match self {
            WriteTestKind::FullWriteVerify => drive_size,
            WriteTestKind::FakeCapacityProbe { samples } => {
                (u64::from(*samples) + 2).saturating_mul(u64::from(block_size)).min(drive_size)
            }
            WriteTestKind::SmrProbe { max_gib, .. } => u64::from(*max_gib) << 30,
            WriteTestKind::BlockRefresh { .. } => drive_size,
            WriteTestKind::Remap { lbas, refresh_slower_than_us } => {
                if refresh_slower_than_us.is_some() {
                    drive_size
                } else {
                    (lbas.len() as u64).saturating_mul(4096)
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteTestConfig {
    pub kind: WriteTestKind,
    pub block_size: u32,
    pub seed: u64,
}

impl WriteTestConfig {
    pub fn new(kind: WriteTestKind) -> Self {
        Self { kind, block_size: 1 << 20, seed: fresh_seed() }
    }
}

/// Is the advertised capacity real?
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityVerdict {
    /// Advertised capacity holds data.
    pub genuine: bool,
    /// Sectors failed verification even though the capacity is real.
    pub defects_found: bool,
    /// For fakes: roughly how much the drive really stores.
    pub usable_bytes_estimate: Option<u64>,
    pub summary: String,
}

/// Throughput over time during the SMR probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmrProbeResult {
    pub window_seconds: f64,
    pub windows_mb_s: Vec<f64>,
    pub bytes_written: u64,
    pub early_mb_s: f64,
    pub late_mb_s: f64,
    pub collapsed: bool,
    pub collapse_after_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct WriteTestOutcome {
    pub kind: WriteTestKind,
    pub write_map: BlockMap,
    pub verify_map: Option<BlockMap>,
    pub bytes_written: u64,
    pub write_errors: u64,
    pub corrupt_units: u64,
    pub aliased_units: u64,
    pub unreadable_units: u64,
    pub first_bad_offset: Option<u64>,
    /// Up to 1000 byte offsets of failing 512-byte units.
    pub bad_offsets: Vec<u64>,
    /// Byte distances between where data was written and where it was found
    /// (wraparound fakes); multiples of the real capacity.
    pub alias_distances: Vec<u64>,
    pub completed: bool,
    pub cancelled: bool,
    pub elapsed: Duration,
    pub capacity: Option<CapacityVerdict>,
    pub cache_cliff: Option<CacheCliff>,
    pub smr: Option<SmrProbeResult>,
    pub repair: Option<crate::repair::RepairSummary>,
}

impl WriteTestOutcome {
    pub(crate) fn empty(kind: WriteTestKind, size: u64, block_size: u32) -> Self {
        Self {
            kind,
            write_map: BlockMap::new(size, block_size),
            verify_map: None,
            bytes_written: 0,
            write_errors: 0,
            corrupt_units: 0,
            aliased_units: 0,
            unreadable_units: 0,
            first_bad_offset: None,
            bad_offsets: Vec::new(),
            alias_distances: Vec::new(),
            completed: false,
            cancelled: false,
            elapsed: Duration::ZERO,
            capacity: None,
            cache_cliff: None,
            smr: None,
            repair: None,
        }
    }

    pub(crate) fn note_bad(&mut self, offset: u64) {
        self.first_bad_offset = Some(self.first_bad_offset.map_or(offset, |f| f.min(offset)));
        if self.bad_offsets.len() < 1000 {
            self.bad_offsets.push(offset);
        }
    }

    pub fn problems_found(&self) -> bool {
        self.write_errors + self.corrupt_units + self.aliased_units + self.unreadable_units > 0
            || self.capacity.as_ref().is_some_and(|c| !c.genuine)
    }
}

/// Runs a destructive test on an open session writer.
pub fn run_write_test(
    dev: &mut dyn RawWriter,
    cfg: &WriteTestConfig,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> WriteTestOutcome {
    match &cfg.kind {
        WriteTestKind::FullWriteVerify => run_pattern(dev, cfg, None, cancel, sink),
        WriteTestKind::FakeCapacityProbe { samples } => run_pattern(dev, cfg, Some(*samples), cancel, sink),
        WriteTestKind::SmrProbe { max_gib, max_minutes } => {
            run_smr(dev, cfg, u64::from(*max_gib) << 30, u64::from(*max_minutes) * 60, cancel, sink)
        }
        WriteTestKind::BlockRefresh { slower_than_us } => {
            crate::repair::run_block_refresh(dev, cfg, *slower_than_us, cancel, sink)
        }
        WriteTestKind::Remap { lbas, refresh_slower_than_us } => {
            crate::repair::run_remap(dev, cfg, lbas, *refresh_slower_than_us, cancel, sink)
        }
    }
}

pub(crate) fn aligned_block_size(requested: u32, sector: u32) -> u32 {
    let unit = sector.max(UNIT as u32);
    (requested / unit).max(1) * unit
}

/// Positions for the quick probe: spread across the drive, always including
/// the first and last block.
pub fn probe_positions(total_blocks: u64, samples: u32, rng: &mut SplitMix64) -> Vec<u64> {
    if total_blocks == 0 {
        return Vec::new();
    }
    let k = u64::from(samples.max(2)).min(total_blocks);
    let stride = total_blocks as f64 / k as f64;
    let span = (stride.floor() as u64).max(1);
    let mut v: Vec<u64> =
        (0..k).map(|i| ((i as f64 * stride) as u64 + rng.below(span)).min(total_blocks - 1)).collect();
    v.push(0);
    v.push(total_blocks - 1);
    v.sort_unstable();
    v.dedup();
    v
}

fn run_pattern(
    dev: &mut dyn RawWriter,
    cfg: &WriteTestConfig,
    samples: Option<u32>,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> WriteTestOutcome {
    let start = Instant::now();
    let size = dev.size_bytes();
    let bs = aligned_block_size(cfg.block_size, dev.logical_sector_size());
    let mut out = WriteTestOutcome::empty(cfg.kind.clone(), size, bs);
    let n = out.write_map.total_blocks();
    let mut rng = SplitMix64::new(cfg.seed ^ 0xA5A5_5A5A_F00D_BEEF);
    let positions: Vec<u64> = match samples {
        None => (0..n).collect(),
        Some(k) => probe_positions(n, k, &mut rng),
    };
    let mut write_order = positions.clone();
    if samples.is_some() {
        rng.shuffle(&mut write_order);
    }

    let mut buf = AlignedBuf::new(bs as usize);
    let mut batcher = Batcher::new(sink);
    let write_pass = if samples.is_some() { "Writing probe blocks" } else { "Writing test pattern" };
    batcher.pass(write_pass, write_order.len() as u64, n, bs);
    let mut write_errors_logged = 0;
    for &index in &write_order {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            break;
        }
        let offset = out.write_map.offset(index);
        let len = out.write_map.block_len(index) as usize;
        fill_signed(&mut buf[..len], cfg.seed, offset / UNIT as u64);
        let t = Instant::now();
        let result = dev.write_at(offset, &buf[..len]);
        let latency = duration_us(dev.simulated_latency().unwrap_or_else(|| t.elapsed()));
        let status = match result {
            Ok(()) => {
                out.bytes_written += len as u64;
                BlockStatus::Ok
            }
            Err(e) => {
                out.write_errors += 1;
                out.note_bad(offset);
                if write_errors_logged < 20 {
                    batcher.log(format!("Write error at byte {offset}: {e}"));
                    write_errors_logged += 1;
                }
                BlockStatus::Error
            }
        };
        out.write_map.set(index, latency, status);
        batcher.push(Some(BlockUpdate { index, latency_us: latency, status }), len as u64);
    }
    if !out.cancelled
        && let Err(e) = dev.flush()
    {
        batcher.log(format!("Flush failed: {e}"));
    }

    if !out.cancelled {
        let mut vmap = BlockMap::new(size, bs);
        let mut verify_order = positions;
        if samples.is_some() {
            rng.shuffle(&mut verify_order);
        }
        batcher.pass("Verifying", verify_order.len() as u64, n, bs);
        for &index in &verify_order {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                break;
            }
            let offset = vmap.offset(index);
            let len = vmap.block_len(index) as usize;
            let t = Instant::now();
            let result = dev.read_at(offset, &mut buf[..len]);
            let latency = duration_us(dev.simulated_latency().unwrap_or_else(|| t.elapsed()));
            let status = match result {
                Err(_) => {
                    out.unreadable_units += (len / UNIT) as u64;
                    out.note_bad(offset);
                    BlockStatus::Error
                }
                Ok(()) => {
                    let check = check_signed(&buf[..len], cfg.seed, offset / UNIT as u64);
                    out.corrupt_units += check.corrupt;
                    out.aliased_units += check.aliased;
                    if let Some((expected, found)) = check.first_alias
                        && out.alias_distances.len() < 4096
                    {
                        out.alias_distances.push(expected.abs_diff(found) * UNIT as u64);
                    }
                    if let Some(first) = check.first_bad {
                        out.note_bad(offset + first * UNIT as u64);
                        BlockStatus::Error
                    } else {
                        BlockStatus::Ok
                    }
                }
            };
            vmap.set(index, latency, status);
            batcher.push(Some(BlockUpdate { index, latency_us: latency, status }), len as u64);
        }
        batcher.flush();
        if !out.cancelled {
            out.capacity = Some(capacity_verdict(&out, &vmap, samples));
            if samples.is_none() {
                out.cache_cliff = analysis::cache_cliff(&out.write_map);
            }
        }
        out.verify_map = Some(vmap);
    }
    out.completed = !out.cancelled;
    out.elapsed = start.elapsed();
    out
}

fn capacity_verdict(out: &WriteTestOutcome, vmap: &BlockMap, samples: Option<u32>) -> CapacityVerdict {
    let size = vmap.device_size;
    let bad_units = out.corrupt_units + out.aliased_units + out.unreadable_units;
    if bad_units == 0 && out.write_errors == 0 {
        let summary = match samples {
            None => format!("All {} written and verified; capacity is genuine.", format_bytes(size)),
            Some(_) => format!(
                "All {} sampled positions across {} verified; capacity looks genuine. A full write + verify confirms it.",
                vmap.scanned_blocks(),
                format_bytes(size)
            ),
        };
        return CapacityVerdict { genuine: true, defects_found: false, usable_bytes_estimate: None, summary };
    }
    let first_bad = out.first_bad_offset.unwrap_or(0);
    let first_block = first_bad / u64::from(vmap.block_size);
    let (mut scanned_after, mut bad_after) = (0u64, 0u64);
    for i in first_block..vmap.total_blocks() {
        match vmap.get(i).1 {
            BlockStatus::NotScanned => {}
            BlockStatus::Error => {
                scanned_after += 1;
                bad_after += 1;
            }
            _ => scanned_after += 1,
        }
    }
    let fraction_bad_after = if scanned_after == 0 { 0.0 } else { bad_after as f64 / scanned_after as f64 };
    let fake = out.aliased_units > 0 || (fraction_bad_after > 0.9 && size - first_bad > size / 100);
    if fake {
        let usable = match samples {
            // Sequential full write: the last lap survives, so the blocks that
            // kept their data approximate what the drive physically stores.
            None => {
                let ok = (0..vmap.total_blocks()).filter(|&i| vmap.get(i).1 == BlockStatus::Ok).count() as u64;
                if out.aliased_units > 0 { ok * u64::from(vmap.block_size) } else { first_bad }
            }
            // Random-order probe: alias distances are multiples of the real
            // capacity, so their greatest common divisor estimates it.
            Some(_) => match out.alias_distances.iter().copied().reduce(gcd) {
                Some(g) if g > 0 && g < size => g,
                _ => first_bad,
            },
        } / (1 << 20)
            * (1 << 20);
        let how = if out.aliased_units > 0 {
            "writes past that point land on earlier data (wraparound)"
        } else {
            "data written past that point is lost"
        };
        CapacityVerdict {
            genuine: false,
            defects_found: true,
            usable_bytes_estimate: Some(usable),
            summary: format!(
                "Fake capacity: the drive reports {} but only about {} holds data; {how}. Typical of counterfeit flash.",
                format_bytes(size),
                format_bytes(usable)
            ),
        }
    } else {
        CapacityVerdict {
            genuine: true,
            defects_found: true,
            usable_bytes_estimate: None,
            summary: format!(
                "{bad_units} sectors failed verification ({} write errors). Capacity appears genuine, but the drive has defects.",
                out.write_errors
            ),
        }
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 }
}

/// Mean of the first fifth of the windows (at least one).
fn early_rate(windows: &[f64]) -> f64 {
    let k = (windows.len() / 5).max(1).min(windows.len());
    mean(&windows[..k])
}

/// Mean of the last fifth of the windows (at least one).
fn late_rate(windows: &[f64]) -> f64 {
    let k = (windows.len() / 5).max(1).min(windows.len());
    mean(&windows[windows.len() - k..])
}

fn run_smr(
    dev: &mut dyn RawWriter,
    cfg: &WriteTestConfig,
    max_bytes: u64,
    max_secs: u64,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> WriteTestOutcome {
    const WINDOW_SECS: f64 = 10.0;
    const MAP_BLOCK: u32 = 1 << 20;
    let start = Instant::now();
    let size = dev.size_bytes();
    let io = aligned_block_size(256 << 10, dev.logical_sector_size());
    let mut out = WriteTestOutcome::empty(cfg.kind.clone(), size, MAP_BLOCK);
    let units = size / u64::from(io);
    if units == 0 {
        out.completed = true;
        return out;
    }
    let mut rng = SplitMix64::new(cfg.seed);
    let mut buf = AlignedBuf::new(io as usize);
    let mut batcher = Batcher::new(sink);
    let total_items = max_bytes / u64::from(io);
    batcher.pass("SMR random-write test", total_items, out.write_map.total_blocks(), MAP_BLOCK);

    let mut windows: Vec<f64> = Vec::new();
    let mut window_bytes_end: Vec<u64> = Vec::new();
    let (mut win_io, mut win_bytes) = (0f64, 0u64);
    let mut io_total = 0f64;
    let mut stopped_early = false;
    while out.bytes_written < max_bytes && io_total < max_secs as f64 {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            break;
        }
        let offset = rng.below(units) * u64::from(io);
        fill_signed(&mut buf, cfg.seed, offset / UNIT as u64);
        let t = Instant::now();
        let result = dev.write_at(offset, &buf);
        let elapsed = dev.simulated_latency().unwrap_or_else(|| t.elapsed());
        let secs = elapsed.as_secs_f64();
        io_total += secs;
        let status = if result.is_ok() {
            out.bytes_written += u64::from(io);
            win_bytes += u64::from(io);
            BlockStatus::Ok
        } else {
            out.write_errors += 1;
            out.note_bad(offset);
            BlockStatus::Error
        };
        win_io += secs;
        let index = offset / u64::from(MAP_BLOCK);
        out.write_map.set(index, duration_us(elapsed), status);
        batcher.push(Some(BlockUpdate { index, latency_us: duration_us(elapsed), status }), u64::from(io));

        if win_io >= WINDOW_SECS {
            let rate = win_bytes as f64 / 1e6 / win_io;
            windows.push(rate);
            window_bytes_end.push(out.bytes_written);
            batcher.log(format!("Window {}: {rate:.1} MB/s", windows.len()));
            win_io = 0.0;
            win_bytes = 0;
            // Stop once a collapse is clearly established.
            if windows.len() >= 9 {
                let early = early_rate(&windows);
                if windows[windows.len() - 6..].iter().all(|&w| w < 0.35 * early) {
                    stopped_early = true;
                    batcher.log("Throughput collapse confirmed; stopping early.");
                    break;
                }
            }
        }
    }
    if win_io > 1.0 {
        windows.push(win_bytes as f64 / 1e6 / win_io);
        window_bytes_end.push(out.bytes_written);
    }
    let _ = dev.flush();
    batcher.flush();

    let early = early_rate(&windows);
    let late = late_rate(&windows);
    let collapsed = windows.len() >= 4 && early > 0.0 && late < 0.35 * early;
    let collapse_after_bytes = if collapsed {
        windows.iter().position(|&w| w < 0.35 * early).map(|i| if i == 0 { 0 } else { window_bytes_end[i - 1] })
    } else {
        None
    };
    if windows.len() < 4 && !out.cancelled {
        batcher.log("Not enough data for a confident SMR verdict; allow more time or data.");
    }
    out.smr = Some(SmrProbeResult {
        window_seconds: WINDOW_SECS,
        windows_mb_s: windows,
        bytes_written: out.bytes_written,
        early_mb_s: early,
        late_mb_s: late,
        collapsed,
        collapse_after_bytes,
    });
    out.completed = !out.cancelled || stopped_early;
    out.elapsed = start.elapsed();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::{DemoDisk, DemoProfile};

    fn run(profile: DemoProfile, kind: WriteTestKind) -> WriteTestOutcome {
        let mut disk = DemoDisk::new(profile, false);
        let cfg = WriteTestConfig { kind, block_size: 1 << 20, seed: 99 };
        run_write_test(&mut disk, &cfg, &AtomicBool::new(false), &mut |_| {})
    }

    #[test]
    fn genuine_drive_passes_full_verify() {
        let out = run(DemoProfile::test_flash(64 << 20, None), WriteTestKind::FullWriteVerify);
        assert!(out.completed);
        let cap = out.capacity.unwrap();
        assert!(cap.genuine && !cap.defects_found, "{}", cap.summary);
        assert_eq!(out.bytes_written, 64 << 20);
    }

    #[test]
    fn wraparound_fake_is_detected_by_full_verify() {
        let out = run(DemoProfile::test_flash(64 << 20, Some(16 << 20)), WriteTestKind::FullWriteVerify);
        let cap = out.capacity.unwrap();
        assert!(!cap.genuine, "{}", cap.summary);
        assert_eq!(cap.usable_bytes_estimate, Some(16 << 20));
        assert!(out.aliased_units > 0);
        // Everything after the real capacity was overwritten by later blocks,
        // and the start was overwritten by the tail: first bad unit is at 0.
        assert_eq!(out.first_bad_offset, Some(0));
    }

    #[test]
    fn wraparound_fake_is_detected_by_probe() {
        let out =
            run(DemoProfile::test_flash(256 << 20, Some(32 << 20)), WriteTestKind::FakeCapacityProbe { samples: 64 });
        let cap = out.capacity.unwrap();
        assert!(!cap.genuine);
        assert_eq!(cap.usable_bytes_estimate, Some(32 << 20), "{}", cap.summary);
    }

    #[test]
    fn probe_positions_include_ends() {
        let mut rng = SplitMix64::new(1);
        let p = probe_positions(1000, 10, &mut rng);
        assert_eq!(p.first(), Some(&0));
        assert_eq!(p.last(), Some(&999));
        assert!(p.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn smr_probe_sees_collapse_on_smr_profile() {
        let out = run(
            DemoProfile::test_hdd(512 << 20).with_smr(64 << 20),
            WriteTestKind::SmrProbe { max_gib: 4, max_minutes: 30 },
        );
        let smr = out.smr.unwrap();
        assert!(smr.collapsed, "{smr:?}");
    }

    #[test]
    fn smr_probe_holds_steady_on_cmr_profile() {
        let out = run(DemoProfile::test_hdd(512 << 20), WriteTestKind::SmrProbe { max_gib: 1, max_minutes: 30 });
        let smr = out.smr.unwrap();
        assert!(!smr.collapsed, "{smr:?}");
    }
}
