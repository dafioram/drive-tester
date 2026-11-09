//! A simulated platform with four drives, used by `--demo` and the tests.
//!
//! The drives model realistic behaviour so every feature can be exercised
//! without hardware: a system NVMe SSD (write-locked), a SATA HDD with bad
//! sectors, a weak region and drive-managed SMR, a counterfeit USB stick that
//! wraps around after 1 GiB, and an SD card with an SLC-style write cache.
//!
//! Simulated drives report their modelled latency through
//! [`RawReader::simulated_latency`]; when `sleep` is on they also sleep for a
//! fraction of it so the GUI animates at a watchable pace.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::health::{
    AtaHealth, AtaIdentify, HealthSnapshot, NvmeHealth, SmartAttribute, ZonedCapability, ata_attribute_name,
    parse_self_test_status,
};
use crate::model::{BusType, DriveInfo, HealthSource, MediaKind, SystemRole, VolumeExtent, VolumeInfo};
use crate::platform::{Platform, PlatformError, RawReader, RawWriter, SectorFile, SelfTestKind};
use crate::policy::WriteAuthorization;
use crate::sd::{SdClaims, SdInfo, parse_cid};
use crate::signature::{MAGIC, SplitMix64, fill_signed};

const CHUNK: u64 = 64 << 10;
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// Demo time runs this many times faster than the modelled drive.
const SPEEDUP: u32 = 8;

/// How a simulated drive behaves.
#[derive(Debug, Clone)]
pub struct DemoProfile {
    pub size: u64,
    /// For fake-capacity media: what the drive really stores (wraps around).
    pub real_capacity: Option<u64>,
    pub sector: u32,
    pub media: MediaKind,
    pub read_mb_s_start: f64,
    pub read_mb_s_end: f64,
    pub write_mb_s: f64,
    /// SLC-style cache: (bytes at full speed, sustained MB/s afterwards).
    pub write_cache: Option<(u64, f64)>,
    pub seek_ms: f64,
    /// (start byte, end byte, slowdown factor).
    pub weak_region: Option<(u64, u64, f64)>,
    /// Byte offsets of spots that take extra milliseconds to read.
    pub slow_spots: Vec<(u64, f64)>,
    /// Unreadable LBAs (logical sectors).
    pub bad_sectors: Vec<u64>,
    /// Drive-managed SMR: random writes collapse after this many bytes.
    pub smr_cache: Option<u64>,
    /// Share of never-written space that reads as zeros (trimmed).
    pub trimmed_fraction: f64,
    /// Writing a bad sector makes the drive reallocate it (as real drives do).
    pub remap_on_write: bool,
}

impl DemoProfile {
    /// A small HDD with two bad sectors in the middle block.
    pub fn test_hdd(size: u64) -> Self {
        let mid_block_lba = (size / 2) / MIB * MIB / 512;
        Self {
            size,
            real_capacity: None,
            sector: 512,
            media: MediaKind::Hdd,
            read_mb_s_start: 200.0,
            read_mb_s_end: 100.0,
            write_mb_s: 150.0,
            write_cache: None,
            seek_ms: 8.0,
            weak_region: None,
            slow_spots: Vec::new(),
            bad_sectors: vec![mid_block_lba + 7, mid_block_lba + 8],
            smr_cache: None,
            trimmed_fraction: 0.0,
            remap_on_write: true,
        }
    }

    /// Flash media; `real` makes it a wraparound fake.
    pub fn test_flash(size: u64, real: Option<u64>) -> Self {
        Self {
            size,
            real_capacity: real,
            sector: 512,
            media: MediaKind::Ssd,
            read_mb_s_start: 80.0,
            read_mb_s_end: 80.0,
            write_mb_s: 40.0,
            write_cache: None,
            seek_ms: 0.0,
            weak_region: None,
            slow_spots: Vec::new(),
            bad_sectors: Vec::new(),
            smr_cache: None,
            trimmed_fraction: 0.5,
            remap_on_write: true,
        }
    }

    pub fn with_smr(mut self, cache: u64) -> Self {
        self.smr_cache = Some(cache);
        self
    }
}

#[derive(Debug, Clone)]
enum Chunk {
    Signed { seed: u64, first_unit: u64 },
    Raw(Vec<u8>),
}

type Store = Arc<Mutex<HashMap<u64, Chunk>>>;

/// State shared by every handle on one simulated drive.
#[derive(Default)]
pub struct DiskState {
    errors_seen: AtomicBool,
    /// Bad sectors the drive has reallocated after being overwritten.
    remapped: Mutex<HashSet<u64>>,
}

impl DiskState {
    fn remapped_count(&self) -> u64 {
        self.remapped.lock().map(|r| r.len() as u64).unwrap_or(0)
    }
}

/// One open handle on a simulated drive.
pub struct DemoDisk {
    profile: DemoProfile,
    store: Store,
    state: Arc<DiskState>,
    sleep: bool,
    last_latency: Duration,
    last_read_end: u64,
    last_write_end: u64,
    sequential_written: u64,
    random_written: u64,
}

impl DemoDisk {
    pub fn new(profile: DemoProfile, sleep: bool) -> Self {
        Self::with_store(profile, Arc::default(), Arc::default(), sleep)
    }

    fn with_store(profile: DemoProfile, store: Store, state: Arc<DiskState>, sleep: bool) -> Self {
        Self {
            profile,
            store,
            state,
            sleep,
            last_latency: Duration::ZERO,
            last_read_end: u64::MAX,
            last_write_end: u64::MAX,
            sequential_written: 0,
            random_written: 0,
        }
    }

    pub fn profile(&self) -> &DemoProfile {
        &self.profile
    }

    fn phys(&self, offset: u64) -> u64 {
        match self.profile.real_capacity {
            Some(real) if real > 0 => offset % real,
            _ => offset,
        }
    }

    fn check_io(&self, offset: u64, len: usize) -> io::Result<()> {
        let sector = u64::from(self.profile.sector);
        if !offset.is_multiple_of(sector) || !(len as u64).is_multiple_of(sector) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unaligned I/O"));
        }
        if offset + len as u64 > self.profile.size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "I/O past end of device"));
        }
        Ok(())
    }

    fn default_content(&self, phys_chunk: u64, out: &mut [u8], _within: u64) -> bool {
        let h = SplitMix64::new(phys_chunk ^ 0xC0FFEE).next_u64();
        if (h % 1000) as f64 / 1000.0 < self.profile.trimmed_fraction {
            out.fill(0);
            return true;
        }
        // Cheap non-zero stand-in for existing user data.
        out.fill((h % 255) as u8 + 1);
        false
    }

    fn materialize(chunk: &Chunk) -> Vec<u8> {
        match chunk {
            Chunk::Signed { seed, first_unit } => {
                let mut v = vec![0u8; CHUNK as usize];
                fill_signed(&mut v, *seed, *first_unit);
                v
            }
            Chunk::Raw(v) => v.clone(),
        }
    }

    fn finish(&mut self, secs: f64) {
        self.last_latency = Duration::from_secs_f64(secs.max(0.0));
        if self.sleep {
            let nap = (self.last_latency / SPEEDUP).min(Duration::from_millis(60));
            if nap >= Duration::from_micros(200) {
                std::thread::sleep(nap);
            }
        }
    }

    fn read_secs(&self, offset: u64, len: u64, zero_only: bool) -> f64 {
        let p = &self.profile;
        if zero_only && p.media == MediaKind::Ssd {
            return 0.000_04 + len as f64 / 5e9;
        }
        let frac = offset as f64 / p.size.max(1) as f64;
        let speed = p.read_mb_s_start + (p.read_mb_s_end - p.read_mb_s_start) * frac;
        let mut secs = len as f64 / 1e6 / speed;
        if p.media == MediaKind::Hdd && offset != self.last_read_end {
            let distance = if self.last_read_end == u64::MAX {
                0.5
            } else {
                offset.abs_diff(self.last_read_end) as f64 / p.size.max(1) as f64
            };
            secs += p.seek_ms / 1000.0 * (0.3 + 0.7 * distance);
        }
        if let Some((start, end, factor)) = p.weak_region
            && offset < end
            && offset + len > start
        {
            secs *= factor;
        }
        for &(spot, ms) in &p.slow_spots {
            if spot >= offset && spot < offset + len {
                secs += ms / 1000.0;
            }
        }
        secs
    }

    fn write_secs(&mut self, offset: u64, len: u64) -> f64 {
        let p = &self.profile;
        let sequential = offset == self.last_write_end;
        let mut speed = p.write_mb_s;
        if let Some((cache, slow)) = p.write_cache
            && self.sequential_written > cache
        {
            speed = slow;
        }
        let mut secs = len as f64 / 1e6 / speed;
        if p.media == MediaKind::Hdd && !sequential {
            secs += p.seek_ms / 1000.0;
            self.random_written += len;
            if let Some(cache) = p.smr_cache
                && self.random_written > cache
            {
                secs *= 20.0;
            }
        }
        self.sequential_written += len;
        secs
    }
}

impl RawReader for DemoDisk {
    fn size_bytes(&self) -> u64 {
        self.profile.size
    }

    fn logical_sector_size(&self) -> u32 {
        self.profile.sector
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.check_io(offset, buf.len())?;
        let len = buf.len() as u64;
        let sector = u64::from(self.profile.sector);
        let (first, last) = (offset / sector, (offset + len - 1) / sector);
        let unreadable = {
            let remapped = self.state.remapped.lock().expect("demo state");
            self.profile.bad_sectors.iter().any(|&b| b >= first && b <= last && !remapped.contains(&b))
        };
        if unreadable {
            self.state.errors_seen.store(true, Ordering::Relaxed);
            let secs = self.read_secs(offset, len, false) + 0.35;
            self.last_read_end = offset + len;
            self.finish(secs);
            return Err(io::Error::other("simulated media error (uncorrectable read)"));
        }
        let mut zero_only = true;
        {
            let store = self.store.lock().expect("demo store");
            let mut pos = 0u64;
            while pos < len {
                let phys = self.phys(offset + pos);
                let chunk = phys / CHUNK;
                let within = phys % CHUNK;
                let n = (CHUNK - within).min(len - pos);
                let out = &mut buf[pos as usize..(pos + n) as usize];
                match store.get(&chunk) {
                    Some(c) => {
                        let data = Self::materialize(c);
                        out.copy_from_slice(&data[within as usize..(within + n) as usize]);
                        zero_only = false;
                    }
                    None => {
                        if !self.default_content(chunk, out, within) {
                            zero_only = false;
                        }
                    }
                }
                pos += n;
            }
        }
        let secs = self.read_secs(offset, len, zero_only);
        self.last_read_end = offset + len;
        self.finish(secs);
        Ok(())
    }

    fn simulated_latency(&self) -> Option<Duration> {
        Some(self.last_latency)
    }
}

impl RawWriter for DemoDisk {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.check_io(offset, buf.len())?;
        let len = buf.len() as u64;
        if self.profile.remap_on_write {
            let sector = u64::from(self.profile.sector);
            let (first, last) = (offset / sector, (offset + len - 1) / sector);
            let mut remapped = self.state.remapped.lock().expect("demo state");
            for &b in self.profile.bad_sectors.iter().filter(|&&b| b >= first && b <= last) {
                remapped.insert(b);
            }
        }
        {
            let mut store = self.store.lock().expect("demo store");
            let mut pos = 0u64;
            while pos < len {
                let phys = self.phys(offset + pos);
                let chunk = phys / CHUNK;
                let within = phys % CHUNK;
                let n = (CHUNK - within).min(len - pos);
                let piece = &buf[pos as usize..(pos + n) as usize];
                let stored = if within == 0 && n == CHUNK && piece[0..8] == MAGIC {
                    let seed = u64::from_le_bytes(piece[8..16].try_into().expect("8 bytes"));
                    let unit = u64::from_le_bytes(piece[16..24].try_into().expect("8 bytes"));
                    let mut expect = vec![0u8; CHUNK as usize];
                    fill_signed(&mut expect, seed, unit);
                    if expect == piece { Chunk::Signed { seed, first_unit: unit } } else { Chunk::Raw(piece.to_vec()) }
                } else {
                    let mut data = match store.get(&chunk) {
                        Some(c) => Self::materialize(c),
                        None => {
                            let mut v = vec![0u8; CHUNK as usize];
                            self.default_content(chunk, &mut v, 0);
                            v
                        }
                    };
                    data[within as usize..(within + n) as usize].copy_from_slice(piece);
                    Chunk::Raw(data)
                };
                store.insert(chunk, stored);
                pos += n;
            }
        }
        let secs = self.write_secs(offset, len);
        self.last_write_end = offset + len;
        self.finish(secs);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- platform

struct DemoDrive {
    info: DriveInfo,
    profile: DemoProfile,
    store: Store,
    state: Arc<DiskState>,
}

/// The simulated platform.
pub struct DemoPlatform {
    drives: Vec<DemoDrive>,
    sleep: bool,
    self_tests: Mutex<HashMap<u32, (Instant, SelfTestKind)>>,
    root: PathBuf,
    /// Volume folders already filled with sample files.
    populated: Mutex<HashSet<String>>,
    /// Files rewritten by a refresh (they read fast afterwards).
    refreshed: Mutex<HashSet<PathBuf>>,
}

/// Sample files per demo volume: (relative path, age in days).
fn sample_files(letter: &str) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = Vec::new();
    let sep = std::path::MAIN_SEPARATOR;
    let ages: &[u64] = match letter {
        "C" | "E" => &[
            2, 5, 9, 14, 20, 26, 40, 75, 110, 150, 210, 260, 300, 340, 400, 480, 560, 650, 800, 950, 1100, 1300, 1500,
            1800,
        ],
        _ => &[3, 30, 200, 500, 900, 1200],
    };
    for (i, age) in ages.iter().enumerate() {
        let folder = match i % 3 {
            0 => format!("Users{sep}demo{sep}Videos"),
            1 => format!("Users{sep}demo{sep}Pictures"),
            _ => format!("Archive{sep}{}", 2026 - (*age as i64 / 365)),
        };
        v.push((format!("{folder}{sep}file-{i:02}.bin"), *age));
    }
    if letter == "C" {
        v.push((format!("Windows{sep}System32{sep}demo-driver.sys"), 1400));
        v.push((format!("Program Files{sep}DemoApp{sep}app-data.bin"), 900));
    }
    v
}

fn volume(letter: char, label: &str, size: u64, offset: u64) -> VolumeInfo {
    VolumeInfo {
        id: format!("\\\\?\\Volume{{demo-{letter}}}\\"),
        mount_points: vec![format!("{letter}:\\")],
        label: label.into(),
        file_system: if size > 32 * GIB { "NTFS".into() } else { "exFAT".into() },
        size_bytes: size,
        free_bytes: size / 3,
        extents: vec![VolumeExtent { disk_offset: offset, length: size }],
        other_disks: Vec::new(),
    }
}

fn demo_drives() -> Vec<(DriveInfo, DemoProfile)> {
    let nvme = DemoProfile {
        size: 32 * GIB,
        real_capacity: None,
        sector: 512,
        media: MediaKind::Ssd,
        read_mb_s_start: 2400.0,
        read_mb_s_end: 2400.0,
        write_mb_s: 1800.0,
        write_cache: Some((8 * GIB, 600.0)),
        seek_ms: 0.0,
        weak_region: None,
        slow_spots: vec![(9 * GIB, 40.0), (21 * GIB, 25.0)],
        bad_sectors: Vec::new(),
        smr_cache: None,
        trimmed_fraction: 0.55,
        remap_on_write: true,
    };
    let hdd_size = 8 * GIB;
    let bad_block = (hdd_size as f64 * 0.71) as u64 / MIB * MIB / 512;
    let hdd = DemoProfile {
        size: hdd_size,
        real_capacity: None,
        sector: 512,
        media: MediaKind::Hdd,
        read_mb_s_start: 210.0,
        read_mb_s_end: 105.0,
        write_mb_s: 180.0,
        write_cache: None,
        seek_ms: 9.0,
        weak_region: Some(((hdd_size as f64 * 0.40) as u64, (hdd_size as f64 * 0.42) as u64, 3.2)),
        slow_spots: vec![
            ((hdd_size as f64 * 0.18) as u64, 230.0),
            ((hdd_size as f64 * 0.555) as u64, 180.0),
            ((hdd_size as f64 * 0.89) as u64, 620.0),
        ],
        bad_sectors: vec![bad_block + 3, bad_block + 4, bad_block + 5, bad_block + 900],
        smr_cache: Some(GIB),
        trimmed_fraction: 0.0,
        remap_on_write: true,
    };
    let usb = DemoProfile {
        size: 4 * GIB,
        real_capacity: Some(GIB),
        sector: 512,
        media: MediaKind::Ssd,
        read_mb_s_start: 32.0,
        read_mb_s_end: 32.0,
        write_mb_s: 14.0,
        write_cache: None,
        seek_ms: 0.0,
        weak_region: None,
        slow_spots: Vec::new(),
        bad_sectors: Vec::new(),
        smr_cache: None,
        trimmed_fraction: 0.7,
        remap_on_write: true,
    };
    let sd = DemoProfile {
        size: 2 * GIB,
        real_capacity: None,
        sector: 512,
        media: MediaKind::Ssd,
        read_mb_s_start: 85.0,
        read_mb_s_end: 85.0,
        write_mb_s: 60.0,
        write_cache: Some((512 * MIB, 15.0)),
        seek_ms: 0.0,
        weak_region: None,
        slow_spots: Vec::new(),
        bad_sectors: Vec::new(),
        smr_cache: None,
        trimmed_fraction: 0.6,
        remap_on_write: true,
    };
    let base =
        |number: u32, vendor: &str, model: &str, serial: &str, bus: BusType, media: MediaKind, size: u64| DriveInfo {
            number,
            device_path: format!("\\\\.\\PhysicalDrive{number}"),
            vendor: vendor.into(),
            model: model.into(),
            serial: serial.into(),
            firmware: "DEMO1.0".into(),
            bus,
            media,
            size_bytes: size,
            logical_sector_size: 512,
            physical_sector_size: if media == MediaKind::Hdd { 4096 } else { 512 },
            removable: bus.is_removable_flash_bus(),
            write_protected: false,
            role: SystemRole::Data,
            volumes: Vec::new(),
            health_source: match bus {
                BusType::Nvme => HealthSource::NvmeLog,
                BusType::Sata => HealthSource::AtaSmart,
                _ => HealthSource::None,
            },
            adapter: match bus {
                BusType::Usb => Some("USB bridge VID_090C PID_1000 (Silicon Motion)".into()),
                BusType::Sd => Some("SD host (native slot)".into()),
                _ => None,
            },
            device_ids: Vec::new(),
        };
    let mut d0 = base(0, "", "Demo NVMe SSD 32G", "DEMO-NVME-0001", BusType::Nvme, MediaKind::Ssd, nvme.size);
    d0.role = SystemRole::System {
        reasons: vec!["holds the Windows volume (C:)".into(), "holds the EFI system partition".into()],
    };
    d0.volumes = vec![volume('C', "Windows", nvme.size - 300 * MIB, 300 * MIB)];
    let mut d1 = base(1, "", "DEMO HDD8000-SMR", "DEMO-HDD-1234", BusType::Sata, MediaKind::Hdd, hdd.size);
    d1.volumes = vec![volume('E', "Archive", hdd.size - MIB, MIB)];
    let mut d2 = base(2, "Generic", "Flash Disk 4G (demo)", "DEMO-USB-FAKE", BusType::Usb, MediaKind::Ssd, usb.size);
    d2.volumes = vec![volume('F', "USB", usb.size - MIB, MIB)];
    let mut d3 = base(3, "", "Demo SD Card 2G", "DEMO-SD-77", BusType::Sd, MediaKind::Ssd, sd.size);
    d3.volumes = vec![volume('G', "SDCARD", sd.size - 4 * MIB, 4 * MIB)];
    vec![(d0, nvme), (d1, hdd), (d2, usb), (d3, sd)]
}

/// The demo HDD's description, for tests.
pub fn sample_drive() -> DriveInfo {
    demo_drives().into_iter().nth(1).expect("demo hdd").0
}

impl DemoPlatform {
    /// Demo with animated (slept) I/O for the GUI and CLI.
    pub fn new() -> Self {
        Self::build(true)
    }

    /// Demo without sleeping, for tests.
    pub fn new_fast() -> Self {
        Self::build(false)
    }

    fn build(sleep: bool) -> Self {
        static INSTANCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let instance = INSTANCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("drivetest-demo-{}-{instance}", std::process::id()));
        let drives = demo_drives()
            .into_iter()
            .map(|(info, profile)| DemoDrive { info, profile, store: Arc::default(), state: Arc::default() })
            .collect();
        Self {
            drives,
            sleep,
            self_tests: Mutex::new(HashMap::new()),
            root,
            populated: Mutex::new(HashSet::new()),
            refreshed: Mutex::new(HashSet::new()),
        }
    }

    fn drive(&self, disk: u32) -> io::Result<&DemoDrive> {
        self.drives
            .iter()
            .find(|d| d.info.number == disk)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no demo disk {disk}")))
    }

    fn open(&self, disk: u32) -> io::Result<DemoDisk> {
        let d = self.drive(disk)?;
        Ok(DemoDisk::with_store(d.profile.clone(), d.store.clone(), d.state.clone(), self.sleep))
    }

    /// Which drive a demo volume folder belongs to.
    fn drive_for_path(&self, path: &Path) -> Option<&DemoDrive> {
        self.drives.iter().find(|d| {
            d.info.volumes.iter().any(|v| {
                let letter: String = v
                    .mount_points
                    .first()
                    .map(|m| m.chars().filter(|c| c.is_ascii_alphanumeric()).collect())
                    .unwrap_or_default();
                path.starts_with(self.root.join(format!("volume-{letter}")))
            })
        })
    }

    fn populate(&self, letter: &str, dir: &Path) {
        let mut done = self.populated.lock().expect("populated");
        if !done.insert(letter.to_string()) {
            return;
        }
        let now = std::time::SystemTime::now();
        for (rel, age) in sample_files(letter) {
            let path = dir.join(&rel);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Sparse 4 MiB files: no real disk space used until written.
            if let Ok(f) = std::fs::File::create(&path) {
                let _ = f.set_len(4 << 20);
                let _ = f.set_modified(now - Duration::from_secs(age * 86_400));
            }
        }
    }

    fn nvme_self_test(&self, disk: u32) -> crate::health::SelfTestStatus {
        let mut log = vec![0u8; 564];
        log[4] = 0xFF; // no test run yet
        if let Some((start, kind)) = self.self_tests.lock().expect("self tests").get(&disk) {
            let total = match kind {
                SelfTestKind::Short => 20.0,
                SelfTestKind::Extended => 60.0,
            };
            let done = start.elapsed().as_secs_f64() / total;
            let code = if *kind == SelfTestKind::Short { 1 } else { 2 };
            if done < 1.0 {
                log[0] = code;
                log[1] = (done * 100.0) as u8;
            } else {
                log[4] = code << 4;
            }
        }
        crate::health::parse_nvme_self_test_log(&log).expect("valid log")
    }

    fn self_test_byte(&self, disk: u32) -> u8 {
        let tests = self.self_tests.lock().expect("self tests");
        match tests.get(&disk) {
            None => 0x00,
            Some((start, kind)) => {
                let total = match kind {
                    SelfTestKind::Short => 20.0,
                    SelfTestKind::Extended => 60.0,
                };
                let done = start.elapsed().as_secs_f64() / total;
                if done >= 1.0 {
                    0x00
                } else {
                    let remaining_tenths = (((1.0 - done) * 10.0).ceil() as u8).min(9);
                    0xF0 | remaining_tenths
                }
            }
        }
    }
}

impl Default for DemoPlatform {
    fn default() -> Self {
        Self::new()
    }
}

fn attr(id: u8, current: u8, worst: u8, threshold: u8, raw: u64) -> SmartAttribute {
    SmartAttribute {
        id,
        name: ata_attribute_name(id).into(),
        flags: 0x0032,
        current,
        worst,
        threshold,
        raw,
        failing_now: threshold > 0 && current <= threshold,
    }
}

impl Platform for DemoPlatform {
    fn name(&self) -> &'static str {
        "Demo"
    }

    fn enumerate(&self) -> Result<Vec<DriveInfo>, PlatformError> {
        Ok(self.drives.iter().map(|d| d.info.clone()).collect())
    }

    fn open_read(&self, disk: u32) -> io::Result<Box<dyn RawReader>> {
        Ok(Box::new(self.open(disk)?))
    }

    fn open_write(&self, auth: &WriteAuthorization) -> io::Result<Box<dyn RawWriter>> {
        let d = self.drive(auth.disk_number())?;
        // Defence in depth, mirroring the Windows layer.
        if d.info.is_protected() || !d.info.serial_matches(auth.serial()) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused: protected or changed drive"));
        }
        Ok(Box::new(self.open(auth.disk_number())?))
    }

    fn lock_volumes(&self, auth: &WriteAuthorization) -> io::Result<Box<dyn Any + Send>> {
        let d = self.drive(auth.disk_number())?;
        if d.info.is_protected() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused: system disk"));
        }
        Ok(Box::new(()))
    }

    fn read_health(&self, disk: u32) -> Result<HealthSnapshot, PlatformError> {
        let d = self.drive(disk)?;
        Ok(match d.info.bus {
            BusType::Nvme => {
                let mut snap = HealthSnapshot::from_nvme(NvmeHealth {
                    critical_warning: 0,
                    temperature_c: 41,
                    available_spare_percent: 100,
                    available_spare_threshold_percent: 10,
                    percentage_used: 12,
                    data_units_read: 48_210_331,
                    data_units_written: 39_876_002,
                    host_read_commands: 912_331_002,
                    host_write_commands: 701_220_441,
                    power_cycles: 1_204,
                    power_on_hours: 3_120,
                    unsafe_shutdowns: 7,
                    media_errors: 0,
                    error_log_entries: 0,
                });
                snap.nvme_self_test = Some(self.nvme_self_test(disk));
                snap
            }
            BusType::Sata => {
                let reallocated = d.state.remapped_count();
                let pending = if reallocated > 0 {
                    0
                } else if d.state.errors_seen.load(Ordering::Relaxed) {
                    6
                } else {
                    2
                };
                let temp = 36 + (self.self_tests.lock().map(|t| t.len()).unwrap_or(0) as u64);
                HealthSnapshot::from_ata(
                    HealthSource::AtaSmart,
                    AtaHealth {
                        attributes: vec![
                            attr(1, 200, 200, 51, 0),
                            attr(3, 175, 172, 21, 4_200),
                            attr(4, 100, 100, 0, 1_534),
                            attr(5, 200, 200, 140, reallocated),
                            attr(9, 85, 85, 0, 11_234),
                            attr(12, 100, 100, 0, 1_530),
                            attr(193, 180, 180, 0, 61_500),
                            attr(194, 113, 98, 0, temp),
                            attr(197, 200, 200, 0, pending),
                            attr(198, 100, 253, 0, 0),
                            attr(199, 200, 200, 0, 0),
                        ],
                        self_test: Some(parse_self_test_status(self.self_test_byte(disk))),
                        identify: Some(AtaIdentify {
                            model: d.info.model.clone(),
                            serial: d.info.serial.clone(),
                            firmware: d.info.firmware.clone(),
                            rotation_rate_rpm: Some(5400),
                            non_rotating: false,
                            trim_supported: true,
                            zoned: ZonedCapability::NotReported,
                            smart_supported: true,
                            smart_enabled: true,
                        }),
                    },
                )
            }
            BusType::Usb => HealthSnapshot::unavailable("This USB bridge does not pass SMART commands (demo)."),
            _ => {
                let mut snap =
                    HealthSnapshot::unavailable("SD cards do not report SMART; card registers shown instead.");
                let mut sd = SdInfo::default();
                sd.apply_device_id("SD\\VID_03&OID_5344&PID_DEMO2&REV_8.0\\5&demo&0&0");
                let mut cid = [0u8; 16];
                cid[0] = 0x03;
                cid[1..3].copy_from_slice(b"SD");
                cid[3..8].copy_from_slice(b"DEMO2");
                cid[8] = 0x80;
                cid[9..13].copy_from_slice(&0x0D3E_7701u32.to_be_bytes());
                cid[13] = 0x01;
                cid[14] = 0x86; // 2024-06
                if let Some(c) = parse_cid(&cid) {
                    sd.apply_cid(&c);
                }
                sd.csd_capacity_bytes = Some(d.info.size_bytes);
                sd.claims = SdClaims {
                    speed_class: Some("C10".into()),
                    uhs_grade: Some("U1".into()),
                    video_class: Some("V10".into()),
                    app_class: Some("A1".into()),
                };
                sd.sources.push("CSD and SD Status registers".into());
                sd.finish(d.info.size_bytes);
                snap.sd = Some(sd);
                snap
            }
        })
    }

    fn start_self_test(&self, disk: u32, kind: SelfTestKind) -> Result<(), PlatformError> {
        let d = self.drive(disk)?;
        if !matches!(d.info.health_source, HealthSource::AtaSmart | HealthSource::NvmeLog) {
            return Err(PlatformError::Unsupported("this drive does not support self-tests".into()));
        }
        self.self_tests.lock().expect("self tests").insert(disk, (Instant::now(), kind));
        Ok(())
    }

    fn volume_free_bytes(&self, root: &Path) -> io::Result<u64> {
        std::fs::create_dir_all(root)?;
        Ok(512 * MIB)
    }

    fn volume_root(&self, volume: &VolumeInfo) -> PathBuf {
        let mount = volume.mount_points.first().cloned().unwrap_or_default();
        let letter: String = mount.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let dir = self.root.join(format!("volume-{letter}"));
        let _ = std::fs::create_dir_all(&dir);
        self.populate(&letter, &dir);
        dir
    }

    fn open_for_refresh(&self, path: &Path) -> io::Result<std::fs::File> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        self.refreshed.lock().expect("refreshed").insert(path.to_path_buf());
        Ok(f)
    }

    fn time_file_read(&self, path: &Path, max_bytes: u64) -> io::Result<(u64, Duration)> {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        while total < max_bytes {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        // Simulated speed: old data on flash reads slower until refreshed.
        let age_days = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
            .map_or(0, |d| d.as_secs() / 86_400);
        let jitter =
            0.9 + (SplitMix64::new(path.to_string_lossy().len() as u64 * 7919).next_u64() % 200) as f64 / 1000.0;
        let (media, base) = self
            .drive_for_path(path)
            .map_or((MediaKind::Ssd, 500.0), |d| (d.profile.media, d.profile.read_mb_s_start.min(2000.0)));
        let refreshed = self.refreshed.lock().expect("refreshed").contains(path);
        let factor = match media {
            MediaKind::Hdd => 0.55 + 0.45 * ((age_days * 37 % 100) as f64 / 100.0),
            _ if refreshed || age_days < 90 => 1.0,
            _ if age_days < 365 => 0.7,
            _ => 0.28,
        };
        let mb_s = base * factor * jitter;
        Ok((total, Duration::from_secs_f64(total as f64 / 1e6 / mb_s)))
    }

    fn map_sectors_to_files(&self, disk: u32, lbas: &[u64]) -> Result<Vec<SectorFile>, PlatformError> {
        let d = self.drive(disk)?;
        let mount = d.info.volumes.first().and_then(|v| v.mount_points.first()).cloned().unwrap_or_default();
        let files = ["Videos\\holiday-2019.mp4", "Backups\\photos-2021.zip", "Projects\\budget.xlsx"];
        Ok(lbas
            .iter()
            .map(|&lba| SectorFile {
                lba,
                location: if lba % 4 == 0 {
                    "Free space (no file)".into()
                } else {
                    format!("{mount}{} (demo)", files[(lba % 3) as usize])
                },
            })
            .collect())
    }
}

impl Drop for DemoPlatform {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraparound_overwrites_earlier_data() {
        let mut disk = DemoDisk::new(DemoProfile::test_flash(4 * MIB, Some(MIB)), false);
        let mut a = vec![0u8; MIB as usize];
        fill_signed(&mut a, 1, 0);
        disk.write_at(0, &a).unwrap();
        let mut b = vec![0u8; MIB as usize];
        fill_signed(&mut b, 1, 2 * MIB / 512);
        disk.write_at(2 * MIB, &b).unwrap();
        let mut back = vec![0u8; MIB as usize];
        disk.read_at(0, &mut back).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn partial_writes_round_trip() {
        let mut disk = DemoDisk::new(DemoProfile::test_flash(MIB, None), false);
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        disk.write_at(512 * 3, &data).unwrap();
        let mut back = vec![0u8; 4096];
        disk.read_at(512 * 3, &mut back).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn platform_lists_one_protected_drive() {
        let p = DemoPlatform::new_fast();
        let drives = p.enumerate().unwrap();
        assert_eq!(drives.len(), 4);
        assert_eq!(drives.iter().filter(|d| d.is_protected()).count(), 1);
    }
}
