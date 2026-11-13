//! Platform-independent engine for Drive Test.
//!
//! The crate holds everything that does not talk to Windows directly:
//!
//! * [`policy`] — the safety rules. It is the only code that can obtain a
//!   writable device handle, and it never issues one for the system disk.
//! * [`scan`], [`writetest`], [`freespace`] — the test engines.
//! * [`health`] — SMART / NVMe parsing, verdicts and SMR assessment.
//! * [`sd`] — SD card CID / CSD / SD Status parsing and counterfeit signals.
//! * [`stale`], [`refresh`], [`speedclass`] — stale-data check, SSD refresh
//!   (file-level) and speed-class verification.
//! * [`report`] — the JSON report written after every run.
//! * [`runner`] — runs a job on a worker thread and streams events.
//! * [`demo`] — a simulated platform used by `--demo` and the unit tests.
//!
//! Real hardware access lives in the `drivetest-win` crate, which implements
//! [`platform::Platform`].

pub mod aligned;
pub mod analysis;
pub mod demo;
pub mod events;
pub mod freespace;
pub mod fsinfo;
pub mod health;
pub mod model;
pub mod platform;
pub mod policy;
pub mod recheck;
pub mod refresh;
pub mod repair;
pub mod report;
pub mod runner;
pub mod scan;
pub mod sd;
pub mod signature;
pub mod speedclass;
pub mod stale;
pub mod stats;
pub mod writetest;

/// Version of the tool, recorded in every report.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
