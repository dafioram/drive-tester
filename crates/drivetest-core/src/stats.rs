//! Latency statistics. Percentiles, not just averages, because the worst
//! latencies are what users notice.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LatencyStats {
    pub samples: u64,
    pub min_us: u32,
    pub mean_us: f64,
    pub p50_us: u32,
    pub p90_us: u32,
    pub p99_us: u32,
    pub p999_us: u32,
    pub max_us: u32,
}

/// Computes statistics over latency samples in microseconds.
pub fn latency_stats(samples: impl IntoIterator<Item = u32>) -> LatencyStats {
    let mut v: Vec<u32> = samples.into_iter().collect();
    if v.is_empty() {
        return LatencyStats::default();
    }
    v.sort_unstable();
    let n = v.len();
    let pick = |p: f64| -> u32 {
        let idx = ((p / 100.0) * (n - 1) as f64).round() as usize;
        v[idx.min(n - 1)]
    };
    let sum: u64 = v.iter().map(|&x| u64::from(x)).sum();
    LatencyStats {
        samples: n as u64,
        min_us: v[0],
        mean_us: sum as f64 / n as f64,
        p50_us: pick(50.0),
        p90_us: pick(90.0),
        p99_us: pick(99.0),
        p999_us: pick(99.9),
        max_us: v[n - 1],
    }
}

/// Fixed-size histogram. Values below 1024 µs are exact; the rest use
/// 256 bins per power of two, with an exact observed minimum and maximum.
pub(crate) struct LatencyHistogram {
    counts: Vec<u64>,
    stats: LatencyStats,
    sum: u128,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self { counts: vec![0; 1024 + 22 * 256], stats: LatencyStats::default(), sum: 0 }
    }
}

impl LatencyHistogram {
    pub fn add(&mut self, us: u32) {
        let index = if us < 1024 {
            us as usize
        } else {
            let shift = us.ilog2() - 8;
            1024 + (shift as usize - 2) * 256 + (us >> shift) as usize - 256
        };
        self.counts[index] += 1;
        if self.stats.samples == 0 || us < self.stats.min_us {
            self.stats.min_us = us;
        }
        self.stats.samples += 1;
        self.stats.max_us = self.stats.max_us.max(us);
        self.sum += u128::from(us);
    }

    pub fn finish(mut self) -> LatencyStats {
        let n = self.stats.samples;
        if n == 0 {
            return self.stats;
        }
        let pick = |p: f64| {
            let mut remaining = ((n - 1) as f64 * p).round() as u64;
            for (i, &count) in self.counts.iter().enumerate() {
                if remaining < count {
                    let value = if i < 1024 {
                        i as u32
                    } else {
                        let bin = i - 1024;
                        ((256 + bin % 256) as u32) << (2 + bin / 256)
                    };
                    return value.max(self.stats.min_us);
                }
                remaining -= count;
            }
            self.stats.max_us
        };
        (self.stats.p50_us, self.stats.p90_us, self.stats.p99_us, self.stats.p999_us) =
            (pick(0.50), pick(0.90), pick(0.99), pick(0.999));
        self.stats.mean_us = self.sum as f64 / n as f64;
        self.stats
    }
}

/// Formats microseconds for display: `850 µs`, `12.4 ms`, `1.20 s`.
pub fn format_latency(us: u32) -> String {
    match us {
        0..=999 => format!("{us} µs"),
        1_000..=999_999 => format!("{:.1} ms", f64::from(us) / 1e3),
        _ => format!("{:.2} s", f64::from(us) / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_on_known_data() {
        let s = latency_stats(1..=1000);
        assert_eq!(s.samples, 1000);
        assert_eq!(s.min_us, 1);
        assert_eq!(s.max_us, 1000);
        assert_eq!(s.p50_us, 501);
        assert_eq!(s.p99_us, 990);
        assert!((s.mean_us - 500.5).abs() < 1e-9);
    }

    #[test]
    fn empty_input_gives_zeroes() {
        assert_eq!(latency_stats(std::iter::empty()), LatencyStats::default());
    }

    #[test]
    fn latency_formatting() {
        assert_eq!(format_latency(850), "850 µs");
        assert_eq!(format_latency(12_400), "12.4 ms");
        assert_eq!(format_latency(1_200_000), "1.20 s");
    }
}
