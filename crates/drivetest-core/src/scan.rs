//! The sector read scan: times every block and classifies it by latency.
//!
//! Read-only, so it is allowed on every drive including the system disk.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::aligned::AlignedBuf;
use crate::events::{Batcher, BlockUpdate, EngineEvent};
use crate::platform::RawReader;
use crate::signature::SplitMix64;

/// Order in which blocks are read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScanMode {
    /// Every block, start to end.
    Sequential,
    /// A random sample covering the whole drive, read in random order.
    RandomSample { percent: f64 },
    /// Alternates between the start and end of the drive (stresses HDD seeks).
    Butterfly,
    /// Every block in a byte range, in order.
    Range { start_byte: u64, end_byte: u64 },
}

impl ScanMode {
    pub fn label(&self) -> String {
        match self {
            ScanMode::Sequential => "Full sequential".into(),
            ScanMode::RandomSample { percent } => format!("Random sample ({percent}%)"),
            ScanMode::Butterfly => "Butterfly".into(),
            ScanMode::Range { start_byte, end_byte } => {
                format!("Range {} – {}", crate::model::format_bytes(*start_byte), crate::model::format_bytes(*end_byte))
            }
        }
    }

    /// Whether position-based analysis (speed curve, weak regions) is meaningful.
    pub fn is_positional(&self) -> bool {
        matches!(self, ScanMode::Sequential | ScanMode::Range { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanConfig {
    pub mode: ScanMode,
    /// Bytes per timed read. Rounded down to the sector size.
    pub block_size: u32,
    /// Extra attempts for a block that fails.
    pub retries: u8,
    /// Narrow failing blocks down to individual sectors.
    pub pinpoint_errors: bool,
    /// Seed for random sampling.
    pub seed: u64,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self { mode: ScanMode::Sequential, block_size: 1 << 20, retries: 2, pinpoint_errors: true, seed: 0x5EED }
    }
}

/// State of one block in a [`BlockMap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum BlockStatus {
    NotScanned = 0,
    Ok = 1,
    /// Read fine and was all zeros. On SSDs this usually means trimmed or
    /// never-written space, which the drive answers without touching flash.
    OkZero = 2,
    Error = 3,
}

impl BlockStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => BlockStatus::Ok,
            2 => BlockStatus::OkZero,
            3 => BlockStatus::Error,
            _ => BlockStatus::NotScanned,
        }
    }

    pub fn is_ok(self) -> bool {
        matches!(self, BlockStatus::Ok | BlockStatus::OkZero)
    }
}

/// Number of latency buckets, including the error bucket.
pub const BUCKET_COUNT: usize = 7;
/// Upper limits (exclusive) of the first five buckets, in microseconds.
pub const BUCKET_LIMITS_US: [u32; 5] = [5_000, 20_000, 50_000, 150_000, 500_000];
pub const BUCKET_LABELS: [&str; BUCKET_COUNT] =
    ["< 5 ms", "5–20 ms", "20–50 ms", "50–150 ms", "150–500 ms", "> 500 ms", "Error"];
/// Blocks at or above this latency are early warnings (internal retries).
pub const SLOW_THRESHOLD_US: u32 = 150_000;

/// Latency bucket for a block, or `None` when it was not scanned.
pub fn bucket_index(latency_us: u32, status: BlockStatus) -> Option<usize> {
    match status {
        BlockStatus::NotScanned => None,
        BlockStatus::Error => Some(BUCKET_COUNT - 1),
        BlockStatus::Ok | BlockStatus::OkZero => {
            Some(BUCKET_LIMITS_US.iter().position(|&limit| latency_us < limit).unwrap_or(5))
        }
    }
}

/// Per-block results for one pass over a drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockMap {
    pub block_size: u32,
    pub device_size: u64,
    pub latency_us: Vec<u32>,
    /// [`BlockStatus`] as `u8`.
    pub status: Vec<u8>,
}

impl BlockMap {
    pub fn new(device_size: u64, block_size: u32) -> Self {
        let n = device_size.div_ceil(u64::from(block_size.max(1))) as usize;
        Self { block_size, device_size, latency_us: vec![0; n], status: vec![0; n] }
    }

    pub fn total_blocks(&self) -> u64 {
        self.status.len() as u64
    }

    pub fn offset(&self, index: u64) -> u64 {
        index * u64::from(self.block_size)
    }

    /// Length of a block; the last one may be shorter.
    pub fn block_len(&self, index: u64) -> u64 {
        let start = self.offset(index);
        u64::from(self.block_size).min(self.device_size.saturating_sub(start))
    }

    pub fn set(&mut self, index: u64, latency_us: u32, status: BlockStatus) {
        let i = index as usize;
        self.latency_us[i] = latency_us;
        self.status[i] = status as u8;
    }

    pub fn get(&self, index: u64) -> (u32, BlockStatus) {
        let i = index as usize;
        (self.latency_us[i], BlockStatus::from_u8(self.status[i]))
    }

    pub fn scanned_blocks(&self) -> u64 {
        self.status.iter().filter(|&&s| s != 0).count() as u64
    }

    /// Blocks per latency bucket.
    pub fn bucket_counts(&self) -> [u64; BUCKET_COUNT] {
        let mut counts = [0u64; BUCKET_COUNT];
        for (lat, st) in self.latency_us.iter().zip(&self.status) {
            if let Some(b) = bucket_index(*lat, BlockStatus::from_u8(*st)) {
                counts[b] += 1;
            }
        }
        counts
    }

    /// Latencies of blocks that read successfully.
    pub fn ok_latencies(&self) -> impl Iterator<Item = u32> + '_ {
        self.latency_us.iter().zip(&self.status).filter(|(_, s)| BlockStatus::from_u8(**s).is_ok()).map(|(l, _)| *l)
    }
}

/// Order of block indices for a scan mode.
pub fn block_order(mode: &ScanMode, map: &BlockMap, seed: u64) -> Vec<u64> {
    let n = map.total_blocks();
    if n == 0 {
        return Vec::new();
    }
    match mode {
        ScanMode::Sequential => (0..n).collect(),
        ScanMode::Range { start_byte, end_byte } => {
            let bs = u64::from(map.block_size);
            let first = (*start_byte / bs).min(n);
            let last = end_byte.div_ceil(bs).min(n);
            (first..last).collect()
        }
        ScanMode::Butterfly => {
            let mut order = Vec::with_capacity(n as usize);
            let (mut lo, mut hi) = (0u64, n);
            while lo < hi {
                order.push(lo);
                lo += 1;
                if lo < hi {
                    hi -= 1;
                    order.push(hi);
                }
            }
            order
        }
        ScanMode::RandomSample { percent } => {
            let p = percent.clamp(0.001, 100.0);
            let k = (((n as f64) * p / 100.0).ceil() as u64).clamp(1, n);
            let stride = n as f64 / k as f64;
            let span = (stride.floor() as u64).max(1);
            let mut rng = SplitMix64::new(seed);
            let mut order: Vec<u64> =
                (0..k).map(|i| ((i as f64 * stride) as u64 + rng.below(span)).min(n - 1)).collect();
            order.dedup();
            rng.shuffle(&mut order);
            order
        }
    }
}

/// Everything a read scan produced.
#[derive(Debug, Clone)]
pub struct ScanOutcome {
    pub map: BlockMap,
    /// LBAs (in logical sectors) that could not be read.
    pub bad_sectors: Vec<u64>,
    /// Blocks where pinpointing stopped early (long runs of failures).
    pub unresolved_blocks: Vec<u64>,
    pub completed: bool,
    pub cancelled: bool,
    pub elapsed: Duration,
    pub bytes_read: u64,
}

const MAX_BAD_SECTORS: usize = 100_000;
const MAX_CONSECUTIVE_FAILURES: u32 = 256;

pub(crate) fn duration_us(d: Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

fn timed_read(dev: &mut dyn RawReader, offset: u64, buf: &mut [u8]) -> (std::io::Result<()>, Duration) {
    let start = Instant::now();
    let result = dev.read_at(offset, buf);
    let elapsed = dev.simulated_latency().unwrap_or_else(|| start.elapsed());
    (result, elapsed)
}

fn is_all_zero(buf: &[u8]) -> bool {
    let (head, body, tail) = unsafe {
        // SAFETY: reinterpreting initialised bytes as u64 is always valid.
        buf.align_to::<u64>()
    };
    head.iter().all(|&b| b == 0) && body.iter().all(|&w| w == 0) && tail.iter().all(|&b| b == 0)
}

/// Runs a read scan. Never writes.
pub fn run_read_scan(
    dev: &mut dyn RawReader,
    cfg: &ScanConfig,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> ScanOutcome {
    let start = Instant::now();
    let size = dev.size_bytes();
    let sector = u64::from(dev.logical_sector_size().max(512));
    let block_size = ((u64::from(cfg.block_size) / sector).max(1) * sector) as u32;
    let mut map = BlockMap::new(size, block_size);
    let order = block_order(&cfg.mode, &map, cfg.seed);

    let mut buf = AlignedBuf::new(block_size as usize);
    let mut sector_buf = AlignedBuf::new(sector as usize);
    let mut bad_sectors = Vec::new();
    let mut unresolved_blocks = Vec::new();
    let mut bytes_read = 0u64;
    let mut cancelled = false;
    let mut errors_logged = 0;

    let mut batcher = Batcher::new(sink);
    batcher.pass("Read scan", order.len() as u64, map.total_blocks(), block_size);

    for &index in &order {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let offset = map.offset(index);
        let len = map.block_len(index) as usize;
        let (mut result, mut elapsed) = timed_read(dev, offset, &mut buf[..len]);
        let mut attempts = 0;
        while result.is_err() && attempts < cfg.retries {
            attempts += 1;
            let (r, e) = timed_read(dev, offset, &mut buf[..len]);
            result = r;
            elapsed += e;
        }
        let status = match result {
            Ok(()) => {
                bytes_read += len as u64;
                if is_all_zero(&buf[..len]) { BlockStatus::OkZero } else { BlockStatus::Ok }
            }
            Err(err) => {
                if errors_logged < 20 {
                    batcher.log(format!("Read error at byte {offset}: {err}"));
                    errors_logged += 1;
                }
                if cfg.pinpoint_errors && bad_sectors.len() < MAX_BAD_SECTORS {
                    let complete = pinpoint(dev, offset, len as u64, sector, &mut sector_buf, &mut bad_sectors, cancel);
                    if !complete {
                        unresolved_blocks.push(index);
                    }
                }
                BlockStatus::Error
            }
        };
        let latency_us = duration_us(elapsed);
        map.set(index, latency_us, status);
        batcher.push(Some(BlockUpdate { index, latency_us, status }), len as u64);
    }
    batcher.flush();

    ScanOutcome {
        completed: !cancelled,
        cancelled,
        map,
        bad_sectors,
        unresolved_blocks,
        elapsed: start.elapsed(),
        bytes_read,
    }
}

/// Reads a failing block one sector at a time. Returns false if it gave up early.
fn pinpoint(
    dev: &mut dyn RawReader,
    offset: u64,
    len: u64,
    sector: u64,
    sector_buf: &mut [u8],
    bad: &mut Vec<u64>,
    cancel: &AtomicBool,
) -> bool {
    let mut consecutive = 0u32;
    let mut pos = offset;
    while pos < offset + len {
        if cancel.load(Ordering::Relaxed) || bad.len() >= MAX_BAD_SECTORS {
            return false;
        }
        let ok = dev.read_at(pos, sector_buf).is_ok() || dev.read_at(pos, sector_buf).is_ok();
        if ok {
            consecutive = 0;
        } else {
            bad.push(pos / sector);
            consecutive += 1;
            if consecutive >= MAX_CONSECUTIVE_FAILURES {
                return false;
            }
        }
        pos += sector;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::{DemoDisk, DemoProfile};

    #[test]
    fn buckets_follow_limits() {
        assert_eq!(bucket_index(4_999, BlockStatus::Ok), Some(0));
        assert_eq!(bucket_index(5_000, BlockStatus::Ok), Some(1));
        assert_eq!(bucket_index(160_000, BlockStatus::OkZero), Some(4));
        assert_eq!(bucket_index(9_000_000, BlockStatus::Ok), Some(5));
        assert_eq!(bucket_index(1, BlockStatus::Error), Some(6));
        assert_eq!(bucket_index(1, BlockStatus::NotScanned), None);
    }

    #[test]
    fn orders_cover_expected_blocks() {
        let map = BlockMap::new(10 << 20, 1 << 20);
        assert_eq!(block_order(&ScanMode::Sequential, &map, 1), (0..10).collect::<Vec<_>>());
        assert_eq!(block_order(&ScanMode::Butterfly, &map, 1), vec![0, 9, 1, 8, 2, 7, 3, 6, 4, 5]);
        let r = block_order(&ScanMode::Range { start_byte: 2 << 20, end_byte: (4 << 20) + 1 }, &map, 1);
        assert_eq!(r, vec![2, 3, 4]);
        let s = block_order(&ScanMode::RandomSample { percent: 50.0 }, &map, 3);
        assert_eq!(s.len(), 5);
        assert!(s.iter().all(|&i| i < 10));
    }

    #[test]
    fn scan_finds_injected_bad_sectors() {
        let mut disk = DemoDisk::new(DemoProfile::test_hdd(64 << 20), false);
        let bad = disk.profile().bad_sectors.clone();
        let cancel = AtomicBool::new(false);
        let mut events = 0;
        let out = run_read_scan(&mut disk, &ScanConfig::default(), &cancel, &mut |_| events += 1);
        assert!(out.completed);
        assert_eq!(out.bad_sectors, bad);
        assert!(out.map.bucket_counts()[BUCKET_COUNT - 1] >= 1);
        assert_eq!(out.map.scanned_blocks(), 64);
        assert!(events > 0);
    }

    #[test]
    fn cancel_stops_the_scan() {
        let mut disk = DemoDisk::new(DemoProfile::test_hdd(64 << 20), false);
        let cancel = AtomicBool::new(true);
        let out = run_read_scan(&mut disk, &ScanConfig::default(), &cancel, &mut |_| {});
        assert!(out.cancelled);
        assert_eq!(out.map.scanned_blocks(), 0);
    }

    #[test]
    fn zero_detection() {
        assert!(is_all_zero(&[0u8; 4099]));
        let mut v = vec![0u8; 4099];
        v[4098] = 1;
        assert!(!is_all_zero(&v));
    }
}
