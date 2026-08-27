use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use crate::app::AppState;
use crate::cli;
use crate::fan_control::CurveStepper;
use crate::style::{
    EXPANSION_SCAN_MS, IDLE_INTERVAL_MS, IDLE_THRESHOLD_MS, POLL_RATE_MIN_MS, VERSIONS_REFRESH_MS,
};
use crate::temp_chart;
use crate::util::{read_lock, with_write_lock};

/// PD history depth to distinguish USB-A cards from transient USB device states.
const MAX_PD_HISTORY: usize = 3;

/// Consecutive EC read failures before reinitializing client (recovers after sleep/resume).
const MAX_EC_IO_FAILURES: u32 = 5;

/// Consecutive EC write failures before reinitializing client (separate from reads).
const MAX_EC_WRITE_FAILURES: u32 = 5;

/// Re-assert interval for converged fan duty; recovers fans if resume event was missed.
const FAN_REASSERT_INTERVAL_MS: u64 = 30_000;

/// Rate-limit for curve fail-safe handover to firmware when temps are empty.
const CURVE_TEMP_FAILOVER_MS: u64 = 30_000;

/// Maximum age of thermal sample before curve considers it stale.
const THERMAL_STALE_MS: u64 = 5_000;

/// Battery data considered stale after this duration without successful read.
const BATTERY_STALE_MS: u64 = 15_000;

/// Resets EC client after spawn panic so next iteration reinitializes.
fn reset_ec_on_panic(state: &AppState) {
    warn!("Resetting EC client after spawn panic");
    state.system.cli_available.store(false, Ordering::Release);
    with_write_lock(&state.system.ec_client, |guard| {
        *guard = Arc::new(None);
    });
    state
        .lifecycle
        .fan_reset_pending
        .store(true, Ordering::Release);
}

/// Forces EC reinitialization after consecutive failures (recovers stale driver after resume).
fn reset_ec_after_failures(state: &AppState, failures: u32) {
    warn!(
        "EC unresponsive after {} consecutive read failures, reinitializing client",
        failures
    );
    state.system.cli_available.store(false, Ordering::Release);
    with_write_lock(&state.system.ec_client, |guard| {
        *guard = Arc::new(None);
    });
    state
        .lifecycle
        .fan_reset_pending
        .store(true, Ordering::Release);
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
            tracing::debug!(
                "[AFFINITY] Verify: core {} exceeds bit width ({}), skipping",
                expected_id,
                usize::BITS
            );
            return;
        }
        let mask = 1usize << expected_id;
        let prev = unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) };
        let prev_core = prev.trailing_zeros() as usize;
        let ok = prev_core == expected_id;
        tracing::debug!(
            "[AFFINITY] Verify: prev_mask=0x{:X}, prev_core={}, expected={} {}",
            prev,
            prev_core,
            expected_id,
            if ok { "OK (confirmed)" } else { "UNEXPECTED" }
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = expected_id;
    }
}

fn push_pd_ports_history(
    pd_ports: &Arc<RwLock<Arc<smallvec::SmallVec<[cli::ec_wrapper::UsbCPort; 4]>>>>,
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

/// Marks ports ever seen as Sink as USB-C (persists beyond history window).
fn mark_pd_usb_c_seen(ports: &[cli::ec_wrapper::UsbCPort], seen_ref: &Arc<RwLock<Arc<Vec<bool>>>>) {
    if !ports.iter().any(|p| p.power_role == Some("Sink")) {
        return;
    }
    with_write_lock(seen_ref, |guard| {
        let mut seen = (**guard).clone();
        // Size vec for highest port index; EC skips failed ports so indices are sparse.
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
    state
        .lifecycle
        .view_generation
        .fetch_add(1, Ordering::Release);
}

/// Returns true if shutdown requested; fan writes must not be issued after quit write.
fn shutdown_requested(state: &AppState) -> bool {
    state.lifecycle.shutdown.load(Ordering::Acquire)
}

fn ensure_per_fan_duty(state: &AppState, fan_count: usize) {
    if fan_count == 0 {
        return;
    }
    // Process-global single-instance assumption; second AppState would cross-contaminate.
    static LAST_FAN_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Fast path: avoid write lock when count unchanged and duties sized.
    let last = LAST_FAN_COUNT.load(Ordering::Acquire) as usize;
    if fan_count == last {
        let len = read_lock(&state.fan.per_fan_duty).len();
        if len >= fan_count {
            return;
        }
    }
    let fill = {
        let config = read_lock(&state.lifecycle.config);
        config.fan.manual.as_ref().map(|m| m.duty_pct).unwrap_or(50)
    };
    let mut resized = false;
    with_write_lock(&state.fan.per_fan_duty, |guard| {
        let duties = Arc::make_mut(guard);
        if duties.len() >= fan_count {
            // Never truncate; preserves saved per-fan duties across undock/dock.
            return;
        }
        duties.resize(fan_count, fill);
        resized = true;
    });
    LAST_FAN_COUNT.store(fan_count as u64, Ordering::Release);
    // NOTE: Resize is not persisted; only tracks hardware changes. Config updates on explicit edits.
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
                // Seed stopped fans at 0.
                0
            } else if max_rpm == 0 {
                10
            } else {
                // Use u64 to avoid overflow.
                ((rpm as u64 * 100) / max_rpm as u64).clamp(10, 100) as u32
            }
        })
    } else {
        None
    }
}

#[allow(clippy::collapsible_if)]
fn record_thermal_sample(state: &AppState, t: cli::ec_wrapper::ThermalData) -> bool {
    // Periodically reset fan_max_rpm (60s) to adapt to hardware changes.
    const FAN_MAX_RPM_RESET_INTERVAL_MS: u64 = 60_000;
    let now_ts = crate::util::monotonic_ms();
    let fan_count = t.fans.len() as u64;
    state.fan.fan_count.store(fan_count, Ordering::Release);
    ensure_per_fan_duty(state, t.fans.len());
    if let Some(max_rpm) = t.fans.iter().map(|f| f.rpm).max() {
        let prev = state.fan.fan_max_rpm.load(Ordering::Acquire) as u32;
        let last_reset = state.fan.last_fan_rpm_reset.load(Ordering::Acquire);
        if now_ts.saturating_sub(last_reset) >= FAN_MAX_RPM_RESET_INTERVAL_MS {
            // Rolling re-baseline only raises max; lowering while idle would mis-seed ramp.
            state
                .fan
                .fan_max_rpm
                .store(max_rpm.max(prev) as u64, Ordering::Release);
            state
                .fan
                .last_fan_rpm_reset
                .store(now_ts, Ordering::Release);
        } else if max_rpm > prev {
            state
                .fan
                .fan_max_rpm
                .store(max_rpm as u64, Ordering::Release);
        }
    }
    // Monotonic timestamp for window pruning; wall clock would break on jumps.
    let now = crate::util::monotonic_ms() as i64;

    // Avoid cloning if unchanged; compare whole payload to catch fan RPM changes.
    let changed = {
        let cur = read_lock(&state.thermal.data);
        match cur.as_ref().as_ref() {
            Some(cur) => *cur != t,
            None => true,
        }
    };
    if !changed {
        // Push periodic sample to keep chart window sliding; throttle to 1/s when idle.
        let history = read_lock(&state.thermal.history);
        if let Some(last_ts) = history.last_timestamp() {
            if now - last_ts < 1_000 {
                return false;
            }
        }
    }

    // Share temps via Arc to avoid deep clone on history reads.
    let temps_for_history = std::sync::Arc::clone(&t.temps);

    // Update sensor cache if key set changed (rare hardware change).
    let keys_changed = {
        let cache = read_lock(&state.thermal.sensor_cache);
        cache.keys.len() != t.temps.len()
            || cache
                .keys
                .iter()
                .zip(t.temps.keys())
                .any(|(a, b)| a.as_str() != b.as_str())
    };
    if keys_changed {
        let new_keys: Vec<String> = t.temps.keys().cloned().collect();
        let config = read_lock(&state.lifecycle.config);
        let sorted =
            crate::types::sorted_sensor_list(&config.telemetry.selected_sensors, &new_keys);
        let colors: Vec<iced::Color> = sorted
            .iter()
            .map(|name| crate::style::sensor_color(name, &new_keys))
            .collect();
        with_write_lock(&state.thermal.sensor_cache, |g| {
            *g = Arc::new(crate::app::SensorCache {
                keys: new_keys,
                sorted: Arc::new(sorted),
                colors: Arc::new(colors),
            });
        });
    }

    with_write_lock(&state.thermal.data, |guard| {
        *guard = Arc::new(Some(t));
    });
    state
        .thermal
        .last_success_ms
        .store(now as u64, Ordering::Release);

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
    let battery_ref = Arc::clone(&state.battery.info);
    let kblight_ref = Arc::clone(&state.peripherals.kblight);
    let pd_ports_ref = Arc::clone(&state.peripherals.pd_ports);
    let pd_history_ref = Arc::clone(&state.peripherals.pd_ports_history);
    let exp_ref = Arc::clone(&state.peripherals.expansion_cards);
    let ec_clone = Arc::clone(ec);
    // Single blocking task for all EC reads to reduce wakes.
    let batch = crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
        (
            ec_clone.thermal(),
            ec_clone.power(),
            ec_clone.kblight_get(),
            ec_clone.pd_ports(),
            ec_clone.expansion_cards(),
        )
    })
    .await;
    let (thermal_result, power_result, kb_result, pd_ports, exp_cards) = match batch {
        Ok((t, p, kb, pd, exp)) => (t, p, kb, pd, exp),
        Err(join_err) => {
            warn!("refresh_all_data spawn panicked: {}", join_err);
            return;
        }
    };

    if let Ok(t) = thermal_result {
        record_thermal_sample(state, t);
    }
    if let Ok(bat) = power_result {
        with_write_lock(&battery_ref, |guard| {
            *guard = Arc::new(Some(crate::types::BatteryInfo { power_info: bat }));
        });
    }
    if let Ok(kb) = kb_result {
        with_write_lock(&kblight_ref, |guard| {
            *guard = Arc::new(Some(kb));
        });
    }
    {
        with_write_lock(&pd_ports_ref, |guard| {
            *guard = Arc::new(pd_ports);
        });
        mark_pd_usb_c_seen(&read_lock(&pd_ports_ref), &state.peripherals.pd_usb_c_seen);
        push_pd_ports_history(&pd_ports_ref, &pd_history_ref);
    }
    {
        with_write_lock(&exp_ref, |guard| {
            *guard = Arc::new(exp_cards);
        });
    }
    mark_view_dirty(state);
}

pub fn spawn(state: AppState) {
    const MAX_CONSECUTIVE_FAILURES: u32 = 10;
    tokio::spawn(async move {
        let mut consecutive_failures: u32 = 0;
        loop {
            let bg_state2 = state.clone();
            let handle = tokio::spawn(async move {
                let saved_duty = bg_state2.fan.last_applied_duty.load(Ordering::Acquire) as u32;
                let (init_fan_mode, init_is_manual) = {
                    let init_config = read_lock(&bg_state2.lifecycle.config);
                    (
                        init_config.fan.mode,
                        matches!(init_config.fan.mode, crate::types::FanControlMode::Manual),
                    )
                };
                // Seed Disabled as None to force firmware restore on first iteration.
                let mut last_fan_mode: Option<crate::types::FanControlMode> =
                    if init_fan_mode == crate::types::FanControlMode::Disabled {
                        None
                    } else {
                        Some(init_fan_mode)
                    };
                let mut last_manual_duty: Option<u32> = None;
                let mut consecutive_ec_failures: u32 = 0;
                let mut consecutive_ec_write_failures: u32 = 0;
                // Last successful duty write time for periodic re-assert.
                let mut last_duty_write_ms: u64 = 0;
                // Last curve fail-safe handover time.
                let mut last_curve_failover_ms: u64 = 0;
                // True on iteration after resume to re-assert manual control.
                let mut just_resumed = false;
                let mut manual_ramp_current: Option<u32> = if saved_duty > 0 {
                    Some(saved_duty)
                } else if init_is_manual {
                    estimate_duty_from_thermal(&bg_state2)
                } else {
                    None
                };
                // Per-fan ramp state for Manual mode (one entry per fan).
                let mut manual_per_fan_ramp: Option<Vec<u32>> = None;
                let mut curve_stepper = if saved_duty > 0 {
                    CurveStepper::with_last_duty(saved_duty)
                } else {
                    CurveStepper::new()
                };
                let start_ms = crate::util::monotonic_ms();
                let mut last_expansion_scan: u64 = start_ms;
                let mut last_versions_scan: u64 = start_ms;
                let mut versions_scan_attempts: u32 = 0;
                let mut last_cpu_power_poll: u64 = 0;
                let mut last_resume_ts: u64 = 0;
                'poll_loop: loop {
                    let resume_ts = bg_state2.lifecycle.last_resume_ts.load(Ordering::Acquire);
                    // Skip resume handling if shutdown requested.
                    if bg_state2.lifecycle.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if resume_ts != 0 && resume_ts != last_resume_ts {
                        last_resume_ts = resume_ts;
                        curve_stepper.reset();
                        last_manual_duty = None;
                        manual_ramp_current = None;
                        manual_per_fan_ramp = None;
                        // EC reverts to firmware control during sleep. Force the next
                        // iteration to re-assert the selected mode and re-seed ramps.
                        last_fan_mode = None;
                        just_resumed = true;
                        bg_state2
                            .system
                            .cli_available
                            .store(false, Ordering::Release);
                        with_write_lock(&bg_state2.system.ec_client, |guard| {
                            *guard = Arc::new(None);
                        });
                        bg_state2
                            .fan
                            .last_fan_rpm_reset
                            .store(resume_ts, Ordering::Release);
                        // Keep fan_max_rpm as the real max-RPM baseline for accurate
                        // ramp seeding after resume; only defer the rolling re-baseline
                        // by 60s via last_fan_rpm_reset.
                        bg_state2.fan.last_applied_duty.store(0, Ordering::Release);
                        // Reset duty write time so the 30s re-assert guard trips
                        // immediately on the resume iteration; otherwise Curve duty
                        // could stay un-asserted for up to 30s after a short sleep.
                        last_duty_write_ms = 0;
                        // Retain thermal history so the chart does not flash blank;
                        // stale samples are pruned by the normal window retention.
                        tracing::warn!(
                            "[RESUME] EC client and fan state reset after system resume (history retained)"
                        );
                    }
                    if bg_state2
                        .lifecycle
                        .fan_reset_pending
                        .swap(false, Ordering::Acquire)
                    {
                        curve_stepper.reset();
                        last_manual_duty = None;
                        manual_ramp_current = None;
                        manual_per_fan_ramp = None;
                        last_fan_mode = None;
                        last_duty_write_ms = 0;
                        bg_state2.fan.last_applied_duty.store(0, Ordering::Release);
                        tracing::warn!(
                            "[EC RESET] fan controller state invalidated, will re-assert mode"
                        );
                    }
                    // Read fan mode from atomic (no config lock needed)
                    let fan_mode = crate::types::FanControlMode::from_u8(
                        bg_state2.fan.mode.load(Ordering::Acquire) as u8,
                    );
                    let interval = match fan_mode {
                        crate::types::FanControlMode::Curve => {
                            // Curve interval is independently configurable.
                            let cfg = read_lock(&bg_state2.lifecycle.config);
                            cfg.fan
                                .curve
                                .as_ref()
                                .map(|c| c.poll_ms)
                                .unwrap_or(POLL_RATE_MIN_MS as u64)
                                .max(POLL_RATE_MIN_MS as u64)
                        }
                        _ => bg_state2
                            .lifecycle
                            .poll_ms
                            .load(Ordering::Acquire)
                            .max(POLL_RATE_MIN_MS as u64),
                    };
                    let mut now_ms = crate::util::monotonic_ms();
                    let last_interaction = bg_state2
                        .lifecycle
                        .last_interaction_ts
                        .load(Ordering::Acquire);
                    let is_idle = now_ms.saturating_sub(last_interaction) > IDLE_THRESHOLD_MS;
                    let effective_interval = match fan_mode {
                        // Curve must keep responding while idle; slowdown only for other modes.
                        crate::types::FanControlMode::Curve => interval,
                        _ if is_idle => IDLE_INTERVAL_MS,
                        _ => interval,
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(effective_interval)).await;
                    if bg_state2.lifecycle.shutdown.load(Ordering::Acquire) {
                        return;
                    }

                    let ec_opt = { read_lock(&bg_state2.system.ec_client) };
                    let ec: Arc<cli::EcClient> = match ec_opt.as_ref().as_ref() {
                        Some(c) => Arc::clone(c),
                        None => {
                            // Wait for init task before creating own client.
                            if !bg_state2.system.ec_init_done.load(Ordering::Acquire) {
                                continue;
                            }
                            let state_cl = bg_state2.clone();
                            match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, cli::EcClient::new).await {
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
                                    state_cl
                                        .system
                                        .cli_available
                                        .store(false, Ordering::Release);
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    continue;
                                }
                                Err(e) => {
                                    warn!("Background loop: EC spawn_blocking panicked: {}", e);
                                    state_cl
                                        .system
                                        .cli_available
                                        .store(false, Ordering::Release);
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    continue;
                                }
                            }
                        }
                    };

                    let ec_clone = Arc::clone(&ec);
                    match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || ec_clone.thermal()).await {
                        Ok(Ok(t)) => {
                            if t.temps.is_empty() {
                                warn!("Thermal read returned empty temps");
                                consecutive_ec_failures += 1;
                                if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                    reset_ec_after_failures(&bg_state2, consecutive_ec_failures);
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    continue;
                                }
                            } else {
                                consecutive_ec_failures = 0;
                                if record_thermal_sample(&bg_state2, t) {
                                    mark_view_dirty(&bg_state2);
                                }
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
                    // Skip UI-only reads while idle to save spawns.
                    if !is_idle {
                        let ec_clone = Arc::clone(&ec);
                        match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || ec_clone.power()).await {
                            Ok(Ok(bat)) => {
                                let ac_now = bat.ac_present == Some(true);
                                // Signal PL reset on AC->battery transition (only on success to avoid spurious).
                                let ac_was =
                                    bg_state2.battery.prev_ac_present.load(Ordering::Acquire);
                                if ac_was && !ac_now {
                                    bg_state2
                                        .lifecycle
                                        .pl_reset_pending
                                        .store(true, Ordering::Release);
                                    tracing::info!(
                                        "AC→battery transition detected, PL1/PL2 reset pending"
                                    );
                                }
                                bg_state2
                                    .battery
                                    .prev_ac_present
                                    .store(ac_now, Ordering::Release);
                                crate::cpu_power::publish_ac_snapshot(ac_now);
                                bg_state2
                                    .battery
                                    .last_success_ms
                                    .store(crate::util::monotonic_ms(), Ordering::Release);
                                with_write_lock(&bg_state2.battery.info, |guard| {
                                    let new_info = crate::types::BatteryInfo { power_info: bat };
                                    if guard.as_ref().as_ref() != Some(&new_info) {
                                        *guard = Arc::new(Some(new_info));
                                    }
                                });
                                mark_view_dirty(&bg_state2);
                            }
                            Ok(Err(e)) => {
                                tracing::debug!("Battery read failed: {}", e);
                                let last_ok =
                                    bg_state2.battery.last_success_ms.load(Ordering::Acquire);
                                let cur = crate::util::monotonic_ms();
                                if last_ok != 0 && cur.saturating_sub(last_ok) > BATTERY_STALE_MS {
                                    with_write_lock(&bg_state2.battery.info, |guard| {
                                        if guard.is_some() {
                                            *guard = Arc::new(None);
                                            mark_view_dirty(&bg_state2);
                                        }
                                    });
                                }
                            }
                            Err(e) => {
                                warn!("Battery spawn panicked: {}", e);
                                let last_ok =
                                    bg_state2.battery.last_success_ms.load(Ordering::Acquire);
                                let cur = crate::util::monotonic_ms();
                                if last_ok != 0 && cur.saturating_sub(last_ok) > BATTERY_STALE_MS {
                                    with_write_lock(&bg_state2.battery.info, |guard| {
                                        if guard.is_some() {
                                            *guard = Arc::new(None);
                                            mark_view_dirty(&bg_state2);
                                        }
                                    });
                                }
                            }
                        }
                    }

                    // Expansion/PD scans; lengthen interval when hidden to save power.
                    now_ms = crate::util::monotonic_ms();
                    let expansion_interval = if bg_state2.lifecycle.visible.load(Ordering::Acquire)
                    {
                        EXPANSION_SCAN_MS
                    } else {
                        30_000
                    };
                    if now_ms.saturating_sub(last_expansion_scan) >= expansion_interval {
                        last_expansion_scan = now_ms;
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(ports) =
                            crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || ec_clone.pd_ports()).await
                        {
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
                            mark_pd_usb_c_seen(
                                &read_lock(&bg_state2.peripherals.pd_ports),
                                &bg_state2.peripherals.pd_usb_c_seen,
                            );
                            push_pd_ports_history(
                                &bg_state2.peripherals.pd_ports,
                                &bg_state2.peripherals.pd_ports_history,
                            );
                        }
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(cards) =
                            crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || ec_clone.expansion_cards()).await
                        {
                            with_write_lock(&bg_state2.peripherals.expansion_cards, |guard| {
                                if **guard != cards {
                                    *guard = Arc::new(cards);
                                    mark_view_dirty(&bg_state2);
                                }
                            });
                        }
                    }
                    // Versions rarely change; only fetch a few times at startup.
                    if versions_scan_attempts < 3
                        && read_lock(&bg_state2.system.versions).is_none()
                        && now_ms.saturating_sub(last_versions_scan) >= VERSIONS_REFRESH_MS
                    {
                        last_versions_scan = now_ms;
                        versions_scan_attempts += 1;
                        let ec_clone = Arc::clone(&ec);
                        if let Ok(Ok(v)) =
                            crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || ec_clone.versions()).await
                        {
                            with_write_lock(&bg_state2.system.versions, |guard| {
                                if guard.as_ref().as_ref() != Some(&v) {
                                    *guard = Arc::new(Some(v));
                                    mark_view_dirty(&bg_state2);
                                }
                            });
                            // Stop after first successful fetch; versions only change after reboot.
                            if read_lock(&bg_state2.system.versions).is_some() {
                                versions_scan_attempts = 3;
                            }
                        }
                    }

                    // Poll CPU power every 5s; skip when window hidden to save power.
                    const CPU_POWER_POLL_MS: u64 = 5000;
                    let visible = bg_state2.lifecycle.visible.load(Ordering::Acquire);
                    if visible
                        && bg_state2.system.intel_cpu.load(Ordering::Acquire)
                        && !is_idle
                        && now_ms.saturating_sub(last_cpu_power_poll) >= CPU_POWER_POLL_MS
                    {
                        last_cpu_power_poll = now_ms;
                        // Run PawnIO ioctls off async worker like other EC I/O.
                        let cpu_power = bg_state2.cpu_power.clone();
                        if let Err(e) =
                            crate::util::spawn_blocking_with_timeout(crate::util::PAWNIO_IO_TIMEOUT, move || cpu_power.refresh()).await
                        {
                            warn!("CPU power refresh task failed: {}", e);
                        }
                        mark_view_dirty(&bg_state2);
                    }

                    // Re-check shutdown after scans to avoid overwriting quit fan control.
                    if bg_state2.lifecycle.shutdown.load(Ordering::Acquire) {
                        return;
                    }

                    // Read needed fields then drop lock to avoid holding across EC I/O.
                    let (
                        manual_duty,
                        curve_hysteresis,
                        curve_rate_limit,
                        curve_rate_limit_down,
                        curve_sensors,
                    ) = {
                        let config = read_lock(&bg_state2.lifecycle.config);
                        let mode_tmp = crate::types::FanControlMode::from_u8(
                            bg_state2.fan.mode.load(Ordering::Acquire) as u8,
                        );
                        let manual = if mode_tmp == crate::types::FanControlMode::Manual {
                            Some(config.fan.manual.as_ref().map(|m| m.duty_pct).unwrap_or(50))
                        } else {
                            config.fan.manual.as_ref().map(|m| m.duty_pct)
                        };
                        (
                            manual,
                            config.fan.curve.as_ref().map(|c| c.curve.hysteresis_c),
                            config
                                .fan
                                .curve
                                .as_ref()
                                .map(|c| c.curve.rate_limit_pct_per_step),
                            config
                                .fan
                                .curve
                                .as_ref()
                                .and_then(|c| c.curve.rate_limit_down_pct_per_step),
                            config
                                .fan
                                .curve
                                .as_ref()
                                .map(|c| c.curve.sensors.clone())
                                .unwrap_or_default(),
                        )
                    }; // config lock released here

                    let mode = crate::types::FanControlMode::from_u8(
                        bg_state2.fan.mode.load(Ordering::Acquire) as u8,
                    );
                    if last_fan_mode.as_ref() != Some(&mode) {
                        let last_duty =
                            bg_state2.fan.last_applied_duty.load(Ordering::Acquire) as u32;
                        if matches!(mode, crate::types::FanControlMode::Curve) && last_duty > 0 {
                            // Seed stepper with last written duty to ramp instead of jump.
                            curve_stepper = CurveStepper::with_last_duty(last_duty);
                        } else {
                            curve_stepper.reset();
                        }
                        last_manual_duty = None;
                        manual_per_fan_ramp = None;
                        if matches!(mode, crate::types::FanControlMode::Manual) {
                            // Prefer last written duty; fallback to RPM estimate.
                            let last_duty =
                                bg_state2.fan.last_applied_duty.load(Ordering::Acquire) as u32;
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
                            // Restore firmware control once when entering Disabled.
                            if last_fan_mode
                                .as_ref()
                                .is_none_or(|m| m != &crate::types::FanControlMode::Disabled)
                            {
                                if shutdown_requested(&bg_state2) {
                                    return;
                                }
                                let mut backoff_ec = false;
                                {
                                    let _ec_guard = crate::util::ec_write_mutex().lock().await;
                                    let ec_clone = Arc::clone(&ec);
                                    match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                                        ec_clone.autofanctrl()
                                    })
                                    .await
                                    {
                                        Ok(result) => {
                                            if let Err(e) = result {
                                                warn!("Failed to restore auto fan control: {}", e);
                                                consecutive_ec_write_failures += 1;
                                                if consecutive_ec_write_failures
                                                    >= MAX_EC_WRITE_FAILURES
                                                {
                                                    reset_ec_after_failures(
                                                        &bg_state2,
                                                        consecutive_ec_write_failures,
                                                    );
                                                    // Drop the EC write lock before backing off
                                                    // so other writers are not blocked for 3s.
                                                    backoff_ec = true;
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
                                }
                                if backoff_ec {
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    continue 'poll_loop;
                                }
                                let mode_now = crate::types::FanControlMode::from_u8(
                                    bg_state2.fan.mode.load(Ordering::Acquire) as u8,
                                );
                                if mode_now != mode {
                                    last_fan_mode = Some(mode);
                                    continue;
                                }
                            }
                        }
                        crate::types::FanControlMode::Manual => {
                            let unified = bg_state2.fan.unified_duty.load(Ordering::Acquire);
                            if unified {
                                if let Some(target) = manual_duty {
                                    let current = manual_ramp_current.unwrap_or(target);
                                    let next =
                                        crate::fan_control::apply_rate_limit(current, target, 10);
                                    let converged = last_manual_duty == Some(next);
                                    // Re-assert periodically even when converged to recover missed resume.
                                    let reassert = converged
                                        && now_ms.saturating_sub(last_duty_write_ms)
                                            >= FAN_REASSERT_INTERVAL_MS;
                                    if converged && !reassert {
                                        manual_ramp_current = Some(next);
                                    } else {
                                        if shutdown_requested(&bg_state2) {
                                            return;
                                        }
                                        let mut backoff_ec = false;
                                        {
                                            let _ec_guard =
                                                crate::util::ec_write_mutex().lock().await;
                                            let ec_clone = Arc::clone(&ec);
                                            match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                                                ec_clone.set_fan_duty(next, None)
                                            })
                                            .await
                                            {
                                                Ok(result) => match result {
                                                    Ok(()) => {
                                                        if shutdown_requested(&bg_state2) {
                                                            return;
                                                        }
                                                        let mode_now =
                                                            crate::types::FanControlMode::from_u8(
                                                                bg_state2
                                                                    .fan
                                                                    .mode
                                                                    .load(Ordering::Acquire)
                                                                    as u8,
                                                            );
                                                        if mode_now != mode {
                                                            last_fan_mode = Some(mode);
                                                            continue;
                                                        }
                                                        consecutive_ec_write_failures = 0;
                                                        last_duty_write_ms = now_ms;
                                                        bg_state2
                                                            .fan
                                                            .last_applied_duty
                                                            .store(next as u64, Ordering::Release);
                                                        last_manual_duty = Some(next);
                                                        manual_ramp_current = Some(next);
                                                    }
                                                    Err(e) => {
                                                        warn!(
                                                            "Failed to set manual fan duty: {}",
                                                            e
                                                        );
                                                        consecutive_ec_write_failures += 1;
                                                        if consecutive_ec_write_failures
                                                            >= MAX_EC_WRITE_FAILURES
                                                        {
                                                            reset_ec_after_failures(
                                                                &bg_state2,
                                                                consecutive_ec_write_failures,
                                                            );
                                                            // Drop the EC write lock before backing off.
                                                            backoff_ec = true;
                                                        }
                                                    }
                                                },
                                                Err(join_err) => {
                                                    warn!(
                                                        "EC spawn panicked (set_fan_duty): {}",
                                                        join_err
                                                    );
                                                    reset_ec_on_panic(&bg_state2);
                                                    continue;
                                                }
                                            }
                                        }
                                        if backoff_ec {
                                            tokio::time::sleep(std::time::Duration::from_secs(3))
                                                .await;
                                            continue 'poll_loop;
                                        }
                                    }
                                }
                            } else {
                                // Per-fan manual ramps independently; not gated by unified ramp state.
                                let per_fan = read_lock(&bg_state2.fan.per_fan_duty);
                                if !per_fan.is_empty() {
                                    let last_duty =
                                        bg_state2.fan.last_applied_duty.load(Ordering::Acquire)
                                            as u32;
                                    let seed = if last_duty > 0 {
                                        last_duty
                                    } else {
                                        estimate_duty_from_thermal(&bg_state2)
                                            .unwrap_or_else(|| per_fan[0])
                                    };
                                    let ramp = manual_per_fan_ramp
                                        .get_or_insert_with(|| vec![seed; per_fan.len()]);
                                    // Grow ramp when fan count increases; otherwise new fans never get written.
                                    if ramp.len() < per_fan.len() {
                                        ramp.resize(per_fan.len(), seed);
                                    }
                                    let mut wrote_any = false;
                                    let fan_count_now =
                                        bg_state2.fan.fan_count.load(Ordering::Acquire) as usize;
                                    // Re-assert once per pass, not per fan, to ensure all fans recover.
                                    let reassert = now_ms.saturating_sub(last_duty_write_ms)
                                        >= FAN_REASSERT_INTERVAL_MS;
                                    let mut backoff_ec = false;
                                    for (idx, &target) in per_fan.iter().enumerate() {
                                        // Skip indices beyond physical fan count (vector retains docked fans).
                                        if idx >= fan_count_now {
                                            continue;
                                        }
                                        let current = ramp.get(idx).copied().unwrap_or(target);
                                        let next_i = crate::fan_control::apply_rate_limit(
                                            current, target, 10,
                                        );
                                        // Write even when converged on resume or periodic re-assert.
                                        if next_i == current && !just_resumed && !reassert {
                                            continue;
                                        }
                                        if shutdown_requested(&bg_state2) {
                                            return;
                                        }
                                        // Re-check mode between fans to avoid writes after mode switch.
                                        let mode_check = crate::types::FanControlMode::from_u8(
                                            bg_state2.fan.mode.load(Ordering::Acquire) as u8,
                                        );
                                        if mode_check != mode {
                                            break;
                                        }
                                        let mut write_backoff = false;
                                        {
                                            let _ec_guard =
                                                crate::util::ec_write_mutex().lock().await;
                                            let ec_clone = Arc::clone(&ec);
                                            let fan_idx = idx as u32;
                                            match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                                                ec_clone.set_fan_duty(next_i, Some(fan_idx))
                                            })
                                            .await
                                            {
                                                Ok(Ok(())) => {
                                                    if shutdown_requested(&bg_state2) {
                                                        return;
                                                    }
                                                    ramp[idx] = next_i;
                                                    wrote_any = true;
                                                    consecutive_ec_write_failures = 0;
                                                    last_duty_write_ms = now_ms;
                                                }
                                                Ok(Err(e)) => {
                                                    warn!(
                                                        "Failed to set fan {} duty: {}",
                                                        fan_idx, e
                                                    );
                                                    consecutive_ec_write_failures += 1;
                                                    if consecutive_ec_write_failures
                                                        >= MAX_EC_WRITE_FAILURES
                                                    {
                                                        reset_ec_after_failures(
                                                            &bg_state2,
                                                            consecutive_ec_write_failures,
                                                        );
                                                        // Drop the EC write lock before backing off.
                                                        write_backoff = true;
                                                    }
                                                }
                                                Err(join_err) => {
                                                    warn!(
                                                        "EC spawn panicked (set_fan_duty fan {}): {}",
                                                        fan_idx, join_err
                                                    );
                                                    reset_ec_on_panic(&bg_state2);
                                                    break;
                                                }
                                            }
                                        }
                                        if write_backoff {
                                            backoff_ec = true;
                                            break;
                                        }
                                    }
                                    if backoff_ec {
                                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                        continue 'poll_loop;
                                    }
                                    // Update on any write, including 0% convergence.
                                    if wrote_any {
                                        let max_applied = ramp.iter().copied().max().unwrap_or(0);
                                        bg_state2
                                            .fan
                                            .last_applied_duty
                                            .store(max_applied as u64, Ordering::Release);
                                    }
                                    // Sync unified ramp seed with per-fan max for mode switch.
                                    manual_ramp_current = ramp.iter().copied().max();
                                }
                            }
                        }
                        crate::types::FanControlMode::Curve => {
                            // Staleness check before using cached thermal.
                            let last_ok = bg_state2.thermal.last_success_ms.load(Ordering::Acquire);
                            let is_stale =
                                last_ok == 0 || now_ms.saturating_sub(last_ok) > THERMAL_STALE_MS;
                            if is_stale {
                                consecutive_ec_failures += 1;
                                if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                    reset_ec_after_failures(&bg_state2, consecutive_ec_failures);
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    continue 'poll_loop;
                                }
                                // Immediate handover when no fresh sample.
                                if shutdown_requested(&bg_state2) {
                                    return;
                                }
                                let _ec_guard = crate::util::ec_write_mutex().lock().await;
                                let ec_clone = Arc::clone(&ec);
                                match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || ec_clone.autofanctrl())
                                    .await
                                {
                                    Ok(Ok(())) => {
                                        last_curve_failover_ms = now_ms;
                                        tracing::warn!(
                                            "Curve mode: stale thermal ({}ms), handed fan control to firmware",
                                            now_ms.saturating_sub(last_ok)
                                        );
                                    }
                                    Ok(Err(e)) => {
                                        warn!("Curve stale fail-safe autofanctrl failed: {}", e);
                                        consecutive_ec_failures += 1;
                                        if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                            reset_ec_after_failures(
                                                &bg_state2,
                                                consecutive_ec_failures,
                                            );
                                            tokio::time::sleep(std::time::Duration::from_secs(3))
                                                .await;
                                            continue 'poll_loop;
                                        }
                                    }
                                    Err(join_err) => {
                                        warn!(
                                            "EC spawn panicked (curve stale fail-safe): {}",
                                            join_err
                                        );
                                        reset_ec_on_panic(&bg_state2);
                                    }
                                }
                                continue 'poll_loop;
                            }
                            let thermal_clone = read_lock(&bg_state2.thermal.data);
                            let Some(ref thermal) = *thermal_clone else {
                                // No sample yet but not stale-timed out above? immediate handover.
                                if shutdown_requested(&bg_state2) {
                                    return;
                                }
                                let _ec_guard = crate::util::ec_write_mutex().lock().await;
                                let ec_clone = Arc::clone(&ec);
                                let _ = crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || ec_clone.autofanctrl())
                                    .await;
                                continue 'poll_loop;
                            };
                            if curve_hysteresis.is_none() || curve_rate_limit.is_none() {
                                continue 'poll_loop;
                            }
                            let hyst = curve_hysteresis.unwrap();
                            let rate = curve_rate_limit.unwrap();
                            // Fail-safe: empty temps means EC read failed; hand back to firmware.
                            if thermal.temps.is_empty() {
                                // Count as read failure toward EC reset.
                                consecutive_ec_failures += 1;
                                if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                    reset_ec_after_failures(&bg_state2, consecutive_ec_failures);
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    continue 'poll_loop;
                                }
                                if now_ms.saturating_sub(last_curve_failover_ms)
                                    >= CURVE_TEMP_FAILOVER_MS
                                {
                                    if shutdown_requested(&bg_state2) {
                                        return;
                                    }
                                    let _ec_guard = crate::util::ec_write_mutex().lock().await;
                                    let ec_clone = Arc::clone(&ec);
                                    match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                                        ec_clone.autofanctrl()
                                    })
                                    .await
                                    {
                                        Ok(Ok(())) => {
                                            last_curve_failover_ms = now_ms;
                                            consecutive_ec_failures = 0;
                                            tracing::warn!(
                                                "Curve mode: EC temps read failed, handed fan control to firmware"
                                            );
                                        }
                                        Ok(Err(e)) => {
                                            warn!("Curve fail-safe autofanctrl failed: {}", e);
                                            // Count failure toward EC reset.
                                            consecutive_ec_failures += 1;
                                            if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                                reset_ec_after_failures(
                                                    &bg_state2,
                                                    consecutive_ec_failures,
                                                );
                                                tokio::time::sleep(std::time::Duration::from_secs(
                                                    3,
                                                ))
                                                .await;
                                                continue 'poll_loop;
                                            }
                                        }
                                        Err(join_err) => {
                                            warn!(
                                                "EC spawn panicked (curve fail-safe autofanctrl): {}",
                                                join_err
                                            );
                                            reset_ec_on_panic(&bg_state2);
                                        }
                                    }
                                }
                                continue 'poll_loop;
                            }
                            let control_temp =
                                crate::types::curve_control_temp(&thermal.temps, &curve_sensors);
                            let full_pts_arc = read_lock(&bg_state2.fan.curve_full_points);
                            let full_pts: &[[u32; 2]] = &full_pts_arc;
                            let mut next = curve_stepper.next(
                                control_temp,
                                hyst,
                                rate,
                                curve_rate_limit_down,
                                full_pts,
                            );
                            // Even when the stepper has converged, periodically
                            // re-assert the last duty. This recovers from a
                            // missed resume event that would otherwise leave
                            // fans in firmware control.
                            if next.is_none()
                                && now_ms.saturating_sub(last_duty_write_ms)
                                    >= FAN_REASSERT_INTERVAL_MS
                            {
                                next = curve_stepper.current_duty();
                            }
                            if let Some(next) = next {
                                let fan_count =
                                    bg_state2.fan.fan_count.load(Ordering::Acquire) as u32;
                                let write_each =
                                    !bg_state2.fan.unified_duty.load(Ordering::Acquire)
                                        && fan_count > 1;
                                // Only advance the stepper when every target fan
                                // was updated. Partial success must not distort
                                // the ramp state.
                                let mut applied = false;
                                if write_each {
                                    let mut all_fans_applied = true;
                                    let mut any_succeeded = false;
                                    let mut failed_fans: Vec<u32> = Vec::new();
                                    for fan_idx in 0..fan_count {
                                        if shutdown_requested(&bg_state2) {
                                            return;
                                        }
                                        let mode_check = crate::types::FanControlMode::from_u8(
                                            bg_state2.fan.mode.load(Ordering::Acquire) as u8,
                                        );
                                        if mode_check != mode {
                                            all_fans_applied = false;
                                            break;
                                        }
                                        let _ec_guard = crate::util::ec_write_mutex().lock().await;
                                        let ec_clone = Arc::clone(&ec);
                                        match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                                            ec_clone.set_fan_duty(next, Some(fan_idx))
                                        })
                                        .await
                                        {
                                            Ok(result) => {
                                                if let Err(e) = result {
                                                    // Transient per-fan failure (e.g. dock unplugged); only all-fail counts toward reset.
                                                    all_fans_applied = false;
                                                    failed_fans.push(fan_idx);
                                                    tracing::debug!(
                                                        "Failed to set fan {} duty (curve): {}",
                                                        fan_idx,
                                                        e
                                                    );
                                                } else {
                                                    any_succeeded = true;
                                                    last_duty_write_ms = now_ms;
                                                }
                                            }
                                            Err(join_err) => {
                                                warn!(
                                                    "EC spawn panicked (set_fan_duty curve fan {}): {}",
                                                    fan_idx, join_err
                                                );
                                                reset_ec_on_panic(&bg_state2);
                                                all_fans_applied = false;
                                                break;
                                            }
                                        }
                                    }
                                    if any_succeeded {
                                        consecutive_ec_write_failures = 0;
                                    } else {
                                        consecutive_ec_write_failures += 1;
                                        if consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                                            reset_ec_after_failures(
                                                &bg_state2,
                                                consecutive_ec_write_failures,
                                            );
                                            tokio::time::sleep(std::time::Duration::from_secs(3))
                                                .await;
                                            continue 'poll_loop;
                                        }
                                    }
                                    if !failed_fans.is_empty() {
                                        warn!(
                                            "Failed to set fan duties (curve) for fans {:?} ({} of {} wrote)",
                                            failed_fans,
                                            fan_count.saturating_sub(failed_fans.len() as u32),
                                            fan_count
                                        );
                                    }
                                    applied = all_fans_applied;
                                    if applied {
                                        bg_state2
                                            .fan
                                            .last_applied_duty
                                            .store(next as u64, Ordering::Release);
                                    }
                                } else {
                                    if shutdown_requested(&bg_state2) {
                                        return;
                                    }
                                    let _ec_guard = crate::util::ec_write_mutex().lock().await;
                                    let ec_clone = Arc::clone(&ec);
                                    match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                                        ec_clone.set_fan_duty(next, None)
                                    })
                                    .await
                                    {
                                        Ok(result) => match result {
                                            Ok(()) => {
                                                if shutdown_requested(&bg_state2) {
                                                    return;
                                                }
                                                let mode_now =
                                                    crate::types::FanControlMode::from_u8(
                                                        bg_state2.fan.mode.load(Ordering::Acquire)
                                                            as u8,
                                                    );
                                                if mode_now != mode {
                                                    last_fan_mode = Some(mode);
                                                    continue;
                                                }
                                                consecutive_ec_write_failures = 0;
                                                last_duty_write_ms = now_ms;
                                                bg_state2
                                                    .fan
                                                    .last_applied_duty
                                                    .store(next as u64, Ordering::Release);
                                                applied = true;
                                            }
                                            Err(e) => {
                                                warn!("Failed to set fan duty (curve): {}", e);
                                                consecutive_ec_write_failures += 1;
                                                if consecutive_ec_write_failures
                                                    >= MAX_EC_WRITE_FAILURES
                                                {
                                                    reset_ec_after_failures(
                                                        &bg_state2,
                                                        consecutive_ec_write_failures,
                                                    );
                                                    tokio::time::sleep(
                                                        std::time::Duration::from_secs(3),
                                                    )
                                                    .await;
                                                    continue 'poll_loop;
                                                }
                                            }
                                        },
                                        Err(join_err) => {
                                            warn!(
                                                "EC spawn panicked (set_fan_duty curve): {}",
                                                join_err
                                            );
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
                    last_fan_mode = Some(mode);
                    just_resumed = false;
                }
            });
            if let Err(e) = handle.await {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    warn!(
                        "Background task failed {} times consecutively, giving up",
                        consecutive_failures
                    );
                    break;
                }
                warn!(
                    "Background polling task crashed: {}, restarting in 3s... ({}/{})",
                    e, consecutive_failures, MAX_CONSECUTIVE_FAILURES
                );
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if state.lifecycle.shutdown.load(Ordering::Acquire) {
                    break;
                }
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
            fans: smallvec::smallvec![FanReading {
                name: "Fan 1".to_string(),
                rpm
            }],
        };

        assert!(record_thermal_sample(&state, sample(2000)));
        assert_eq!(
            read_lock(&state.thermal.data)
                .as_ref()
                .as_ref()
                .unwrap()
                .fans[0]
                .rpm,
            2000
        );

        // Same temps, different RPM — must still be stored and flagged dirty.
        assert!(record_thermal_sample(&state, sample(3200)));
        assert_eq!(
            read_lock(&state.thermal.data)
                .as_ref()
                .as_ref()
                .unwrap()
                .fans[0]
                .rpm,
            3200
        );

        // Identical data — no write, not dirty.
        assert!(!record_thermal_sample(&state, sample(3200)));
    }

    #[test]
    fn ensure_per_fan_duty_never_truncates_saved_values() {
        use crate::app::AppState;
        let state = AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        with_write_lock(&state.fan.per_fan_duty, |guard| {
            *guard = Arc::new(vec![50, 50, 30, 30]);
        });

        // Undock: fan count drops to 2 — values for fans 3-4 must survive.
        ensure_per_fan_duty(&state, 2);
        assert_eq!(*read_lock(&state.fan.per_fan_duty), vec![50, 50, 30, 30]);

        // Re-dock with 4 fans: length unchanged, no fill overwrites.
        ensure_per_fan_duty(&state, 4);
        assert_eq!(*read_lock(&state.fan.per_fan_duty), vec![50, 50, 30, 30]);

        // Genuine growth still pads with the manual duty.
        ensure_per_fan_duty(&state, 6);
        assert_eq!(
            *read_lock(&state.fan.per_fan_duty),
            vec![50, 50, 30, 30, 50, 50]
        );
    }
}

