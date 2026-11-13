//! System-disk detection. Fails closed: if a check cannot complete, disks
//! not positively identified are marked `Undetermined`, which the policy treats
//! exactly like the system disk.

use std::collections::{BTreeMap, HashMap};

use drivetest_core::model::{BusType, SystemRole};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError};
use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

use crate::handle::{from_wide, wide};
use crate::volumes::{DiskExtent, VolumeRecord, extents_of};

/// Result of detection for all disks.
#[derive(Debug, Default)]
pub struct SystemDisks {
    reasons: BTreeMap<u32, Vec<String>>,
    undetermined: Option<String>,
}

impl SystemDisks {
    fn add(&mut self, disk: u32, reason: String) {
        let list = self.reasons.entry(disk).or_default();
        if !list.contains(&reason) {
            list.push(reason);
        }
    }

    fn add_all(&mut self, extents: &[DiskExtent], reason: &str) {
        for e in extents {
            self.add(e.disk, reason.to_string());
        }
    }

    fn fail(&mut self, reason: impl Into<String>) {
        self.undetermined.get_or_insert(reason.into());
    }

    pub fn role(&self, disk: u32) -> SystemRole {
        if let Some(reasons) = self.reasons.get(&disk) {
            SystemRole::System { reasons: reasons.clone() }
        } else if let Some(reason) = &self.undetermined {
            SystemRole::Undetermined { reason: reason.clone() }
        } else {
            SystemRole::Data
        }
    }
}

/// Reads a registry value as UTF-16 (REG_SZ or REG_MULTI_SZ).
fn registry_value(subkey: &str, value: &str, flags: u32) -> std::io::Result<Vec<u16>> {
    let (k, v) = (wide(subkey), wide(value));
    let mut buf = vec![0u16; 4096];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buffers valid for the sizes given.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    buf.truncate(size as usize / 2);
    Ok(buf)
}

fn registry_string(subkey: &str, value: &str) -> Option<String> {
    registry_value(subkey, value, RRF_RT_REG_SZ).ok().map(|b| from_wide(&b)).filter(|s| !s.is_empty())
}

fn registry_multi_string(subkey: &str, value: &str) -> std::io::Result<Vec<String>> {
    registry_value(subkey, value, RRF_RT_REG_MULTI_SZ)
        .map(|b| b.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect())
}

/// The real Windows folder (not a per-user one on Remote Desktop hosts).
fn windows_directory() -> Option<String> {
    let mut buf = [0u16; 520];
    // SAFETY: buffer valid for its length.
    let n = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
    (n > 0 && (n as usize) < buf.len()).then(|| from_wide(&buf))
}

enum Presence {
    Present,
    Absent,
    /// The check itself failed; the file may be there (in use, access denied…).
    Unknown(u32),
}

fn presence(path: &str) -> Presence {
    let w = wide(path);
    // SAFETY: null-terminated path.
    if unsafe { GetFileAttributesW(w.as_ptr()) } != INVALID_FILE_ATTRIBUTES {
        return Presence::Present;
    }
    // SAFETY: no arguments.
    match unsafe { GetLastError() } {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Presence::Absent,
        code => Presence::Unknown(code),
    }
}

/// Extents of a drive letter such as `C:`.
fn letter_extents(letter: &str) -> std::io::Result<Vec<DiskExtent>> {
    extents_of(&format!("\\\\.\\{letter}"))
}

/// Decides which disks carry the running system.
///
/// `volumes` is `None` when the volume list itself could not be read.
pub fn detect(volumes: Option<&[VolumeRecord]>, buses: &HashMap<u32, BusType>) -> SystemDisks {
    let mut s = SystemDisks::default();
    let Some(volumes) = volumes else {
        s.fail("could not list the system's volumes");
        return s;
    };

    // 1. The disk(s) holding the Windows folder.
    match windows_directory() {
        Some(dir) if dir.len() >= 2 && dir.as_bytes()[1] == b':' => {
            let letter = &dir[..2];
            match letter_extents(letter) {
                Ok(ext) if !ext.is_empty() => {
                    s.add_all(&ext, &format!("holds the Windows folder ({letter})"));
                    for e in &ext {
                        match buses.get(&e.disk) {
                            Some(BusType::Virtual) => {
                                s.fail("Windows runs from a virtual disk, so the drive holding it cannot be identified")
                            }
                            Some(BusType::StorageSpaces) => {
                                s.fail("Windows runs on Storage Spaces, so its physical drives cannot be identified")
                            }
                            None => s.fail("the Windows volume is on a disk that could not be inspected"),
                            _ => {}
                        }
                    }
                }
                Ok(_) => s.fail("the Windows volume reported no disk extents"),
                Err(e) => s.fail(format!("could not locate the Windows volume ({e})")),
            }
        }
        _ => s.fail("could not find the Windows folder"),
    }

    // 2. The boot / EFI system partition.
    match registry_string("SYSTEM\\Setup", "SystemPartition") {
        Some(device) => match extents_of(&format!("\\\\?\\GLOBALROOT{device}")) {
            Ok(ext) if !ext.is_empty() => s.add_all(&ext, "holds the boot / EFI system partition"),
            Ok(_) => s.fail("the boot partition reported no disk extents"),
            Err(e) => s.fail(format!("could not open the boot partition ({e})")),
        },
        None => s.fail("could not find the boot partition in the registry"),
    }

    // 3. Page files the memory manager reports as in use (e.g. "\??\D:\pagefile.sys").
    let mm = "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management";
    let page_files = match registry_multi_string(mm, "ExistingPageFiles") {
        Ok(files) => files,
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => Vec::new(),
        Err(error) => {
            s.fail(format!("could not read the active page file list ({error})"));
            Vec::new()
        }
    };
    for entry in page_files {
        let path = entry.trim_start_matches("\\??\\");
        if path.len() >= 2 && path.as_bytes()[1] == b':' {
            let letter = &path[..2];
            match letter_extents(letter) {
                Ok(ext) if !ext.is_empty() => s.add_all(&ext, &format!("holds an active page file ({letter})")),
                Ok(_) => s.fail(format!("no disk extents for page file volume {letter}")),
                Err(e) => s.fail(format!("could not locate the page file volume {letter} ({e})")),
            }
        } else {
            s.fail(format!("could not resolve active page file path {entry}"));
        }
    }

    // 4. Pagefile, hibernation and swap files on every mounted volume.
    for v in volumes.iter().filter(|v| !v.is_irrelevant()) {
        if !v.mount_points_known {
            let reason = format!("could not read mount points for {}", v.guid_path);
            if v.extents_known {
                s.add_all(&v.extents, &reason);
            } else {
                s.fail(reason);
            }
        }
        for mount in &v.mount_points {
            for file in ["pagefile.sys", "hiberfil.sys", "swapfile.sys"] {
                let reason = match presence(&format!("{mount}{file}")) {
                    Presence::Absent => continue,
                    Presence::Present => format!("holds {file} ({mount})"),
                    Presence::Unknown(code) => format!("could not check {mount} for {file} (error {code})"),
                };
                if v.extents_known {
                    // Mark this volume's disks only; unknown results fail closed per disk.
                    s.add_all(&v.extents, &reason);
                } else {
                    s.fail(format!("{reason}, and its disk could not be identified"));
                }
            }
        }
    }
    s
}
