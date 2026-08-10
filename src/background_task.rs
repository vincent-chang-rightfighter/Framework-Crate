use std::sync::Arc;
use std::sync::atomic::Ordering;
use parking_lot::RwLock;
use tracing::warn;

use crate::app::{AppState, read_lock, with_write_lock};
use crate::cli;
use crate::fan_control::CurveStepper;
use crate::style::{POLL_RATE_MIN_MS, IDLE_THRESHOLD_MS, IDLE_INTERVAL_MS, EXPANSION_SCAN_MS, VERSIONS_REFRESH_MS};
use crate::temp_chart;

/// PD port state history depth. We keep 3 samples to distinguish USB-A
/// expansion cards (stable Source/Dfp/no-PD state) from USB devices that
/// may briefly share the same signature during enumeration. 3 samples at
/// the fixed 10s expansion scan interval gives ~20-30s of history.
const MAX_PD_HISTORY: usize = 3;

/// Reset the EC client and mark it unavailable so the next loop iteration
/// will reinitialize it. Called when a spawn_blocking panics, indicating
/// the EC may be in a bad state.
fn reset_ec_on_panic(state: &AppState) {
    warn!("Resetting EC client after spawn panic");
    state.cli_available.store(false, Ordering::Release);
    with_write_lock(&state.ec_client, |guard| {
        *guard = Arc::new(None);
    });
}

pub fn pin_to_slowest_core() {
    if let Some(cores) = core_affinity::get_core_ids() {
        if let Some(&slowest) = cores.last() {
            if core_affinity::set_for_current(slowest) {
                tracing::debug!("[AFFINITY] Pinned to LP-E core (id={})", slowest.id);
                #[cfg(debug_assertions)]
                verify_affinity(slowest.id);
            } else {
                tracing::debug!("[AFFINITY] Failed to pin to core {}", slowest.id);
            }
        }
    }
}

fn verify_affinity(expected_id: usize) {
    #[cfg(target_os = "windows")]
    {
        extern "system" {
            fn GetCurrentThread() -> *mut core::ffi::c_void;
            fn SetThreadAffinityMask(hThread: *mut core::ffi::c_void, dwMask: usize) -> usize;
        }
        if expected_id >= usize::BITS as usize {
            tracing::debug!("[AFFINITY] Verify: core {} exceeds bit width ({}), skipping", expected_id, usize::BITS);
            return;
        }
        let mask = 1usize << expected_id;
        let prev = unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) };
        let prev_core = prev.trailing_zeros() as usize;
        let ok = prev_core == expected_id;
        tracing::debug!(
            "[AFFINITY] Verify: prev_mask=0x{:X}, prev_core={}, expected={} {}",
            prev, prev_core, expected_id, if ok { "OK (confirmed)" } else { "UNEXPECTED" }
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = expected_id;
    }
}

fn push_pd_ports_history(
    pd_ports: &Arc<RwLock<Arc<Vec<cli::ec_wrapper::UsbCPort>>>>,
    history: &Arc<RwLock<Arc<crate::app::PdPortsHistory>>>,
) {
    let snapshot = Arc::clone(&read_lock(pd_ports));
    with_write_lock(history, |hist| {
        let h = Arc::make_mut(hist);
        h.push_back(snapshot);
        if h.len() > MAX_PD_HISTORY {
            h.pop_front();
        }
    });
}

fn estimate_duty_from_thermal(state: &AppState) -> Option<u32> {
    let thermal_snap = read_lock(&state.thermal);
    if let Some(ref t) = *thermal_snap {
        t.fans.iter().map(|f| f.rpm).max().map(|rpm| {
            let max_rpm = state.fan_max_rpm.load(Ordering::Acquire) as u32;
            if max_rpm > 0 {
                (rpm * 100 / max_rpm).clamp(10, 100)
            } else {
                50 // Safe default when max RPM is unknown
            }
        })
    } else {
        None
    }
}

fn record_thermal_sample(state: &AppState, t: cli::ec_wrapper::ThermalData) {
    // Periodically reset fan_max_rpm to avoid stale readings from hardware changes.
    // Reset every 60 seconds so the max adapts to the actual current fan capability.
    const FAN_MAX_RPM_RESET_INTERVAL_MS: u64 = 60_000;
    let now_ts = crate::util::current_time_ms();
    if let Some(max_rpm) = t.fans.iter().map(|f| f.rpm).max() {
        let prev = state.fan_max_rpm.load(Ordering::Acquire) as u32;
        let last_reset = state.last_fan_rpm_reset.load(Ordering::Acquire);
        if now_ts.saturating_sub(last_reset) >= FAN_MAX_RPM_RESET_INTERVAL_MS {
            // Reset: use the current max as the new baseline
            state.fan_max_rpm.store(max_rpm as u64, Ordering::Release);
            state.last_fan_rpm_reset.store(now_ts, Ordering::Release);
        } else if max_rpm > prev {
            state.fan_max_rpm.store(max_rpm as u64, Ordering::Release);
        }
    }
    let now = crate::util::current_time_ms_i64();

    // Read-only comparison first to avoid cloning when unchanged
    let changed = {
        let cur = read_lock(&state.thermal);
        match cur.as_ref().as_ref() {
            Some(cur) => *cur.temps != *t.temps,
            None => true,
        }
    };
    if !changed { return; }

    // Wrap temps in Arc for history samples — avoids deep clone on every history read
    let temps_for_history = std::sync::Arc::clone(&t.temps);

    // Update cached sensor_keys if the key set changed (rare — only on hardware change)
    // Zero-alloc comparison: check length + element-wise before cloning
    let keys_changed = {
        let cache = read_lock(&state.sensor_cache);
        cache.keys.len() != t.temps.len()
            || cache.keys.iter().zip(t.temps.keys()).any(|(a, b)| a.as_str() != b.as_str())
    };
    if keys_changed {
        let new_keys: Vec<String> = t.temps.keys().cloned().collect();
        let config = read_lock(&state.config);
        let sorted = crate::types::sorted_sensor_list(&config.telemetry.selected_sensors, &new_keys);
        let colors: Vec<iced::Color> = sorted.iter()
            .map(|name| crate::style::sensor_color(name, &new_keys))
            .collect();
        with_write_lock(&state.sensor_cache, |g| {
            *g = Arc::new(crate::app::SensorCache { keys: new_keys, sorted: Arc::new(sorted), colors: Arc::new(colors) });
        });
    }

    with_write_lock(&state.thermal, |guard| {
        *guard = Arc::new(Some(t));
    });

    with_write_lock(&state.temp_history, |hist| {
        let h = Arc::make_mut(hist);
        let sample = temp_chart::TempSample {
            ts_ms: now,
            temps: temps_for_history,
        };
        h.push(sample);
        let cutoff = now - crate::temp_chart::HISTORY_MS;
        h.retain(|s| s.ts_ms > cutoff);
    });
}

pub(crate) async fn refresh_all_data(state: &AppState, ec: &std::sync::Arc<cli::EcClient>) {
    let state_clone = state.clone();
    let ec_clone = Arc::clone(ec);
    let thermal_result = tokio::task::spawn_blocking(move || ec_clone.thermal()).await;
    let state_clone2 = state.clone();
    let ec_clone2 = Arc::clone(ec);
    let power_result = tokio::task::spawn_blocking(move || ec_clone2.power()).await;
    let state_clone3 = state.clone();
    let ec_clone3 = Arc::clone(ec);
    let kb_result = tokio::task::spawn_blocking(move || ec_clone3.kblight_get()).await;
    let state_clone4 = state.clone();
    let ec_clone4 = Arc::clone(ec);
    let pd_result = tokio::task::spawn_blocking(move || ec_clone4.pd_ports()).await;
    let state_clone5 = state.clone();
    let ec_clone5 = Arc::clone(ec);
    let exp_result = tokio::task::spawn_blocking(move || ec_clone5.expansion_cards()).await;

    if let Ok(Ok(t)) = thermal_result {
        record_thermal_sample(&state_clone, t);
    }
    if let Ok(Ok(bat)) = power_result {
        with_write_lock(&state_clone2.battery, |guard| {
            *guard = Arc::new(Some(crate::types::BatteryInfo { power_info: bat }));
        });
    }
    if let Ok(Ok(kb)) = kb_result {
        with_write_lock(&state_clone3.kblight, |guard| {
            *guard = Arc::new(Some(kb));
        });
    }
    if let Ok(ports) = pd_result {
        with_write_lock(&state_clone4.pd_ports, |guard| {
            *guard = Arc::new(ports);
        });
        push_pd_ports_history(&state_clone4.pd_ports, &state_clone4.pd_ports_history);
    }
    if let Ok(cards) = exp_result {
        with_write_lock(&state_clone5.expansion_cards, |guard| {
            *guard = Arc::new(cards);
        });
    }
}

pub fn spawn(state: AppState) {
    const MAX_CONSECUTIVE_FAILURES: u32 = 10;
    tokio::spawn(async move {
        pin_to_slowest_core();
        let mut consecutive_failures: u32 = 0;
        loop {
            let bg_state2 = state.clone();
            let handle = tokio::spawn(async move {
                pin_to_slowest_core();
                let saved_duty = bg_state2.last_applied_duty.load(Ordering::Acquire) as u32;
                let (init_fan_mode, init_is_manual) = {
                    let init_config = read_lock(&bg_state2.config);
                    (init_config.fan.mode, matches!(init_config.fan.mode, crate::types::FanControlMode::Manual))
                };
                let mut last_fan_mode: Option<crate::types::FanControlMode> = Some(init_fan_mode);
                let mut last_manual_duty: Option<u32> = None;
                let mut manual_ramp_current: Option<u32> = if saved_duty > 0 {
                    Some(saved_duty)
                } else if init_is_manual {
                    estimate_duty_from_thermal(&bg_state2)
                } else {
                    None
                };
                let mut curve_stepper = if saved_duty > 0 { CurveStepper::with_last_duty(saved_duty) } else { CurveStepper::new() };
                let start_ms = crate::util::current_time_ms();
                let mut last_expansion_scan: u64 = start_ms;
                let mut last_versions_scan: u64 = start_ms;
                loop {
                    // Read fan mode from atomic (no config lock needed)
                    let fan_mode = crate::types::FanControlMode::from_u8(
                        bg_state2.fan_mode.load(Ordering::Acquire) as u8
                    );
                    let interval = match fan_mode {
                        crate::types::FanControlMode::Curve => {
                            bg_state2.curve_poll_ms.load(Ordering::Acquire).max(POLL_RATE_MIN_MS as u64)
                        }
                        _ => bg_state2.poll_ms.load(Ordering::Acquire).max(POLL_RATE_MIN_MS as u64),
                    };
                    let mut now_ms = crate::util::current_time_ms();
                    let last_interaction = bg_state2.last_interaction_ts.load(Ordering::Acquire);
                    let is_idle = now_ms.saturating_sub(last_interaction) > IDLE_THRESHOLD_MS;
                    let effective_interval = match fan_mode {
                        // Fan curve must keep responding to temperature even when the
                        // user is idle; the idle slowdown only applies to non-curve modes.
                        crate::types::FanControlMode::Curve => interval,
                        _ if is_idle => IDLE_INTERVAL_MS,
                        _ => interval,
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(effective_interval)).await;
                    if bg_state2.shutdown.load(Ordering::Acquire) { return; }
                    if !bg_state2.cli_available.load(Ordering::Acquire) { continue; }

                    let ec_opt = { read_lock(&bg_state2.ec_client) };
                    let ec: Arc<cli::EcClient> = match ec_opt.as_ref().as_ref() {
                        Some(c) => Arc::clone(c),
                        None => {
                            let state_cl = bg_state2.clone();
                            match tokio::task::spawn_blocking(cli::EcClient::new).await {
                                Ok(Ok(c)) => {
                                    let arc_ec = Arc::new(c);
                                    with_write_lock(&state_cl.ec_client, |guard| {
                                        *guard = Arc::new(Some(Arc::clone(&arc_ec)));
                                    });
                                    state_cl.cli_available.store(true, Ordering::Release);
                                    arc_ec
                                }
                                Ok(Err(e)) => {
                                    warn!("Background loop: failed to init EC: {}", e);
                                    state_cl.cli_available.store(false, Ordering::Release);
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    continue;
                                }
                                Err(e) => {
                                    warn!("Background loop: EC spawn_blocking panicked: {}", e);
                                    state_cl.cli_available.store(false, Ordering::Release);
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    continue;
                                }
                            }
                        }
                    };

                    let ec_clone = Arc::clone(&ec);
                    if let Ok(Ok(t)) = tokio::task::spawn_blocking(move || ec_clone.thermal()).await {
                        record_thermal_sample(&bg_state2, t);
                    }
                    // While the user is idle, skip the per-cycle UI-only reads (battery) to
                    // save subprocess spawns; thermal still feeds the fan curve.
                    if !is_idle {
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(Ok(bat)) = tokio::task::spawn_blocking(move || ec_clone.power()).await {
                            with_write_lock(&bg_state2.battery, |guard| {
                                let new_info = crate::types::BatteryInfo { power_info: bat };
                                if guard.as_ref().as_ref() != Some(&new_info) {
                                    *guard = Arc::new(Some(new_info));
                                }
                            });
                        }
                    }

                    // Expansion / PD scans run on fixed wall-clock intervals
                    // so hotplug is always detectable.
                    now_ms = crate::util::current_time_ms();
                    if now_ms.saturating_sub(last_expansion_scan) >= EXPANSION_SCAN_MS {
                        last_expansion_scan = now_ms;
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(ports) = tokio::task::spawn_blocking(move || ec_clone.pd_ports()).await {
                            let changed = {
                                let current = read_lock(&bg_state2.pd_ports);
                                *current != ports
                            };
                            if changed {
                                with_write_lock(&bg_state2.pd_ports, |guard| {
                                    *guard = Arc::new(ports);
                                });
                            }
                            push_pd_ports_history(&bg_state2.pd_ports, &bg_state2.pd_ports_history);
                        }
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(cards) = tokio::task::spawn_blocking(move || ec_clone.expansion_cards()).await {
                            with_write_lock(&bg_state2.expansion_cards, |guard| {
                                if **guard != cards {
                                    *guard = Arc::new(cards);
                                }
                            });
                        }
                    }
                    if now_ms.saturating_sub(last_versions_scan) >= VERSIONS_REFRESH_MS {
                        last_versions_scan = now_ms;
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(Ok(v)) = tokio::task::spawn_blocking(move || ec_clone.versions()).await {
                            with_write_lock(&bg_state2.versions, |guard| {
                                if guard.as_ref().as_ref() != Some(&v) {
                                    *guard = Arc::new(Some(v));
                                }
                            });
                        }
                    }

                    // Shutdown re-check after scans: a quit initiated during the
                    // async thermal/scan awaits must not be overwritten by fan
                    // control (restore or quit duty) below.
                    if bg_state2.shutdown.load(Ordering::Acquire) { return; }

                    // Read only the fields we need, then drop the lock immediately.
                    // This avoids holding the config lock across ~100ms EC I/O.
                    let (manual_duty, curve_poll, curve_hysteresis, curve_rate_limit, curve_points_ref, curve_rate_limit_down) = {
                        let config = read_lock(&bg_state2.config);
                        (
                            config.fan.manual.as_ref().map(|m| m.duty_pct),
                            config.fan.curve.as_ref().map(|c| c.poll_ms),
                            config.fan.curve.as_ref().map(|c| c.curve.hysteresis_c),
                            config.fan.curve.as_ref().map(|c| c.curve.rate_limit_pct_per_step),
                            config.fan.curve.as_ref().map(|c| c.curve.points.clone()),
                            config.fan.curve.as_ref().and_then(|c| c.curve.rate_limit_down_pct_per_step),
                        )
                    }; // config lock released here

                    let mode = crate::types::FanControlMode::from_u8(
                        bg_state2.fan_mode.load(Ordering::Acquire) as u8
                    );
                    if last_fan_mode.as_ref() != Some(&mode) {
                        curve_stepper.reset();
                        last_manual_duty = None;
                        if matches!(mode, crate::types::FanControlMode::Manual) {
                            manual_ramp_current = estimate_duty_from_thermal(&bg_state2);
                        } else {
                            manual_ramp_current = None;
                        }
                    }
                    match &mode {
                        crate::types::FanControlMode::Disabled => {
                            if last_fan_mode.as_ref().map(|m| m != &crate::types::FanControlMode::Disabled).unwrap_or(false) {
                                let ec_clone = Arc::clone(&ec);
                                match tokio::task::spawn_blocking(move || ec_clone.autofanctrl()).await {
                                    Ok(result) => {
                                        if let Err(e) = result {
                                            warn!("Failed to restore auto fan control: {}", e);
                                        }
                                    }
                                    Err(join_err) => {
                                        warn!("EC spawn panicked (autofanctrl): {}", join_err);
                                        reset_ec_on_panic(&bg_state2);
                                        continue;
                                    }
                                }
                                let mode_now = crate::types::FanControlMode::from_u8(
                                    bg_state2.fan_mode.load(Ordering::Acquire) as u8
                                );
                                if mode_now != mode { last_fan_mode = Some(mode); continue; }
                            }
                        }
                        crate::types::FanControlMode::Manual => {
                            if let Some(target) = manual_duty {
                                let current = manual_ramp_current.unwrap_or(target);
                                let next = crate::fan_control::apply_rate_limit(current, target, 10);
                                if last_manual_duty != Some(next) {
                                    let ec_clone = Arc::clone(&ec);
                                    match tokio::task::spawn_blocking(move || ec_clone.set_fan_duty(next, None)).await {
                                        Ok(result) => match result {
                                            Ok(()) => {
                                                let mode_now = crate::types::FanControlMode::from_u8(
                                                    bg_state2.fan_mode.load(Ordering::Acquire) as u8
                                                );
                                                if mode_now != mode { last_fan_mode = Some(mode); continue; }
                                                bg_state2.last_applied_duty.store(next as u64, Ordering::Release);
                                                last_manual_duty = Some(next);
                                                manual_ramp_current = Some(next);
                                            }
                                            Err(e) => warn!("Failed to set manual fan duty: {}", e),
                                        },
                                        Err(join_err) => {
                                            warn!("EC spawn panicked (set_fan_duty): {}", join_err);
                                            reset_ec_on_panic(&bg_state2);
                                            continue;
                                        }
                                    }
                                } else {
                                    manual_ramp_current = Some(next);
                                }
                            }
                        }
                        crate::types::FanControlMode::Curve => {
                            let thermal_clone = read_lock(&bg_state2.thermal);
                            if let Some(ref thermal) = *thermal_clone {
                                if let (Some(_poll), Some(hyst), Some(rate), Some(ref pts)) =
                                    (curve_poll, curve_hysteresis, curve_rate_limit, curve_points_ref)
                                {
                                    let max_temp = thermal.temps.values().copied().max().unwrap_or(0);
                                    let full_pts_arc = read_lock(&bg_state2.curve_full_points);
                                    let full_pts: &[[u32; 2]] = &full_pts_arc;
                                    let curve_cfg = crate::types::CurveConfig {
                                        sensors: vec![],
                                        points: pts.clone(),
                                        hysteresis_c: hyst,
                                        rate_limit_pct_per_step: rate,
                                        rate_limit_down_pct_per_step: curve_rate_limit_down,
                                    };
                                    if let Some(next) = curve_stepper.next(max_temp, &curve_cfg, full_pts) {
                                        let ec_clone = Arc::clone(&ec);
                                        match tokio::task::spawn_blocking(move || ec_clone.set_fan_duty(next, None)).await {
                                            Ok(result) => match result {
                                                Ok(()) => {
                                                    let mode_now = crate::types::FanControlMode::from_u8(
                                                        bg_state2.fan_mode.load(Ordering::Acquire) as u8
                                                    );
                                                    if mode_now != mode { last_fan_mode = Some(mode); continue; }
                                                    bg_state2.last_applied_duty.store(next as u64, Ordering::Release);
                                                }
                                                Err(e) => warn!("Failed to set fan duty (curve): {}", e),
                                            },
                                            Err(join_err) => {
                                                warn!("EC spawn panicked (set_fan_duty curve): {}", join_err);
                                                reset_ec_on_panic(&bg_state2);
                                                continue;
                                            }
                                        }
                                        curve_stepper.note_applied(next);
                                    }
                                }
                            }
                        }
                    }
                    last_fan_mode = Some(mode);
                }
            });
            if let Err(e) = handle.await {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    warn!("Background task failed {} times consecutively, giving up", consecutive_failures);
                    break;
                }
                warn!("Background polling task crashed: {}, restarting in 3s... ({}/{})", e, consecutive_failures, MAX_CONSECUTIVE_FAILURES);
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if state.shutdown.load(Ordering::Acquire) { break; }
            } else {
                // Inner task exited normally (shouldn't happen, but handle gracefully)
                // Restart instead of permanently stopping background polling.
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    warn!("Background task exited unexpectedly {} times, giving up", consecutive_failures);
                    break;
                }
                warn!("Background polling task exited unexpectedly, restarting in 3s...");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if state.shutdown.load(Ordering::Acquire) { break; }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_to_slowest_core_does_not_panic() {
        pin_to_slowest_core();
    }
}
