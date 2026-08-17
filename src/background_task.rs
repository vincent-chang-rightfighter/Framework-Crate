use std::sync::Arc;
use std::sync::atomic::Ordering;
use parking_lot::RwLock;
use tracing::warn;

use crate::app::AppState;
use crate::util::{read_lock, with_write_lock};
use crate::cli;
use crate::fan_control::CurveStepper;
use crate::style::{POLL_RATE_MIN_MS, IDLE_THRESHOLD_MS, IDLE_INTERVAL_MS, EXPANSION_SCAN_MS, VERSIONS_REFRESH_MS};
use crate::temp_chart;

/// PD port state history depth. We keep 3 samples to distinguish USB-A
/// expansion cards (stable Source/Dfp/no-PD state) from USB devices that
/// may briefly share the same signature during enumeration. 3 samples at
/// the fixed 10s expansion scan interval gives ~20-30s of history.
const MAX_PD_HISTORY: usize = 3;

/// Consecutive EC read failures tolerated before forcing a client
/// reinitialization. Recovers from a dead EC driver after sleep/resume
/// even if the tray's PowerResumed event was missed.
const MAX_EC_IO_FAILURES: u32 = 5;

/// Consecutive EC fan duty write failures tolerated before forcing a
/// client reinitialization (separate from reads: a healthy thermal poll
/// would otherwise keep resetting the read counter while writes fail).
const MAX_EC_WRITE_FAILURES: u32 = 5;

/// Re-assert the configured fan duty even when the ramp has converged if
/// no successful duty write happened for this long. The EC resets fan
/// control during sleep; if the tray's PowerResumed event is missed, the
/// fans would otherwise stay off (0 RPM) forever with no write to bring
/// them back. This window bounds that recovery to 30s.
const FAN_REASSERT_INTERVAL_MS: u64 = 30_000;

/// Rate-limit for the curve-mode fail-safe: when the EC temperature read
/// fails (empty temps map), hand fan control back to the firmware with
/// autofanctrl() at most once per interval. The EC firmware has its own
/// thermal protection; writing 0% duty from a 0°C control temp would
/// otherwise leave the fans off with no protection.
const CURVE_TEMP_FAILOVER_MS: u64 = 30_000;

/// Reset the EC client and mark it unavailable so the next loop iteration
/// will reinitialize it. Called when a spawn_blocking panics, indicating
/// the EC may be in a bad state.
fn reset_ec_on_panic(state: &AppState) {
    warn!("Resetting EC client after spawn panic");
    state.system.cli_available.store(false, Ordering::Release);
    with_write_lock(&state.system.ec_client, |guard| {
        *guard = Arc::new(None);
    });
}

/// Force EC client reinitialization after consecutive I/O failures. The
/// driver can go stale after sleep/resume (or any missed PowerResumed
/// event); recreating the client and reopening the device recovers it.
fn reset_ec_after_failures(state: &AppState, failures: u32) {
    warn!("EC unresponsive after {} consecutive read failures, reinitializing client", failures);
    state.system.cli_available.store(false, Ordering::Release);
    with_write_lock(&state.system.ec_client, |guard| {
        *guard = Arc::new(None);
    });
}

pub fn pin_to_slowest_core() {
    if let Some(cores) = core_affinity::get_core_ids()
        && let Some(&slowest) = cores.last()
    {
        if core_affinity::set_for_current(slowest) {
            tracing::debug!("[AFFINITY] Pinned to LP-E core (id={})", slowest.id);
            #[cfg(debug_assertions)]
            verify_affinity(slowest.id);
        } else {
            tracing::debug!("[AFFINITY] Failed to pin to core {}", slowest.id);
        }
    }
}

#[cfg(debug_assertions)]
fn verify_affinity(expected_id: usize) {
    #[cfg(target_os = "windows")]
    {
        unsafe extern "system" {
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
    history: &Arc<RwLock<Arc<crate::sub_state::PdPortsHistory>>>,
) {
    let snapshot = Arc::clone(&read_lock(pd_ports));
    with_write_lock(history, |hist| {
        let mut h = (**hist).clone();
        h.push_back(snapshot);
        if h.len() > MAX_PD_HISTORY {
            h.pop_front();
        }
        *hist = Arc::new(h);
    });
}

/// Permanently mark any port seen reporting a Sink power role as USB-C.
/// Only USB-C ports can sink, so the marker never expires — this keeps a
/// USB-C expansion-card port from being reclassified as USB-A once its
/// short history window no longer contains the idle Sink samples.
fn mark_pd_usb_c_seen(
    ports: &[cli::ec_wrapper::UsbCPort],
    seen_ref: &Arc<RwLock<Arc<Vec<bool>>>>,
) {
    if !ports.iter().any(|p| p.power_role == Some("Sink")) {
        return;
    }
    with_write_lock(seen_ref, |guard| {
        let mut seen = (**guard).clone();
        // pd_ports() skips ports whose EC read failed, so the port index is
        // not a contiguous 0..len sequence — size the vec for the highest
        // seen port to avoid indexing out of bounds.
        let need = ports.iter().map(|p| p.port as usize + 1).max().unwrap_or(0);
        if seen.len() < need {
            seen.resize(need, false);
        }
        for p in ports {
            if p.power_role == Some("Sink") {
                seen[p.port as usize] = true;
            }
        }
        *guard = Arc::new(seen);
    });
}

fn mark_view_dirty(state: &AppState) {
    state.lifecycle.view_dirty.store(true, Ordering::Release);
}

fn ensure_per_fan_duty(state: &AppState, fan_count: usize) {
    if fan_count == 0 {
        return;
    }
    let fill = {
        let config = read_lock(&state.lifecycle.config);
        config.fan.manual.as_ref().map(|m| m.duty_pct).unwrap_or(50)
    };
    let mut resized = false;
    with_write_lock(&state.fan.per_fan_duty, |guard| {
        let duties = Arc::make_mut(guard);
        if duties.len() == fan_count {
            return;
        }
        if duties.len() < fan_count {
            duties.resize(fan_count, fill);
        } else {
            duties.truncate(fan_count);
        }
        resized = true;
    });
    // Note: the resize is deliberately NOT written back to config. It only
    // tracks hardware fan-count changes (docking, module swaps); persisting
    // it would overwrite the user's saved per-fan values with fill values
    // derived from the current manual duty. The config is updated only when
    // the user explicitly edits a per-fan duty (FanPerDutyChanged).
    if resized {
        mark_view_dirty(state);
    }
}

fn estimate_duty_from_thermal(state: &AppState) -> Option<u32> {
    let thermal_snap = read_lock(&state.thermal.data);
    if let Some(ref t) = *thermal_snap {
        t.fans.iter().map(|f| f.rpm).max().map(|rpm| {
            let max_rpm = state.fan.fan_max_rpm.load(Ordering::Acquire) as u32;
            if rpm == 0 {
                // Stopped fans: seed the ramp at 0, not the 10% floor.
                0
            } else if let Some(pct) = (rpm * 100).checked_div(max_rpm) {
                pct.clamp(10, 100)
            } else {
                // When max RPM is unknown, use minimum duty to avoid a
                // sudden fan speed jump on mode switch. The background
                // loop will ramp to the target duty via rate limiting.
                10
            }
        })
    } else {
        None
    }
}

fn record_thermal_sample(state: &AppState, t: cli::ec_wrapper::ThermalData) -> bool {
    // Periodically reset fan_max_rpm to avoid stale readings from hardware changes.
    // Reset every 60 seconds so the max adapts to the actual current fan capability.
    const FAN_MAX_RPM_RESET_INTERVAL_MS: u64 = 60_000;
    let now_ts = crate::util::monotonic_ms();
    let fan_count = t.fans.len() as u64;
    state.fan.fan_count.store(fan_count, Ordering::Release);
    ensure_per_fan_duty(state, t.fans.len());
    if let Some(max_rpm) = t.fans.iter().map(|f| f.rpm).max() {
        let prev = state.fan.fan_max_rpm.load(Ordering::Acquire) as u32;
        let last_reset = state.fan.last_fan_rpm_reset.load(Ordering::Acquire);
        if now_ts.saturating_sub(last_reset) >= FAN_MAX_RPM_RESET_INTERVAL_MS {
            // Rolling re-baseline: only ever RAISE the max-RPM baseline,
            // never lower it. Lowering it while the fans idle (e.g. 800 RPM)
            // makes estimate_duty_from_thermal read ~100%, so the next
            // Manual-mode entry would seed the ramp at ~90% duty — the exact
            // failure the resume path avoids by keeping fan_max_rpm across
            // sleep. The seed only affects the ramp start, never the final
            // converged duty, so a high baseline is always safe.
            state.fan.fan_max_rpm.store(max_rpm.max(prev) as u64, Ordering::Release);
            state.fan.last_fan_rpm_reset.store(now_ts, Ordering::Release);
        } else if max_rpm > prev {
            state.fan.fan_max_rpm.store(max_rpm as u64, Ordering::Release);
        }
    }
    // Monotonic timestamp: the sample ts is only used for relative window
    // pruning and chart ratios, never displayed as absolute time — using the
    // wall clock here would let a system clock jump prune or freeze the
    // history.
    let now = crate::util::monotonic_ms() as i64;

    // Read-only comparison first to avoid cloning when unchanged.
    // Compare the whole payload: fan RPM can change while temperatures stay
    // flat (manual ramping, EC recovery), and that data must still be stored.
    let changed = {
        let cur = read_lock(&state.thermal.data);
        match cur.as_ref().as_ref() {
            Some(cur) => *cur != t,
            None => true,
        }
    };
    if !changed { return false; }

    // Wrap temps in Arc for history samples — avoids deep clone on every history read
    let temps_for_history = std::sync::Arc::clone(&t.temps);

    // Update cached sensor_keys if the key set changed (rare — only on hardware change)
    // Zero-alloc comparison: check length + element-wise before cloning
    let keys_changed = {
        let cache = read_lock(&state.thermal.sensor_cache);
        cache.keys.len() != t.temps.len()
            || cache.keys.iter().zip(t.temps.keys()).any(|(a, b)| a.as_str() != b.as_str())
    };
    if keys_changed {
        let new_keys: Vec<String> = t.temps.keys().cloned().collect();
        let config = read_lock(&state.lifecycle.config);
        let sorted = crate::types::sorted_sensor_list(&config.telemetry.selected_sensors, &new_keys);
        let colors: Vec<iced::Color> = sorted.iter()
            .map(|name| crate::style::sensor_color(name, &new_keys))
            .collect();
        with_write_lock(&state.thermal.sensor_cache, |g| {
            *g = Arc::new(crate::app::SensorCache { keys: new_keys, sorted: Arc::new(sorted), colors: Arc::new(colors) });
        });
    }

    with_write_lock(&state.thermal.data, |guard| {
        *guard = Arc::new(Some(t));
    });

    with_write_lock(&state.thermal.history, |hist| {
        let hist = Arc::make_mut(hist);
        hist.push_sample(
            temp_chart::TempSample {
                ts_ms: now,
                temps: temps_for_history,
            },
            now,
        );
    });
    true
}

pub(crate) async fn refresh_all_data(state: &AppState, ec: &std::sync::Arc<cli::EcClient>) {
    let ec_clone = Arc::clone(ec);
    let thermal_fut = tokio::task::spawn_blocking(move || ec_clone.thermal());
    let battery_ref = Arc::clone(&state.battery.info);
    let ec_clone2 = Arc::clone(ec);
    let power_fut = tokio::task::spawn_blocking(move || ec_clone2.power());
    let kblight_ref = Arc::clone(&state.peripherals.kblight);
    let ec_clone3 = Arc::clone(ec);
    let kb_fut = tokio::task::spawn_blocking(move || ec_clone3.kblight_get());
    let pd_ports_ref = Arc::clone(&state.peripherals.pd_ports);
    let pd_history_ref = Arc::clone(&state.peripherals.pd_ports_history);
    let ec_clone4 = Arc::clone(ec);
    let pd_fut = tokio::task::spawn_blocking(move || ec_clone4.pd_ports());
    let exp_ref = Arc::clone(&state.peripherals.expansion_cards);
    let ec_clone5 = Arc::clone(ec);
    let exp_fut = tokio::task::spawn_blocking(move || ec_clone5.expansion_cards());

    let (thermal_result, power_result, kb_result, pd_result, exp_result) = tokio::join!(
        thermal_fut, power_fut, kb_fut, pd_fut, exp_fut
    );

    if let Ok(Ok(t)) = thermal_result {
        record_thermal_sample(state, t);
    }
    if let Ok(Ok(bat)) = power_result {
        with_write_lock(&battery_ref, |guard| {
            *guard = Arc::new(Some(crate::types::BatteryInfo { power_info: bat }));
        });
    }
    if let Ok(Ok(kb)) = kb_result {
        with_write_lock(&kblight_ref, |guard| {
            *guard = Arc::new(Some(kb));
        });
    }
    if let Ok(ports) = pd_result {
        with_write_lock(&pd_ports_ref, |guard| {
            *guard = Arc::new(ports);
        });
        mark_pd_usb_c_seen(&read_lock(&pd_ports_ref), &state.peripherals.pd_usb_c_seen);
        push_pd_ports_history(&pd_ports_ref, &pd_history_ref);
    }
    if let Ok(cards) = exp_result {
        with_write_lock(&exp_ref, |guard| {
            *guard = Arc::new(cards);
        });
    }
    mark_view_dirty(state);
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
                let saved_duty = bg_state2.fan.last_applied_duty.load(Ordering::Acquire) as u32;
                let (init_fan_mode, init_is_manual) = {
                    let init_config = read_lock(&bg_state2.lifecycle.config);
                    (init_config.fan.mode, matches!(init_config.fan.mode, crate::types::FanControlMode::Manual))
                };
                // Startup in Disabled (Auto) mode must still restore firmware
                // fan control: the EC may hold a stale manual duty from a
                // previous session (crash or "quit without restore"). Seed
                // last_fan_mode as None so the Disabled branch's "entered
                // Disabled" action runs once on the first iteration. Other
                // modes keep Some(init) so their first iteration is not
                // treated as a mode change (which would discard the
                // saved-duty ramp seed).
                let mut last_fan_mode: Option<crate::types::FanControlMode> =
                    if init_fan_mode == crate::types::FanControlMode::Disabled {
                        None
                    } else {
                        Some(init_fan_mode)
                    };
                let mut last_manual_duty: Option<u32> = None;
                let mut consecutive_ec_failures: u32 = 0;
                let mut consecutive_ec_write_failures: u32 = 0;
                // Monotonic time of the last successful fan duty write.
                // Drives the periodic re-assert that recovers the fans when
                // a sleep/resume cycle was not detected (missed event).
                let mut last_duty_write_ms: u64 = 0;
                // Monotonic time of the last curve fail-safe autofanctrl()
                // handover (see CURVE_TEMP_FAILOVER_MS).
                let mut last_curve_failover_ms: u64 = 0;
                // True on the iteration right after a resume was detected.
                // Manual control then re-asserts itself with at least one
                // duty write even when the ramp estimate already equals the
                // target, since the EC reverted to firmware control during
                // sleep and needs an explicit write to hand control back.
                let mut just_resumed = false;
                let mut manual_ramp_current: Option<u32> = if saved_duty > 0 {
                    Some(saved_duty)
                } else if init_is_manual {
                    estimate_duty_from_thermal(&bg_state2)
                } else {
                    None
                };
                // Per-fan ramp state for Manual mode (unified_duty = false).
                // One entry per fan: the last duty actually written to that
                // fan. Seeded from the RPM-based estimate on first use so
                // per-fan mode ramps like the unified path instead of
                // jumping straight to the target duty.
                let mut manual_per_fan_ramp: Option<Vec<u32>> = None;
                let mut curve_stepper = if saved_duty > 0 { CurveStepper::with_last_duty(saved_duty) } else { CurveStepper::new() };
                let start_ms = crate::util::monotonic_ms();
                let mut last_expansion_scan: u64 = start_ms;
                let mut last_versions_scan: u64 = start_ms;
                let mut last_cpu_power_poll: u64 = 0;
                let mut last_resume_ts: u64 = 0;
                'poll_loop: loop {
                    let resume_ts = bg_state2.lifecycle.last_resume_ts.load(Ordering::Acquire);
                    if resume_ts != 0 && resume_ts != last_resume_ts {
                        last_resume_ts = resume_ts;
                        curve_stepper.reset();
                        last_manual_duty = None;
                        manual_ramp_current = None;
                        manual_per_fan_ramp = None;
                        // Also forget the last seen mode: the EC may have
                        // reverted to firmware fan control during sleep, so
                        // the next iteration must re-assert the selected mode
                        // (Disabled → run autofanctrl once; Manual/Curve →
                        // re-seed the ramp states and write again).
                        last_fan_mode = None;
                        just_resumed = true;
                        bg_state2.system.cli_available.store(false, Ordering::Release);
                        with_write_lock(&bg_state2.system.ec_client, |guard| {
                            *guard = Arc::new(None);
                        });
                        bg_state2.fan.last_fan_rpm_reset.store(resume_ts, Ordering::Release);
                        // Keep fan_max_rpm from before sleep: it is the fan's
                        // real max-RPM baseline, so estimate_duty_from_thermal
                        // still computes the actual current duty after resume
                        // and the ramp starts from the real fan speed. Only
                        // last_fan_rpm_reset moves forward so the rolling
                        // re-baseline is deferred by 60s past resume. Resetting
                        // fan_max_rpm here made the estimate read ~100% (the
                        // first post-resume rpm would become the new max).
                        bg_state2.fan.last_applied_duty.store(0, Ordering::Release);
                        with_write_lock(&bg_state2.thermal.data, |guard| {
                            *guard = Arc::new(None);
                        });
                        with_write_lock(&bg_state2.thermal.history, |hist| {
                            let hist = Arc::make_mut(hist);
                            *hist = temp_chart::ThermalHistory::new();
                        });
                        with_write_lock(&bg_state2.thermal.sensor_cache, |guard| {
                            *guard = Arc::new(crate::app::SensorCache::default());
                        });
                        tracing::warn!("[RESUME] EC client, fan state, and thermal history reset after system resume");
                    }
                    // Read fan mode from atomic (no config lock needed)
                    let fan_mode = crate::types::FanControlMode::from_u8(
                        bg_state2.fan.mode.load(Ordering::Acquire) as u8
                    );
                    let interval = match fan_mode {
                        crate::types::FanControlMode::Curve => {
                            bg_state2.fan.curve_poll_ms.load(Ordering::Acquire).max(POLL_RATE_MIN_MS as u64)
                        }
                        _ => bg_state2.lifecycle.poll_ms.load(Ordering::Acquire).max(POLL_RATE_MIN_MS as u64),
                    };
                    let mut now_ms = crate::util::monotonic_ms();
                    let last_interaction = bg_state2.lifecycle.last_interaction_ts.load(Ordering::Acquire);
                    let is_idle = now_ms.saturating_sub(last_interaction) > IDLE_THRESHOLD_MS;
                    let effective_interval = match fan_mode {
                        // Fan curve must keep responding to temperature even when the
                        // user is idle; the idle slowdown only applies to non-curve modes.
                        crate::types::FanControlMode::Curve => interval,
                        _ if is_idle => IDLE_INTERVAL_MS,
                        _ => interval,
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(effective_interval)).await;
                    if bg_state2.lifecycle.shutdown.load(Ordering::Acquire) { return; }

                    let ec_opt = { read_lock(&bg_state2.system.ec_client) };
                    let ec: Arc<cli::EcClient> = match ec_opt.as_ref().as_ref() {
                        Some(c) => Arc::clone(c),
                        None => {
                                let state_cl = bg_state2.clone();
                                match tokio::task::spawn_blocking(cli::EcClient::new).await {
                                    Ok(Ok(c)) => {
                                        let arc_ec = Arc::new(c);
                                        with_write_lock(&state_cl.system.ec_client, |guard| {
                                            *guard = Arc::new(Some(Arc::clone(&arc_ec)));
                                        });
                                        state_cl.system.cli_available.store(true, Ordering::Release);
                                    arc_ec
                                }
                                Ok(Err(e)) => {
                                    warn!("Background loop: failed to init EC: {}", e);
                                    state_cl.system.cli_available.store(false, Ordering::Release);
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    continue;
                                }
                                Err(e) => {
                                    warn!("Background loop: EC spawn_blocking panicked: {}", e);
                                    state_cl.system.cli_available.store(false, Ordering::Release);
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    continue;
                                }
                            }
                        }
                    };

                    let ec_clone = Arc::clone(&ec);
                    match tokio::task::spawn_blocking(move || ec_clone.thermal()).await {
                        Ok(Ok(t)) => {
                            consecutive_ec_failures = 0;
                            if record_thermal_sample(&bg_state2, t) {
                                mark_view_dirty(&bg_state2);
                            }
                        }
                        Ok(Err(e)) => {
                            warn!("Thermal read failed: {}", e);
                            consecutive_ec_failures += 1;
                            if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                reset_ec_after_failures(&bg_state2, consecutive_ec_failures);
                                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                continue;
                            }
                        }
                        Err(join_err) => {
                            warn!("EC thermal spawn panicked: {}", join_err);
                            consecutive_ec_failures += 1;
                            if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                reset_ec_after_failures(&bg_state2, consecutive_ec_failures);
                                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                continue;
                            }
                        }
                    }
                    // While the user is idle, skip the per-cycle UI-only reads (battery) to
                    // save subprocess spawns; thermal still feeds the fan curve.
                    if !is_idle {
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(Ok(bat)) = tokio::task::spawn_blocking(move || ec_clone.power()).await {
                            let ac_now = bat.ac_present == Some(true);
                            // Detect AC→battery transition: signal PL reset on next tick.
                            let ac_was = bg_state2.battery.prev_ac_present.load(Ordering::Acquire);
                            if ac_was && !ac_now {
                                bg_state2.lifecycle.pl_reset_pending.store(true, Ordering::Release);
                                tracing::info!("AC→battery transition detected, PL1/PL2 reset pending");
                            }
                            bg_state2.battery.prev_ac_present.store(ac_now, Ordering::Release);

                            with_write_lock(&bg_state2.battery.info, |guard| {
                                let new_info = crate::types::BatteryInfo { power_info: bat };
                                if guard.as_ref().as_ref() != Some(&new_info) {
                                    *guard = Arc::new(Some(new_info));
                                }
                            });
                            mark_view_dirty(&bg_state2);
                        }
                    }

                    // Expansion / PD scans run on fixed wall-clock intervals
                    // so hotplug is always detectable.
                    now_ms = crate::util::monotonic_ms();
                    if now_ms.saturating_sub(last_expansion_scan) >= EXPANSION_SCAN_MS {
                        last_expansion_scan = now_ms;
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(ports) = tokio::task::spawn_blocking(move || ec_clone.pd_ports()).await {
                            let changed = {
                                let current = read_lock(&bg_state2.peripherals.pd_ports);
                                *current != ports
                            };
                            if changed {
                                with_write_lock(&bg_state2.peripherals.pd_ports, |guard| {
                                    *guard = Arc::new(ports);
                                });
                                mark_view_dirty(&bg_state2);
                            }
                            mark_pd_usb_c_seen(&read_lock(&bg_state2.peripherals.pd_ports), &bg_state2.peripherals.pd_usb_c_seen);
                            push_pd_ports_history(&bg_state2.peripherals.pd_ports, &bg_state2.peripherals.pd_ports_history);
                        }
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(cards) = tokio::task::spawn_blocking(move || ec_clone.expansion_cards()).await {
                            with_write_lock(&bg_state2.peripherals.expansion_cards, |guard| {
                                if **guard != cards {
                                    *guard = Arc::new(cards);
                                    mark_view_dirty(&bg_state2);
                                }
                            });
                        }
                    }
                    if now_ms.saturating_sub(last_versions_scan) >= VERSIONS_REFRESH_MS {
                        last_versions_scan = now_ms;
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(Ok(v)) = tokio::task::spawn_blocking(move || ec_clone.versions()).await {
                            with_write_lock(&bg_state2.system.versions, |guard| {
                                if guard.as_ref().as_ref() != Some(&v) {
                                    *guard = Arc::new(Some(v));
                                    mark_view_dirty(&bg_state2);
                                }
                            });
                        }
                    }

                    // CPU power (PL1/PL2) via PawnIO — poll every 5 seconds.
                    // This is a slow operation (MSR + MMIO reads) so we keep
                    // the interval long and only refresh when the UI is active.
                    const CPU_POWER_POLL_MS: u64 = 5000;
                    if bg_state2.system.intel_cpu.load(Ordering::Acquire)
                        && !is_idle
                        && now_ms.saturating_sub(last_cpu_power_poll) >= CPU_POWER_POLL_MS
                    {
                        last_cpu_power_poll = now_ms;
                        // refresh() runs PawnIO ioctls; keep it off the async
                        // worker like the other EC I/O in this loop.
                        let cpu_power = bg_state2.cpu_power.clone();
                        let _ = tokio::task::spawn_blocking(move || cpu_power.refresh()).await;
                        mark_view_dirty(&bg_state2);
                    }

                    // Shutdown re-check after scans: a quit initiated during the
                    // async thermal/scan awaits must not be overwritten by fan
                    // control (restore or quit duty) below.
                    if bg_state2.lifecycle.shutdown.load(Ordering::Acquire) { return; }

                    // Read only the fields we need, then drop the lock immediately.
                    // This avoids holding the config lock across ~100ms EC I/O.
                    let (manual_duty, curve_hysteresis, curve_rate_limit, curve_rate_limit_down, curve_sensors) = {
                        let config = read_lock(&bg_state2.lifecycle.config);
                        (
                            config.fan.manual.as_ref().map(|m| m.duty_pct),
                            config.fan.curve.as_ref().map(|c| c.curve.hysteresis_c),
                            config.fan.curve.as_ref().map(|c| c.curve.rate_limit_pct_per_step),
                            config.fan.curve.as_ref().and_then(|c| c.curve.rate_limit_down_pct_per_step),
                            config.fan.curve.as_ref().map(|c| c.curve.sensors.clone()).unwrap_or_default(),
                        )
                    }; // config lock released here

                    let mode = crate::types::FanControlMode::from_u8(
                        bg_state2.fan.mode.load(Ordering::Acquire) as u8
                    );
                    if last_fan_mode.as_ref() != Some(&mode) {
                        curve_stepper.reset();
                        last_manual_duty = None;
                        manual_per_fan_ramp = None;
                        if matches!(mode, crate::types::FanControlMode::Manual) {
                            // Prefer the last duty actually written (most
                            // reliable seed); fall back to the RPM estimate
                            // only when nothing was written yet (fresh boot,
                            // or after resume where last_applied_duty is 0
                            // and the EC reset fan control during sleep).
                            let last_duty = bg_state2.fan.last_applied_duty.load(Ordering::Acquire) as u32;
                            manual_ramp_current = if last_duty > 0 {
                                Some(last_duty)
                            } else {
                                estimate_duty_from_thermal(&bg_state2)
                            };
                        } else {
                            manual_ramp_current = None;
                        }
                    }
                    match &mode {
                        crate::types::FanControlMode::Disabled => {
                            // None (startup in Auto mode) counts as "entered
                            // Disabled": restore firmware fan control once.
                            if last_fan_mode.as_ref().is_none_or(|m| m != &crate::types::FanControlMode::Disabled) {
                                let ec_clone = Arc::clone(&ec);
                                match tokio::task::spawn_blocking(move || ec_clone.autofanctrl()).await {
                                    Ok(result) => {
                                        if let Err(e) = result {
                                            warn!("Failed to restore auto fan control: {}", e);
                                            consecutive_ec_write_failures += 1;
                                            if consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                                                reset_ec_after_failures(&bg_state2, consecutive_ec_write_failures);
                                                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                                continue 'poll_loop;
                                            }
                                        } else {
                                            consecutive_ec_write_failures = 0;
                                            last_duty_write_ms = now_ms;
                                        }
                                    }
                                    Err(join_err) => {
                                        warn!("EC spawn panicked (autofanctrl): {}", join_err);
                                        reset_ec_on_panic(&bg_state2);
                                        continue;
                                    }
                                }
                                let mode_now = crate::types::FanControlMode::from_u8(
                                    bg_state2.fan.mode.load(Ordering::Acquire) as u8
                                );
                                if mode_now != mode { last_fan_mode = Some(mode); continue; }
                            }
                        }
                        crate::types::FanControlMode::Manual => {
                            let unified = bg_state2.fan.unified_duty.load(Ordering::Acquire);
                            if unified {
                                if let Some(target) = manual_duty {
                                    let current = manual_ramp_current.unwrap_or(target);
                                    let next = crate::fan_control::apply_rate_limit(current, target, 10);
                                    let converged = last_manual_duty == Some(next);
                                    // Even when converged, periodically re-assert the duty:
                                    // the EC resets fan control during sleep, and if the
                                    // resume event was missed the fan would stay off with
                                    // no write to bring it back.
                                    let reassert = converged
                                        && now_ms.saturating_sub(last_duty_write_ms) >= FAN_REASSERT_INTERVAL_MS;
                                    if converged && !reassert {
                                        manual_ramp_current = Some(next);
                                    } else {
                                        let ec_clone = Arc::clone(&ec);
                                        match tokio::task::spawn_blocking(move || ec_clone.set_fan_duty(next, None)).await {
                                            Ok(result) => match result {
                                                Ok(()) => {
                                                    let mode_now = crate::types::FanControlMode::from_u8(
                                                        bg_state2.fan.mode.load(Ordering::Acquire) as u8
                                                    );
                                                    if mode_now != mode { last_fan_mode = Some(mode); continue; }
                                                    consecutive_ec_write_failures = 0;
                                                    last_duty_write_ms = now_ms;
                                                    bg_state2.fan.last_applied_duty.store(next as u64, Ordering::Release);
                                                    last_manual_duty = Some(next);
                                                    manual_ramp_current = Some(next);
                                                }
                                                Err(e) => {
                                                    warn!("Failed to set manual fan duty: {}", e);
                                                    consecutive_ec_write_failures += 1;
                                                    if consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                                                        reset_ec_after_failures(&bg_state2, consecutive_ec_write_failures);
                                                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                                        continue 'poll_loop;
                                                    }
                                                }
                                            },
                                            Err(join_err) => {
                                                warn!("EC spawn panicked (set_fan_duty): {}", join_err);
                                                reset_ec_on_panic(&bg_state2);
                                                continue;
                                            }
                                        }
                                    }
                                }
                            } else {
                                // Per-fan manual: each fan ramps independently toward its
                                // configured duty. Deliberately NOT gated by the unified
                                // duty's ramp state (last_manual_duty), otherwise slider
                                // changes would stop being applied once the unified ramp
                                // converges. last_applied_duty tracks the max actually
                                // written so the quit warning shows a real value.
                                let per_fan = read_lock(&bg_state2.fan.per_fan_duty);
                                if !per_fan.is_empty() {
                                    let ramp = manual_per_fan_ramp.get_or_insert_with(|| {
                                        let last_duty = bg_state2.fan.last_applied_duty.load(Ordering::Acquire) as u32;
                                        let seed = if last_duty > 0 {
                                            last_duty
                                        } else {
                                            estimate_duty_from_thermal(&bg_state2)
                                                .unwrap_or_else(|| per_fan[0])
                                        };
                                        vec![seed; per_fan.len()]
                                    });
                                    let mut wrote_any = false;
                                    for (idx, &target) in per_fan.iter().enumerate() {
                                        let current = ramp.get(idx).copied().unwrap_or(target);
                                        let next_i = crate::fan_control::apply_rate_limit(current, target, 10);
                                        // On the resume cycle, still write when the
                                        // estimate already equals the target: the EC
                                        // reverted to firmware control during sleep
                                        // and needs one explicit duty write to hand
                                        // control back. Same for the periodic
                                        // re-assert when the ramp has converged — a
                                        // missed resume event would otherwise leave
                                        // the fans off with no write to bring them
                                        // back.
                                        let reassert = now_ms.saturating_sub(last_duty_write_ms)
                                            >= FAN_REASSERT_INTERVAL_MS;
                                        if next_i == current && !just_resumed && !reassert {
                                            continue;
                                        }
                                        // Re-check mode between fans to avoid setting duties
                                        // after the user has switched away from Manual mode.
                                        let mode_check = crate::types::FanControlMode::from_u8(
                                            bg_state2.fan.mode.load(Ordering::Acquire) as u8
                                        );
                                        if mode_check != mode {
                                            break;
                                        }
                                        let ec_clone = Arc::clone(&ec);
                                        let fan_idx = idx as u32;
                                        match tokio::task::spawn_blocking(move || ec_clone.set_fan_duty(next_i, Some(fan_idx))).await {
                                            Ok(Ok(())) => {
                                                ramp[idx] = next_i;
                                                wrote_any = true;
                                                consecutive_ec_write_failures = 0;
                                                last_duty_write_ms = now_ms;
                                            }
                                            Ok(Err(e)) => {
                                                warn!("Failed to set fan {} duty: {}", fan_idx, e);
                                                consecutive_ec_write_failures += 1;
                                                if consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                                                    reset_ec_after_failures(&bg_state2, consecutive_ec_write_failures);
                                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                                    continue 'poll_loop;
                                                }
                                            }
                                            Err(join_err) => {
                                                warn!("EC spawn panicked (set_fan_duty fan {}): {}", fan_idx, join_err);
                                                reset_ec_on_panic(&bg_state2);
                                                break;
                                            }
                                        }
                                    }
                                    // Update on any successful write, including convergence at 0% —
                                    // otherwise the quit warning and ramp seed
                                    // would keep a stale pre-per-fan value.
                                    if wrote_any {
                                        let max_applied = ramp.iter().copied().max().unwrap_or(0);
                                        bg_state2.fan.last_applied_duty.store(max_applied as u64, Ordering::Release);
                                    }
                                    // Keep the unified ramp seed in sync with the per-fan
                                    // duties so switching back to the unified slider starts
                                    // from the level the fans are actually at, not a stale
                                    // pre-per-fan value.
                                    manual_ramp_current = ramp.iter().copied().max();
                                }
                            }
                        }
                        crate::types::FanControlMode::Curve => {
                            let thermal_clone = read_lock(&bg_state2.thermal.data);
                            if let Some(ref thermal) = *thermal_clone
                                && let (Some(hyst), Some(rate)) =
                                    (curve_hysteresis, curve_rate_limit)
                            {
                                    // Fail-safe: an empty temps map means the
                                    // EC temperature read failed (ec_wrapper
                                    // returns Ok with empty temps on read
                                    // error while fan RPMs may still read).
                                    // curve_control_temp would then return 0
                                    // and the stepper would write 0% duty —
                                    // fans off with no firmware protection.
                                    // Hand control back to the EC firmware,
                                    // which has its own thermal protection.
                                    if thermal.temps.is_empty() {
                                        // Treat silent empty temps like a read
                                        // error so a dead EC is eventually
                                        // reset: autofanctrl() alone can never
                                        // recover it, and counting here feeds
                                        // the same MAX_EC_IO_FAILURES path as
                                        // an explicit thermal() error.
                                        consecutive_ec_failures += 1;
                                        if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                            reset_ec_after_failures(&bg_state2, consecutive_ec_failures);
                                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                            continue 'poll_loop;
                                        }
                                        if now_ms.saturating_sub(last_curve_failover_ms) >= CURVE_TEMP_FAILOVER_MS {
                                            let ec_clone = Arc::clone(&ec);
                                            match tokio::task::spawn_blocking(move || ec_clone.autofanctrl()).await {
                                                Ok(Ok(())) => {
                                                    last_curve_failover_ms = now_ms;
                                                    consecutive_ec_failures = 0;
                                                    tracing::warn!("Curve mode: EC temps read failed, handed fan control to firmware");
                                                }
                                                Ok(Err(e)) => warn!("Curve fail-safe autofanctrl failed: {}", e),
                                                Err(join_err) => {
                                                    warn!("EC spawn panicked (curve fail-safe autofanctrl): {}", join_err);
                                                    reset_ec_on_panic(&bg_state2);
                                                }
                                            }
                                        }
                                        continue 'poll_loop;
                                    }
                                    let control_temp = crate::types::curve_control_temp(&thermal.temps, &curve_sensors);
                                    let full_pts_arc = read_lock(&bg_state2.fan.curve_full_points);
                                    let full_pts: &[[u32; 2]] = &full_pts_arc;
                                    let mut next = curve_stepper.next(control_temp, hyst, rate, curve_rate_limit_down, full_pts);
                                    // The stepper only yields a value when the duty
                                    // changes. When it has converged, periodically
                                    // re-assert the last duty anyway: a missed resume
                                    // event would otherwise leave the fans off.
                                    if next.is_none()
                                        && now_ms.saturating_sub(last_duty_write_ms) >= FAN_REASSERT_INTERVAL_MS
                                    {
                                        next = curve_stepper.current_duty();
                                    }
                                    if let Some(next) = next {
                                        let fan_count = bg_state2.fan.fan_count.load(Ordering::Acquire) as u32;
                                        let write_each = !bg_state2.fan.unified_duty.load(Ordering::Acquire) && fan_count > 1;
                                        // Only advance the stepper when the duty was
                                        // actually applied to every target fan: a
                                        // partial success must not distort the
                                        // rate-limit basis (the next step would be
                                        // computed from an unapplied value).
                                        let mut applied = false;
                                        if write_each {
                                            let mut all_fans_applied = true;
                                            for fan_idx in 0..fan_count {
                                                let mode_check = crate::types::FanControlMode::from_u8(
                                                    bg_state2.fan.mode.load(Ordering::Acquire) as u8
                                                );
                                                if mode_check != mode {
                                                    all_fans_applied = false;
                                                    break;
                                                }
                                                let ec_clone = Arc::clone(&ec);
                                                match tokio::task::spawn_blocking(move || ec_clone.set_fan_duty(next, Some(fan_idx))).await {
                                                    Ok(result) => {
                                                        if let Err(e) = result {
                                                            warn!("Failed to set fan {} duty (curve): {}", fan_idx, e);
                                                            consecutive_ec_write_failures += 1;
                                                            all_fans_applied = false;
                                                            if consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                                                                reset_ec_after_failures(&bg_state2, consecutive_ec_write_failures);
                                                                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                                                continue 'poll_loop;
                                                            }
                                                        } else {
                                                            consecutive_ec_write_failures = 0;
                                                            last_duty_write_ms = now_ms;
                                                        }
                                                    }
                                                    Err(join_err) => {
                                                        warn!("EC spawn panicked (set_fan_duty curve fan {}): {}", fan_idx, join_err);
                                                        reset_ec_on_panic(&bg_state2);
                                                        all_fans_applied = false;
                                                        break;
                                                    }
                                                }
                                            }
                                            applied = all_fans_applied;
                                            if applied {
                                                bg_state2.fan.last_applied_duty.store(next as u64, Ordering::Release);
                                            }
                                        } else {
                                            let ec_clone = Arc::clone(&ec);
                                            match tokio::task::spawn_blocking(move || ec_clone.set_fan_duty(next, None)).await {
                                                Ok(result) => match result {
                                                    Ok(()) => {
                                                        let mode_now = crate::types::FanControlMode::from_u8(
                                                            bg_state2.fan.mode.load(Ordering::Acquire) as u8
                                                        );
                                                        if mode_now != mode { last_fan_mode = Some(mode); continue; }
                                                        consecutive_ec_write_failures = 0;
                                                        last_duty_write_ms = now_ms;
                                                        bg_state2.fan.last_applied_duty.store(next as u64, Ordering::Release);
                                                        applied = true;
                                                    }
                                                    Err(e) => {
                                                        warn!("Failed to set fan duty (curve): {}", e);
                                                        consecutive_ec_write_failures += 1;
                                                        if consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                                                            reset_ec_after_failures(&bg_state2, consecutive_ec_write_failures);
                                                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                                            continue 'poll_loop;
                                                        }
                                                    }
                                                },
                                                Err(join_err) => {
                                                    warn!("EC spawn panicked (set_fan_duty curve): {}", join_err);
                                                    reset_ec_on_panic(&bg_state2);
                                                    continue;
                                                }
                                            }
                                        }
                                        if applied {
                                            curve_stepper.note_applied(next);
                                        }
                                    }
                            }
                        }
                    }
                    last_fan_mode = Some(mode);
                    just_resumed = false;
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
                if state.lifecycle.shutdown.load(Ordering::Acquire) { break; }
            } else {
                // Inner task exited normally — only happens when shutdown is set.
                // Break immediately to avoid wasting resources during shutdown.
                break;
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

    #[test]
    fn record_thermal_sample_updates_on_fan_rpm_change() {
        use crate::app::AppState;
        use crate::cli::ec_wrapper::{FanReading, ThermalData};
        use std::collections::BTreeMap;

        let state = AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        let mut temps = BTreeMap::new();
        temps.insert("CPU".to_string(), 60);
        let sample = |rpm: u32| ThermalData {
            temps: Arc::new(temps.clone()),
            fans: vec![FanReading { name: "Fan 1".to_string(), rpm }],
        };

        assert!(record_thermal_sample(&state, sample(2000)));
        assert_eq!(
            read_lock(&state.thermal.data).as_ref().as_ref().unwrap().fans[0].rpm,
            2000
        );

        // Same temps, different RPM — must still be stored and flagged dirty.
        assert!(record_thermal_sample(&state, sample(3200)));
        assert_eq!(
            read_lock(&state.thermal.data).as_ref().as_ref().unwrap().fans[0].rpm,
            3200
        );

        // Identical data — no write, not dirty.
        assert!(!record_thermal_sample(&state, sample(3200)));
    }
}
