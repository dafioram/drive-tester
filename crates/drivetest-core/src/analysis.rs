//! Analysis of block maps: speed curves, weak regions and SSD cache cliffs.

use serde::{Deserialize, Serialize};

use crate::scan::{BlockMap, BlockStatus, SLOW_THRESHOLD_US};

/// Throughput per equal-sized segment of the drive, in MB/s.
/// Segments with no successful blocks are 0.
pub fn speed_curve(map: &BlockMap, segments: usize) -> Vec<f64> {
    let n = map.total_blocks() as usize;
    if n == 0 || segments == 0 {
        return Vec::new();
    }
    let segments = segments.min(n);
    let mut bytes = vec![0f64; segments];
    let mut secs = vec![0f64; segments];
    for i in 0..n {
        let (lat, status) = map.get(i as u64);
        if !status.is_ok() || lat == 0 {
            continue;
        }
        let seg = i * segments / n;
        bytes[seg] += map.block_len(i as u64) as f64;
        secs[seg] += f64::from(lat) / 1e6;
    }
    bytes.iter().zip(&secs).map(|(b, s)| if *s > 0.0 { b / 1e6 / s } else { 0.0 }).collect()
}

/// A stretch of the drive that read markedly slower than its neighbours.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeakRegion {
    pub start_byte: u64,
    pub end_byte: u64,
    pub mb_s: f64,
    pub expected_mb_s: f64,
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Finds segments slower than 60% of the median of their neighbours.
/// On an HDD the curve normally falls smoothly from start to end; dips are weak areas.
pub fn weak_regions(map: &BlockMap, curve: &[f64]) -> Vec<WeakRegion> {
    let segs = curve.len();
    if segs < 8 {
        return Vec::new();
    }
    let seg_bytes = map.device_size as f64 / segs as f64;
    let mut regions: Vec<WeakRegion> = Vec::new();
    for i in 0..segs {
        if curve[i] <= 0.0 {
            continue;
        }
        let lo = i.saturating_sub(5);
        let hi = (i + 5).min(segs - 1);
        let neighbours: Vec<f64> = (lo..=hi).filter(|&j| j != i && curve[j] > 0.0).map(|j| curve[j]).collect();
        let expected = median(neighbours);
        if expected > 0.0 && curve[i] < 0.6 * expected {
            let start = (i as f64 * seg_bytes) as u64;
            let end = (((i + 1) as f64) * seg_bytes) as u64;
            match regions.last_mut() {
                Some(last) if last.end_byte == start => {
                    last.end_byte = end;
                    last.mb_s = last.mb_s.min(curve[i]);
                }
                _ => regions.push(WeakRegion {
                    start_byte: start,
                    end_byte: end,
                    mb_s: curve[i],
                    expected_mb_s: expected,
                }),
            }
        }
    }
    regions
}

/// Blocks at or above the slow threshold (likely internal retries).
pub fn slow_blocks(map: &BlockMap) -> u64 {
    map.latency_us
        .iter()
        .zip(&map.status)
        .filter(|(l, s)| BlockStatus::from_u8(**s).is_ok() && **l >= SLOW_THRESHOLD_US)
        .count() as u64
}

/// Blocks that read as all zeros.
pub fn zero_blocks(map: &BlockMap) -> u64 {
    map.status.iter().filter(|&&s| s == BlockStatus::OkZero as u8).count() as u64
}

/// Where sustained writes drop from cache speed to native flash speed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheCliff {
    pub after_bytes: u64,
    pub fast_mb_s: f64,
    pub sustained_mb_s: f64,
}

/// Looks for a lasting drop below half the initial write speed.
pub fn cache_cliff(write_map: &BlockMap) -> Option<CacheCliff> {
    let curve = speed_curve(write_map, 200);
    let segs = curve.len();
    if segs < 20 {
        return None;
    }
    let head = (segs / 20).max(2);
    let fast = median(curve[..head].to_vec());
    if fast <= 0.0 {
        return None;
    }
    let seg_bytes = write_map.device_size as f64 / segs as f64;
    for i in head..segs.saturating_sub(3) {
        if curve[i..i + 3].iter().all(|&s| s > 0.0 && s < 0.5 * fast) {
            let sustained = median(curve[i..].iter().copied().filter(|&s| s > 0.0).collect());
            return Some(CacheCliff {
                after_bytes: (i as f64 * seg_bytes) as u64,
                fast_mb_s: fast,
                sustained_mb_s: sustained,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with(latencies: &[u32]) -> BlockMap {
        let mut m = BlockMap::new(latencies.len() as u64 * (1 << 20), 1 << 20);
        for (i, &l) in latencies.iter().enumerate() {
            m.set(i as u64, l, BlockStatus::Ok);
        }
        m
    }

    #[test]
    fn curve_reports_mb_per_second() {
        // 1 MiB in 10 ms ≈ 104.9 MB/s
        let curve = speed_curve(&map_with(&[10_000; 10]), 10);
        assert!(curve.iter().all(|&s| (s - 104.857_6).abs() < 0.01));
    }

    #[test]
    fn dips_are_reported_as_weak_regions() {
        let mut lat = vec![10_000u32; 100];
        for l in &mut lat[40..43] {
            *l = 40_000;
        }
        let map = map_with(&lat);
        let curve = speed_curve(&map, 100);
        let weak = weak_regions(&map, &curve);
        assert_eq!(weak.len(), 1);
        assert_eq!(weak[0].start_byte, 40 << 20);
        assert_eq!(weak[0].end_byte, 43 << 20);
    }

    #[test]
    fn cache_cliff_is_found() {
        let mut lat = vec![2_000u32; 400];
        for l in &mut lat[100..] {
            *l = 10_000;
        }
        let cliff = cache_cliff(&map_with(&lat)).expect("cliff");
        assert_eq!(cliff.after_bytes, 100 << 20);
        assert!(cliff.fast_mb_s > 4.0 * cliff.sustained_mb_s);
    }

    #[test]
    fn steady_writes_have_no_cliff() {
        assert!(cache_cliff(&map_with(&[5_000; 400])).is_none());
    }
}
