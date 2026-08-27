use super::{App, Message};
use crate::style::*;
use crate::types::FanControlMode;
use crate::util::{read_lock, with_write_lock};
use iced::Task;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

impl App {
    pub(crate) fn handle_config_message(&mut self, message: &Message) -> Option<Task<Message>> {
        match *message {
            Message::FanModeChanged(mode) => {
                self.state
                    .fan
                    .mode
                    .store(mode.to_u8() as u64, Ordering::Release);
                self.mutate_config(|cfg| {
                    if mode == FanControlMode::Curve && cfg.fan.curve.is_none() {
                        cfg.fan.curve = Some(crate::types::GlobalCurveConfig::default());
                    }
                    if mode == FanControlMode::Manual && cfg.fan.manual.is_none() {
                        cfg.fan.manual = Some(crate::types::ManualConfig { duty_pct: 50 });
                    }
                    cfg.fan.mode = mode;
                });
                self.update_curve_full_points();
                self.save_config();
                Some(Task::none())
            }
            Message::FanDutyChanged(duty) => {
                let duty = duty.clamp(0, 100);
                self.mutate_config(|cfg| {
                    cfg.fan.manual = Some(crate::types::ManualConfig { duty_pct: duty });
                });
                // Do not update last_applied_duty here; it tracks actual EC writes.
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::FanUnifiedDutyToggled(unified) => {
                self.state
                    .fan
                    .unified_duty
                    .store(unified, Ordering::Release);
                self.mutate_config(|cfg| {
                    cfg.fan.unified_duty = unified;
                });
                self.save_config();
                Some(Task::none())
            }
            Message::FanPerDutyChanged(fan_idx, duty) => {
                let duty = duty.clamp(0, 100);
                with_write_lock(&self.state.fan.per_fan_duty, |guard| {
                    let duties = Arc::make_mut(guard);
                    if fan_idx < duties.len() {
                        duties[fan_idx] = duty;
                    }
                });
                self.mutate_config(|cfg| {
                    // Copy live duties to preserve values for newly added fans.
                    let live = read_lock(&self.state.fan.per_fan_duty);
                    cfg.fan.per_fan_duty = (*live).clone();
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurvePointMoved(idx, temp, duty) => {
                let mut duty = duty.clamp(0, 100);
                if temp >= crate::types::CURVE_TEMP_LOCK_START {
                    duty = 100;
                }
                // Locked points (100,110) are not editable.
                {
                    let cfg = read_lock(&self.state.lifecycle.config);
                    if let Some(curve) = cfg.fan.curve.as_ref()
                        && let Some(&[t, _]) = curve.curve.points.get(idx)
                        && t >= crate::types::CURVE_TEMP_LOCK_START
                    {
                        return Some(Task::none());
                    }
                }
                // Clamp temperature between neighbors to avoid duplicate temps collapsing control points.
                // Editable range is 0..99; 100–110 is locked 100%.
                let temp = {
                    let cfg = read_lock(&self.state.lifecycle.config);
                    let points = cfg
                        .fan
                        .curve
                        .as_ref()
                        .map(|c| c.curve.points.as_slice())
                        .unwrap_or(&[]);
                    let orig_temp = points.get(idx).map(|p| p[0] as i64).unwrap_or(temp as i64);
                    // Locked zone points are fixed; keep original if trying to edit them (already returned).
                    let max_t = crate::types::CURVE_TEMP_EDIT_MAX as i64;
                    let mut others: Vec<i64> = points
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != idx)
                        .map(|(_, p)| p[0] as i64)
                        .filter(|&t| t < crate::types::CURVE_TEMP_LOCK_START as i64)
                        .collect();
                    others.sort_unstable();
                    let mut lo: i64 = -1; // nothing below → allow down to 0
                    let mut hi: i64 = max_t + 1; // editable max is 99, locked 100 above
                    for &t in &others {
                        if t < temp as i64 {
                            lo = lo.max(t);
                        } else {
                            hi = hi.min(t);
                        }
                    }
                    // No valid slot between neighbors (gap <=1) would force a
                    // duplicate that validate would silently drop; keep original.
                    if hi - lo <= 1 {
                        orig_temp.clamp(0, max_t) as u32
                    } else {
                        let min_t = (lo + 1).max(0);
                        let max_allowed = (hi - 1).min(max_t);
                        (temp.clamp(0, crate::types::CURVE_TEMP_EDIT_MAX) as i64)
                            .clamp(min_t, max_allowed) as u32
                    }
                };
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve
                        && idx < curve.curve.points.len()
                    {
                        curve.curve.points[idx] = [temp, duty];
                    }
                });
                self.pending_curve_update = true;
                self.last_curve_edit_ts = Instant::now();
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurveHysteresisChanged(h) => {
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        curve.curve.hysteresis_c = h;
                    }
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurveRateLimitChanged(r) => {
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        curve.curve.rate_limit_pct_per_step = r;
                    }
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::CurvePollMsChanged(ms) => {
                let ms = ms.clamp(
                    crate::types::CURVE_POLL_MS_MIN,
                    crate::types::CURVE_POLL_MS_MAX,
                );
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        curve.poll_ms = ms;
                    }
                });
                // Background loop reads poll interval directly; no tick reschedule needed.
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::ChargeLimitToggled(enabled) => {
                self.mutate_config(|cfg| {
                    let limit =
                        cfg.battery
                            .charge_limit_max_pct
                            .get_or_insert(crate::types::SettingU8 {
                                enabled: false,
                                value: CHARGE_LIMIT_MIN as u8,
                            });
                    limit.enabled = enabled;
                    if limit.value < CHARGE_LIMIT_MIN as u8 {
                        limit.value = CHARGE_LIMIT_MIN as u8;
                    }
                });
                self.save_config();
                Some(Task::none())
            }
            Message::ChargeLimitChanged(value) => {
                self.mutate_config(|cfg| {
                    let limit =
                        cfg.battery
                            .charge_limit_max_pct
                            .get_or_insert(crate::types::SettingU8 {
                                enabled: false,
                                value: CHARGE_LIMIT_MIN as u8,
                            });
                    limit.value = value.clamp(CHARGE_LIMIT_MIN, CHARGE_LIMIT_MAX) as u8;
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::SensorToggled(idx, enabled) => {
                let (name, all_keys) = {
                    let cache = read_lock(&self.state.thermal.sensor_cache);
                    (cache.keys.get(idx).cloned(), cache.keys.clone())
                };
                let Some(name) = name else {
                    return Some(Task::none());
                };
                self.mutate_config(|cfg| {
                    if cfg.telemetry.selected_sensors.is_empty() {
                        cfg.telemetry.selected_sensors = all_keys.clone();
                    }
                    if enabled {
                        if !cfg.telemetry.selected_sensors.contains(&name) {
                            cfg.telemetry.selected_sensors.push(name);
                        }
                    } else {
                        cfg.telemetry.selected_sensors.retain(|s| s != &name);
                    }
                });
                self.rebuild_sensor_cache();
                self.save_config();
                Some(Task::none())
            }
            Message::CurveSensorSelected(idx) => {
                let name = {
                    let cache = read_lock(&self.state.thermal.sensor_cache);
                    cache.keys.get(idx).cloned()
                };
                let Some(name) = name else {
                    return Some(Task::none());
                };
                self.mutate_config(|cfg| {
                    if let Some(curve) = cfg.fan.curve.as_mut() {
                        // Curve is driven by single selected sensor.
                        curve.curve.sensors = vec![name];
                    }
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::PollRateChanged(ms) => {
                let ms = ms.clamp(POLL_RATE_MIN_MS as u64, crate::types::POLL_MS_MAX);
                self.mutate_config(|cfg| {
                    cfg.telemetry.poll_ms = ms;
                });
                self.state.lifecycle.poll_ms.store(ms, Ordering::Relaxed);
                self.save_config();
                Some(Task::none())
            }
            Message::UiRefreshRateChanged(ms) => {
                let ms = ms.clamp(50, 1000);
                self.mutate_config(|cfg| {
                    cfg.telemetry.ui_refresh_ms = ms;
                });
                self.tick_interval_ms = ms;
                self.last_tick = Instant::now();
                self.save_config();
                Some(Task::none())
            }
            _ => None,
        }
    }
}
