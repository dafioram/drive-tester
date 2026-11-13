//! Drive and volume descriptions shared by every crate.

use serde::{Deserialize, Serialize};

/// How the drive is attached, as reported by the storage stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusType {
    Nvme,
    Sata,
    Ata,
    Sas,
    Scsi,
    Usb,
    Sd,
    Mmc,
    Raid,
    StorageSpaces,
    Virtual,
    Other,
    Unknown,
}

impl BusType {
    pub fn label(self) -> &'static str {
        match self {
            BusType::Nvme => "NVMe",
            BusType::Sata => "SATA",
            BusType::Ata => "ATA",
            BusType::Sas => "SAS",
            BusType::Scsi => "SCSI",
            BusType::Usb => "USB",
            BusType::Sd => "SD",
            BusType::Mmc => "MMC",
            BusType::Raid => "RAID",
            BusType::StorageSpaces => "Storage Spaces",
            BusType::Virtual => "Virtual",
            BusType::Other => "Other",
            BusType::Unknown => "Unknown bus",
        }
    }

    /// Removable flash media where fake-capacity testing matters most.
    pub fn is_removable_flash_bus(self) -> bool {
        matches!(self, BusType::Usb | BusType::Sd | BusType::Mmc)
    }
}

/// Spinning disk or solid state, from the drive's seek-penalty report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Hdd,
    Ssd,
    Unknown,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Hdd => "HDD",
            MediaKind::Ssd => "SSD / flash",
            MediaKind::Unknown => "Unknown media",
        }
    }
}

/// Whether a disk carries the running system.
///
/// Anything other than [`SystemRole::Data`] is write-protected by the policy.
/// `Undetermined` exists so that detection failures fail *closed*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SystemRole {
    /// Not involved in running Windows.
    Data,
    /// Holds the Windows volume, boot/EFI partition, pagefile or hibernation file.
    System { reasons: Vec<String> },
    /// Detection could not rule the disk out, so it is treated as a system disk.
    Undetermined { reason: String },
}

impl SystemRole {
    pub fn is_protected(&self) -> bool {
        !matches!(self, SystemRole::Data)
    }

    pub fn describe(&self) -> String {
        match self {
            SystemRole::Data => "Data drive".to_string(),
            SystemRole::System { reasons } => format!("System disk: {}", reasons.join("; ")),
            SystemRole::Undetermined { reason } => {
                format!("Treated as system disk (could not confirm otherwise): {reason}")
            }
        }
    }
}

/// Where a volume sits on this disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeExtent {
    pub disk_offset: u64,
    pub length: u64,
}

/// A mounted (or mountable) volume that lives on a disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// Volume GUID path, e.g. `\\?\Volume{...}\`.
    pub id: String,
    /// Drive letters and folder mount points, e.g. `E:\`.
    pub mount_points: Vec<String>,
    pub label: String,
    pub file_system: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
    /// Extents of this volume on the owning disk.
    pub extents: Vec<VolumeExtent>,
    /// Other disks this volume also occupies (spanned / striped dynamic volumes).
    #[serde(default)]
    pub other_disks: Vec<u32>,
}

impl VolumeInfo {
    pub fn display_name(&self) -> String {
        let mount = self.mount_points.first().cloned().unwrap_or_else(|| "(no letter)".into());
        if self.label.is_empty() { mount } else { format!("{mount} {}", self.label) }
    }
}

/// Which health interface the drive is expected to answer on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthSource {
    /// ATA SMART through ATA pass-through (SATA, ATA).
    AtaSmart,
    /// ATA SMART wrapped in SCSI (SAT), used by USB enclosures.
    SatSmart,
    /// NVMe SMART / Health Information log page.
    NvmeLog,
    /// No health interface.
    None,
}

impl HealthSource {
    pub fn label(self) -> &'static str {
        match self {
            HealthSource::AtaSmart => "SMART (ATA)",
            HealthSource::SatSmart => "SMART over USB (SAT)",
            HealthSource::NvmeLog => "NVMe health log",
            HealthSource::None => "Not available",
        }
    }
}

/// Everything known about one physical drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveInfo {
    /// N in `\\.\PhysicalDriveN`.
    pub number: u32,
    pub device_path: String,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub bus: BusType,
    pub media: MediaKind,
    pub size_bytes: u64,
    pub logical_sector_size: u32,
    pub physical_sector_size: u32,
    pub removable: bool,
    pub write_protected: bool,
    pub role: SystemRole,
    pub volumes: Vec<VolumeInfo>,
    /// Interface the health reader will try first.
    pub health_source: HealthSource,
    /// What connects the drive, when known: a USB bridge or SD host
    /// (e.g. "USB bridge VID_152D PID_0578 (JMicron)").
    #[serde(default)]
    pub adapter: Option<String>,
    /// Plug-and-play device IDs from the drive up through its parents.
    #[serde(default)]
    pub device_ids: Vec<String>,
}

impl DriveInfo {
    /// Compare the attributes that identify a device independently of its disk number.
    pub fn same_device(&self, other: &Self) -> bool {
        self.bus == other.bus
            && self.model == other.model
            && normalize_serial(&self.serial) == normalize_serial(&other.serial)
            && self.size_bytes == other.size_bytes
            && self.logical_sector_size == other.logical_sector_size
            && self.device_ids == other.device_ids
    }

    /// A saved set of addresses requires a usable device identity.
    pub fn matches_report_device(&self, other: &Self) -> bool {
        let serial = normalize_serial(&self.serial);
        !serial.is_empty() && serial.bytes().any(|byte| byte != b'0') && self.same_device(other)
    }

    /// Human name: model, falling back to vendor or disk number.
    pub fn title(&self) -> String {
        let model = self.model.trim();
        let vendor = self.vendor.trim();
        if model.is_empty() && vendor.is_empty() {
            format!("Disk {}", self.number)
        } else if model.is_empty() {
            vendor.to_string()
        } else if vendor.is_empty() || model.to_uppercase().starts_with(&vendor.to_uppercase()) {
            model.to_string()
        } else {
            format!("{vendor} {model}")
        }
    }

    /// One-line summary: `Disk 2 · USB · SSD / flash · 64.0 GB`.
    pub fn summary_line(&self) -> String {
        format!(
            "Disk {} · {} · {} · {}",
            self.number,
            self.bus.label(),
            self.media.label(),
            format_bytes(self.size_bytes)
        )
    }

    pub fn is_protected(&self) -> bool {
        self.role.is_protected()
    }

    /// True when `typed` names this drive's serial (whitespace and case ignored).
    pub fn serial_matches(&self, typed: &str) -> bool {
        let own = normalize_serial(&self.serial);
        !own.is_empty() && own == normalize_serial(typed)
    }

    /// Finds a volume on this drive by one of its mount points (case-insensitive).
    pub fn volume_by_mount(&self, mount: &str) -> Option<&VolumeInfo> {
        let wanted = normalize_mount(mount);
        self.volumes.iter().find(|v| v.mount_points.iter().any(|m| normalize_mount(m) == wanted))
    }
}

/// Vendor names for USB IDs commonly seen on storage bridges and flash drives.
pub fn usb_vendor_name(vid: u16) -> Option<&'static str> {
    Some(match vid {
        0x152D => "JMicron",
        0x174C => "ASMedia",
        0x0BDA => "Realtek",
        0x2109 => "VIA Labs",
        0x05E3 => "Genesys Logic",
        0x1F75 => "Innostor",
        0x090C => "Silicon Motion",
        0x13FE => "Phison",
        0x058F => "Alcor Micro",
        0x0781 => "SanDisk",
        0x0951 => "Kingston",
        0x8564 => "Transcend",
        0x04E8 => "Samsung",
        0x1058 => "Western Digital",
        0x0BC2 => "Seagate",
        0x0480 => "Toshiba",
        _ => return None,
    })
}

/// Describes the adapter from a device-ID chain (drive first, then parents).
pub fn describe_adapter(ids: &[String]) -> Option<String> {
    for id in ids {
        let upper = id.to_ascii_uppercase();
        if upper.starts_with("SD\\") {
            return Some("SD host (native slot)".into());
        }
        if let Some(rest) = upper.strip_prefix("USB\\VID_") {
            let vid = u16::from_str_radix(rest.get(..4)?, 16).ok()?;
            let pid = rest.split("PID_").nth(1).and_then(|p| p.get(..4)).unwrap_or("????");
            let name = usb_vendor_name(vid).map(|n| format!(" ({n})")).unwrap_or_default();
            return Some(format!("USB bridge VID_{vid:04X} PID_{pid}{name}"));
        }
    }
    None
}

/// Serial numbers are compared without whitespace and case.
pub fn normalize_serial(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_uppercase()
}

fn normalize_mount(m: &str) -> String {
    let mut s = m.trim().to_uppercase();
    if !s.ends_with('\\') && !s.ends_with('/') {
        s.push('\\');
    }
    s
}

/// Formats a byte count the way drives are labelled (decimal units).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, f64); 4] = [("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("KB", 1e3)];
    let b = bytes as f64;
    for (unit, size) in UNITS {
        if b >= size {
            return format!("{:.1} {unit}", b / size);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_matching_ignores_case_and_spaces() {
        let d = crate::demo::sample_drive();
        assert!(d.serial_matches(&d.serial.to_lowercase()));
        assert!(d.serial_matches(&format!("  {}  ", d.serial)));
        assert!(!d.serial_matches("nope"));
        assert!(!d.serial_matches(""));
    }

    #[test]
    fn adapters_are_described() {
        let ids = vec!["USBSTOR\\DISK&VEN_X\\1".to_string(), "USB\\VID_152D&PID_0578\\ABC".to_string()];
        assert_eq!(describe_adapter(&ids).as_deref(), Some("USB bridge VID_152D PID_0578 (JMicron)"));
        assert_eq!(describe_adapter(&["SD\\VID_03&OID_5344".to_string()]).as_deref(), Some("SD host (native slot)"));
    }

    #[test]
    fn bytes_are_formatted_in_decimal_units() {
        assert_eq!(format_bytes(500_107_862_016), "500.1 GB");
        assert_eq!(format_bytes(2_000_000_000_000), "2.0 TB");
        assert_eq!(format_bytes(12), "12 B");
    }
}
