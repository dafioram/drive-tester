//! The health score from install-healthcheck.sh, using native drive telemetry.

use serde::{Deserialize, Serialize};

use crate::health::HealthSnapshot;
use crate::model::MediaKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreStatus {
    Good,
    Caution,
    Bad,
}

impl ScoreStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Good => "Good",
            Self::Caution => "Caution",
            Self::Bad => "Bad",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthScore {
    pub percent: u8,
    pub status: ScoreStatus,
    pub base_percent: u8,
    pub base_source: String,
    pub adjustments: Vec<String>,
}

struct Calculator {
    score: HealthScore,
    value: f64,
    force_caution: bool,
}

impl Calculator {
    fn new(base_percent: u8, source: impl Into<String>) -> Self {
        Self {
            score: HealthScore {
                percent: 0,
                status: ScoreStatus::Good,
                base_percent,
                base_source: source.into(),
                adjustments: Vec::new(),
            },
            value: f64::from(base_percent),
            force_caution: false,
        }
    }

    fn penalty(&mut self, count: u64, weight: f64, cap: f64, label: &str) {
        if count == 0 {
            return;
        }
        let points = (weight * (count as f64).ln_1p()).min(cap);
        self.value *= 1.0 - points / 100.0;
        self.score.adjustments.push(format!("-{points:.0} {label} ({count})"));
    }

    fn age(&mut self, hours: u64) {
        let years = (hours as f64 / 8766.0 * 100.0).round_ties_even() / 100.0;
        let points = ((years - 3.0) * 2.0).clamp(0.0, 20.0);
        if points > 0.0 {
            self.value *= 1.0 - points / 100.0;
            self.score.adjustments.push(format!("-{points:.0} age ({years:.1} years)"));
        }
    }

    fn cap(&mut self, ceiling: u8, reason: &str) {
        if self.value > f64::from(ceiling) {
            self.value = f64::from(ceiling);
            self.score.adjustments.push(format!("Capped at {ceiling}%: {reason}"));
        }
    }

    fn finish(mut self) -> HealthScore {
        self.score.percent = self.value.clamp(0.0, 100.0).round_ties_even() as u8;
        self.score.status = match self.score.percent {
            70..=100 if !self.force_caution => ScoreStatus::Good,
            30..=100 => ScoreStatus::Caution,
            _ => ScoreStatus::Bad,
        };
        self.score
    }
}

impl HealthSnapshot {
    pub fn health_score(&self, media: MediaKind) -> Option<HealthScore> {
        if let Some(nvme) = &self.nvme {
            let mut calc = Calculator::new(
                100u8.saturating_sub(nvme.percentage_used),
                format!("Percentage Used = {}%", nvme.percentage_used),
            );
            calc.penalty(nvme.media_errors, 8.0, 40.0, "media errors");
            calc.force_caution = nvme.media_errors > 0;
            if nvme.critical_warning & 0x0c != 0 {
                calc.cap(10, "critical warning: reliability degraded or read-only");
            } else if nvme.critical_warning & 1 != 0
                || nvme.available_spare_percent < nvme.available_spare_threshold_percent
            {
                calc.cap(20, "available spare below threshold");
            }
            return Some(calc.finish());
        }

        let ata = self.ata.as_ref()?;
        if ata.attributes.is_empty()
            || ata.smart_passed == Some(false)
            || ata.attributes.iter().any(|attribute| attribute.failing_now && attribute.flags & 1 != 0)
        {
            return None;
        }
        // ATA IDs are vendor-specific. Use an explicit remaining-life label where
        // available and prefer the standardized Device Statistics endurance value.
        let life = ata
            .attributes
            .iter()
            .find(|a| a.id == 231 && a.current <= 100 && a.name.eq_ignore_ascii_case("SSD life left"));
        let ssd = if let Some(id) = &ata.identify {
            if id.non_rotating {
                true
            } else if id.rotation_rate_rpm.is_some() {
                false
            } else {
                media == MediaKind::Ssd || life.is_some() || ata.endurance_used.is_some()
            }
        } else {
            media == MediaKind::Ssd || life.is_some() || ata.endurance_used.is_some()
        };
        let raw = |id| self.attribute(id).map_or(0, |a| a.raw);
        let pending = raw(197);
        let offline = raw(198);
        let mut calc = if ssd {
            let mut calc = match (ata.endurance_used, life) {
                (Some(used), _) => Calculator::new(100u8.saturating_sub(used), "device statistics"),
                (None, Some(attr)) => Calculator::new(attr.current, format!("attribute {} {}", attr.id, attr.name)),
                _ => return None,
            };
            calc.penalty(raw(5), 3.0, 30.0, "retired/reallocated blocks");
            calc.penalty(raw(187), 8.0, 40.0, "uncorrectable reads");
            calc.penalty(pending, 10.0, 50.0, "pending sectors");
            calc.penalty(offline, 10.0, 50.0, "offline uncorrectable");
            calc
        } else {
            let mut calc = Calculator::new(100, "new drive");
            calc.penalty(raw(5), 6.0, 40.0, "reallocated sectors");
            calc.penalty(pending, 10.0, 50.0, "pending sectors");
            calc.penalty(offline, 10.0, 50.0, "offline uncorrectable");
            calc.penalty(raw(187), 8.0, 40.0, "uncorrectable reads");
            let timeout = raw(188);
            if timeout <= 65535 {
                calc.penalty(timeout, 2.0, 15.0, "command timeouts");
            }
            calc.age(raw(9));
            calc
        };
        calc.force_caution = pending > 0 || offline > 0;
        Some(calc.finish())
    }
}
