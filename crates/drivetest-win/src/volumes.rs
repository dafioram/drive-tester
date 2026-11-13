//! Volumes: enumeration, extents, lock/dismount and free space.

use std::io;
use std::mem::{offset_of, size_of};
use std::path::Path;
use std::thread;
use std::time::Duration;

use drivetest_core::model::{VolumeExtent, VolumeInfo};
use windows_sys::Win32::Foundation::{
    ERROR_MORE_DATA, ERROR_NO_MORE_FILES, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, GetVolumePathNamesForVolumeNameW,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Ioctl::{
    DISK_EXTENT, FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, VOLUME_DISK_EXTENTS,
};

use crate::handle::{Handle, from_wide, i64_at, ioctl_none, ioctl_out, open, u32_at, wide};

const EXTENTS_AT: usize = offset_of!(VOLUME_DISK_EXTENTS, Extents);
const EXTENT_SIZE: usize = size_of::<DISK_EXTENT>();
const EXTENT_OFFSET_AT: usize = offset_of!(DISK_EXTENT, StartingOffset);
const EXTENT_LENGTH_AT: usize = offset_of!(DISK_EXTENT, ExtentLength);

/// One extent of a volume on a disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskExtent {
    pub disk: u32,
    pub offset: u64,
    pub length: u64,
}

/// A volume and every disk extent it occupies.
#[derive(Debug, Clone)]
pub struct VolumeRecord {
    pub guid_path: String,
    pub mount_points: Vec<String>,
    pub mount_points_known: bool,
    pub label: String,
    pub file_system: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
    pub extents: Vec<DiskExtent>,
    /// False when the volume's disk extents could not be read.
    pub extents_known: bool,
    /// GetDriveTypeW of the first mount point (0 when unmounted).
    pub drive_type: u32,
}

impl VolumeRecord {
    pub fn on_disk(&self, disk: u32) -> bool {
        self.extents.iter().any(|e| e.disk == disk)
    }

    pub fn to_info(&self, disk: u32) -> VolumeInfo {
        VolumeInfo {
            id: self.guid_path.clone(),
            mount_points: self.mount_points.clone(),
            label: self.label.clone(),
            file_system: self.file_system.clone(),
            size_bytes: self.size_bytes,
            free_bytes: self.free_bytes,
            extents: self
                .extents
                .iter()
                .filter(|e| e.disk == disk)
                .map(|e| VolumeExtent { disk_offset: e.offset, length: e.length })
                .collect(),
            other_disks: {
                let mut others: Vec<u32> = self.extents.iter().map(|e| e.disk).filter(|&d| d != disk).collect();
                others.sort_unstable();
                others.dedup();
                others
            },
        }
    }

    /// Device path without the trailing backslash (needed to open the volume).
    pub fn device_path(&self) -> &str {
        self.guid_path.trim_end_matches('\\')
    }
}

/// Extents for an openable volume path such as `\\.\C:` or a GLOBALROOT device.
pub fn extents_of(path: &str) -> io::Result<Vec<DiskExtent>> {
    let h = open(path, 0, 0)?;
    let mut out = vec![0u8; EXTENTS_AT + EXTENT_SIZE * 8];
    let returned = loop {
        match ioctl_out(&h, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &mut out) {
            Ok(n) => break n as usize,
            Err(e) if e.raw_os_error() == Some(ERROR_MORE_DATA as i32) => {
                let count = u32_at(&out, 0) as usize;
                let required = EXTENTS_AT + EXTENT_SIZE * count;
                if required <= out.len() || count > 65_536 {
                    return Err(io::Error::other("Invalid volume extent count"));
                }
                out.resize(required, 0);
            }
            Err(e) => return Err(e),
        }
    };
    parse_extents(&out[..returned.min(out.len())])
}

fn parse_extents(out: &[u8]) -> io::Result<Vec<DiskExtent>> {
    if out.len() < EXTENTS_AT {
        return Err(io::Error::other("Truncated volume extent header"));
    }
    let count = u32_at(out, 0) as usize;
    if count == 0 || count > (out.len() - EXTENTS_AT) / EXTENT_SIZE {
        return Err(io::Error::other("Incomplete volume extent data"));
    }
    (0..count)
        .map(|i| {
            let base = EXTENTS_AT + i * EXTENT_SIZE;
            let offset = i64_at(out, base + EXTENT_OFFSET_AT);
            let length = i64_at(out, base + EXTENT_LENGTH_AT);
            if offset < 0 || length <= 0 {
                return Err(io::Error::other("Invalid volume extent bounds"));
            }
            Ok(DiskExtent { disk: u32_at(out, base), offset: offset as u64, length: length as u64 })
        })
        .collect()
}

fn mount_points(guid: &str) -> io::Result<Vec<String>> {
    let w = wide(guid);
    let mut buf = vec![0u16; 512];
    loop {
        let mut needed = 0u32;
        let ok =
            unsafe { GetVolumePathNamesForVolumeNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32, &mut needed) };
        if ok != 0 {
            return Ok(buf.split(|&c| c == 0).take_while(|s| !s.is_empty()).map(String::from_utf16_lossy).collect());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_MORE_DATA as i32) || needed as usize <= buf.len() {
            return Err(error);
        }
        buf.resize(needed as usize, 0);
    }
}

fn label_and_fs(guid: &str) -> (String, String) {
    let w = wide(guid);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    // SAFETY: buffers are valid for the lengths passed; optional outputs are null.
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 { (String::new(), String::new()) } else { (from_wide(&label), from_wide(&fs)) }
}

/// (total, free) bytes of a mounted volume root.
pub fn space(root: &str) -> Option<(u64, u64)> {
    let w = wide(root);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: valid out-pointers.
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((total, avail))
}

pub fn free_bytes(root: &Path) -> io::Result<u64> {
    let mut s = root.display().to_string();
    if !s.ends_with('\\') {
        s.push('\\');
    }
    space(&s).map(|(_, free)| free).ok_or_else(io::Error::last_os_error)
}

/// Bytes per cluster of a mounted volume.
pub fn cluster_size(root: &str) -> Option<u64> {
    let w = wide(root);
    let (mut spc, mut bps, mut freec, mut totalc) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: valid out-pointers.
    let ok = unsafe { GetDiskFreeSpaceW(w.as_ptr(), &mut spc, &mut bps, &mut freec, &mut totalc) };
    (ok != 0 && spc > 0 && bps > 0).then(|| u64::from(spc) * u64::from(bps))
}

const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

fn drive_type(mount: &str) -> u32 {
    let w = wide(mount);
    // SAFETY: null-terminated path.
    unsafe { GetDriveTypeW(w.as_ptr()) }
}

impl VolumeRecord {
    /// Optical, network and RAM-disk volumes never sit on a physical disk we test.
    pub fn is_irrelevant(&self) -> bool {
        matches!(self.drive_type, DRIVE_REMOTE | DRIVE_CDROM | DRIVE_RAMDISK)
    }

    /// A volume whose location is unknown, so it might be on any disk.
    pub fn location_unknown(&self) -> bool {
        !self.extents_known && !self.is_irrelevant()
    }
}

/// Every volume on the system with its disk extents.
pub fn list_volumes() -> io::Result<Vec<VolumeRecord>> {
    let mut records = Vec::new();
    let mut name = [0u16; 512];
    // SAFETY: `name` is valid for its length.
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE || find.is_null() {
        return Err(io::Error::last_os_error());
    }
    let result = loop {
        let guid = from_wide(&name);
        let mounts = mount_points(&guid);
        let mounts_known = mounts.is_ok();
        let mounts = mounts.unwrap_or_default();
        let (label, file_system) = label_and_fs(&guid);
        let (size_bytes, free_bytes) = space(&guid).unwrap_or((0, 0));
        let extents = extents_of(guid.trim_end_matches('\\'));
        let drive_type = drive_type(mounts.first().unwrap_or(&guid));
        records.push(VolumeRecord {
            guid_path: guid,
            mount_points: mounts,
            mount_points_known: mounts_known,
            label,
            file_system,
            size_bytes,
            free_bytes,
            extents_known: extents.is_ok(),
            extents: extents.unwrap_or_default(),
            drive_type,
        });
        // SAFETY: `find` is a valid search handle; `name` is valid.
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            let error = io::Error::last_os_error();
            break match error.raw_os_error() {
                Some(code) if code == ERROR_NO_MORE_FILES as i32 => Ok(records),
                _ => Err(error),
            };
        }
    };
    // SAFETY: closing the search handle we opened.
    unsafe { FindVolumeClose(find) };
    result
}

/// Volumes locked and dismounted for a destructive test. Unlocked on drop.
pub struct VolumeLocks {
    handles: Vec<Handle>,
}

impl Drop for VolumeLocks {
    fn drop(&mut self) {
        for h in &self.handles {
            let _ = ioctl_none(h, FSCTL_UNLOCK_VOLUME);
        }
    }
}

/// Locks and dismounts every volume with an extent on `disk`. Fails if any
/// volume is in use, so nothing is written while files are open.
pub fn lock_and_dismount(disk: u32) -> io::Result<VolumeLocks> {
    let mut locks = VolumeLocks { handles: Vec::new() };
    let volumes = list_volumes()?;
    if let Some(v) = volumes.iter().find(|v| v.location_unknown()) {
        let name = v.mount_points.first().cloned().unwrap_or_else(|| v.guid_path.clone());
        return Err(io::Error::other(format!(
            "could not tell which disk volume {name} is on, so it cannot be ruled out"
        )));
    }
    if volumes.iter().any(|v| v.on_disk(disk) && v.extents.iter().any(|e| e.disk != disk)) {
        return Err(io::Error::other("The target is part of a multi-disk volume"));
    }
    for v in volumes.into_iter().filter(|v| v.on_disk(disk)) {
        let name = v.mount_points.first().cloned().unwrap_or_else(|| v.guid_path.clone());
        let h = open(v.device_path(), GENERIC_READ | GENERIC_WRITE, 0)
            .map_err(|e| io::Error::new(e.kind(), format!("cannot open volume {name}: {e}")))?;
        let mut locked = false;
        for _ in 0..10 {
            if ioctl_none(&h, FSCTL_LOCK_VOLUME).is_ok() {
                locked = true;
                break;
            }
            thread::sleep(Duration::from_millis(300));
        }
        if !locked {
            return Err(io::Error::other(format!("volume {name} is in use")));
        }
        ioctl_none(&h, FSCTL_DISMOUNT_VOLUME)
            .map_err(|e| io::Error::new(e.kind(), format!("cannot dismount volume {name}: {e}")))?;
        locks.handles.push(h);
    }
    Ok(locks)
}

/// The volume GUID path (`\\?\Volume{...}\`) that a file-system path lives on.
pub fn volume_of_path(path: &str) -> io::Result<String> {
    let w = wide(path);
    let mut mount = [0u16; 1024];
    // SAFETY: buffers valid for the lengths given.
    if unsafe { GetVolumePathNameW(w.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mount_str = from_wide(&mount);
    // A path given as a volume GUID path is already the answer.
    if mount_str.to_ascii_lowercase().starts_with("\\\\?\\volume{") {
        return Ok(mount_str);
    }
    let m = wide(&mount_str);
    let mut name = [0u16; 128];
    // SAFETY: buffers valid for the lengths given.
    if unsafe { GetVolumeNameForVolumeMountPointW(m.as_ptr(), name.as_mut_ptr(), name.len() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(from_wide(&name))
}
