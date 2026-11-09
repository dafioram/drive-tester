//! `WinPlatform`: the real-hardware implementation of `Platform`.

use std::any::Any;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use drivetest_core::health::HealthSnapshot;
use drivetest_core::model::{
    BusType, DriveInfo, HealthSource, MediaKind, VolumeInfo, describe_adapter, normalize_serial,
};
use drivetest_core::platform::{Platform, PlatformError, RawReader, RawWriter, SectorFile, SelfTestKind};
use drivetest_core::policy::WriteAuthorization;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_READ, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH, FILE_NAME_NORMALIZED,
    FILE_SHARE_READ, GetFinalPathNameByHandleW, SetFileAttributesW, VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
use windows_sys::Win32::UI::Shell::IsUserAnAdmin;

use crate::handle::{disk_path, open};
use crate::rawdisk::WinDisk;
use crate::volumes::{VolumeRecord, free_bytes, list_volumes, lock_and_dismount, volume_of_path};
use crate::{devtree, enumerate, filemap, health, sdcard, system};

/// Highest `PhysicalDriveN` probed. Numbers can have gaps, so we do not stop early.
const MAX_DISKS: u32 = 64;

/// Real Windows hardware.
#[derive(Debug, Default)]
pub struct WinPlatform;

impl WinPlatform {
    pub fn new() -> Self {
        Self
    }

    fn bus_of(&self, disk: u32) -> BusType {
        open(&disk_path(disk), 0, 0)
            .ok()
            .and_then(|h| enumerate::descriptor(&h).ok())
            .map(|d| d.bus)
            .unwrap_or(BusType::Unknown)
    }

    /// Re-reads one drive from scratch (used before any write).
    fn fresh(&self, disk: u32) -> io::Result<DriveInfo> {
        let drives = self.enumerate().map_err(|e| io::Error::other(e.to_string()))?;
        drives
            .into_iter()
            .find(|d| d.number == disk)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "drive no longer present"))
    }

    /// Defence in depth: refuse unless the drive is still a data drive with
    /// the serial the user confirmed, independent of the policy's own checks.
    fn recheck(&self, auth: &WriteAuthorization) -> io::Result<DriveInfo> {
        let d = self.fresh(auth.disk_number())?;
        if d.is_protected() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("refused: {}", d.role.describe())));
        }
        if normalize_serial(&d.serial) != normalize_serial(auth.serial()) || normalize_serial(&d.serial).is_empty() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused: the drive's serial number changed"));
        }
        if d.size_bytes != auth.size_bytes() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused: the drive's capacity changed"));
        }
        if d.write_protected {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused: the drive is write-protected"));
        }
        if let Some(v) = d.volumes.iter().find(|v| !v.other_disks.is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refused: volume {} spans several disks", v.display_name()),
            ));
        }
        Ok(d)
    }
}

fn health_source_for(bus: BusType) -> HealthSource {
    match bus {
        BusType::Nvme => HealthSource::NvmeLog,
        BusType::Sata | BusType::Ata | BusType::Raid | BusType::Scsi | BusType::Sas => HealthSource::AtaSmart,
        BusType::Usb => HealthSource::SatSmart,
        _ => HealthSource::None,
    }
}

struct AwakeGuard;

impl Drop for AwakeGuard {
    fn drop(&mut self) {
        // SAFETY: plain flag call on the current thread.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

impl Platform for WinPlatform {
    fn name(&self) -> &'static str {
        "Windows"
    }

    fn enumerate(&self) -> Result<Vec<DriveInfo>, PlatformError> {
        let listed = list_volumes();
        let volumes: Vec<VolumeRecord> = listed.as_ref().map(|v| v.clone()).unwrap_or_default();
        let mut drives = Vec::new();
        let mut denied = false;
        for n in 0..MAX_DISKS {
            let path = disk_path(n);
            let h = match open(&path, GENERIC_READ, 0) {
                Ok(h) => h,
                Err(e) => {
                    match e.raw_os_error().map(|c| c as u32) {
                        Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND) => {}
                        Some(ERROR_ACCESS_DENIED) => denied = true,
                        _ => {}
                    }
                    continue;
                }
            };
            let Ok(desc) = enumerate::descriptor(&h) else { continue };
            let size = enumerate::length(&h).unwrap_or(0);
            let (logical, physical) = enumerate::alignment(&h)
                .or_else(|| enumerate::geometry_sector(&h).map(|s| (s, s)))
                .unwrap_or((512, 512));
            let media = match enumerate::seek_penalty(&h) {
                Some(true) => MediaKind::Hdd,
                Some(false) => MediaKind::Ssd,
                None => MediaKind::Unknown,
            };
            drives.push(DriveInfo {
                number: n,
                device_path: path,
                vendor: desc.vendor,
                model: desc.product,
                serial: desc.serial,
                firmware: desc.revision,
                bus: desc.bus,
                media,
                size_bytes: size,
                logical_sector_size: logical,
                physical_sector_size: physical,
                removable: desc.removable,
                write_protected: enumerate::write_protected(&h),
                role: drivetest_core::model::SystemRole::Data,
                volumes: volumes.iter().filter(|v| v.on_disk(n)).map(|v| v.to_info(n)).collect(),
                health_source: health_source_for(desc.bus),
                adapter: None,
                device_ids: Vec::new(),
            });
        }
        let chains = devtree::device_id_chains();
        for d in &mut drives {
            if let Some(ids) = chains.get(&d.number) {
                d.adapter = describe_adapter(ids);
                d.device_ids = ids.clone();
            }
        }
        if drives.is_empty() && denied {
            return Err(PlatformError::AccessDenied("raw disk access needs administrator rights".into()));
        }
        let buses: HashMap<u32, BusType> = drives.iter().map(|d| (d.number, d.bus)).collect();
        let roles = system::detect(listed.as_ref().ok().map(|v| v.as_slice()), &buses);
        for d in &mut drives {
            d.role = roles.role(d.number);
        }
        Ok(drives)
    }

    fn open_read(&self, disk: u32) -> io::Result<Box<dyn RawReader>> {
        Ok(Box::new(WinDisk::open_read(disk)?))
    }

    fn open_write(&self, auth: &WriteAuthorization) -> io::Result<Box<dyn RawWriter>> {
        self.recheck(auth)?;
        Ok(Box::new(WinDisk::open_write_verified(auth)?))
    }

    fn lock_volumes(&self, auth: &WriteAuthorization) -> io::Result<Box<dyn Any + Send>> {
        self.recheck(auth)?;
        Ok(Box::new(lock_and_dismount(auth.disk_number())?))
    }

    fn read_health(&self, disk: u32) -> Result<HealthSnapshot, PlatformError> {
        let bus = self.bus_of(disk);
        let mut snap = health::read(disk, bus)?;
        if matches!(bus, BusType::Sd | BusType::Mmc) {
            let size =
                open(&disk_path(disk), GENERIC_READ, 0).ok().and_then(|h| enumerate::length(&h).ok()).unwrap_or(0);
            let ids = devtree::device_id_chains().remove(&disk).unwrap_or_default();
            snap.sd = Some(sdcard::read(disk, size, &ids));
        }
        Ok(snap)
    }

    fn start_self_test(&self, disk: u32, kind: SelfTestKind) -> Result<(), PlatformError> {
        health::start_self_test(disk, self.bus_of(disk), kind)
    }

    fn keep_awake(&self) -> Box<dyn Any> {
        // SAFETY: plain flag call on the current (worker) thread.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
        Box::new(AwakeGuard)
    }

    fn volume_free_bytes(&self, root: &Path) -> io::Result<u64> {
        free_bytes(root)
    }

    fn volume_root(&self, volume: &VolumeInfo) -> PathBuf {
        // `\\?\Volume{GUID}\` always names this exact volume, unlike a drive
        // letter or folder mount point, which can be reassigned or fall through.
        PathBuf::from(&volume.id)
    }

    fn path_is_on_volume(&self, dir: &Path, volume_id: &str) -> io::Result<bool> {
        let found = volume_of_path(&dir.display().to_string())?;
        Ok(found.eq_ignore_ascii_case(volume_id))
    }

    fn open_test_file(&self, path: &Path, write: bool) -> io::Result<File> {
        let mut options = std::fs::OpenOptions::new();
        if write {
            options.write(true).create_new(true);
        } else {
            options.read(true);
        }
        options.custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH);
        options.open(path)
    }

    fn open_unbuffered_read(&self, path: &Path) -> io::Result<File> {
        std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_NO_BUFFERING).open(path)
    }

    fn open_for_refresh(&self, path: &Path) -> io::Result<File> {
        // Other processes may keep reading, but nobody else may write while we
        // rewrite the file; writes go straight through to the drive.
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ)
            // Open a link itself rather than its target, so links are seen and skipped.
            .custom_flags(FILE_FLAG_WRITE_THROUGH | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    fn resolve_on_volume(&self, file: &File, _path: &Path, _root: &Path, volume_id: &str) -> io::Result<PathBuf> {
        // The real path of the opened handle, expressed on its volume GUID:
        // resolves junctions, links and 8.3 short names.
        let mut buf = vec![0u16; 32_768];
        // SAFETY: valid handle and buffer.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle() as HANDLE,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
            )
        };
        if n == 0 || n as usize >= buf.len() {
            return Err(io::Error::last_os_error());
        }
        let real = String::from_utf16_lossy(&buf[..n as usize]);
        let prefix = volume_id.trim_end_matches('\\');
        match real.get(..prefix.len()) {
            Some(head) if head.eq_ignore_ascii_case(prefix) => {
                Ok(PathBuf::from(real[prefix.len()..].trim_start_matches('\\')))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("file resolves to {real}, outside the volume"),
            )),
        }
    }

    fn restore_attributes(&self, path: &Path, attributes: u32) -> io::Result<()> {
        // Only attributes that SetFileAttributesW accepts.
        const SETTABLE: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x100 | 0x2000;
        let mut a = attributes & SETTABLE;
        if a == 0 {
            a = 0x80; // FILE_ATTRIBUTE_NORMAL
        }
        let w = crate::handle::wide(&path.display().to_string());
        // SAFETY: null-terminated path.
        if unsafe { SetFileAttributesW(w.as_ptr(), a) } == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn map_sectors_to_files(&self, disk: u32, lbas: &[u64]) -> Result<Vec<SectorFile>, PlatformError> {
        let sector = open(&disk_path(disk), GENERIC_READ, 0)
            .ok()
            .and_then(|h| enumerate::alignment(&h).map(|(l, _)| u64::from(l)))
            .unwrap_or(512);
        let volumes = list_volumes().map_err(PlatformError::Io)?;
        Ok(filemap::map(disk, lbas, sector, &volumes))
    }

    fn is_elevated(&self) -> bool {
        // SAFETY: no arguments.
        unsafe { IsUserAnAdmin() != 0 }
    }
}
