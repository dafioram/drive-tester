//! Health data: ATA SMART (direct), ATA SMART over SAT (USB bridges) and the
//! NVMe SMART / Health Information log. Commands here only read state or start
//! the drive's own self-test; they never write user data.

use std::io;
use std::mem::{offset_of, size_of};

use drivetest_core::health::{
    AtaHealth, HealthSnapshot, parse_ata_identify, parse_ata_smart, parse_nvme_health, parse_nvme_self_test_log,
    parse_self_test_status,
};
use drivetest_core::model::{BusType, HealthSource};
use drivetest_core::platform::{PlatformError, SelfTestKind};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::IscsiDisc::{
    ATA_FLAGS_DATA_IN, ATA_FLAGS_DRDY_REQUIRED, ATA_PASS_THROUGH_EX, IOCTL_ATA_PASS_THROUGH, IOCTL_SCSI_PASS_THROUGH,
    SCSI_IOCTL_DATA_IN, SCSI_IOCTL_DATA_UNSPECIFIED, SCSI_PASS_THROUGH,
};
use windows_sys::Win32::System::Ioctl::{
    IOCTL_STORAGE_PROTOCOL_COMMAND, IOCTL_STORAGE_QUERY_PROPERTY, NVMeDataTypeLogPage, PropertyStandardQuery,
    ProtocolTypeNvme, STORAGE_PROTOCOL_COMMAND, STORAGE_PROTOCOL_COMMAND_FLAG_ADAPTER_REQUEST,
    STORAGE_PROTOCOL_COMMAND_LENGTH_NVME, STORAGE_PROTOCOL_SPECIFIC_DATA, STORAGE_PROTOCOL_SPECIFIC_NVME_ADMIN_COMMAND,
    STORAGE_PROTOCOL_STATUS_SUCCESS, STORAGE_PROTOCOL_STRUCTURE_VERSION, StorageAdapterProtocolSpecificProperty,
    StorageDeviceProtocolSpecificProperty,
};

use crate::handle::{Handle, disk_path, ioctl_bytes, ioctl_inplace, open, u32_at};

const SECTOR: usize = 512;
const NVME_LOG_HEALTH: u32 = 0x02;

#[repr(C)]
#[derive(Clone, Copy)]
struct AtaBuffer {
    apt: ATA_PASS_THROUGH_EX,
    filler: u32,
    data: [u8; SECTOR],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ScsiBuffer {
    spt: SCSI_PASS_THROUGH,
    filler: u32,
    sense: [u8; 32],
    data: [u8; SECTOR],
}

/// Where STORAGE_PROPERTY_QUERY.AdditionalParameters (and the reply's
/// protocol data) start: after PropertyId and QueryType.
const NVME_PROTOCOL_AT: usize = 8;
const _: () = assert!(size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>() == 40);
const NVME_LOG_SELF_TEST: u32 = 0x06;
const NVME_SELF_TEST_LOG_LEN: usize = 564;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    /// IOCTL_ATA_PASS_THROUGH (SATA / ATA controllers).
    Ata,
    /// ATA PASS-THROUGH(16) wrapped in SCSI (USB-SATA bridges).
    Sat,
}

struct AtaCommand {
    features: u8,
    count: u8,
    lba_low: u8,
    lba_mid: u8,
    lba_high: u8,
    command: u8,
    data_in: bool,
}

const SMART_MID: u8 = 0x4F;
const SMART_HIGH: u8 = 0xC2;
const SMART: u8 = 0xB0;

fn identify() -> AtaCommand {
    AtaCommand { features: 0, count: 1, lba_low: 0, lba_mid: 0, lba_high: 0, command: 0xEC, data_in: true }
}

fn smart(features: u8, lba_low: u8, data_in: bool) -> AtaCommand {
    AtaCommand {
        features,
        count: u8::from(data_in),
        lba_low,
        lba_mid: SMART_MID,
        lba_high: SMART_HIGH,
        command: SMART,
        data_in,
    }
}

fn run_ata(h: &Handle, cmd: &AtaCommand) -> io::Result<[u8; SECTOR]> {
    // SAFETY: plain-old-data structure; all-zero is a valid initial state.
    let mut buf: AtaBuffer = unsafe { std::mem::zeroed() };
    buf.apt.Length = size_of::<ATA_PASS_THROUGH_EX>() as u16;
    buf.apt.AtaFlags = (ATA_FLAGS_DRDY_REQUIRED | if cmd.data_in { ATA_FLAGS_DATA_IN } else { 0 }) as u16;
    buf.apt.DataTransferLength = if cmd.data_in { SECTOR as u32 } else { 0 };
    buf.apt.TimeOutValue = 10;
    buf.apt.DataBufferOffset = offset_of!(AtaBuffer, data);
    buf.apt.CurrentTaskFile = [cmd.features, cmd.count, cmd.lba_low, cmd.lba_mid, cmd.lba_high, 0xA0, cmd.command, 0];
    ioctl_inplace(h, IOCTL_ATA_PASS_THROUGH, &mut buf)?;
    // Byte 6 now holds the status register; bit 0 is ERR.
    if buf.apt.CurrentTaskFile[6] & 0x01 != 0 {
        return Err(io::Error::other("the drive rejected the ATA command"));
    }
    Ok(buf.data)
}

fn run_sat(h: &Handle, cmd: &AtaCommand) -> io::Result<[u8; SECTOR]> {
    // SAFETY: plain-old-data structure; all-zero is a valid initial state.
    let mut buf: ScsiBuffer = unsafe { std::mem::zeroed() };
    buf.spt.Length = size_of::<SCSI_PASS_THROUGH>() as u16;
    buf.spt.CdbLength = 16;
    buf.spt.SenseInfoLength = buf.sense.len() as u8;
    buf.spt.DataIn = if cmd.data_in { SCSI_IOCTL_DATA_IN } else { SCSI_IOCTL_DATA_UNSPECIFIED } as u8;
    buf.spt.DataTransferLength = if cmd.data_in { SECTOR as u32 } else { 0 };
    buf.spt.TimeOutValue = 10;
    buf.spt.DataBufferOffset = offset_of!(ScsiBuffer, data);
    buf.spt.SenseInfoOffset = offset_of!(ScsiBuffer, sense) as u32;
    // ATA PASS-THROUGH(16): protocol 4 = PIO data-in, 3 = non-data.
    let (protocol, flags) = if cmd.data_in { (4u8, 0x0Eu8) } else { (3u8, 0x00u8) };
    buf.spt.Cdb = [
        0x85,
        protocol << 1,
        flags,
        0,
        cmd.features,
        0,
        cmd.count,
        0,
        cmd.lba_low,
        0,
        cmd.lba_mid,
        0,
        cmd.lba_high,
        0xA0,
        cmd.command,
        0,
    ];
    ioctl_inplace(h, IOCTL_SCSI_PASS_THROUGH, &mut buf)?;
    if buf.spt.ScsiStatus != 0 {
        return Err(io::Error::other("the USB bridge rejected the SAT command"));
    }
    Ok(buf.data)
}

/// Only these commands are ever sent: IDENTIFY DEVICE, and SMART READ DATA,
/// READ THRESHOLDS and EXECUTE OFF-LINE IMMEDIATE (self-test). None writes
/// user data. Anything else is refused, so a later edit cannot slip in a write.
fn allowed(cmd: &AtaCommand) -> bool {
    match cmd.command {
        0xEC => true,
        SMART => {
            let read = matches!(cmd.features, 0xD0 | 0xD1);
            // Self-test in off-line mode only (1 = short, 2 = extended), never captive.
            let self_test = cmd.features == 0xD4 && matches!(cmd.lba_low, 1 | 2);
            (read || self_test) && cmd.lba_mid == SMART_MID && cmd.lba_high == SMART_HIGH
        }
        _ => false,
    }
}

fn run(h: &Handle, t: Transport, cmd: &AtaCommand) -> io::Result<[u8; SECTOR]> {
    if !allowed(cmd) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "ATA command not on the read-only allowlist"));
    }
    match t {
        Transport::Ata => run_ata(h, cmd),
        Transport::Sat => run_sat(h, cmd),
    }
}

/// SMART data structures end with a checksum byte making the sum zero.
fn checksum_ok(data: &[u8]) -> bool {
    data.iter().fold(0u8, |a, &b| a.wrapping_add(b)) == 0 && data.iter().any(|&b| b != 0)
}

fn ata_health(h: &Handle, t: Transport) -> io::Result<AtaHealth> {
    let data = run(h, t, &smart(0xD0, 0, true))?;
    if !checksum_ok(&data) {
        return Err(io::Error::other("SMART data failed its checksum"));
    }
    let thresholds = run(h, t, &smart(0xD1, 0, true)).ok().filter(|d| checksum_ok(d));
    let identify = run(h, t, &identify()).ok().and_then(|d| parse_ata_identify(&d));
    Ok(AtaHealth {
        attributes: parse_ata_smart(&data, thresholds.as_ref().map(|t| &t[..])),
        self_test: Some(parse_self_test_status(data[363])),
        identify,
    })
}

fn open_for_commands(disk: u32) -> Result<Handle, PlatformError> {
    open(&disk_path(disk), GENERIC_READ | GENERIC_WRITE, 0).map_err(|e| {
        if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
            PlatformError::AccessDenied("run Drive Test as administrator to read health data".into())
        } else {
            PlatformError::Io(e)
        }
    })
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Reads an NVMe log page through IOCTL_STORAGE_QUERY_PROPERTY.
fn nvme_log(h: &Handle, property: i32, log_id: u32, len: usize) -> io::Result<Vec<u8>> {
    let spec = |field: usize| NVME_PROTOCOL_AT + field;
    let total = NVME_PROTOCOL_AT + size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>() + len;
    let mut buf = vec![0u8; total];
    put_u32(&mut buf, 0, property as u32);
    put_u32(&mut buf, 4, PropertyStandardQuery as u32);
    put_u32(&mut buf, spec(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolType)), ProtocolTypeNvme as u32);
    put_u32(&mut buf, spec(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, DataType)), NVMeDataTypeLogPage as u32);
    put_u32(&mut buf, spec(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataRequestValue)), log_id);
    put_u32(
        &mut buf,
        spec(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataOffset)),
        size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>() as u32,
    );
    put_u32(&mut buf, spec(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataLength)), len as u32);
    let input = buf.clone();
    ioctl_bytes(h, IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut buf)?;
    // The reply is a STORAGE_PROTOCOL_DATA_DESCRIPTOR; locate the data via its own fields.
    let data_length = u32_at(&buf, spec(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataLength))) as usize;
    if data_length < len {
        return Err(io::Error::other("the NVMe log reply was too short"));
    }
    let start =
        NVME_PROTOCOL_AT + u32_at(&buf, spec(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataOffset))) as usize;
    buf.get(start..start + len).map(<[u8]>::to_vec).ok_or_else(|| io::Error::other("unexpected NVMe reply layout"))
}

fn nvme_log_any(h: &Handle, log_id: u32, len: usize) -> io::Result<Vec<u8>> {
    nvme_log(h, StorageDeviceProtocolSpecificProperty, log_id, len)
        .or_else(|_| nvme_log(h, StorageAdapterProtocolSpecificProperty, log_id, len))
}

/// Starts an NVMe Device Self-test (admin opcode 14h) through
/// IOCTL_STORAGE_PROTOCOL_COMMAND. `code` is 1 (short) or 2 (extended).
/// Older Windows builds and some vendor drivers refuse this command.
fn nvme_start_self_test(h: &Handle, code: u8) -> io::Result<()> {
    if !matches!(code, 1 | 2) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid self-test code"));
    }
    let cmd_at = offset_of!(STORAGE_PROTOCOL_COMMAND, Command);
    let mut buf = vec![0u8; cmd_at + STORAGE_PROTOCOL_COMMAND_LENGTH_NVME as usize];
    put_u32(&mut buf, offset_of!(STORAGE_PROTOCOL_COMMAND, Version), STORAGE_PROTOCOL_STRUCTURE_VERSION);
    put_u32(&mut buf, offset_of!(STORAGE_PROTOCOL_COMMAND, Length), size_of::<STORAGE_PROTOCOL_COMMAND>() as u32);
    put_u32(&mut buf, offset_of!(STORAGE_PROTOCOL_COMMAND, ProtocolType), ProtocolTypeNvme as u32);
    put_u32(&mut buf, offset_of!(STORAGE_PROTOCOL_COMMAND, Flags), STORAGE_PROTOCOL_COMMAND_FLAG_ADAPTER_REQUEST);
    put_u32(&mut buf, offset_of!(STORAGE_PROTOCOL_COMMAND, CommandLength), STORAGE_PROTOCOL_COMMAND_LENGTH_NVME);
    put_u32(&mut buf, offset_of!(STORAGE_PROTOCOL_COMMAND, TimeOutValue), 10);
    put_u32(
        &mut buf,
        offset_of!(STORAGE_PROTOCOL_COMMAND, CommandSpecific),
        STORAGE_PROTOCOL_SPECIFIC_NVME_ADMIN_COMMAND,
    );
    // NVMe command: CDW0 opcode 14h, NSID all namespaces, CDW10 = self-test code.
    buf[cmd_at] = 0x14;
    put_u32(&mut buf, cmd_at + 4, 0xFFFF_FFFF);
    put_u32(&mut buf, cmd_at + 40, u32::from(code));
    let input = buf.clone();
    ioctl_bytes(h, IOCTL_STORAGE_PROTOCOL_COMMAND, &input, &mut buf)?;
    let status = u32_at(&buf, offset_of!(STORAGE_PROTOCOL_COMMAND, ReturnStatus));
    if status != STORAGE_PROTOCOL_STATUS_SUCCESS {
        return Err(io::Error::other(format!("the drive refused the self-test (status {status})")));
    }
    Ok(())
}

/// Reads health using the interface the bus suggests, falling back where useful.
pub fn read(disk: u32, bus: BusType) -> Result<HealthSnapshot, PlatformError> {
    if bus == BusType::Nvme {
        let h = open_for_commands(disk).or_else(|_| open(&disk_path(disk), 0, 0).map_err(PlatformError::Io))?;
        let log = nvme_log_any(&h, NVME_LOG_HEALTH, SECTOR)?;
        let mut snap = parse_nvme_health(&log)
            .map(HealthSnapshot::from_nvme)
            .ok_or_else(|| PlatformError::Other("could not parse the NVMe health log".into()))?;
        snap.nvme_self_test = nvme_log_any(&h, NVME_LOG_SELF_TEST, NVME_SELF_TEST_LOG_LEN)
            .ok()
            .and_then(|l| parse_nvme_self_test_log(&l));
        return Ok(snap);
    }
    let h = open_for_commands(disk)?;
    let order: &[(Transport, HealthSource)] = if bus == BusType::Usb {
        &[(Transport::Sat, HealthSource::SatSmart), (Transport::Ata, HealthSource::AtaSmart)]
    } else {
        &[(Transport::Ata, HealthSource::AtaSmart), (Transport::Sat, HealthSource::SatSmart)]
    };
    let mut last_error = None;
    for &(t, source) in order {
        match ata_health(&h, t) {
            Ok(ata) => return Ok(HealthSnapshot::from_ata(source, ata)),
            Err(e) => last_error = Some(e),
        }
    }
    let why = match bus {
        BusType::Usb => "this USB bridge or flash drive does not pass health commands",
        BusType::Sd | BusType::Mmc => "SD / MMC cards do not expose SMART",
        BusType::Raid | BusType::StorageSpaces => "the RAID / Storage Spaces layer hides the physical drives",
        BusType::Virtual => "virtual disks have no health data",
        _ => "the drive did not answer SMART commands",
    };
    Ok(HealthSnapshot::unavailable(match last_error {
        Some(e) => format!("No health data: {why} ({e})."),
        None => format!("No health data: {why}."),
    }))
}

/// Starts the ATA built-in self-test (short = subcommand 1, extended = 2).
pub fn start_self_test(disk: u32, bus: BusType, kind: SelfTestKind) -> Result<(), PlatformError> {
    if bus == BusType::Nvme {
        let h = open_for_commands(disk)?;
        let code = match kind {
            SelfTestKind::Short => 1,
            SelfTestKind::Extended => 2,
        };
        return nvme_start_self_test(&h, code).map_err(|e| {
            PlatformError::Unsupported(format!(
                "this NVMe driver did not accept the self-test command ({e}); it needs Windows 10 1903 or later and the Microsoft NVMe driver"
            ))
        });
    }
    let h = open_for_commands(disk)?;
    let sub = match kind {
        SelfTestKind::Short => 1,
        SelfTestKind::Extended => 2,
    };
    let cmd = smart(0xD4, sub, false);
    let order = if bus == BusType::Usb { [Transport::Sat, Transport::Ata] } else { [Transport::Ata, Transport::Sat] };
    let mut last = None;
    for t in order {
        match run(&h, t, &cmd) {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(PlatformError::Other(format!(
        "the drive did not accept the self-test command ({})",
        last.map(|e| e.to_string()).unwrap_or_default()
    )))
}
