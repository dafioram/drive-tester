//! SD card identity and capability registers.
//!
//! * **CID** (card identification): manufacturer, OEM, product name, revision,
//!   serial number and manufacture date.
//! * **CSD** (card-specific data): the capacity the card's controller reports,
//!   independent of what the partition table or a faked controller claims.
//! * **SD Status** (ACMD13): speed class, UHS grade, video and application class.
//!
//! Parsing is platform-independent so it can be tested with captured buffers.
//! Blank, impossible or mismatched values are reported as counterfeit signals.

use serde::{Deserialize, Serialize};

use crate::model::format_bytes;

/// Everything known about an SD card.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SdInfo {
    /// Where the data came from, e.g. "card registers" or "device ID".
    pub sources: Vec<String>,
    pub manufacturer_id: Option<u8>,
    pub manufacturer: Option<String>,
    pub oem_id: Option<String>,
    pub product_name: Option<String>,
    pub revision: Option<String>,
    pub serial: Option<u32>,
    /// "YYYY-MM".
    pub manufactured: Option<String>,
    /// Capacity according to the CSD register.
    pub csd_capacity_bytes: Option<u64>,
    pub claims: SdClaims,
    /// Counterfeit or inconsistency signals.
    pub warnings: Vec<String>,
    /// Registers that could not be read, and why.
    pub notes: Vec<String>,
}

/// Speed ratings the card reports about itself (SD Status register).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdClaims {
    /// "C2", "C4", "C6", "C10".
    pub speed_class: Option<String>,
    /// "U1", "U3".
    pub uhs_grade: Option<String>,
    /// "V6" … "V90".
    pub video_class: Option<String>,
    /// "A1", "A2".
    pub app_class: Option<String>,
}

impl SdClaims {
    pub fn is_empty(&self) -> bool {
        self.speed_class.is_none() && self.uhs_grade.is_none() && self.video_class.is_none() && self.app_class.is_none()
    }

    /// All claims as speed-class labels, strongest first.
    pub fn labels(&self) -> Vec<String> {
        [&self.video_class, &self.uhs_grade, &self.speed_class, &self.app_class]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }
}

/// Manufacturer IDs as commonly reported. Not an official registry.
pub fn sd_manufacturer(mid: u8) -> Option<&'static str> {
    Some(match mid {
        0x01 => "Panasonic",
        0x02 => "Toshiba / Kioxia",
        0x03 => "SanDisk",
        0x1B => "Samsung",
        0x1D => "ADATA",
        0x27 => "Phison",
        0x28 => "Lexar",
        0x31 => "Silicon Power",
        0x41 => "Kingston",
        0x74 => "Transcend",
        0x76 => "Patriot",
        0x82 => "Sony",
        0x9C => "Angelbird / Hoodman",
        _ => return None,
    })
}

/// Bits `[hi:lo]` of a big-endian register of `bytes.len() * 8` bits.
fn bits(bytes: &[u8], hi: usize, lo: usize) -> u64 {
    let total = bytes.len() * 8;
    let mut v = 0u64;
    for bit in (lo..=hi).rev() {
        let pos = total - 1 - bit;
        let b = (bytes[pos / 8] >> (7 - pos % 8)) & 1;
        v = (v << 1) | u64::from(b);
    }
    v
}

fn printable(bytes: &[u8]) -> Option<String> {
    let s: String = bytes.iter().map(|&b| b as char).collect();
    (!s.trim().is_empty() && bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ')).then(|| s.trim().to_string())
}

/// Parsed CID register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cid {
    pub mid: u8,
    pub oid: String,
    pub pnm: String,
    pub prv: String,
    pub psn: u32,
    pub year: u16,
    pub month: u8,
}

fn parse_cid_exact(c: &[u8; 16]) -> Option<Cid> {
    let oid = printable(&c[1..3])?;
    let pnm = printable(&c[3..8])?;
    let year = 2000 + bits(c, 19, 12) as u16;
    let month = bits(c, 11, 8) as u8;
    if !(1..=12).contains(&month) || year > 2040 {
        return None;
    }
    Some(Cid {
        mid: c[0],
        oid,
        pnm,
        prv: format!("{}.{}", c[8] >> 4, c[8] & 0x0F),
        psn: u32::from_be_bytes([c[9], c[10], c[11], c[12]]),
        year,
        month,
    })
}

/// Parses a CID register. Drivers return the 128-bit response in different
/// byte orders (and sometimes without the CRC byte), so the usual layouts are
/// tried and the first plausible one wins.
pub fn parse_cid(raw: &[u8]) -> Option<Cid> {
    if raw.len() < 15 {
        return None;
    }
    let mut direct = [0u8; 16];
    let n = raw.len().min(16);
    direct[..n].copy_from_slice(&raw[..n]);
    let mut reversed = direct;
    reversed.reverse();
    // Responses delivered without the CRC byte and least-significant byte
    // first end up shifted by one byte once reversed.
    let mut shifted = [0u8; 16];
    shifted[..15].copy_from_slice(&reversed[1..]);
    [direct, reversed, shifted].iter().find_map(parse_cid_exact)
}

/// Capacity from a CSD register (versions 1.0, 2.0 and 3.0).
pub fn parse_csd_capacity(raw: &[u8]) -> Option<u64> {
    if raw.len() < 16 {
        return None;
    }
    let try_one = |c: &[u8]| -> Option<u64> {
        match bits(c, 127, 126) {
            0 => {
                let c_size = bits(c, 73, 62);
                let mult = bits(c, 49, 47);
                let read_bl_len = bits(c, 83, 80);
                if !(9..=11).contains(&read_bl_len) {
                    return None;
                }
                Some((c_size + 1) * (1 << (mult + 2)) * (1 << read_bl_len))
            }
            1 => Some((bits(c, 69, 48) + 1) * 512 * 1024),
            2 => Some((bits(c, 75, 48) + 1) * 512 * 1024),
            _ => None,
        }
    };
    let direct = &raw[..16];
    let reversed: Vec<u8> = direct.iter().rev().copied().collect();
    [direct.to_vec(), reversed].iter().find_map(|c| try_one(c).filter(|b| ((8 << 20)..=(4u64 << 40)).contains(b)))
}

/// Speed claims from the 64-byte SD Status register.
pub fn parse_sd_status(raw: &[u8]) -> Option<SdClaims> {
    if raw.len() < 22 {
        return None;
    }
    let speed_class = match raw[8] {
        0 => None,
        1 => Some("C2"),
        2 => Some("C4"),
        3 => Some("C6"),
        4 => Some("C10"),
        _ => return None,
    };
    let uhs_grade = match raw[14] >> 4 {
        0 => None,
        1 => Some("U1"),
        3 => Some("U3"),
        _ => return None,
    };
    let video_class = match raw[15] {
        0 => None,
        v @ (6 | 10 | 30 | 60 | 90) => Some(format!("V{v}")),
        _ => return None,
    };
    let app_class = match raw[21] & 0x0F {
        0 => None,
        1 => Some("A1"),
        2 => Some("A2"),
        _ => return None,
    };
    Some(SdClaims {
        speed_class: speed_class.map(str::to_string),
        uhs_grade: uhs_grade.map(str::to_string),
        video_class,
        app_class: app_class.map(str::to_string),
    })
}

impl SdInfo {
    /// Fills identity fields from a Windows SD device ID such as
    /// `SD\VID_03&OID_5344&PID_SU16G&REV_8.0\...`.
    pub fn apply_device_id(&mut self, id: &str) {
        let first = id.split('\\').nth(1).unwrap_or("");
        let mut found = false;
        for part in first.split('&') {
            let Some((key, value)) = part.split_once('_') else { continue };
            match key.to_ascii_uppercase().as_str() {
                "VID" => {
                    if let Ok(mid) = u8::from_str_radix(value, 16) {
                        self.manufacturer_id.get_or_insert(mid);
                        found = true;
                    }
                }
                "OID" => {
                    let bytes: Vec<u8> = (0..value.len() / 2)
                        .filter_map(|i| u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok())
                        .collect();
                    if let Some(s) = printable(&bytes) {
                        self.oem_id.get_or_insert(s);
                    }
                    found = true;
                }
                "PID" => {
                    self.product_name.get_or_insert(value.to_string());
                    found = true;
                }
                "REV" => {
                    self.revision.get_or_insert(value.to_string());
                    found = true;
                }
                _ => {}
            }
        }
        if found {
            self.sources.push("SD device ID".into());
        }
    }

    pub fn apply_cid(&mut self, cid: &Cid) {
        self.manufacturer_id = Some(cid.mid);
        self.oem_id = Some(cid.oid.clone());
        self.product_name = Some(cid.pnm.clone());
        self.revision = Some(cid.prv.clone());
        self.serial = Some(cid.psn);
        self.manufactured = Some(format!("{:04}-{:02}", cid.year, cid.month));
        self.sources.push("CID register".into());
    }

    /// Fills derived fields and computes warnings against the drive's reported size.
    pub fn finish(&mut self, reported_size: u64) {
        if let Some(mid) = self.manufacturer_id {
            self.manufacturer = sd_manufacturer(mid).map(str::to_string);
            if mid == 0 || mid == 0xFF {
                self.warnings.push(format!("Manufacturer ID 0x{mid:02X} is blank or invalid"));
            }
        }
        if self.serial == Some(0) || self.serial == Some(u32::MAX) {
            self.warnings.push("Card serial number is blank".into());
        }
        if let Some(oem) = &self.oem_id
            && oem.chars().all(|c| c == '0' || c == ' ')
        {
            self.warnings.push("OEM ID is blank".into());
        }
        if let Some(csd) = self.csd_capacity_bytes
            && reported_size > 0
        {
            let diff = csd.abs_diff(reported_size) as f64 / reported_size as f64;
            if diff > 0.02 {
                self.warnings.push(format!(
                    "Card registers report {} but the drive reports {} — a strong sign of fake capacity",
                    format_bytes(csd),
                    format_bytes(reported_size)
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid_bytes() -> [u8; 16] {
        // SanDisk "SD" "SU16G" rev 8.0, serial 0x12345678, 2021-03.
        let mut c = [0u8; 16];
        c[0] = 0x03;
        c[1..3].copy_from_slice(b"SD");
        c[3..8].copy_from_slice(b"SU16G");
        c[8] = 0x80;
        c[9..13].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        // MDT bits [19:8]: year offset 21 (0x15), month 3 -> 0x153
        c[13] = 0x01;
        c[14] = 0x53;
        c[15] = 0x01;
        c
    }

    #[test]
    fn cid_parses_in_either_byte_order() {
        let c = cid_bytes();
        let a = parse_cid(&c).unwrap();
        assert_eq!((a.mid, a.oid.as_str(), a.pnm.as_str()), (0x03, "SD", "SU16G"));
        assert_eq!((a.year, a.month, a.psn), (2021, 3, 0x1234_5678));
        let rev: Vec<u8> = c.iter().rev().copied().collect();
        assert_eq!(parse_cid(&rev).unwrap(), a);
    }

    #[test]
    fn csd_v2_capacity() {
        // C_SIZE = 30_000 -> (30_001) * 512 KiB.
        let mut c = [0u8; 16];
        c[0] = 0x40; // CSD_STRUCTURE = 1
        let c_size: u32 = 30_000;
        // bits [69:48] live in bytes 7..=9 (bit 69 = byte 7 bit 5).
        c[7] = ((c_size >> 16) & 0x3F) as u8;
        c[8] = (c_size >> 8) as u8;
        c[9] = c_size as u8;
        assert_eq!(parse_csd_capacity(&c), Some(30_001 * 512 * 1024));
    }

    #[test]
    fn sd_status_claims() {
        let mut s = [0u8; 64];
        s[8] = 4;
        s[14] = 0x30;
        s[15] = 30;
        s[21] = 0x02;
        let c = parse_sd_status(&s).unwrap();
        assert_eq!(c.labels(), vec!["V30", "U3", "C10", "A2"]);
    }

    #[test]
    fn device_id_and_warnings() {
        let mut info = SdInfo::default();
        info.apply_device_id("SD\\VID_03&OID_5344&PID_SU16G&REV_8.0\\5&abc&0&0");
        assert_eq!(info.manufacturer_id, Some(3));
        assert_eq!(info.oem_id.as_deref(), Some("SD"));
        assert_eq!(info.product_name.as_deref(), Some("SU16G"));
        info.csd_capacity_bytes = Some(4_000_000_000);
        info.finish(16_000_000_000);
        assert_eq!(info.manufacturer.as_deref(), Some("SanDisk"));
        assert!(info.warnings.iter().any(|w| w.contains("fake capacity")));
    }
}
