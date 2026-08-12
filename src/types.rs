use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, warn};

// Validation constants for config values
// POLL_MS_MIN reuses the public UI constant to avoid duplication.
const POLL_MS_MIN: u64 = crate::style::POLL_RATE_MIN_MS as u64;
const POLL_MS_MAX: u64 = 2000;
const UI_REFRESH_MS_MIN: u64 = 50;
const UI_REFRESH_MS_MAX: u64 = 1000;
const DUTY_PCT_MIN: u32 = 10;
const DUTY_PCT_MAX: u32 = 100;
const CURVE_POLL_MS_MIN: u64 = 500;
const CURVE_POLL_MS_MAX: u64 = 10000;
const HYSTERESIS_C_MAX: u32 = 10;
const RATE_LIMIT_MIN: u32 = 1;
const RATE_LIMIT_MAX: u32 = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Config {
    #[serde(default)]
    pub fan: FanControlConfig,
    #[serde(default)]
    pub battery: BatteryConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
}

impl Config {
    /// Validate and clamp all config values to their allowed ranges.
    ///
    /// This ensures the config file never contains values that could cause
    /// hardware damage or application instability.
    pub fn validate(&mut self) {
        // Telemetry settings
        self.telemetry.poll_ms = self.telemetry.poll_ms.clamp(POLL_MS_MIN, POLL_MS_MAX);
        self.telemetry.ui_refresh_ms = self.telemetry.ui_refresh_ms.clamp(UI_REFRESH_MS_MIN, UI_REFRESH_MS_MAX);

        // Fan control settings
        if let Some(ref mut manual) = self.fan.manual {
            manual.duty_pct = manual.duty_pct.clamp(DUTY_PCT_MIN, DUTY_PCT_MAX);
        }
        if let Some(ref mut curve) = self.fan.curve {
            curve.poll_ms = curve.poll_ms.clamp(CURVE_POLL_MS_MIN, CURVE_POLL_MS_MAX);
            curve.curve.hysteresis_c = curve.curve.hysteresis_c.min(HYSTERESIS_C_MAX);
            curve.curve.rate_limit_pct_per_step = curve.curve.rate_limit_pct_per_step.clamp(RATE_LIMIT_MIN, RATE_LIMIT_MAX);
            if let Some(ref mut down) = curve.curve.rate_limit_down_pct_per_step {
                *down = (*down).clamp(RATE_LIMIT_MIN, RATE_LIMIT_MAX);
            }
        }

        // Battery settings
        if let Some(ref mut limit) = self.battery.charge_limit_max_pct {
            limit.value = limit.value.clamp(crate::style::CHARGE_LIMIT_MIN as u8, crate::style::CHARGE_LIMIT_MAX as u8);
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum FanControlMode {
    #[default]
    Disabled = 0,
    Manual = 1,
    Curve = 2,
}

impl FanControlMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Disabled,
            1 => Self::Manual,
            2 => Self::Curve,
            other => {
                warn!("Unknown FanControlMode value: {}, defaulting to Disabled", other);
                Self::Disabled
            }
        }
    }
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::Manual => 1,
            Self::Curve => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct FanControlConfig {
    #[serde(default)]
    pub mode: FanControlMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual: Option<ManualConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curve: Option<GlobalCurveConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManualConfig {
    pub duty_pct: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct CurveConfig {
    #[serde(default)]
    pub sensors: Vec<String>,
    #[serde(default = "default_points")]
    pub points: Vec<[u32; 2]>,
    #[serde(default = "default_hysteresis_c")]
    pub hysteresis_c: u32,
    #[serde(default = "default_rate_limit_pct_per_step")]
    pub rate_limit_pct_per_step: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_down_pct_per_step: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct GlobalCurveConfig {
    #[serde(flatten)]
    pub curve: CurveConfig,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
}

fn default_points() -> Vec<[u32; 2]> {
    vec![[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]]
}
fn default_poll_ms() -> u64 {
    500
}
fn default_hysteresis_c() -> u32 {
    2
}
fn default_rate_limit_pct_per_step() -> u32 {
    10
}

pub fn curve_full_points(points: &[[u32; 2]]) -> Vec<[u32; 2]> {
    let mut full = Vec::with_capacity(points.len() + 2);
    let has_zero = points.iter().any(|p| p[0] == 0);
    let has_hundred = points.iter().any(|p| p[0] == 100);
    if !has_zero { full.push([0, 0]); }
    full.extend(points.iter().copied());
    if !has_hundred { full.push([100, 100]); }
    full.sort_by_key(|p| p[0]);
    let before = full.len();
    full.dedup_by_key(|p| p[0]);
    if full.len() < before {
        debug!("Curve has duplicate temperatures — later points override earlier ones");
    }
    full
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub poll_ms: u64,
    #[serde(default)]
    pub ui_refresh_ms: u64,
    #[serde(default)]
    pub selected_sensors: Vec<String>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            poll_ms: 500,
            ui_refresh_ms: 500,
            selected_sensors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
pub struct SettingU8 {
    pub enabled: bool,
    pub value: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct BatteryConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charge_limit_max_pct: Option<SettingU8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BatteryInfo {
    pub power_info: crate::cli::ec_wrapper::BatteryData,
}

pub fn sorted_sensor_list(selected: &[String], sensor_keys: &[String]) -> Vec<String> {
    let fallback = sensor_keys.len();
    let pos_map: HashMap<&str, usize> = sensor_keys.iter()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    let mut list: Vec<String> = if selected.is_empty() {
        sensor_keys.to_vec()
    } else {
        selected.iter().filter(|s| sensor_keys.contains(s)).cloned().collect()
    };
    list.sort_by_key(|a| *pos_map.get(a.as_str()).unwrap_or(&fallback));
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_poll_ms_below_min_clamps() {
        let mut c = Config::default();
        c.telemetry.poll_ms = 50;
        c.validate();
        assert_eq!(c.telemetry.poll_ms, 200);
    }

    #[test]
    fn validate_poll_ms_above_min_unchanged() {
        let mut c = Config::default();
        c.telemetry.poll_ms = 1000;
        c.validate();
        assert_eq!(c.telemetry.poll_ms, 1000);
    }

    #[test]
    fn validate_ui_refresh_ms_below_min_clamps() {
        let mut c = Config::default();
        c.telemetry.ui_refresh_ms = 10;
        c.validate();
        assert_eq!(c.telemetry.ui_refresh_ms, 50);
    }

    #[test]
    fn validate_ui_refresh_ms_above_max_clamps() {
        let mut c = Config::default();
        c.telemetry.ui_refresh_ms = 5000;
        c.validate();
        assert_eq!(c.telemetry.ui_refresh_ms, 1000);
    }

    #[test]
    fn validate_ui_refresh_ms_in_range_unchanged() {
        let mut c = Config::default();
        c.telemetry.ui_refresh_ms = 500;
        c.validate();
        assert_eq!(c.telemetry.ui_refresh_ms, 500);
    }

    #[test]
    fn validate_manual_duty_pct_above_100_clamps() {
        let mut c = Config::default();
        c.fan.manual = Some(ManualConfig { duty_pct: 200 });
        c.validate();
        assert_eq!(c.fan.manual.as_ref().unwrap().duty_pct, 100);
    }

    #[test]
    fn validate_manual_duty_pct_below_10_clamps() {
        let mut c = Config::default();
        c.fan.manual = Some(ManualConfig { duty_pct: 5 });
        c.validate();
        assert_eq!(c.fan.manual.as_ref().unwrap().duty_pct, 10);
    }

    #[test]
    fn validate_manual_duty_pct_in_range_unchanged() {
        let mut c = Config::default();
        c.fan.manual = Some(ManualConfig { duty_pct: 60 });
        c.validate();
        assert_eq!(c.fan.manual.as_ref().unwrap().duty_pct, 60);
    }

    #[test]
    fn validate_curve_poll_ms_below_min_clamps() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig::default(),
            poll_ms: 100,
        });
        c.validate();
        assert_eq!(c.fan.curve.as_ref().unwrap().poll_ms, 500);
    }

    #[test]
    fn validate_curve_poll_ms_above_min_unchanged() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig::default(),
            poll_ms: 2000,
        });
        c.validate();
        assert_eq!(c.fan.curve.as_ref().unwrap().poll_ms, 2000);
    }

    #[test]
    fn validate_no_manual_no_curve_no_panic() {
        let mut c = Config::default();
        c.validate();
        assert!(c.fan.manual.is_none());
        assert!(c.fan.curve.is_none());
    }
}
