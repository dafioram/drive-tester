//! Live (and replayed) test view: block grid, legend, speed curve, latency stats.

use std::collections::VecDeque;

use drivetest_core::events::{BlockUpdate, Progress};
use drivetest_core::report::Report;
use drivetest_core::scan::{BUCKET_COUNT, BUCKET_LABELS, BlockMap, BlockStatus, bucket_index};
use drivetest_core::stats::{LatencyStats, format_latency};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};

/// Fine-grained cells kept per pass; the grid aggregates them to fit the screen.
const FINE_MAX: u64 = 8192;
const CURVE_SEGMENTS: usize = 200;
const HIST_BINS: usize = 350;

/// Severity per cell: 0 not scanned, 1 zero block, 2..=7 latency buckets, 8 error.
pub const SEV_NOT_SCANNED: u8 = 0;
pub const SEV_ZERO: u8 = 1;
pub const SEV_ERROR: u8 = 8;

pub fn severity_color(sev: u8) -> Color32 {
    match sev {
        SEV_ZERO => Color32::from_rgb(112, 132, 160),
        2 => Color32::from_rgb(46, 160, 67),
        3 => Color32::from_rgb(125, 190, 60),
        4 => Color32::from_rgb(205, 200, 50),
        5 => Color32::from_rgb(238, 160, 40),
        6 => Color32::from_rgb(230, 100, 40),
        7 => Color32::from_rgb(200, 45, 40),
        SEV_ERROR => Color32::from_rgb(150, 0, 60),
        _ => Color32::TRANSPARENT,
    }
}

fn severity(latency_us: u32, status: BlockStatus) -> u8 {
    match (status, bucket_index(latency_us, status)) {
        (BlockStatus::NotScanned, _) | (_, None) => SEV_NOT_SCANNED,
        (BlockStatus::Error, _) => SEV_ERROR,
        // Fast zero blocks are shown separately (trimmed / never written);
        // slow ones keep their latency colour.
        (BlockStatus::OkZero, Some(0)) => SEV_ZERO,
        (_, Some(b)) => 2 + b as u8,
    }
}

fn hist_bin(us: u32) -> usize {
    let v = f64::from(us.max(10)).log10();
    (((v - 1.0) * 50.0) as usize).min(HIST_BINS - 1)
}

fn hist_value(bin: usize) -> u32 {
    10f64.powf(1.0 + (bin as f64 + 0.5) / 50.0) as u32
}

#[derive(Default, Clone)]
pub struct LiveStats {
    pub count: u64,
    pub p50: u32,
    pub p99: u32,
    pub p999: u32,
    pub max: u32,
}

pub struct LiveView {
    pub pass: String,
    pub phase: String,
    pub total_blocks: u64,
    pub block_size: u32,
    fine: Vec<u8>,
    pub buckets: [u64; BUCKET_COUNT],
    pub zero_blocks: u64,
    hist: Vec<u64>,
    max_latency: u32,
    seg_bytes: Vec<f64>,
    seg_secs: Vec<f64>,
    pub progress: Progress,
    pub temps: Vec<(f64, i32)>,
    pub log: VecDeque<String>,
    /// Exact statistics from a finished report.
    pub exact: Option<LatencyStats>,
    pub reported_curve: Option<Vec<f64>>,
}

impl Default for LiveView {
    fn default() -> Self {
        Self {
            pass: String::new(),
            phase: String::new(),
            total_blocks: 0,
            block_size: 0,
            fine: Vec::new(),
            buckets: [0; BUCKET_COUNT],
            zero_blocks: 0,
            hist: vec![0; HIST_BINS],
            max_latency: 0,
            seg_bytes: vec![0.0; CURVE_SEGMENTS],
            seg_secs: vec![0.0; CURVE_SEGMENTS],
            progress: Progress::default(),
            temps: Vec::new(),
            log: VecDeque::new(),
            exact: None,
            reported_curve: None,
        }
    }
}

impl LiveView {
    pub fn start_pass(&mut self, name: &str, total_blocks: u64, block_size: u32) {
        let temps = std::mem::take(&mut self.temps);
        let log = std::mem::take(&mut self.log);
        let phase = std::mem::take(&mut self.phase);
        *self = Self { temps, log, phase, ..Self::default() };
        self.pass = name.to_string();
        self.total_blocks = total_blocks;
        self.block_size = block_size;
        self.fine = vec![SEV_NOT_SCANNED; total_blocks.min(FINE_MAX) as usize];
    }

    pub fn has_grid(&self) -> bool {
        !self.fine.is_empty()
    }

    pub fn push_log(&mut self, line: String) {
        if self.log.len() >= 300 {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    pub fn apply(&mut self, updates: &[BlockUpdate]) {
        if self.total_blocks == 0 {
            return;
        }
        let fine_len = self.fine.len() as u64;
        for u in updates {
            if u.index >= self.total_blocks {
                continue;
            }
            let sev = severity(u.latency_us, u.status);
            let cell = (u.index * fine_len / self.total_blocks) as usize;
            if sev > self.fine[cell] {
                self.fine[cell] = sev;
            }
            if let Some(b) = bucket_index(u.latency_us, u.status) {
                self.buckets[b] += 1;
            }
            if u.status == BlockStatus::OkZero {
                self.zero_blocks += 1;
            }
            if u.status.is_ok() {
                self.hist[hist_bin(u.latency_us)] += 1;
                self.max_latency = self.max_latency.max(u.latency_us);
                if u.latency_us > 0 {
                    let seg = (u.index * CURVE_SEGMENTS as u64 / self.total_blocks) as usize;
                    self.seg_bytes[seg] += f64::from(self.block_size);
                    self.seg_secs[seg] += f64::from(u.latency_us) / 1e6;
                }
            }
        }
    }

    pub fn stats(&self) -> LiveStats {
        if let Some(e) = &self.exact {
            return LiveStats { count: e.samples, p50: e.p50_us, p99: e.p99_us, p999: e.p999_us, max: e.max_us };
        }
        let count: u64 = self.hist.iter().sum();
        if count == 0 {
            return LiveStats::default();
        }
        let pick = |p: f64| {
            let target = (count as f64 * p).ceil() as u64;
            let mut acc = 0;
            for (i, &c) in self.hist.iter().enumerate() {
                acc += c;
                if acc >= target {
                    return hist_value(i);
                }
            }
            hist_value(HIST_BINS - 1)
        };
        LiveStats { count, p50: pick(0.5), p99: pick(0.99), p999: pick(0.999), max: self.max_latency }
    }

    pub fn curve(&self) -> Vec<f64> {
        if let Some(curve) = &self.reported_curve {
            return curve.clone();
        }
        self.seg_bytes.iter().zip(&self.seg_secs).map(|(b, s)| if *s > 0.0 { b / 1e6 / s } else { 0.0 }).collect()
    }

    fn load_map(&mut self, name: &str, map: &BlockMap) {
        self.start_pass(name, map.total_blocks(), map.block_size);
        let mut updates = Vec::with_capacity(1024);
        for i in 0..map.total_blocks() {
            let (latency_us, status) = map.get(i);
            if status != BlockStatus::NotScanned {
                updates.push(BlockUpdate { index: i, latency_us, status });
                if updates.len() == 1024 {
                    self.apply(&updates);
                    updates.clear();
                }
            }
        }
        self.apply(&updates);
    }

    /// Rebuilds a view from a finished report.
    pub fn from_report(report: &Report) -> Self {
        let mut v = Self::default();
        if let Some(s) = &report.scan {
            v.load_map("Read scan (finished)", &s.blocks);
            v.exact = Some(s.latency.clone());
            v.buckets = s.bucket_counts;
            v.zero_blocks = s.zero_blocks;
            v.reported_curve = Some(s.speed_curve_mb_s.clone());
        } else if let Some(w) = &report.write_test {
            match &w.verify_blocks {
                Some(map) => v.load_map("Verify pass (finished)", map),
                None => v.load_map("Write pass (finished)", &w.write_blocks),
            }
        } else {
            v.pass = format!("{} (finished)", report.test.kind);
        }
        v.temps = report.temperatures.iter().map(|t| (t.elapsed_s, t.celsius)).collect();
        v.progress = Progress { done: 1, total: 1, ..Default::default() };
        v.log = report.log.iter().cloned().collect();
        v
    }
}

// ------------------------------------------------------------------ painting

/// The block grid. Each cell covers a range of the drive, coloured by the
/// worst result inside it.
pub fn draw_grid(ui: &mut egui::Ui, view: &LiveView) {
    if !view.has_grid() {
        return;
    }
    let cell = 10.0;
    let gap = 2.0;
    let width = ui.available_width().max(200.0);
    let cols = ((width + gap) / (cell + gap)).floor().max(1.0) as usize;
    let fine = view.fine.len();
    let display = fine.min(cols * 26);
    let rows = display.div_ceil(cols);
    let size = vec2(width, rows as f32 * (cell + gap));
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let empty_stroke = Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color);

    let cell_rect = |k: usize| -> Rect {
        let (r, c) = (k / cols, k % cols);
        Rect::from_min_size(
            pos2(rect.left() + c as f32 * (cell + gap), rect.top() + r as f32 * (cell + gap)),
            Vec2::splat(cell),
        )
    };
    let fine_range = |k: usize| (k * fine / display, ((k + 1) * fine / display).max(k * fine / display + 1));
    for k in 0..display {
        let (a, b) = fine_range(k);
        let sev = view.fine[a..b.min(fine)].iter().copied().max().unwrap_or(0);
        let r = cell_rect(k);
        if sev == SEV_NOT_SCANNED {
            painter.rect_stroke(r, 2.0, empty_stroke, StrokeKind::Inside);
        } else {
            painter.rect_filled(r, 2.0, severity_color(sev));
        }
    }

    if let Some(pos) = response.hover_pos() {
        let rel = pos - rect.min;
        let (c, r) = ((rel.x / (cell + gap)) as usize, (rel.y / (cell + gap)) as usize);
        let k = r * cols + c;
        if c < cols && k < display {
            let (a, b) = fine_range(k);
            let sev = view.fine[a..b.min(fine)].iter().copied().max().unwrap_or(0);
            let total_bytes = view.total_blocks * u64::from(view.block_size);
            let start = a as u64 * total_bytes / fine as u64;
            let end = (b as u64 * total_bytes / fine as u64).min(total_bytes);
            let label = match sev {
                SEV_NOT_SCANNED => "Not scanned yet".to_string(),
                SEV_ZERO => "Zeros, fast (trimmed / never written)".to_string(),
                SEV_ERROR => "Read or verify error".to_string(),
                s => format!("Worst block: {}", BUCKET_LABELS[(s - 2) as usize]),
            };
            response.on_hover_ui_at_pointer(|ui| {
                ui.label(format!(
                    "{} – {}",
                    drivetest_core::model::format_bytes(start),
                    drivetest_core::model::format_bytes(end)
                ));
                ui.label(label);
            });
        }
    }
}

/// Legend with live counts.
pub fn draw_legend(ui: &mut egui::Ui, view: &LiveView) {
    ui.horizontal_wrapped(|ui| {
        for (i, label) in BUCKET_LABELS.iter().enumerate() {
            let sev = if i == BUCKET_COUNT - 1 { SEV_ERROR } else { 2 + i as u8 };
            swatch(ui, severity_color(sev));
            ui.label(format!("{label}: {}", view.buckets[i]));
            ui.add_space(8.0);
        }
        if view.zero_blocks > 0 {
            swatch(ui, severity_color(SEV_ZERO));
            ui.label(format!("zeros: {}", view.zero_blocks)).on_hover_text(
                "Blocks that read as all zeros. On SSDs these are usually trimmed or never written, \
                 and the drive answers without touching flash, so their speed is not meaningful.",
            );
        }
    });
}

fn swatch(ui: &mut egui::Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    ui.painter().rect_filled(r, 2.0, color);
}

/// Throughput across the drive.
pub fn draw_curve(ui: &mut egui::Ui, view: &LiveView, title: &str) {
    let curve = view.curve();
    let max = curve.iter().copied().fold(0.0, f64::max);
    if max <= 0.0 {
        return;
    }
    let size = vec2(ui.available_width().max(200.0), 120.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    painter.rect_stroke(rect, 4.0, visuals.widgets.noninteractive.bg_stroke, StrokeKind::Inside);
    let plot = rect.shrink2(vec2(8.0, 18.0));
    let n = curve.len();
    let points: Vec<Pos2> = curve
        .iter()
        .enumerate()
        .filter(|(_, v)| **v > 0.0)
        .map(|(i, v)| {
            pos2(
                plot.left() + plot.width() * i as f32 / (n - 1).max(1) as f32,
                plot.bottom() - plot.height() * (*v / max) as f32,
            )
        })
        .collect();
    if points.len() >= 2 {
        painter.line(points, Stroke::new(1.8, Color32::from_rgb(70, 130, 220)));
    }
    let text_color = visuals.weak_text_color();
    painter.text(rect.left_top() + vec2(8.0, 3.0), Align2::LEFT_TOP, title, FontId::proportional(12.0), text_color);
    painter.text(
        rect.right_top() + vec2(-8.0, 3.0),
        Align2::RIGHT_TOP,
        format!("peak {max:.0} MB/s"),
        FontId::proportional(12.0),
        text_color,
    );
    painter.text(
        rect.left_bottom() + vec2(8.0, -3.0),
        Align2::LEFT_BOTTOM,
        "Start of drive",
        FontId::proportional(11.0),
        text_color,
    );
    painter.text(
        rect.right_bottom() + vec2(-8.0, -3.0),
        Align2::RIGHT_BOTTOM,
        "End of drive",
        FontId::proportional(11.0),
        text_color,
    );
}

/// Percentile tiles.
pub fn draw_stats(ui: &mut egui::Ui, view: &LiveView) {
    let s = view.stats();
    if s.count == 0 {
        return;
    }
    ui.horizontal(|ui| {
        for (label, value) in [("p50", s.p50), ("p99", s.p99), ("p99.9", s.p999), ("max", s.max)] {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_width(90.0);
                ui.vertical(|ui| {
                    ui.weak(format!("{label} latency"));
                    ui.strong(format_latency(value));
                });
            });
        }
        if view.exact.is_none() {
            ui.weak("(live values are approximate)");
        }
    });
}
