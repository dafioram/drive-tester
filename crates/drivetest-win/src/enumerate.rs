//! Per-disk storage queries: descriptor, sector sizes, media type, length.

use std::io;
use std::mem::offset_of;

use drivetest_core::model::BusType;
use windows_sys::Win32::Foundation::ERROR_WRITE_PROTECT;
use windows_sys::Win32::System::Ioctl::{
    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_DISK_IS_WRITABLE,
    IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_ID,
    STORAGE_PROPERTY_QUERY, StorageAccessAlignmentProperty, StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
};

use crate::handle::{Handle, i64_at, ioctl_in_out, ioctl_out, u32_at};

// The descriptor is parsed from bytes; check our offsets against the SDK layout.
const OFF_REMOVABLE: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia);
const OFF_VENDOR: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset);
const OFF_PRODUCT: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset);
const OFF_REVISION: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductRevisionOffset);
const OFF_SERIAL: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, SerialNumberOffset);
const OFF_BUS: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType);
const OFF_LOGICAL: usize = offset_of!(STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, BytesPerLogicalSector);
const OFF_PHYSICAL: usize = offset_of!(STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, BytesPerPhysicalSector);

pub struct Descriptor {
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub serial: String,
    pub bus: BusType,
    pub removable: bool,
}

fn query(property: STORAGE_PROPERTY_ID) -> STORAGE_PROPERTY_QUERY {
    STORAGE_PROPERTY_QUERY { PropertyId: property, QueryType: PropertyStandardQuery, AdditionalParameters: [0] }
}

fn cstr_at(buf: &[u8], offset: u32) -> String {
    let o = offset as usize;
    if o == 0 || o >= buf.len() {
        return String::new();
    }
    let end = buf[o..].iter().position(|&b| b == 0).map_or(buf.len(), |p| o + p);
    String::from_utf8_lossy(&buf[o..end]).trim().to_string()
}

pub fn bus_from_storage(bus: u32) -> BusType {
    match bus {
        0x01 => BusType::Scsi,
        0x02 | 0x03 => BusType::Ata,
        0x07 => BusType::Usb,
        0x08 => BusType::Raid,
        0x0A => BusType::Sas,
        0x0B => BusType::Sata,
        0x0C => BusType::Sd,
        0x0D => BusType::Mmc,
        0x0E | 0x0F => BusType::Virtual,
        0x10 => BusType::StorageSpaces,
        0x11 => BusType::Nvme,
        0x00 => BusType::Unknown,
        _ => BusType::Other,
    }
}

pub fn descriptor(h: &Handle) -> io::Result<Descriptor> {
    let mut out = vec![0u8; 4096];
    ioctl_in_out(h, IOCTL_STORAGE_QUERY_PROPERTY, &query(StorageDeviceProperty), &mut out)?;
    Ok(Descriptor {
        vendor: cstr_at(&out, u32_at(&out, OFF_VENDOR)),
        product: cstr_at(&out, u32_at(&out, OFF_PRODUCT)),
        revision: cstr_at(&out, u32_at(&out, OFF_REVISION)),
        serial: cstr_at(&out, u32_at(&out, OFF_SERIAL)),
        bus: bus_from_storage(u32_at(&out, OFF_BUS)),
        removable: out[OFF_REMOVABLE] != 0,
    })
}

/// (logical, physical) sector sizes.
pub fn alignment(h: &Handle) -> Option<(u32, u32)> {
    let mut out = [0u8; 64];
    ioctl_in_out(h, IOCTL_STORAGE_QUERY_PROPERTY, &query(StorageAccessAlignmentProperty), &mut out).ok()?;
    let (l, p) = (u32_at(&out, OFF_LOGICAL), u32_at(&out, OFF_PHYSICAL));
    (l >= 512).then_some((l, p.max(l)))
}

/// Sector size from the drive geometry (fallback).
pub fn geometry_sector(h: &Handle) -> Option<u32> {
    let mut out = [0u8; 256];
    ioctl_out(h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &mut out).ok()?;
    let bps = u32_at(&out, 20);
    (bps >= 512).then_some(bps)
}

/// `Some(true)` when the drive reports a seek penalty (spinning disk).
pub fn seek_penalty(h: &Handle) -> Option<bool> {
    let mut out = [0u8; 16];
    ioctl_in_out(h, IOCTL_STORAGE_QUERY_PROPERTY, &query(StorageDeviceSeekPenaltyProperty), &mut out).ok()?;
    Some(out[8] != 0)
}

pub fn length(h: &Handle) -> io::Result<u64> {
    let mut out = [0u8; 8];
    ioctl_out(h, IOCTL_DISK_GET_LENGTH_INFO, &mut out)?;
    Ok(i64_at(&out, 0).max(0) as u64)
}

/// True only when the drive explicitly reports write protection.
pub fn write_protected(h: &Handle) -> bool {
    match ioctl_out(h, IOCTL_DISK_IS_WRITABLE, &mut []) {
        Ok(_) => false,
        Err(e) => e.raw_os_error() == Some(ERROR_WRITE_PROTECT as i32),
    }
}

/// The disk number Windows reports for an open handle (N in PhysicalDriveN).
pub fn device_number(h: &Handle) -> io::Result<u32> {
    let mut out = [0u8; std::mem::size_of::<STORAGE_DEVICE_NUMBER>()];
    ioctl_out(h, IOCTL_STORAGE_GET_DEVICE_NUMBER, &mut out)?;
    Ok(u32_at(&out, offset_of!(STORAGE_DEVICE_NUMBER, DeviceNumber)))
}
