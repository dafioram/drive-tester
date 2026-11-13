//! The JSON report written after every run. It holds the raw data (every
//! block's latency and status), so the GUI can redraw grids and graphs and
//! other tools can analyse results without image exports.

use std::fs;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::analysis::{CacheCliff, WeakRegion};
use crate::freespace::FreeSpaceOutcome;
use crate::health::{HealthChange, HealthSnapshot, HealthVerdict, SmrAssessment, VerdictLevel};
use crate::model::DriveInfo;
use crate::platform::SectorFile;
use crate::policy::{Mode, Tier};
use crate::scan::{BUCKET_COUNT, BlockMap, ScanMode};
use crate::stats::LatencyStats;
use crate::writetest::{CapacityVerdict, SmrProbeResult, WriteTestKind};

/// Bump when the report layout changes incompatibly.
pub const REPORT_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Passed,
    Problems,
    Incomplete,
    Unsupported,
    Error,
}

impl JobStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "Passed",
            Self::Problems => "Problems found",
            Self::Incomplete => "Incomplete",
            Self::Unsupported => "Unsupported",
            Self::Error => "Error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub tool_version: String,
    pub platform: String,
    pub drive: DriveInfo,
    pub test: TestInfo,
    pub health: HealthSection,
    pub smr: Option<SmrAssessment>,
    pub scan: Option<ScanReport>,
    pub write_test: Option<WriteTestReport>,
    pub free_space: Option<FreeSpaceOutcome>,
    #[serde(default)]
    pub stale: Option<crate::stale::StaleOutcome>,
    #[serde(default)]
    pub file_refresh: Option<crate::refresh::FileRefreshOutcome>,
    #[serde(default)]
    pub speed_class: Option<crate::speedclass::SpeedClassOutcome>,
    pub self_test: Option<SelfTestReport>,
    pub temperatures: Vec<TemperatureSample>,
    /// Plain-language findings, most important first.
    pub findings: Vec<String>,
    pub log: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestInfo {
    pub kind: String,
    pub tier: Tier,
    pub mode: Mode,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status: Option<JobStatus>,
    pub completed: bool,
    pub cancelled: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub unsupported: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthSection {
    pub before: Option<HealthSnapshot>,
    pub after: Option<HealthSnapshot>,
    /// Watched counters that rose during the test.
    pub changes: Vec<HealthChange>,
    pub verdict: Option<HealthVerdict>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemperatureSample {
    pub elapsed_s: f64,
    pub celsius: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfTestReport {
    pub kind: String,
    pub started: bool,
    pub final_status: Option<String>,
    pub note: Option<String>,
    #[serde(default)]
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanReport {
    pub mode: ScanMode,
    pub block_size: u32,
    pub total_blocks: u64,
    pub scanned_blocks: u64,
    pub bytes_read: u64,
    pub elapsed_s: f64,
    pub average_mb_s: f64,
    pub latency: LatencyStats,
    pub bucket_counts: [u64; BUCKET_COUNT],
    pub slow_blocks: u64,
    #[serde(default)]
    pub slow_offsets: Vec<u64>,
    #[serde(default)]
    pub recovered_blocks: u64,
    #[serde(default)]
    pub scan_config: Option<crate::scan::ScanConfig>,
    /// Blocks that read as all zeros (on SSDs: trimmed / never written).
    pub zero_blocks: u64,
    #[serde(default)]
    pub data_bytes: Option<u64>,
    pub bad_sectors: Vec<u64>,
    pub bad_sector_files: Vec<SectorFile>,
    pub unresolved_blocks: Vec<u64>,
    pub weak_regions: Vec<WeakRegion>,
    pub speed_curve_mb_s: Vec<f64>,
    pub blocks: BlockMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteTestReport {
    pub kind: WriteTestKind,
    pub bytes_written: u64,
    pub elapsed_s: f64,
    pub write_errors: u64,
    #[serde(default)]
    pub flush_error: Option<String>,
    pub corrupt_units: u64,
    pub aliased_units: u64,
    pub unreadable_units: u64,
    pub first_bad_offset: Option<u64>,
    pub bad_offsets: Vec<u64>,
    pub capacity: Option<CapacityVerdict>,
    pub cache_cliff: Option<CacheCliff>,
    pub smr: Option<SmrProbeResult>,
    #[serde(default)]
    pub repair: Option<crate::repair::RepairSummary>,
    pub write_latency: LatencyStats,
    pub write_speed_curve_mb_s: Vec<f64>,
    pub write_blocks: BlockMap,
    pub verify_blocks: Option<BlockMap>,
}

impl Report {
    pub fn new(platform: &str, drive: &DriveInfo, kind: &str, tier: Tier, mode: Mode) -> Self {
        Self {
            schema: REPORT_SCHEMA,
            tool_version: crate::TOOL_VERSION.to_string(),
            platform: platform.to_string(),
            drive: drive.clone(),
            test: TestInfo {
                kind: kind.to_string(),
                tier,
                mode,
                started_at: Utc::now(),
                finished_at: None,
                status: None,
                completed: false,
                cancelled: false,
                error: None,
                unsupported: false,
            },
            health: HealthSection::default(),
            smr: None,
            scan: None,
            write_test: None,
            free_space: None,
            stale: None,
            file_refresh: None,
            speed_class: None,
            self_test: None,
            temperatures: Vec::new(),
            findings: Vec::new(),
            log: Vec::new(),
        }
    }

    /// Use the same outcome for human-readable and machine-readable output.
    pub fn status(&self) -> JobStatus {
        if self.test.unsupported {
            JobStatus::Unsupported
        } else if self.test.error.is_some() {
            JobStatus::Error
        } else if self.test.cancelled || !self.test.completed {
            JobStatus::Incomplete
        } else if self.problems_found() {
            JobStatus::Problems
        } else {
            JobStatus::Passed
        }
    }

    /// True when anything worth attention was found.
    pub fn problems_found(&self) -> bool {
        let health_bad = self.health.verdict.as_ref().is_some_and(|v| v.level >= VerdictLevel::Caution);
        let scan_bad =
            self.scan.as_ref().is_some_and(|s| !s.bad_sectors.is_empty() || s.bucket_counts[BUCKET_COUNT - 1] > 0);
        let write_bad = self.write_test.as_ref().is_some_and(|w| {
            w.write_errors + w.corrupt_units + w.aliased_units + w.unreadable_units > 0
                || w.capacity.as_ref().is_some_and(|c| !c.genuine || c.defects_found)
        });
        let free_bad = self.free_space.as_ref().is_some_and(|f| f.problems_found());
        let stale_bad = self.stale.as_ref().is_some_and(|s| s.stale_detected);
        let refresh_bad = self.file_refresh.as_ref().is_some_and(|f| f.problems_found());
        let speed_bad = self.speed_class.as_ref().is_some_and(|s| s.problems_found());
        let repair_bad = self
            .write_test
            .as_ref()
            .and_then(|w| w.repair.as_ref())
            .is_some_and(|r| !r.still_bad_sectors.is_empty() || r.verify_failures > 0 || r.unreadable_blocks > 0);
        health_bad
            || scan_bad
            || write_bad
            || free_bad
            || stale_bad
            || refresh_bad
            || speed_bad
            || repair_bad
            || self.self_test.as_ref().is_some_and(|test| test.failed)
            || !self.health.changes.is_empty()
    }

    /// Writes compact JSON (per-block arrays make pretty-printing very large).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::File::create(path)?;
        serde_json::to_writer(BufWriter::new(file), self).map_err(io::Error::other)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let file = fs::File::open(path)?;
        serde_json::from_reader(io::BufReader::new(file)).map_err(io::Error::other)
    }

    /// File name like `disk2-read-scan-20261108-224512.json`.
    pub fn default_file_name(&self) -> String {
        let kind: String =
            self.test.kind.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let kind = kind.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
        let local = self.test.started_at.with_timezone(&chrono::Local);
        format!("disk{}-{}-{}.json", self.drive.number, kind, local.format("%Y%m%d-%H%M%S"))
    }
}

/// `Documents\DriveTest\reports` on Windows, `./drivetest-reports` elsewhere.
pub fn default_report_dir() -> PathBuf {
    if cfg!(windows)
        && let Some(profile) = std::env::var_os("USERPROFILE")
    {
        return PathBuf::from(profile).join("Documents").join("DriveTest").join("reports");
    }
    PathBuf::from("drivetest-reports")
}
