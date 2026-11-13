//! Read-only recheck of the exact addresses saved by a scan.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::aligned::AlignedBuf;
use crate::events::{Batcher, EngineEvent};
use crate::model::DriveInfo;
use crate::platform::{RawReader, is_media_read_error};
use crate::report::{REPORT_SCHEMA, Report};
use crate::scan::{SLOW_THRESHOLD_US, duration_us};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecheckState {
    Readable,
    StillSlow,
    Unreadable,
    IoError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecheckRead {
    pub offset: u64,
    pub length: u32,
    pub latency_us: u32,
    pub state: RecheckState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecheckOutcome {
    pub source_started_at: chrono::DateTime<chrono::Utc>,
    pub planned: usize,
    pub reads: Vec<RecheckRead>,
    pub cancelled: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RecheckPlan {
    pub drive: DriveInfo,
    source_started_at: chrono::DateTime<chrono::Utc>,
    targets: BTreeSet<(u64, u32)>,
}

impl RecheckPlan {
    pub fn from_report(report: &Report, platform: &str) -> Result<Self, String> {
        if report.schema != REPORT_SCHEMA || report.platform != platform {
            return Err("The report schema or platform does not match this app.".into());
        }
        let scan = report.scan.as_ref().ok_or("Choose a read scan report.")?;
        let drive = report.drive.clone();
        let sector = u64::from(drive.logical_sector_size);
        let block = u64::from(scan.block_size);
        if sector < 512 || block < sector || block > 64 << 20 || !block.is_multiple_of(sector) {
            return Err("The report contains invalid scan geometry.".into());
        }
        let mut targets = BTreeSet::new();
        let mut add = |offset: Option<u64>, wanted: u64| -> Result<(), String> {
            let offset = offset
                .filter(|&value| value < drive.size_bytes && value.is_multiple_of(sector))
                .ok_or("The report contains an invalid read location.")?;
            let length = wanted.min(drive.size_bytes - offset);
            if !length.is_multiple_of(sector) {
                return Err("The report contains an incomplete sector.".into());
            }
            targets.insert((offset, length as u32));
            Ok(())
        };
        for &lba in &scan.bad_sectors {
            add(lba.checked_mul(sector), sector)?;
        }
        for &offset in &scan.slow_offsets {
            add(Some(offset), block)?;
        }
        for &index in &scan.unresolved_blocks {
            add(index.checked_mul(block), block)?;
        }
        if targets.is_empty() {
            return Err("The report contains no saved problem locations.".into());
        }
        Ok(Self { drive, source_started_at: report.test.started_at, targets })
    }
}

pub fn run(
    reader: &mut dyn RawReader,
    plan: &RecheckPlan,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(EngineEvent),
) -> RecheckOutcome {
    let mut outcome = RecheckOutcome {
        source_started_at: plan.source_started_at,
        planned: plan.targets.len(),
        reads: Vec::new(),
        cancelled: false,
        error: None,
    };
    let max_length = plan.targets.iter().map(|&(_, length)| length).max().unwrap_or(0);
    let mut buffer = AlignedBuf::new(max_length as usize);
    let mut batcher = Batcher::new(sink);
    batcher.pass("Rechecking saved locations", plan.targets.len() as u64, 0, 0);
    for &(offset, length) in &plan.targets {
        if cancel.load(Ordering::Relaxed) {
            outcome.cancelled = true;
            break;
        }
        let started = Instant::now();
        let result = reader.read_at(offset, &mut buffer[..length as usize]);
        if cancel.load(Ordering::Relaxed) {
            outcome.cancelled = true;
            break;
        }
        let latency_us = duration_us(reader.simulated_latency().unwrap_or_else(|| started.elapsed()));
        let state = match result {
            Ok(()) if latency_us >= SLOW_THRESHOLD_US => RecheckState::StillSlow,
            Ok(()) => RecheckState::Readable,
            Err(error) if is_media_read_error(&error) => RecheckState::Unreadable,
            Err(error) => {
                outcome.error = Some(format!("Recheck stopped at byte {offset}: {error}"));
                RecheckState::IoError
            }
        };
        outcome.reads.push(RecheckRead { offset, length, latency_us, state });
        batcher.push(None, u64::from(length));
        if outcome.error.is_some() {
            break;
        }
    }
    batcher.flush();
    outcome
}
