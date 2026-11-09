//! SD card registers through the Windows SD storage driver (sffdisk.h).
//!
//! Best effort: the SD bus driver only passes some commands through, and
//! cards behind USB readers are not reachable this way at all. Everything
//! returned is validated by the parsers in `drivetest_core::sd`, so a driver
//! that answers with garbage is reported as "not available" rather than
//! believed. These commands only read card registers.

use std::io;
use std::mem::{offset_of, size_of};

use drivetest_core::sd::{SdInfo, parse_cid, parse_csd_capacity, parse_sd_status};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};

use crate::handle::{Handle, disk_path, ioctl_bytes, open};

/// CTL_CODE(FILE_DEVICE_DISK, 0x7A1, METHOD_BUFFERED, FILE_WRITE_ACCESS) = 0x79E84 (sffdisk.h)
const IOCTL_SFFDISK_DEVICE_COMMAND: u32 = (0x7 << 16) | (0x2 << 14) | (0x7A1 << 2);
const _: () = assert!(IOCTL_SFFDISK_DEVICE_COMMAND == 0x79E84);
const SFFDISK_DC_DEVICE_COMMAND: u32 = 3;

// SDCMD_DESCRIPTOR enums (sddef.h).
const SDCC_STANDARD: u32 = 0;
const SDCC_APP_CMD: u32 = 1;
const SDTD_UNSPECIFIED: u32 = 0;
const SDTD_READ: u32 = 1;
const SDTT_CMD_ONLY: u32 = 1;
const SDTT_SINGLE_BLOCK: u32 = 2;
const SDRT_1: u32 = 2;
const SDRT_2: u32 = 4;

/// SFFDISK_DEVICE_COMMAND_DATA without its trailing `Data[1]`.
#[repr(C)]
struct CommandHeader {
    header_size: u16,
    flags: u16,
    command: u32,
    protocol_argument_size: u16,
    device_data_buffer_size: u32,
    information: usize,
}

#[repr(C)]
struct SdCmdDescriptor {
    cmd: u8,
    cmd_class: u32,
    transfer_direction: u32,
    transfer_type: u32,
    response_type: u32,
}

const HEADER: usize = size_of::<CommandHeader>();
const DESCRIPTOR: usize = size_of::<SdCmdDescriptor>();
const _: () = assert!(HEADER == 24 && DESCRIPTOR == 20);

struct SdCommand {
    cmd: u8,
    class: u32,
    direction: u32,
    transfer: u32,
    response: u32,
    data_len: usize,
}

fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Only register reads are allowed through here.
fn allowed(c: &SdCommand) -> bool {
    matches!((c.class, c.cmd), (SDCC_STANDARD, 9) | (SDCC_STANDARD, 10) | (SDCC_APP_CMD, 13))
}

fn run(h: &Handle, c: &SdCommand) -> io::Result<Vec<u8>> {
    if !allowed(c) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "SD command not on the read-only allowlist"));
    }
    let mut buf = vec![0u8; HEADER + DESCRIPTOR + c.data_len];
    // The descriptor follows the header at HeaderSize (= offset of Data).
    put_u16(&mut buf, offset_of!(CommandHeader, header_size), HEADER as u16);
    put_u32(&mut buf, offset_of!(CommandHeader, command), SFFDISK_DC_DEVICE_COMMAND);
    put_u16(&mut buf, offset_of!(CommandHeader, protocol_argument_size), DESCRIPTOR as u16);
    put_u32(&mut buf, offset_of!(CommandHeader, device_data_buffer_size), c.data_len as u32);
    let d = HEADER;
    buf[d + offset_of!(SdCmdDescriptor, cmd)] = c.cmd;
    put_u32(&mut buf, d + offset_of!(SdCmdDescriptor, cmd_class), c.class);
    put_u32(&mut buf, d + offset_of!(SdCmdDescriptor, transfer_direction), c.direction);
    put_u32(&mut buf, d + offset_of!(SdCmdDescriptor, transfer_type), c.transfer);
    put_u32(&mut buf, d + offset_of!(SdCmdDescriptor, response_type), c.response);
    let input = buf.clone();
    ioctl_bytes(h, IOCTL_SFFDISK_DEVICE_COMMAND, &input, &mut buf)?;
    Ok(buf[HEADER + DESCRIPTOR..].to_vec())
}

/// Collects what can be learned about an SD card.
pub fn read(disk: u32, reported_size: u64, device_ids: &[String]) -> SdInfo {
    let mut info = SdInfo::default();
    if let Some(id) = device_ids.iter().find(|i| i.to_ascii_uppercase().starts_with("SD\\")) {
        info.apply_device_id(id);
    }
    match open(&disk_path(disk), GENERIC_READ | GENERIC_WRITE, 0) {
        Err(e) => info.notes.push(format!("Card registers not read: {e}")),
        Ok(h) => {
            let reg = |cmd: u8| SdCommand {
                cmd,
                class: SDCC_STANDARD,
                direction: SDTD_UNSPECIFIED,
                transfer: SDTT_CMD_ONLY,
                response: SDRT_2,
                data_len: 16,
            };
            match run(&h, &reg(10)).ok().and_then(|r| parse_cid(&r)) {
                Some(cid) => info.apply_cid(&cid),
                None => info.notes.push("CID register not available through this SD host driver".into()),
            }
            match run(&h, &reg(9)).ok().and_then(|r| parse_csd_capacity(&r)) {
                Some(cap) => {
                    info.csd_capacity_bytes = Some(cap);
                    info.sources.push("CSD register".into());
                }
                None => info.notes.push("CSD register not available through this SD host driver".into()),
            }
            let status = SdCommand {
                cmd: 13,
                class: SDCC_APP_CMD,
                direction: SDTD_READ,
                transfer: SDTT_SINGLE_BLOCK,
                response: SDRT_1,
                data_len: 64,
            };
            match run(&h, &status).ok().and_then(|r| parse_sd_status(&r)) {
                Some(claims) if !claims.is_empty() => {
                    info.claims = claims;
                    info.sources.push("SD Status register".into());
                }
                _ => info.notes.push("SD Status (speed class) not available through this SD host driver".into()),
            }
        }
    }
    info.finish(reported_size);
    info
}
