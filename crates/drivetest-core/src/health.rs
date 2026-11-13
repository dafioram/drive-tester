//! Health data: parsing of raw SMART / NVMe buffers, plain-language verdicts,
//! before/after comparison and SMR/CMR assessment.
//!
//! Parsing lives here (not in the Windows crate) so it can be unit tested on
//! any platform with captured buffers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{HealthSource, MediaKind};

// ---------------------------------------------------------------- ATA SMART

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartAttribute {
    pub id: u8,
    pub name: String,
    pub flags: u16,
    /// Normalised value, usually 100 or 200 when new; lower is worse.
    pub current: u8,
    pub worst: u8,
    pub threshold: u8,
    /// Vendor-defined raw counter (48 bits).
    pub raw: u64,
    /// Current value at or below a non-zero threshold.
    pub failing_now: bool,
}

pub fn ata_attribute_name(id: u8) -> &'static str {
    match id {
        1 => "Raw read error rate",
        3 => "Spin-up time",
        4 => "Start/stop count",
        5 => "Reallocated sectors",
        7 => "Seek error rate",
        9 => "Power-on hours",
        10 => "Spin retry count",
        12 => "Power cycle count",
        170 => "Available reserved space",
        171 => "Program fail count",
        172 => "Erase fail count",
        173 | 177 => "Wear leveling count",
        174 => "Unexpected power loss",
        179 => "Used reserved blocks",
        181 => "Program fail count",
        182 => "Erase fail count",
        183 => "Runtime bad blocks",
        184 => "End-to-end errors",
        187 => "Reported uncorrectable errors",
        188 => "Command timeouts",
        189 => "High fly writes",
        190 => "Airflow temperature",
        191 => "G-sense errors",
        192 => "Power-off retract count",
        193 => "Load cycle count",
        194 => "Temperature",
        195 => "Hardware ECC recovered",
        196 => "Reallocation events",
        197 => "Current pending sectors",
        198 => "Offline uncorrectable sectors",
        199 => "Interface CRC errors",
        200 => "Write error rate",
        202 => "Percent lifetime used",
        231 => "SSD life left",
        232 => "Available reserved space",
        233 => "Media wearout indicator",
        241 => "Total LBAs written",
        242 => "Total LBAs read",
        _ => "Vendor specific",
    }
}

/// Parses the 512-byte SMART READ DATA buffer, joining thresholds when given.
pub fn parse_ata_smart(data: &[u8], thresholds: Option<&[u8]>) -> Vec<SmartAttribute> {
    let mut out = Vec::new();
    if data.len() < 362 {
        return out;
    }
    for i in 0..30 {
        let o = 2 + i * 12;
        let id = data[o];
        if id == 0 {
            continue;
        }
        let flags = u16::from_le_bytes([data[o + 1], data[o + 2]]);
        let current = data[o + 3];
        let worst = data[o + 4];
        let mut raw_bytes = [0u8; 8];
        raw_bytes[..6].copy_from_slice(&data[o + 5..o + 11]);
        let raw = u64::from_le_bytes(raw_bytes);
        let threshold = thresholds
            .and_then(|t| {
                (0..30).map(|j| 2 + j * 12).filter(|&p| p + 1 < t.len()).find(|&p| t[p] == id).map(|p| t[p + 1])
            })
            .unwrap_or(0);
        out.push(SmartAttribute {
            id,
            name: ata_attribute_name(id).to_string(),
            flags,
            current,
            worst,
            threshold,
            raw,
            failing_now: threshold > 0 && current <= threshold,
        });
    }
    out
}

/// Built-in self-test status (SMART data byte 363).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfTestStatus {
    pub code: u8,
    pub description: String,
    pub in_progress: bool,
    pub percent_remaining: Option<u8>,
    /// The last completed self-test found a fault.
    #[serde(default)]
    pub failed: bool,
}

pub fn parse_self_test_status(byte: u8) -> SelfTestStatus {
    let code = byte >> 4;
    let description = match code {
        0 => "Last self-test completed without error",
        1 => "Self-test aborted by the host",
        2 => "Self-test interrupted by a reset",
        3 => "Self-test could not complete (fatal error)",
        4 => "Self-test failed (unknown element)",
        5 => "Self-test failed (electrical element)",
        6 => "Self-test failed (servo / seek element)",
        7 => "Self-test failed (read element)",
        8 => "Self-test failed (handling damage suspected)",
        15 => "Self-test in progress",
        _ => "Reserved self-test status",
    };
    SelfTestStatus {
        code,
        description: description.to_string(),
        in_progress: code == 15,
        percent_remaining: (code == 15).then(|| (byte & 0x0F) * 10),
        failed: (3..=8).contains(&code),
    }
}

/// Parses the NVMe Device Self-test log (log page 06h, 564 bytes).
pub fn parse_nvme_self_test_log(data: &[u8]) -> Option<SelfTestStatus> {
    if data.len() < 32 {
        return None;
    }
    let current = data[0] & 0x0F;
    if current != 0 {
        let done = (data[1] & 0x7F).min(100);
        let kind = if current == 1 { "short" } else { "extended" };
        return Some(SelfTestStatus {
            code: 15,
            description: format!("{kind} self-test in progress"),
            in_progress: true,
            percent_remaining: Some(100 - done),
            failed: false,
        });
    }
    // Newest result is the first 28-byte entry at offset 4.
    let entry = data[4];
    let result = entry & 0x0F;
    let kind = match entry >> 4 {
        1 => "Short self-test",
        2 => "Extended self-test",
        0xE => "Vendor self-test",
        _ => "Self-test",
    };
    let description = match result {
        0x0 => format!("{kind} completed without error"),
        0x1 => format!("{kind} aborted by a later self-test command"),
        0x2 => format!("{kind} aborted by a controller reset"),
        0x3 => format!("{kind} aborted by namespace removal"),
        0x4 => format!("{kind} aborted by a format command"),
        0x5 => format!("{kind} failed with a fatal or unknown error"),
        0x6 => format!("{kind} completed with a failed segment"),
        0x7 => format!("{kind} completed with one or more failed segments"),
        0x8 => format!("{kind} aborted for an unknown reason"),
        0x9 => format!("{kind} aborted by a sanitize operation"),
        0xF => "No self-test has been run".to_string(),
        _ => format!("{kind}: reserved result code {result}"),
    };
    Some(SelfTestStatus {
        code: result,
        description,
        in_progress: false,
        percent_remaining: None,
        failed: matches!(result, 0x5..=0x7),
    })
}

/// Zoned-device capability from IDENTIFY word 69.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZonedCapability {
    NotReported,
    HostAware,
    DeviceManaged,
    Reserved,
}

/// The fields of ATA IDENTIFY DEVICE that the app uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtaIdentify {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    /// Nominal rotation rate; `None` when not reported.
    pub rotation_rate_rpm: Option<u16>,
    pub non_rotating: bool,
    pub trim_supported: bool,
    pub zoned: ZonedCapability,
    pub smart_supported: bool,
    pub smart_enabled: bool,
}

fn ata_string(data: &[u8], first_word: usize, words: usize) -> String {
    let mut bytes = Vec::with_capacity(words * 2);
    for w in first_word..first_word + words {
        bytes.push(data[w * 2 + 1]);
        bytes.push(data[w * 2]);
    }
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Parses the 512-byte IDENTIFY DEVICE buffer.
pub fn parse_ata_identify(data: &[u8]) -> Option<AtaIdentify> {
    if data.len() < 512 {
        return None;
    }
    let word = |n: usize| u16::from_le_bytes([data[n * 2], data[n * 2 + 1]]);
    let rotation = word(217);
    let zoned = match word(69) & 0b11 {
        0 => ZonedCapability::NotReported,
        1 => ZonedCapability::HostAware,
        2 => ZonedCapability::DeviceManaged,
        _ => ZonedCapability::Reserved,
    };
    Some(AtaIdentify {
        model: ata_string(data, 27, 20),
        serial: ata_string(data, 10, 10),
        firmware: ata_string(data, 23, 4),
        rotation_rate_rpm: (0x0401..=0xFFFE).contains(&rotation).then_some(rotation),
        non_rotating: rotation == 1,
        trim_supported: word(169) & 1 == 1,
        zoned,
        smart_supported: word(82) & 1 == 1,
        smart_enabled: word(85) & 1 == 1,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtaHealth {
    #[serde(default)]
    pub endurance_used: Option<u8>,
    #[serde(default)]
    pub smart_passed: Option<bool>,
    pub attributes: Vec<SmartAttribute>,
    pub self_test: Option<SelfTestStatus>,
    pub identify: Option<AtaIdentify>,
}

/// ATA Device Statistics page 7, first statistic. Requires supported, valid, normalized data.
pub fn parse_ata_endurance_page(data: &[u8]) -> Option<u8> {
    if data.get(2) != Some(&7) {
        return None;
    }
    let entry = u64::from_le_bytes(data.get(8..16)?.try_into().ok()?);
    let required = (1u64 << 63) | (1u64 << 62) | (1u64 << 61);
    ((entry & required) == required).then_some(entry as u8)
}

// ---------------------------------------------------------------- NVMe

/// NVMe SMART / Health Information log (log page 02h).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NvmeHealth {
    pub critical_warning: u8,
    pub temperature_c: i32,
    pub available_spare_percent: u8,
    pub available_spare_threshold_percent: u8,
    pub percentage_used: u8,
    /// In units of 512,000 bytes.
    pub data_units_read: u64,
    /// In units of 512,000 bytes.
    pub data_units_written: u64,
    pub host_read_commands: u64,
    pub host_write_commands: u64,
    pub power_cycles: u64,
    pub power_on_hours: u64,
    pub unsafe_shutdowns: u64,
    pub media_errors: u64,
    pub error_log_entries: u64,
}

impl NvmeHealth {
    pub fn bytes_written(&self) -> u64 {
        self.data_units_written.saturating_mul(512_000)
    }

    pub fn bytes_read(&self) -> u64 {
        self.data_units_read.saturating_mul(512_000)
    }
}

fn u128_at(data: &[u8], offset: usize) -> u64 {
    let v = u128::from_le_bytes(data[offset..offset + 16].try_into().expect("16 bytes"));
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// Parses the 512-byte NVMe health log.
pub fn parse_nvme_health(data: &[u8]) -> Option<NvmeHealth> {
    if data.len() < 192 {
        return None;
    }
    let kelvin = u16::from_le_bytes([data[1], data[2]]);
    Some(NvmeHealth {
        critical_warning: data[0],
        temperature_c: i32::from(kelvin) - 273,
        available_spare_percent: data[3],
        available_spare_threshold_percent: data[4],
        percentage_used: data[5],
        data_units_read: u128_at(data, 32),
        data_units_written: u128_at(data, 48),
        host_read_commands: u128_at(data, 64),
        host_write_commands: u128_at(data, 80),
        power_cycles: u128_at(data, 112),
        power_on_hours: u128_at(data, 128),
        unsafe_shutdowns: u128_at(data, 144),
        media_errors: u128_at(data, 160),
        error_log_entries: u128_at(data, 176),
    })
}

fn nvme_critical_warnings(bits: u8) -> Vec<&'static str> {
    let names = [
        "available spare below threshold",
        "temperature outside safe range",
        "reliability degraded by media errors",
        "media placed in read-only mode",
        "volatile memory backup failed",
        "persistent memory region read-only",
    ];
    names.iter().enumerate().filter(|(i, _)| bits & (1 << i) != 0).map(|(_, n)| *n).collect()
}

// ---------------------------------------------------------------- snapshot

/// Health at one moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub taken_at: DateTime<Utc>,
    pub source: HealthSource,
    /// Why no data is available, when `source` is `None`.
    pub unavailable_reason: Option<String>,
    pub temperature_c: Option<i32>,
    pub ata: Option<AtaHealth>,
    pub nvme: Option<NvmeHealth>,
    /// NVMe device self-test status (log page 06h).
    #[serde(default)]
    pub nvme_self_test: Option<SelfTestStatus>,
    /// SD card registers, for SD / MMC cards.
    #[serde(default)]
    pub sd: Option<crate::sd::SdInfo>,
}

impl HealthSnapshot {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            taken_at: Utc::now(),
            source: HealthSource::None,
            unavailable_reason: Some(reason.into()),
            temperature_c: None,
            ata: None,
            nvme: None,
            nvme_self_test: None,
            sd: None,
        }
    }

    pub fn from_ata(source: HealthSource, ata: AtaHealth) -> Self {
        let temp = ata
            .attributes
            .iter()
            .find(|a| a.id == 194)
            .or_else(|| ata.attributes.iter().find(|a| a.id == 190))
            .map(|a| (a.raw & 0xFF) as i32)
            .filter(|t| (1..=120).contains(t));
        Self {
            taken_at: Utc::now(),
            source,
            unavailable_reason: None,
            temperature_c: temp,
            ata: Some(ata),
            nvme: None,
            nvme_self_test: None,
            sd: None,
        }
    }

    pub fn from_nvme(nvme: NvmeHealth) -> Self {
        let temp = Some(nvme.temperature_c).filter(|t| (-40..=150).contains(t));
        Self {
            taken_at: Utc::now(),
            source: HealthSource::NvmeLog,
            unavailable_reason: None,
            temperature_c: temp,
            ata: None,
            nvme: Some(nvme),
            nvme_self_test: None,
            sd: None,
        }
    }

    pub fn is_available(&self) -> bool {
        self.ata.is_some() || self.nvme.is_some()
    }

    pub fn attribute(&self, id: u8) -> Option<&SmartAttribute> {
        self.ata.as_ref()?.attributes.iter().find(|a| a.id == id)
    }

    /// Self-test status from ATA SMART or the NVMe self-test log.
    pub fn self_test(&self) -> Option<&SelfTestStatus> {
        self.ata.as_ref().and_then(|a| a.self_test.as_ref()).or(self.nvme_self_test.as_ref())
    }

    pub fn identify(&self) -> Option<&AtaIdentify> {
        self.ata.as_ref()?.identify.as_ref()
    }

    /// Plain-language verdict with reasons.
    pub fn verdict(&self) -> HealthVerdict {
        let mut v = if let Some(n) = &self.nvme {
            let mut v = nvme_verdict(n);
            if let Some(st) = &self.nvme_self_test
                && st.failed
            {
                v.raise(VerdictLevel::Replace, st.description.clone());
            }
            v
        } else if let Some(a) = &self.ata {
            ata_verdict(a)
        } else {
            HealthVerdict {
                level: VerdictLevel::Unknown,
                reasons: vec![self.unavailable_reason.clone().unwrap_or_else(|| "No health data".into())],
                remaining_life_percent: None,
            }
        };
        if let Some(sd) = &self.sd {
            for w in &sd.warnings {
                v.raise(VerdictLevel::Caution, w.clone());
            }
        }
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictLevel {
    Unknown,
    Good,
    Caution,
    Replace,
}

impl VerdictLevel {
    pub fn label(self) -> &'static str {
        match self {
            VerdictLevel::Good => "Good",
            VerdictLevel::Caution => "Caution",
            VerdictLevel::Replace => "Replace",
            VerdictLevel::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthVerdict {
    pub level: VerdictLevel,
    pub reasons: Vec<String>,
    /// SSD life remaining, from the drive's own wear indicator.
    pub remaining_life_percent: Option<u8>,
}

impl HealthVerdict {
    fn raise(&mut self, level: VerdictLevel, reason: String) {
        self.level = self.level.max(level);
        self.reasons.push(reason);
    }
}

fn nvme_verdict(n: &NvmeHealth) -> HealthVerdict {
    let mut v = HealthVerdict {
        level: VerdictLevel::Good,
        reasons: Vec::new(),
        remaining_life_percent: Some(100u8.saturating_sub(n.percentage_used.min(100))),
    };
    for w in nvme_critical_warnings(n.critical_warning) {
        v.raise(VerdictLevel::Replace, format!("Drive reports a critical warning: {w}"));
    }
    if n.available_spare_percent < n.available_spare_threshold_percent {
        v.raise(
            VerdictLevel::Replace,
            format!(
                "Spare blocks at {}%, below the {}% threshold",
                n.available_spare_percent, n.available_spare_threshold_percent
            ),
        );
    }
    if n.percentage_used >= 100 {
        v.raise(VerdictLevel::Replace, format!("Rated endurance used: {}%", n.percentage_used));
    } else if n.percentage_used >= 80 {
        v.raise(VerdictLevel::Caution, format!("Rated endurance used: {}%", n.percentage_used));
    }
    if n.media_errors > 0 {
        v.raise(VerdictLevel::Caution, format!("{} media / data integrity errors recorded", n.media_errors));
    }
    if v.reasons.is_empty() {
        v.reasons.push(format!("No warnings; {}% of rated endurance used", n.percentage_used));
    }
    v
}

fn ata_verdict(a: &AtaHealth) -> HealthVerdict {
    let raw = |id: u8| a.attributes.iter().find(|x| x.id == id).map(|x| x.raw & 0xFFFF_FFFF).unwrap_or(0);
    let mut v = HealthVerdict { level: VerdictLevel::Good, reasons: Vec::new(), remaining_life_percent: None };
    if a.smart_passed == Some(false) {
        v.raise(VerdictLevel::Replace, "Drive reports failing SMART status".into());
    }

    for attr in a.attributes.iter().filter(|x| x.failing_now) {
        v.raise(VerdictLevel::Replace, format!("{} (ID {}) is at or below its failure threshold", attr.name, attr.id));
    }
    let reallocated = raw(5);
    let pending = raw(197);
    let uncorrectable = raw(198);
    if reallocated > 0 {
        v.raise(VerdictLevel::Caution, format!("{reallocated} reallocated sectors"));
    }
    if pending > 0 {
        v.raise(VerdictLevel::Caution, format!("{pending} sectors waiting to be reallocated"));
    }
    if uncorrectable > 0 {
        v.raise(VerdictLevel::Caution, format!("{uncorrectable} uncorrectable sectors"));
    }
    if reallocated + pending + uncorrectable >= 50 {
        v.raise(VerdictLevel::Replace, "Large number of bad sectors".into());
    }
    if raw(187) > 0 {
        v.raise(VerdictLevel::Caution, format!("{} uncorrectable errors reported to the host", raw(187)));
    }
    if raw(199) > 0 {
        v.raise(
            VerdictLevel::Caution,
            format!("{} interface CRC errors — usually a bad cable or connector, not the drive", raw(199)),
        );
    }
    if let Some(st) = &a.self_test
        && st.failed
    {
        v.raise(VerdictLevel::Replace, st.description.clone());
    }
    let life =
        [231u8, 233, 177].iter().find_map(|id| a.attributes.iter().find(|x| x.id == *id)).map(|x| x.current.min(100));
    v.remaining_life_percent = life;
    if let Some(l) = life
        && l <= 10
    {
        v.raise(VerdictLevel::Caution, format!("Wear indicator shows {l}% life remaining"));
    }
    if v.reasons.is_empty() {
        v.reasons.push("No SMART warnings".into());
    }
    v
}

// ---------------------------------------------------------------- before / after

/// A watched counter that rose during a test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthChange {
    pub metric: String,
    pub before: u64,
    pub after: u64,
}

const WATCHED_ATA: [u8; 7] = [5, 196, 197, 198, 187, 188, 199];

/// Counters that increased between two snapshots. A rise in reallocated,
/// pending or media-error counts during a test is a strong failure signal.
pub fn health_changes(before: &HealthSnapshot, after: &HealthSnapshot) -> Vec<HealthChange> {
    let mut out = Vec::new();
    for id in WATCHED_ATA {
        if let (Some(b), Some(a)) = (before.attribute(id), after.attribute(id)) {
            let (b, a) = (b.raw & 0xFFFF_FFFF, a.raw & 0xFFFF_FFFF);
            if a > b {
                out.push(HealthChange { metric: format!("{} (ID {id})", ata_attribute_name(id)), before: b, after: a });
            }
        }
    }
    if let (Some(b), Some(a)) = (&before.nvme, &after.nvme) {
        let pairs = [
            ("Media / data integrity errors", b.media_errors, a.media_errors),
            ("Error log entries", b.error_log_entries, a.error_log_entries),
            ("Critical warning flags", u64::from(b.critical_warning), u64::from(a.critical_warning)),
        ];
        for (name, b, a) in pairs {
            if a > b {
                out.push(HealthChange { metric: name.into(), before: b, after: a });
            }
        }
    }
    out
}

// ---------------------------------------------------------------- SMR / CMR

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingVerdict {
    Cmr,
    Smr,
    LikelySmr,
    Unknown,
    NotApplicable,
}

impl RecordingVerdict {
    pub fn label(self) -> &'static str {
        match self {
            RecordingVerdict::Cmr => "CMR",
            RecordingVerdict::Smr => "SMR",
            RecordingVerdict::LikelySmr => "Likely SMR",
            RecordingVerdict::Unknown => "Unknown",
            RecordingVerdict::NotApplicable => "Not applicable",
        }
    }
}

/// SMR/CMR verdict and the layer of evidence that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmrAssessment {
    pub verdict: RecordingVerdict,
    /// "write test", "self-report", "hint" or "none".
    pub layer: String,
    pub evidence: String,
}

impl SmrAssessment {
    pub fn headline(&self) -> String {
        match self.verdict {
            RecordingVerdict::NotApplicable | RecordingVerdict::Unknown => self.verdict.label().to_string(),
            _ => format!("{} (by {})", self.verdict.label(), self.layer),
        }
    }
}

/// Model-number prefixes that manufacturers have listed as drive-managed SMR.
/// A hint only: the list is not exhaustive.
const KNOWN_SMR_PREFIXES: [&str; 18] = [
    "WD20EFAX",
    "WD30EFAX",
    "WD40EFAX",
    "WD60EFAX",
    "WD10SPZX",
    "WD20SPZX",
    "ST2000DM008",
    "ST4000DM004",
    "ST6000DM003",
    "ST8000DM004",
    "ST2000LM015",
    "ST3000LM024",
    "ST4000LM024",
    "ST5000LM000",
    "ST8000AS0002",
    "HDWD240",
    "HDWD260",
    "DT02ABA",
];

pub fn model_on_smr_list(model: &str) -> bool {
    let m = model.to_uppercase();
    KNOWN_SMR_PREFIXES.iter().any(|p| m.split_whitespace().any(|w| w.starts_with(p)))
}

/// Combines the write test, the drive's self-report and hints.
pub fn assess_smr(
    media: MediaKind,
    model: &str,
    identify: Option<&AtaIdentify>,
    write_probe: Option<&crate::writetest::SmrProbeResult>,
) -> SmrAssessment {
    if media == MediaKind::Ssd || identify.is_some_and(|i| i.non_rotating) {
        return SmrAssessment {
            verdict: RecordingVerdict::NotApplicable,
            layer: "none".into(),
            evidence: "Solid-state drives do not use shingled recording".into(),
        };
    }
    if let Some(p) = write_probe.filter(|p| p.windows_mb_s.len() >= 4) {
        return if p.collapsed {
            SmrAssessment {
                verdict: RecordingVerdict::LikelySmr,
                layer: "write test".into(),
                evidence: format!(
                    "Suggestive only: random-write throughput fell from {:.1} MB/s to {:.1} MB/s{}",
                    p.early_mb_s,
                    p.late_mb_s,
                    p.collapse_after_bytes
                        .map(|b| format!(" after {}", crate::model::format_bytes(b)))
                        .unwrap_or_default()
                ),
            }
        } else {
            SmrAssessment {
                verdict: RecordingVerdict::Unknown,
                layer: "write test".into(),
                evidence: format!(
                    "Recording type remains unknown: random-write throughput held steady ({:.1} → {:.1} MB/s over {})",
                    p.early_mb_s,
                    p.late_mb_s,
                    crate::model::format_bytes(p.bytes_written)
                ),
            }
        };
    }
    if let Some(id) = identify {
        match id.zoned {
            ZonedCapability::HostAware => {
                return SmrAssessment {
                    verdict: RecordingVerdict::Smr,
                    layer: "self-report".into(),
                    evidence: "Drive reports host-aware zoned recording".into(),
                };
            }
            ZonedCapability::DeviceManaged => {
                return SmrAssessment {
                    verdict: RecordingVerdict::Smr,
                    layer: "self-report".into(),
                    evidence: "Drive reports device-managed zoned recording".into(),
                };
            }
            _ => {}
        }
        if id.trim_supported && !id.non_rotating {
            return SmrAssessment {
                verdict: RecordingVerdict::LikelySmr,
                layer: "hint".into(),
                evidence: "Spinning drive supports TRIM, typical of drive-managed SMR".into(),
            };
        }
    }
    if model_on_smr_list(model) {
        return SmrAssessment {
            verdict: RecordingVerdict::LikelySmr,
            layer: "hint".into(),
            evidence: "Model appears on manufacturers' published SMR lists".into(),
        };
    }
    SmrAssessment {
        verdict: RecordingVerdict::Unknown,
        layer: "none".into(),
        evidence: if media == MediaKind::Hdd {
            "No self-report or hints; the SMR write test can look for a sustained throughput drop".into()
        } else {
            "Media type unknown".into()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smart_buffer(attrs: &[(u8, u8, u64)]) -> (Vec<u8>, Vec<u8>) {
        let mut data = vec![0u8; 512];
        let mut thr = vec![0u8; 512];
        for (i, (id, current, raw)) in attrs.iter().enumerate() {
            let o = 2 + i * 12;
            data[o] = *id;
            data[o + 3] = *current;
            data[o + 4] = *current;
            data[o + 5..o + 11].copy_from_slice(&raw.to_le_bytes()[..6]);
            thr[o] = *id;
            thr[o + 1] = 10;
        }
        data[363] = 0x00;
        (data, thr)
    }

    #[test]
    fn ata_smart_parses_and_flags_problems() {
        let (data, thr) = smart_buffer(&[(5, 100, 8), (194, 100, 0x0028_0012_0026), (197, 100, 0), (9, 5, 1234)]);
        let attrs = parse_ata_smart(&data, Some(&thr));
        assert_eq!(attrs.len(), 4);
        assert_eq!(attrs[0].raw, 8);
        assert_eq!(attrs[0].threshold, 10);
        assert!(attrs[3].failing_now); // current 5 <= threshold 10
        let snap = HealthSnapshot::from_ata(
            HealthSource::AtaSmart,
            AtaHealth {
                endurance_used: None,
                smart_passed: None,
                attributes: attrs,
                self_test: Some(parse_self_test_status(data[363])),
                identify: None,
            },
        );
        assert_eq!(snap.temperature_c, Some(0x26));
        let v = snap.verdict();
        assert_eq!(v.level, VerdictLevel::Replace);
        assert!(v.reasons.iter().any(|r| r.contains("8 reallocated")));
    }

    #[test]
    fn self_test_status_decodes() {
        let s = parse_self_test_status(0xF3);
        assert!(s.in_progress);
        assert_eq!(s.percent_remaining, Some(30));
        assert_eq!(parse_self_test_status(0x70).code, 7);
    }

    #[test]
    fn identify_decodes_strings_and_flags() {
        let mut d = vec![0u8; 512];
        let put = |d: &mut Vec<u8>, word: usize, s: &str| {
            for (i, pair) in s.as_bytes().chunks(2).enumerate() {
                d[(word + i) * 2 + 1] = pair[0];
                d[(word + i) * 2] = *pair.get(1).unwrap_or(&b' ');
            }
        };
        put(&mut d, 27, "WDC WD40EFAX-68JH4N1                    ");
        put(&mut d, 10, "WD-ABC123           ");
        d[217 * 2..217 * 2 + 2].copy_from_slice(&5400u16.to_le_bytes());
        d[169 * 2] = 1;
        let id = parse_ata_identify(&d).unwrap();
        assert_eq!(id.model, "WDC WD40EFAX-68JH4N1");
        assert_eq!(id.serial, "WD-ABC123");
        assert_eq!(id.rotation_rate_rpm, Some(5400));
        assert!(id.trim_supported);
        let a = assess_smr(MediaKind::Hdd, &id.model, Some(&id), None);
        assert_eq!(a.verdict, RecordingVerdict::LikelySmr);
    }

    #[test]
    fn nvme_log_parses() {
        let mut d = vec![0u8; 512];
        d[1..3].copy_from_slice(&313u16.to_le_bytes());
        d[3] = 100;
        d[4] = 10;
        d[5] = 85;
        d[48] = 0x10; // 16 data units written
        d[160] = 2; // media errors
        let n = parse_nvme_health(&d).unwrap();
        assert_eq!(n.temperature_c, 40);
        assert_eq!(n.data_units_written, 16);
        assert_eq!(n.bytes_written(), 16 * 512_000);
        let v = HealthSnapshot::from_nvme(n).verdict();
        assert_eq!(v.level, VerdictLevel::Caution);
        assert_eq!(v.remaining_life_percent, Some(15));
    }

    #[test]
    fn changes_only_report_increases() {
        let (b, thr) = smart_buffer(&[(5, 100, 1), (197, 100, 3)]);
        let (a, _) = smart_buffer(&[(5, 100, 4), (197, 100, 0)]);
        let snap = |d: &[u8]| {
            HealthSnapshot::from_ata(
                HealthSource::AtaSmart,
                AtaHealth {
                    endurance_used: None,
                    smart_passed: None,
                    attributes: parse_ata_smart(d, Some(&thr)),
                    self_test: None,
                    identify: None,
                },
            )
        };
        let changes = health_changes(&snap(&b), &snap(&a));
        assert_eq!(changes.len(), 1);
        assert_eq!((changes[0].before, changes[0].after), (1, 4));
    }

    #[test]
    fn nvme_self_test_log_decodes() {
        let mut log = vec![0u8; 564];
        log[0] = 2;
        log[1] = 40;
        let s = parse_nvme_self_test_log(&log).unwrap();
        assert!(s.in_progress);
        assert_eq!(s.percent_remaining, Some(60));
        log[0] = 0;
        log[4] = 0x17; // short test, failed segments
        let s = parse_nvme_self_test_log(&log).unwrap();
        assert!(!s.in_progress && s.failed);
        log[4] = 0x20;
        assert!(!parse_nvme_self_test_log(&log).unwrap().failed);
    }

    #[test]
    fn ssd_is_never_smr() {
        assert_eq!(assess_smr(MediaKind::Ssd, "WD40EFAX", None, None).verdict, RecordingVerdict::NotApplicable);
    }
}
