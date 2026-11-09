//! Events streamed from the engines to front ends, and the batching helper
//! that keeps the channel from flooding on fast drives.

use std::time::{Duration, Instant};

use crate::scan::BlockStatus;

/// Result for one block, sent in batches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockUpdate {
    pub index: u64,
    pub latency_us: u32,
    pub status: BlockStatus,
}

/// Progress through the current pass.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub bytes: u64,
    pub elapsed: Duration,
    /// Throughput over roughly the last second.
    pub recent_mb_s: f64,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 { 0.0 } else { (self.done as f64 / self.total as f64) as f32 }
    }

    pub fn eta(&self) -> Option<Duration> {
        if self.done == 0 || self.done >= self.total {
            return None;
        }
        let per_item = self.elapsed.as_secs_f64() / self.done as f64;
        Some(Duration::from_secs_f64(per_item * (self.total - self.done) as f64))
    }
}

/// Engine output.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// A new pass over the drive begins; front ends reset their grid.
    Pass {
        name: String,
        total_blocks: u64,
        block_size: u32,
    },
    Blocks(Vec<BlockUpdate>),
    Progress(Progress),
    Log(String),
}

/// Collects block updates and progress and forwards them at a steady rate.
pub struct Batcher<'a> {
    sink: &'a mut dyn FnMut(EngineEvent),
    pending: Vec<BlockUpdate>,
    last_flush: Instant,
    pass_start: Instant,
    window_start: Instant,
    window_bytes: u64,
    recent_mb_s: f64,
    done: u64,
    total: u64,
    bytes: u64,
}

const FLUSH_EVERY: Duration = Duration::from_millis(100);
const MAX_PENDING: usize = 4096;

impl<'a> Batcher<'a> {
    pub fn new(sink: &'a mut dyn FnMut(EngineEvent)) -> Self {
        let now = Instant::now();
        Self {
            sink,
            pending: Vec::with_capacity(256),
            last_flush: now,
            pass_start: now,
            window_start: now,
            window_bytes: 0,
            recent_mb_s: 0.0,
            done: 0,
            total: 0,
            bytes: 0,
        }
    }

    /// Starts a pass: flushes the previous one and resets counters.
    pub fn pass(&mut self, name: &str, total_items: u64, total_blocks: u64, block_size: u32) {
        self.flush();
        let now = Instant::now();
        self.pass_start = now;
        self.window_start = now;
        self.window_bytes = 0;
        self.recent_mb_s = 0.0;
        self.done = 0;
        self.total = total_items;
        self.bytes = 0;
        (self.sink)(EngineEvent::Pass { name: name.to_string(), total_blocks, block_size });
    }

    pub fn log(&mut self, msg: impl Into<String>) {
        (self.sink)(EngineEvent::Log(msg.into()));
    }

    /// Records a finished item (usually one block).
    pub fn push(&mut self, update: Option<BlockUpdate>, bytes: u64) {
        if let Some(u) = update {
            self.pending.push(u);
        }
        self.done += 1;
        self.bytes += bytes;
        self.window_bytes += bytes;
        if self.pending.len() >= MAX_PENDING || self.last_flush.elapsed() >= FLUSH_EVERY {
            self.flush();
        }
    }

    /// Sends pending updates and a progress event.
    pub fn flush(&mut self) {
        let window = self.window_start.elapsed();
        if window >= Duration::from_millis(900) {
            self.recent_mb_s = self.window_bytes as f64 / 1e6 / window.as_secs_f64();
            self.window_start = Instant::now();
            self.window_bytes = 0;
        }
        if !self.pending.is_empty() {
            let batch = std::mem::replace(&mut self.pending, Vec::with_capacity(256));
            (self.sink)(EngineEvent::Blocks(batch));
        }
        (self.sink)(EngineEvent::Progress(Progress {
            done: self.done,
            total: self.total,
            bytes: self.bytes,
            elapsed: self.pass_start.elapsed(),
            recent_mb_s: self.recent_mb_s,
        }));
        self.last_flush = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_scales_with_remaining_work() {
        let p = Progress { done: 25, total: 100, bytes: 0, elapsed: Duration::from_secs(10), recent_mb_s: 0.0 };
        assert_eq!(p.eta().unwrap().as_secs(), 30);
        assert!((p.fraction() - 0.25).abs() < 1e-6);
    }
}
