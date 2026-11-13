use std::io;
use std::sync::atomic::AtomicBool;

use drivetest_core::demo::{DemoDisk, DemoProfile};
use drivetest_core::health::{
    AtaHealth, HealthSnapshot, SmartAttribute, ata_attribute_name, parse_ata_endurance_page, parse_nvme_health,
};
use drivetest_core::health_score::ScoreStatus;
use drivetest_core::model::{HealthSource, MediaKind};
use drivetest_core::platform::{MediaReadError, RawReader, RawWriter};
use drivetest_core::scan::{ScanConfig, ScanMode, run_read_scan};
use drivetest_core::writetest::{WriteTestConfig, WriteTestKind, run_write_test};

struct FaultDisk {
    disk: DemoDisk,
    read_error: bool,
    media_error: bool,
    flush_error: bool,
    writes: usize,
    reads: usize,
}

impl FaultDisk {
    fn new(sector: u32) -> Self {
        let mut profile = DemoProfile::test_flash(1 << 20, None);
        profile.sector = sector;
        Self {
            disk: DemoDisk::new(profile, false),
            read_error: false,
            media_error: false,
            flush_error: false,
            writes: 0,
            reads: 0,
        }
    }
}

impl RawReader for FaultDisk {
    fn size_bytes(&self) -> u64 {
        self.disk.size_bytes()
    }
    fn logical_sector_size(&self) -> u32 {
        self.disk.logical_sector_size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.reads += 1;
        if self.read_error {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "simulated access failure"));
        }
        if self.media_error {
            return Err(io::Error::other(MediaReadError(io::Error::other("simulated media error"))));
        }
        self.disk.read_at(offset, buf)
    }
}

impl RawWriter for FaultDisk {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.writes += 1;
        assert_eq!(buf.as_ptr() as usize % 4096, 0, "raw buffer must remain aligned");
        self.disk.write_at(offset, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.flush_error {
            return Err(io::Error::other("simulated flush error"));
        }
        self.disk.flush()
    }
}

#[test]
fn remap_aborts_without_writing_on_an_ambiguous_read() {
    for lbas in [vec![4], vec![]] {
        let mut disk = FaultDisk::new(512);
        disk.read_error = true;
        let cfg = WriteTestConfig::new(WriteTestKind::Remap { lbas, refresh_slower_than_us: None });
        let out = run_write_test(&mut disk, &cfg, &AtomicBool::new(false), &mut |_| {});
        assert!(!out.completed);
        assert!(out.repair.unwrap().aborted.unwrap().contains("access failure"));
        assert_eq!((disk.reads, disk.writes), (1, 0));
    }
}

#[test]
fn remap_reuses_aligned_storage_on_512e_and_4kn() {
    for sector in [512, 4096] {
        let mut disk = FaultDisk::new(sector);
        let cfg = WriteTestConfig::new(WriteTestKind::Remap { lbas: vec![4], refresh_slower_than_us: None });
        let out = run_write_test(&mut disk, &cfg, &AtomicBool::new(false), &mut |_| {});
        assert!(out.completed);
        assert_eq!(out.repair.unwrap().refreshed_sectors, vec![4]);
        assert_eq!(disk.writes, 1);
    }
}

#[test]
fn failed_flush_prevents_a_clean_capacity_verdict() {
    let mut disk = FaultDisk::new(512);
    disk.flush_error = true;
    let out = run_write_test(
        &mut disk,
        &WriteTestConfig::new(WriteTestKind::FullWriteVerify),
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert!(!out.completed);
    assert!(out.capacity.is_none());
    assert!(out.flush_error.is_some());
    assert!(out.verify_map.is_some());
}

#[test]
fn scan_aborts_host_errors_and_records_unresolved_media_blocks() {
    let mut disk = FaultDisk::new(512);
    disk.read_error = true;
    let cfg = ScanConfig { block_size: 4096, ..Default::default() };
    let out = run_read_scan(&mut disk, &cfg, &AtomicBool::new(false), &mut |_| {});
    assert!(!out.completed);
    assert_eq!(disk.reads, 1);
    assert_eq!(out.unresolved_blocks, vec![0]);
    assert!(out.bad_sectors.is_empty());
    disk.read_error = false;
    disk.media_error = true;
    let cfg = ScanConfig { pinpoint_errors: false, ..cfg };
    let out = run_read_scan(&mut disk, &cfg, &AtomicBool::new(false), &mut |_| {});
    assert_eq!(out.unresolved_blocks, (0..32).collect::<Vec<_>>());
    assert!(!out.completed);
}

#[test]
fn narrow_range_on_large_disk_uses_a_bounded_map() {
    struct LargeDisk;
    impl RawReader for LargeDisk {
        fn size_bytes(&self) -> u64 {
            20_000_000_000_000
        }
        fn logical_sector_size(&self) -> u32 {
            4096
        }
        fn read_at(&mut self, _offset: u64, buf: &mut [u8]) -> io::Result<()> {
            buf.fill(1);
            Ok(())
        }
    }
    let cfg = ScanConfig {
        block_size: 4096,
        mode: ScanMode::Range { start_byte: 4096, end_byte: 8192 },
        ..Default::default()
    };
    let out = run_read_scan(&mut LargeDisk, &cfg, &AtomicBool::new(false), &mut |_| {});
    assert_eq!(out.scanned_blocks, 1);
    assert_eq!(out.total_blocks, 4_882_812_500);
    assert!(out.map.total_blocks() <= 65_536);
}

fn ata(stats: &[(u8, u8, u64)], endurance_used: Option<u8>) -> HealthSnapshot {
    let attributes = stats
        .iter()
        .map(|&(id, current, raw)| SmartAttribute {
            id,
            name: ata_attribute_name(id).into(),
            current,
            raw,
            flags: 0,
            worst: current,
            threshold: 0,
            failing_now: false,
        })
        .collect();
    HealthSnapshot::from_ata(
        HealthSource::AtaSmart,
        AtaHealth { attributes, endurance_used, smart_passed: Some(true), identify: None, self_test: None },
    )
}

#[test]
fn custom_hdd_and_sata_scores_match_the_script_rules() {
    let stats = [
        (5, 100, 4),
        (197, 100, 1),
        (198, 100, 2),
        (187, 100, 3),
        (188, 100, 70_000),
        (9, 100, 52_596),
        (199, 100, 100),
    ];
    let hdd = ata(&stats, None).health_score(MediaKind::Hdd).unwrap();
    assert_eq!((hdd.percent, hdd.status), (63, ScoreStatus::Caution));
    assert_eq!(hdd.adjustments.len(), 5);
    let ssd = ata(&stats, Some(12)).health_score(MediaKind::Ssd).unwrap();
    assert_eq!((ssd.base_percent, ssd.percent, ssd.status), (88, 62, ScoreStatus::Caution));
    assert!(ata(&stats, None).health_score(MediaKind::Ssd).is_none());
    let fallback = ata(&[(231, 77, 0), (197, 100, 1)], None).health_score(MediaKind::Ssd).unwrap();
    assert_eq!((fallback.base_percent, fallback.percent, fallback.status), (77, 72, ScoreStatus::Caution));
    assert!(HealthSnapshot::unavailable("no SMART").health_score(MediaKind::Ssd).is_none());
}

#[test]
fn custom_nvme_score_applies_penalty_and_critical_caps() {
    let mut page = [0u8; 512];
    page[5] = 3;
    page[160] = 1;
    let score =
        |data: &[u8]| HealthSnapshot::from_nvme(parse_nvme_health(data).unwrap()).health_score(MediaKind::Ssd).unwrap();
    assert_eq!((score(&page).percent, score(&page).status), (92, ScoreStatus::Caution));
    page[3] = 9;
    page[4] = 10;
    assert_eq!(score(&page).percent, 20);
    page[0] = 0x0c;
    assert_eq!(score(&page).percent, 10);
    page[5] = 110;
    assert_eq!(score(&page).percent, 0);
}

#[test]
fn ata_endurance_requires_valid_normalized_device_statistics() {
    let mut page = [0; 512];
    page[2] = 7;
    page[8] = 23;
    page[15] = 0xc0;
    assert_eq!(parse_ata_endurance_page(&page), None);
    page[15] = 0xe0;
    assert_eq!(parse_ata_endurance_page(&page), Some(23));
    assert_eq!(parse_ata_endurance_page(&page[..8]), None);
}

#[test]
fn invalid_remap_list_does_not_turn_into_a_full_scan() {
    let mut disk = FaultDisk::new(512);
    let cfg = WriteTestConfig::new(WriteTestKind::Remap { lbas: vec![u64::MAX], refresh_slower_than_us: None });
    let out = run_write_test(&mut disk, &cfg, &AtomicBool::new(false), &mut |_| {});
    assert!(!out.completed);
    assert!(out.repair.unwrap().aborted.is_some());
    assert_eq!((disk.reads, disk.writes), (0, 0));
}

#[test]
fn saved_reports_are_complete_and_do_not_replace_existing_files() {
    use drivetest_core::demo::sample_drive;
    use drivetest_core::policy::{Mode, Tier};
    use drivetest_core::report::Report;
    let dir = std::env::temp_dir().join(format!("drivetest-report-{}", drivetest_core::signature::fresh_seed()));
    let report = Report::new("Demo", &sample_drive(), "read scan", Tier::ReadOnly, Mode::ReadOnly);
    let path = dir.join(report.default_file_name());
    report.save(&path).unwrap();
    assert_eq!(Report::load(&path).unwrap(), report);
    assert_eq!(report.save(&path).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}
