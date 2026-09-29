use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use tracing::{debug, warn};

// POLL_MS_MIN reuses public UI constant to avoid duplication.
pub const POLL_MS_MIN: u64 = crate::style::POLL_RATE_MIN_MS as u64;
pub const POLL_MS_MAX: u64 = 2000;
pub const UI_REFRESH_MS_MIN: u64 = 50;
pub const UI_REFRESH_MS_MAX: u64 = 1000;
const DUTY_PCT_MIN: u32 = 0;
const DUTY_PCT_MAX: u32 = 100;
pub const CURVE_POLL_MS_MIN: u64 = 500;
pub const CURVE_POLL_MS_MAX: u64 = 5000;
const HYSTERESIS_C_MAX: u32 = 10;
const RATE_LIMIT_MIN: u32 = 1;
const RATE_LIMIT_MAX: u32 = 100;
/// Maximum curve domain temperature (°C); extends legacy 0–100 to avoid pinning.
pub const CURVE_TEMP_MAX: u32 = 110;
/// Start of locked 100% zone; 100–110°C is forced 100% and not editable.
pub const CURVE_TEMP_LOCK_START: u32 = 100;
/// Max editable temperature (lock start is fixed).
pub const CURVE_TEMP_EDIT_MAX: u32 = CURVE_TEMP_LOCK_START - 1;

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
    /// Clamps all values to safe ranges to prevent hardware damage or instability.
    pub fn validate(&mut self) {
        self.telemetry.poll_ms = self.telemetry.poll_ms.clamp(POLL_MS_MIN, POLL_MS_MAX);
        self.telemetry.ui_refresh_ms = self
            .telemetry
            .ui_refresh_ms
            .clamp(UI_REFRESH_MS_MIN, UI_REFRESH_MS_MAX);

        if let Some(ref mut manual) = self.fan.manual {
            manual.duty_pct = manual.duty_pct.clamp(DUTY_PCT_MIN, DUTY_PCT_MAX);
        }
        if let Some(ref mut curve) = self.fan.curve {
            curve.poll_ms = curve.poll_ms.clamp(CURVE_POLL_MS_MIN, CURVE_POLL_MS_MAX);
            curve.curve.hysteresis_c = curve.curve.hysteresis_c.min(HYSTERESIS_C_MAX);
            curve.curve.rate_limit_pct_per_step = curve
                .curve
                .rate_limit_pct_per_step
                .clamp(RATE_LIMIT_MIN, RATE_LIMIT_MAX);
            if let Some(ref mut down) = curve.curve.rate_limit_down_pct_per_step {
                *down = (*down).clamp(RATE_LIMIT_MIN, RATE_LIMIT_MAX);
            }
            if curve.curve.points.is_empty() {
                // Empty points leaves editor undraggable; restore defaults.
                curve.curve.points = default_points();
                debug!("Curve points were empty — restored default points");
            }
            if curve.curve.sensors.len() > 1 {
                // Curve uses single sensor; migrate old multi-select, prefer non-battery.
                if let Some(idx) = curve
                    .curve
                    .sensors
                    .iter()
                    .position(|s| !is_battery_sensor(s))
                {
                    let keep = curve.curve.sensors[idx].clone();
                    debug!(
                        "Curve sensors were multi-select — kept {} (Battery removed)",
                        keep
                    );
                    curve.curve.sensors = vec![keep];
                } else {
                    curve.curve.sensors.truncate(1);
                    debug!(
                        "Curve sensors were multi-select — kept first ({})",
                        curve.curve.sensors[0]
                    );
                }
            }
            for point in &mut curve.curve.points {
                // Clamp to plot domain to keep points drawable.
                point[0] = point[0].clamp(0, CURVE_TEMP_MAX);
                point[1] = point[1].clamp(0, 100);
            }
            // Dedupe by temp keeping highest duty; use BTreeMap to share
            // dedupe/sort semantics with `curve_full_points` and avoid drift.
            let mut map: BTreeMap<u32, u32> = BTreeMap::new();
            for &[t, d] in &curve.curve.points {
                map.entry(t).and_modify(|e| *e = (*e).max(d)).or_insert(d);
            }
            let deduped: Vec<[u32; 2]> = map.into_iter().map(|(t, d)| [t, d]).collect();
            // Locked zone: 100–110°C must be 100% fixed, remove any editable points in that range.
            let mut filtered: Vec<[u32; 2]> = Vec::new();
            for &[t, d] in &deduped {
                if t < CURVE_TEMP_LOCK_START {
                    filtered.push([t, d]);
                }
            }
            if filtered.is_empty() {
                // Keep at least one editable point before lock; fallback to defaults truncated
                let defaults = default_points();
                for p in defaults {
                    if p[0] < CURVE_TEMP_LOCK_START {
                        filtered.push(p);
                    }
                }
            }
            filtered.sort_by_key(|p| p[0]);
            // Migrate old 4-point configs to 5 editable points (5+2 locked =7 total)
            if filtered.len() == 4 {
                let desired = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
                for &[t, d] in &desired {
                    if filtered.len() >= 5 {
                        break;
                    }
                    if !filtered.iter().any(|p| p[0] == t) {
                        filtered.push([t, d]);
                    }
                }
                filtered.sort_by_key(|p| p[0]);
            }
            // Downgrade 6-point configs (previous 92°C addition) back to 5
            if filtered.len() == 6 {
                if let Some(pos) = filtered.iter().position(|p| p[0] == 92) {
                    filtered.remove(pos);
                } else {
                    filtered.truncate(5);
                }
            } else if filtered.len() > 6 {
                let desired = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
                let mut trimmed: Vec<[u32; 2]> = Vec::new();
                for &[t, _] in &desired {
                    if let Some(&p) = filtered.iter().find(|p| p[0] == t) {
                        trimmed.push(p);
                    }
                }
                for &p in &filtered {
                    if trimmed.len() >= 5 {
                        break;
                    }
                    if !trimmed.contains(&p) {
                        trimmed.push(p);
                    }
                }
                trimmed.sort_by_key(|p| p[0]);
                trimmed.truncate(5);
                filtered = trimmed;
            }
            // Append locked endpoints
            filtered.push([CURVE_TEMP_LOCK_START, 100]);
            filtered.push([CURVE_TEMP_MAX, 100]);
            curve.curve.points = filtered;
        }
        for duty in &mut self.fan.per_fan_duty {
            *duty = (*duty).clamp(DUTY_PCT_MIN, DUTY_PCT_MAX);
        }

        if let Some(ref mut limit) = self.battery.charge_limit_max_pct {
            limit.value = limit.value.clamp(
                crate::style::CHARGE_LIMIT_MIN as u8,
                crate::style::CHARGE_LIMIT_MAX as u8,
            );
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
                warn!(
                    "Unknown FanControlMode value: {}, defaulting to Disabled",
                    other
                );
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FanControlConfig {
    #[serde(default)]
    pub mode: FanControlMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual: Option<ManualConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curve: Option<GlobalCurveConfig>,
    #[serde(default = "default_true")]
    pub unified_duty: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub per_fan_duty: Vec<u32>,
}

impl Default for FanControlConfig {
    fn default() -> Self {
        Self {
            mode: FanControlMode::default(),
            manual: None,
            curve: None,
            unified_duty: true,
            per_fan_duty: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManualConfig {
    /// Missing duty defaults to 0% (safe neutral) instead of rejecting file.
    #[serde(default)]
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
    // 5 editable points + 2 locked (100/110) = 7 total
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
    // BTreeMap sorted by temp; keep max duty on duplicates to match validate.
    let mut map: BTreeMap<u32, u32> = BTreeMap::new();
    let has_zero = points.iter().any(|p| p[0] == 0);
    if !has_zero {
        map.insert(0, 0);
    }
    for &[temp, duty] in points {
        // Locked zone points are forced; ignore user duty there, will be overwritten.
        if temp >= CURVE_TEMP_LOCK_START {
            continue;
        }
        map.entry(temp)
            .and_modify(|e| *e = (*e).max(duty))
            .or_insert(duty);
    }
    // Force locked zone 100–110°C to 100% (user cannot change).
    map.insert(CURVE_TEMP_LOCK_START, 100);
    map.insert(CURVE_TEMP_MAX, 100);
    let full: Vec<[u32; 2]> = map.into_iter().map(|(t, d)| [t, d]).collect();
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
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub value: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct BatteryConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charge_limit_max_pct: Option<SettingU8>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BatteryInfo {
    pub power_info: crate::cli::ec_wrapper::BatteryData,
}

pub fn is_battery_sensor(name: &str) -> bool {
    name.eq_ignore_ascii_case("battery")
}

/// Temp driving fan curve: configured sensor or hottest non-battery fallback.
///
/// Deliberately prefers a real thermal sensor over Battery: battery
/// temperature lags and can sit far below CPU temperature. Battery is only
/// used when it is literally the only reading available, which beats leaving
/// the fans at their last duty with no control input at all.
pub fn curve_control_temp(temps: &BTreeMap<String, i32>, sensors: &[String]) -> i32 {
    let non_battery = || {
        temps
            .iter()
            .filter(|(name, _)| !is_battery_sensor(name))
            .map(|(_, t)| *t)
            .max()
    };
    if sensors.is_empty() {
        return non_battery()
            .or_else(|| temps.values().copied().max())
            .unwrap_or(0);
    }
    sensors
        .iter()
        .filter_map(|s| temps.get(s).copied())
        .max()
        .or_else(non_battery)
        .or_else(|| temps.values().copied().max())
        .unwrap_or(0)
}

pub fn battery_health_pct(last_full_mah: u32, design_mah: u32) -> Option<u32> {
    if design_mah == 0 {
        return None;
    }
    Some((((last_full_mah as f32 / design_mah as f32) * 100.0).round() as u32).min(100))
}

pub fn sorted_sensor_list(selected: &[String], sensor_keys: &[String]) -> Vec<String> {
    let fallback = sensor_keys.len();
    let pos_map: HashMap<&str, usize> = sensor_keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    let mut list: Vec<String> = if selected.is_empty() {
        sensor_keys.to_vec()
    } else {
        selected
            .iter()
            .filter(|s| sensor_keys.contains(s))
            .cloned()
            .collect()
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
    fn validate_manual_duty_pct_zero_unchanged() {
        let mut c = Config::default();
        c.fan.manual = Some(ManualConfig { duty_pct: 0 });
        c.validate();
        assert_eq!(c.fan.manual.as_ref().unwrap().duty_pct, 0);
    }

    #[test]
    fn curve_control_temp_skips_battery_by_default() {
        let mut temps = BTreeMap::new();
        temps.insert("F75303_CPU".into(), 55);
        temps.insert("Battery".into(), 90);
        temps.insert("PECI".into(), 48);
        assert_eq!(curve_control_temp(&temps, &[]), 55);
    }

    #[test]
    fn curve_control_temp_uses_configured_sensors() {
        let mut temps = BTreeMap::new();
        temps.insert("F75303_CPU".into(), 55);
        temps.insert("PECI".into(), 70);
        assert_eq!(curve_control_temp(&temps, &["PECI".into()]), 70);
    }

    #[test]
    fn battery_health_pct_from_full_charge() {
        assert_eq!(battery_health_pct(4500, 5000), Some(90));
        assert_eq!(battery_health_pct(5000, 5000), Some(100));
        assert_eq!(battery_health_pct(3000, 5000), Some(60));
        assert_eq!(battery_health_pct(1000, 0), None);
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

    #[test]
    fn validate_curve_points_clamp() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig {
                sensors: vec![],
                points: vec![[150, 200], [40, 10]],
                hysteresis_c: 2,
                rate_limit_pct_per_step: 10,
                rate_limit_down_pct_per_step: None,
            },
            poll_ms: 1000,
        });
        c.validate();
        let pts = &c.fan.curve.as_ref().unwrap().curve.points;
        assert_eq!(pts[0], [40, 10]);
        assert_eq!(pts[1], [100, 100]);
        assert_eq!(pts[2], [110, 100]);
    }

    #[test]
    fn validate_empty_curve_points_restores_defaults() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig {
                sensors: vec![],
                points: vec![],
                hysteresis_c: 2,
                rate_limit_pct_per_step: 10,
                rate_limit_down_pct_per_step: None,
            },
            poll_ms: 1000,
        });
        c.validate();
        let pts = &c.fan.curve.as_ref().unwrap().curve.points;
        let mut expected = default_points();
        expected.push([CURVE_TEMP_LOCK_START, 100]);
        expected.push([CURVE_TEMP_MAX, 100]);
        assert_eq!(pts, &expected);
        assert!(!pts.is_empty());
    }

    #[test]
    fn validate_truncates_curve_sensors_to_single_selection() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig {
                sensors: vec!["CPU".into(), "Battery".into(), "SSD".into()],
                points: default_points(),
                hysteresis_c: 2,
                rate_limit_pct_per_step: 10,
                rate_limit_down_pct_per_step: None,
            },
            poll_ms: 1000,
        });
        c.validate();
        let sensors = &c.fan.curve.as_ref().unwrap().curve.sensors;
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0], "CPU");
    }

    #[test]
    fn validate_migrates_4_points_to_5() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig {
                sensors: vec![],
                points: vec![[30, 0], [45, 20], [60, 40], [75, 80]],
                hysteresis_c: 2,
                rate_limit_pct_per_step: 10,
                rate_limit_down_pct_per_step: None,
            },
            poll_ms: 1000,
        });
        c.validate();
        let pts = &c.fan.curve.as_ref().unwrap().curve.points;
        // 4 editable -> 5 editable +2 locked =7 total
        assert_eq!(pts.len(), 7);
        assert!(pts.contains(&[85, 100]));
        assert!(pts.contains(&[100, 100]));
        assert!(pts.contains(&[110, 100]));
    }

    #[test]
    fn validate_downgrades_6_with_92_to_5() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig {
                sensors: vec![],
                points: vec![[30, 0], [45, 20], [60, 40], [75, 80], [85, 100], [92, 100]],
                hysteresis_c: 2,
                rate_limit_pct_per_step: 10,
                rate_limit_down_pct_per_step: None,
            },
            poll_ms: 1000,
        });
        c.validate();
        let pts = &c.fan.curve.as_ref().unwrap().curve.points;
        // 6 with 92 -> 5+2=7, 92 removed
        assert_eq!(pts.len(), 7);
        assert!(!pts.contains(&[92, 100]));
        assert!(pts.contains(&[85, 100]));
    }

    #[test]
    fn validate_locked_zone_dropped() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig {
                sensors: vec![],
                points: vec![[30, 0], [101, 20], [105, 50]],
                hysteresis_c: 2,
                rate_limit_pct_per_step: 10,
                rate_limit_down_pct_per_step: None,
            },
            poll_ms: 1000,
        });
        c.validate();
        let pts = &c.fan.curve.as_ref().unwrap().curve.points;
        assert!(!pts.iter().any(|p| p[0] == 101));
        assert!(!pts.iter().any(|p| p[0] == 105));
        assert!(pts.contains(&[100, 100]));
        assert!(pts.contains(&[110, 100]));
    }

    #[test]
    fn curve_full_points_drops_locked_input() {
        let pts = curve_full_points(&[[30, 0], [101, 20], [60, 40]]);
        assert!(!pts.iter().any(|p| p[0] == 101));
        assert!(pts.contains(&[100, 100]));
        assert!(pts.contains(&[110, 100]));
    }
}
