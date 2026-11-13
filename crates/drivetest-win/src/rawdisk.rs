//! Unbuffered raw access to `\\.\PhysicalDriveN`.

use std::io;

use drivetest_core::model::normalize_serial;
use drivetest_core::platform::{RawReader, RawWriter};
use drivetest_core::policy::WriteAuthorization;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_BEGIN, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FlushFileBuffers, ReadFile, SetFilePointerEx,
    WriteFile,
};

use crate::enumerate;
use crate::handle::{Handle, disk_path, open};

pub struct WinDisk {
    handle: Handle,
    size: u64,
    sector: u32,
    buffer_alignment: usize,
}

impl WinDisk {
    /// Opens a disk read-only.
    pub(crate) fn open_read(disk: u32) -> io::Result<Self> {
        Self::open(disk, GENERIC_READ)
    }

    /// Opens a disk for writing, then verifies the *opened handle* is the
    /// authorized drive: same disk number, serial and capacity. This closes
    /// the gap between the checks and the open (re-numbering, swaps).
    pub(crate) fn open_write_verified(auth: &WriteAuthorization) -> io::Result<Self> {
        let disk = Self::open(auth.disk_number(), GENERIC_READ | GENERIC_WRITE)?;
        let refuse = |why: &str| io::Error::new(io::ErrorKind::PermissionDenied, format!("refused: {why}"));
        if enumerate::device_number(&disk.handle)? != auth.disk_number() {
            return Err(refuse("the opened device has a different disk number"));
        }
        let serial = enumerate::descriptor(&disk.handle)?.serial;
        if normalize_serial(&serial).is_empty() || normalize_serial(&serial) != normalize_serial(auth.serial()) {
            return Err(refuse("the opened device has a different serial number"));
        }
        if disk.size != auth.size_bytes() {
            return Err(refuse("the opened device has a different capacity"));
        }
        Ok(disk)
    }

    fn open(disk: u32, access: u32) -> io::Result<Self> {
        let handle = open(&disk_path(disk), access, FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH)?;
        let size = enumerate::length(&handle)?;
        let (sector, physical) = enumerate::alignment(&handle)
            .or_else(|| enumerate::geometry_sector(&handle).map(|s| (s, s)))
            .ok_or_else(|| io::Error::other("could not determine the disk sector alignment"))?;
        let buffer_alignment = physical as usize;
        if !buffer_alignment.is_power_of_two() || buffer_alignment > drivetest_core::aligned::IO_ALIGN {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Unsupported I/O buffer alignment: {physical} bytes"),
            ));
        }
        Ok(Self { handle, size, sector, buffer_alignment })
    }

    fn seek(&self, offset: u64) -> io::Result<()> {
        let to = i64::try_from(offset).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        // SAFETY: valid handle; null new-position pointer is allowed.
        let ok = unsafe { SetFilePointerEx(self.handle.raw(), to, std::ptr::null_mut(), FILE_BEGIN) };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn check(&self, offset: u64, len: usize) -> io::Result<()> {
        let s = u64::from(self.sector);
        if !offset.is_multiple_of(s) || !(len as u64).is_multiple_of(s) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "I/O not aligned to the sector size"));
        }
        if offset.saturating_add(len as u64) > self.size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "I/O past the end of the drive"));
        }
        Ok(())
    }
}

impl RawReader for WinDisk {
    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn logical_sector_size(&self) -> u32 {
        self.sector
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.check(offset, buf.len())?;
        if !((buf.as_ptr() as usize).is_multiple_of(self.buffer_alignment)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "I/O buffer does not meet physical-sector alignment",
            ));
        }
        self.seek(offset)?;
        let mut done = 0usize;
        while done < buf.len() {
            let want = (buf.len() - done).min(u32::MAX as usize & !0xFFF) as u32;
            let mut got = 0u32;
            // SAFETY: the buffer region is valid for `want` bytes.
            let ok =
                unsafe { ReadFile(self.handle.raw(), buf[done..].as_mut_ptr(), want, &mut got, std::ptr::null_mut()) };
            if ok == 0 {
                let error = io::Error::last_os_error();
                // CRC and generic device errors can also come from the transport.
                if error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_SECTOR_NOT_FOUND as i32) {
                    return Err(io::Error::other(drivetest_core::platform::MediaReadError(error)));
                }
                return Err(error);
            }
            if got == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
            }
            done += got as usize;
        }
        Ok(())
    }
}

impl RawWriter for WinDisk {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.check(offset, buf.len())?;
        if !((buf.as_ptr() as usize).is_multiple_of(self.buffer_alignment)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "I/O buffer does not meet physical-sector alignment",
            ));
        }
        self.seek(offset)?;
        let mut done = 0usize;
        while done < buf.len() {
            let want = (buf.len() - done).min(u32::MAX as usize & !0xFFF) as u32;
            let mut put = 0u32;
            // SAFETY: the buffer region is valid for `want` bytes.
            let ok =
                unsafe { WriteFile(self.handle.raw(), buf[done..].as_ptr(), want, &mut put, std::ptr::null_mut()) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if put == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
            }
            done += put as usize;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        // SAFETY: valid handle.
        if unsafe { FlushFileBuffers(self.handle.raw()) } == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
}
