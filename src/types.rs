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
    /// UI theme id ("default", "light", "dracula", "nord", "black",
    /// "gruvbox", "mocha", "nightfly"). Unknown values fall back to
    /// default on load so a typo never breaks the UI.
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_theme() -> String {
    "default".to_string()
}

impl Config {
    /// Clamps all values to safe ranges to prevent hardware damage or instability.
    pub fn validate(&mut self) {
        if crate::style::ThemeId::parse(&self.theme).is_none() {
            self.theme = default_theme();
        }
        // Bound vec lengths first: a crafted TOML with millions of entries
        // would otherwise spin the loops below (DoS via config file).
        const MAX_SENSORS: usize = 64;
        const MAX_CURVE_POINTS: usize = 32;
        self.telemetry.selected_sensors.truncate(MAX_SENSORS);
        self.telemetry.poll_ms = self.telemetry.poll_ms.clamp(POLL_MS_MIN, POLL_MS_MAX);
        self.telemetry.ui_refresh_ms = self
            .telemetry
            .ui_refresh_ms
            .clamp(UI_REFRESH_MS_MIN, UI_REFRESH_MS_MAX);
        if !crate::temp_chart::HISTORY_WINDOW_OPTIONS.contains(&self.telemetry.chart_window_seconds)
        {
            self.telemetry.chart_window_seconds = default_chart_window_seconds();
        }

        // Normalize mode-specific config so a hand-edited file cannot leave
        // the background loop with a missing curve/manual section forever.
        if self.fan.mode == FanControlMode::Curve && self.fan.curve.is_none() {
            self.fan.curve = Some(GlobalCurveConfig::default());
        }
        if self.fan.mode == FanControlMode::Manual && self.fan.manual.is_none() {
            self.fan.manual = Some(ManualConfig { duty_pct: 50 });
        }
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
            if curve
                .curve
                .sensors
                .first()
                .is_some_and(|s| is_battery_sensor(s))
            {
                curve.curve.sensors.clear();
                debug!("Curve sensor was Battery — using hottest non-battery sensor");
            }
            curve.curve.points.truncate(MAX_CURVE_POINTS);
            curve.curve.sensors.truncate(8);
            for point in &mut curve.curve.points {
                // Clamp to plot domain to keep points drawable.
                point[0] = point[0].clamp(0, CURVE_TEMP_MAX);
                point[1] = point[1].clamp(0, 100);
                // High-temp floor: an 80C+ point at 0% would cook the
                // machine while temps look "fresh" (no stale failover).
                // Force at least 60% duty at/above 80C.
                if point[0] >= 80 {
                    point[1] = point[1].max(60);
                }
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
            // Preserve user-authored points. Earlier versions silently
            // downgraded 6-point curves to 5 (dropping 92C), which lost
            // hand-edited settings on every load. Only the absolute safety
            // cap applies now.
            filtered.truncate(MAX_CURVE_POINTS.saturating_sub(2));
            // Duty must never decrease as temperature rises. Without this,
            // a legal pair such as [79, 0] -> [85, 60] interpolates to a
            // sub-60% duty at 80C even though both stored points obey the
            // per-point floor. Raising later points keeps the curve safe
            // and also makes interpolation monotonic.
            enforce_monotonic_duties(&mut filtered);
            // Append locked endpoints
            filtered.push([CURVE_TEMP_LOCK_START, 100]);
            filtered.push([CURVE_TEMP_MAX, 100]);
            curve.curve.points = filtered;
        }
        for duty in &mut self.fan.per_fan_duty {
            *duty = (*duty).clamp(DUTY_PCT_MIN, DUTY_PCT_MAX);
        }
        // Bound the vec: the manual loop iterates it every cycle (skipping
        // past fan_count), so a crafted config would spin, not just sit.
        // Real machines have a handful of fans; 8 leaves headroom.
        const MAX_PER_FAN_ENTRIES: usize = 8;
        self.fan.per_fan_duty.truncate(MAX_PER_FAN_ENTRIES);

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

impl Default for CurveConfig {
    fn default() -> Self {
        Self {
            sensors: Vec::new(),
            points: default_points(),
            hysteresis_c: default_hysteresis_c(),
            rate_limit_pct_per_step: default_rate_limit_pct_per_step(),
            rate_limit_down_pct_per_step: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobalCurveConfig {
    #[serde(flatten)]
    pub curve: CurveConfig,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
}

impl Default for GlobalCurveConfig {
    fn default() -> Self {
        Self {
            curve: CurveConfig::default(),
            poll_ms: default_poll_ms(),
        }
    }
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

/// Enforces non-decreasing duty over points already sorted by temperature.
/// Raises later duties instead of lowering earlier ones, so a drag can never
/// make a warmer temperature spin the fan slower.
pub fn enforce_monotonic_duties(points: &mut [[u32; 2]]) {
    let mut floor = 0;
    for point in points {
        floor = floor.max(point[1]);
        point[1] = floor;
    }
}

pub fn curve_full_points(points: &[[u32; 2]]) -> Vec<[u32; 2]> {
    // BTreeMap sorted by temp; keep max duty on duplicates to match validate.
    // High-temp floor lives here (not only in validate) so runtime drags
    // can't hold 80C+ at 0%: every write path goes through this function.
    // Truncate first: a crafted config with millions of points would spin
    // the BTreeMap insert loop (DoS via config file).
    let points: &[[u32; 2]] = if points.len() > 1024 {
        &points[..1024]
    } else {
        points
    };
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
        let duty = if temp >= 80 { duty.max(60) } else { duty };
        map.entry(temp)
            .and_modify(|e| *e = (*e).max(duty))
            .or_insert(duty);
    }
    // Force locked zone 100–110°C to 100% (user cannot change).
    map.insert(CURVE_TEMP_LOCK_START, 100);
    map.insert(CURVE_TEMP_MAX, 100);
    let mut full: Vec<[u32; 2]> = map.into_iter().map(|(t, d)| [t, d]).collect();
    // Interpolation is only safe when duty is non-decreasing with
    // temperature; otherwise a rising temperature can lower fan speed.
    enforce_monotonic_duties(&mut full);
    full
}

fn default_telemetry_poll_ms() -> u64 {
    500
}
fn default_telemetry_ui_ms() -> u64 {
    500
}

fn default_chart_window_seconds() -> i64 {
    crate::temp_chart::HISTORY_SECONDS
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryConfig {
    #[serde(default = "default_telemetry_poll_ms")]
    pub poll_ms: u64,
    #[serde(default = "default_telemetry_ui_ms")]
    pub ui_refresh_ms: u64,
    #[serde(default)]
    pub selected_sensors: Vec<String>,
    #[serde(default = "default_chart_window_seconds")]
    pub chart_window_seconds: i64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            poll_ms: 500,
            ui_refresh_ms: 500,
            selected_sensors: Vec::new(),
            chart_window_seconds: default_chart_window_seconds(),
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

/// Temp driving fan curve: configured non-Battery sensor or hottest non-Battery
/// fallback. Returns `None` when only Battery readings are available, so the
/// caller can hand fan control back to firmware instead of undercooling from a
/// lagging battery temperature.
pub fn curve_control_temp(temps: &BTreeMap<String, i32>, sensors: &[String]) -> Option<i32> {
    let non_battery = || {
        temps
            .iter()
            .filter(|(name, _)| !is_battery_sensor(name))
            .map(|(_, t)| *t)
            .max()
    };
    if sensors.is_empty() {
        return non_battery();
    }
    sensors
        .iter()
        .filter(|s| !is_battery_sensor(s))
        .filter_map(|s| temps.get(s).copied())
        .max()
        .or_else(non_battery)
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
    fn validate_unknown_theme_falls_back_to_default() {
        let mut c = Config {
            theme: "nope".to_string(),
            ..Default::default()
        };
        c.validate();
        assert_eq!(c.theme, "default");
    }

    #[test]
    fn validate_known_theme_unchanged() {
        let mut c = Config {
            theme: "Dracula".to_string(),
            ..Default::default()
        };
        c.validate();
        assert_eq!(c.theme, "Dracula");
    }

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
    fn validate_chart_window_unknown_falls_back_to_default() {
        let mut c = Config::default();
        c.telemetry.chart_window_seconds = 17;
        c.validate();
        assert_eq!(
            c.telemetry.chart_window_seconds,
            crate::temp_chart::HISTORY_SECONDS
        );
    }

    #[test]
    fn validate_chart_window_known_value_unchanged() {
        let mut c = Config::default();
        c.telemetry.chart_window_seconds = 60;
        c.validate();
        assert_eq!(c.telemetry.chart_window_seconds, 60);
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
        assert_eq!(curve_control_temp(&temps, &[]), Some(55));
    }

    #[test]
    fn curve_control_temp_uses_configured_sensors() {
        let mut temps = BTreeMap::new();
        temps.insert("F75303_CPU".into(), 55);
        temps.insert("PECI".into(), 70);
        assert_eq!(curve_control_temp(&temps, &["PECI".into()]), Some(70));
    }

    #[test]
    fn validate_drops_battery_curve_sensor() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig {
                sensors: vec!["Battery".into()],
                points: default_points(),
                hysteresis_c: 2,
                rate_limit_pct_per_step: 10,
                rate_limit_down_pct_per_step: None,
            },
            poll_ms: 1000,
        });
        c.validate();
        let sensors = &c.fan.curve.as_ref().unwrap().curve.sensors;
        assert!(sensors.is_empty());
    }

    #[test]
    fn curve_control_temp_battery_only_is_none() {
        let mut temps = BTreeMap::new();
        temps.insert("Battery".into(), 30);
        assert_eq!(curve_control_temp(&temps, &[]), None);
        assert_eq!(curve_control_temp(&temps, &["Battery".into()]), None);
    }

    #[test]
    fn curve_control_temp_ignores_explicit_battery_selection() {
        let mut temps = BTreeMap::new();
        temps.insert("Battery".into(), 30);
        temps.insert("F75303_CPU".into(), 65);
        assert_eq!(curve_control_temp(&temps, &["Battery".into()]), Some(65));
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
    fn validate_per_fan_duty_truncates_long_vec() {
        let mut c = Config::default();
        c.fan.per_fan_duty = vec![50; 100];
        c.validate();
        assert_eq!(c.fan.per_fan_duty.len(), 8);
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
    fn curve_config_default_is_usable() {
        // Deriving Default used to leave points empty and poll_ms at 0,
        // so switching to Curve created an undraggable, 200ms-poll curve
        // until the config was saved and reloaded.
        let cfg = GlobalCurveConfig::default();
        assert_eq!(cfg.curve.points, default_points());
        assert_eq!(cfg.poll_ms, default_poll_ms());
        assert_eq!(cfg.curve.hysteresis_c, default_hysteresis_c());
        assert_eq!(
            cfg.curve.rate_limit_pct_per_step,
            default_rate_limit_pct_per_step()
        );
    }

    #[test]
    fn validate_curve_mode_without_curve_creates_default() {
        let mut c = Config::default();
        c.fan.mode = FanControlMode::Curve;
        c.validate();
        let curve = c.fan.curve.expect("curve mode must get a default curve");
        assert_eq!(curve.curve.points, {
            let mut expected = default_points();
            expected.push([CURVE_TEMP_LOCK_START, 100]);
            expected.push([CURVE_TEMP_MAX, 100]);
            expected
        });
    }

    #[test]
    fn validate_manual_mode_without_manual_creates_default() {
        let mut c = Config::default();
        c.fan.mode = FanControlMode::Manual;
        c.validate();
        assert_eq!(c.fan.manual.map(|m| m.duty_pct), Some(50));
    }

    #[test]
    fn validate_enforces_monotonic_duty() {
        let mut c = Config::default();
        c.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig {
                sensors: vec![],
                points: vec![[30, 100], [60, 0], [80, 0]],
                hysteresis_c: 2,
                rate_limit_pct_per_step: 10,
                rate_limit_down_pct_per_step: None,
            },
            poll_ms: 1000,
        });
        c.validate();
        let pts = &c.fan.curve.as_ref().unwrap().curve.points;
        let mut previous = 0;
        for p in pts {
            assert!(
                p[1] >= previous,
                "duty dropped in validated points: {pts:?}"
            );
            previous = p[1];
        }
    }

    #[test]
    fn enforce_monotonic_duties_raises_not_lowers() {
        let mut points = [[30, 20], [45, 10], [60, 80], [75, 40]];
        enforce_monotonic_duties(&mut points);
        assert_eq!(points, [[30, 20], [45, 20], [60, 80], [75, 80]]);
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
    fn validate_preserves_a_six_point_curve() {
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
        // 6 editable + 2 locked; hand-authored points are preserved instead
        // of being silently downgraded to 5.
        assert_eq!(pts.len(), 8);
        assert!(pts.contains(&[92, 100]));
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
