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

mod fan_curve;
mod fan_disabled;
mod fan_manual;

/// PD history depth to distinguish USB-A cards from transient USB device states.
const MAX_PD_HISTORY: usize = 3;

/// Consecutive EC read failures before reinitializing client (recovers after sleep/resume).
pub(super) const MAX_EC_IO_FAILURES: u32 = 5;

/// Consecutive EC write failures before reinitializing client (separate from reads).
pub(super) const MAX_EC_WRITE_FAILURES: u32 = 5;

/// Re-assert interval for converged fan duty; recovers fans if resume event was missed.
pub(super) const FAN_REASSERT_INTERVAL_MS: u64 = 30_000;

/// Rate-limit for curve fail-safe handover to firmware when temps are empty.
pub(super) const CURVE_TEMP_FAILOVER_MS: u64 = 1_000;

/// Maximum age of thermal sample before curve considers it stale.
pub(super) const THERMAL_STALE_MS: u64 = 5_000;

/// Battery data considered stale after this duration without successful read.
const BATTERY_STALE_MS: u64 = 15_000;

/// Outcome of one fan-mode pass. The mode bodies live in submodules and
/// cannot `continue 'poll_loop` or `return` across the function boundary,
/// so they report how the parent loop should proceed instead.
pub(super) enum Flow {
    /// Arm finished; run the epilogue (`last_fan_mode`, `just_resumed`).
    Done,
    /// Skip the epilogue and start the next poll iteration.
    Next,
    /// Shutdown was requested; exit the worker.
    Stop,
}

/// Per-iteration fan state shared with the mode submodules.
///
/// Plain scalars are owned copies, written back by the parent after the
/// call; the stepper and per-fan ramp stay borrowed so no state is lost.
pub(super) struct FanCtx<'a> {
    pub state: &'a AppState,
    pub ec: &'a Arc<cli::EcClient>,
    pub mode: crate::types::FanControlMode,
    pub now_ms: u64,
    pub manual_duty: Option<u32>,
    pub curve_hysteresis: Option<u32>,
    pub curve_rate_limit: Option<u32>,
    pub curve_rate_limit_down: Option<u32>,
    pub curve_sensors: Vec<String>,
    pub consecutive_ec_failures: u32,
    pub consecutive_ec_write_failures: u32,
    pub last_duty_write_ms: u64,
    pub last_curve_failover_ms: u64,
    pub last_manual_duty: Option<u32>,
    pub manual_ramp_current: Option<u32>,
    pub manual_per_fan_ramp: &'a mut Option<Vec<u32>>,
    pub curve_stepper: &'a mut CurveStepper,
    pub last_fan_mode: Option<crate::types::FanControlMode>,
    pub just_resumed: bool,
}

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
    // Pinning is disabled by default; OS scheduler is better than heuristic. Opt-in via FRAMEWORK_PIN_CORE=1
    if std::env::var_os("FRAMEWORK_PIN_CORE").is_none() {
        return;
    }
    // core_affinity order is OS enumeration, not perf order
    if let Some(cores) = core_affinity::get_core_ids()
        && !cores.is_empty()
    {
        // Prefer first core as E-core heuristic; previously used `last()` which is arbitrary.
        let target = cores.first().copied().unwrap_or(cores[0]);
        if core_affinity::set_for_current(target) {
            tracing::debug!(
                "[AFFINITY] Pinned to core (id={}) heuristic (cores={})",
                target.id,
                cores.len()
            );
            #[cfg(debug_assertions)]
            verify_affinity(target.id);
        } else {
            tracing::debug!("[AFFINITY] Failed to pin to core {}", target.id);
        }
    }
}

#[cfg(debug_assertions)]
fn verify_affinity(expected_id: usize) {
    {
        unsafe extern "system" {
            fn GetCurrentProcessorNumber() -> u32;
        }
        let cur = unsafe { GetCurrentProcessorNumber() } as usize;
        let ok = cur == expected_id;
        tracing::debug!(
            "[AFFINITY] Verify: cur={}, expected={} {}",
            cur,
            expected_id,
            if ok {
                "OK"
            } else {
                "mismatch (OS scheduled elsewhere)"
            }
        );
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
    /// Corrupt EC data must not drive a huge allocation.
    const MAX_PD_PORTS: usize = 8;
    if !ports
        .iter()
        .any(|p| p.power_role == Some("Sink") && (p.port as usize) < MAX_PD_PORTS)
    {
        return;
    }
    with_write_lock(seen_ref, |guard| {
        let mut seen = (**guard).clone();
        // Size vec for highest port index; EC skips failed ports so indices are sparse.
        let need = ports
            .iter()
            .filter(|p| (p.port as usize) < MAX_PD_PORTS)
            .map(|p| p.port as usize + 1)
            .max()
            .unwrap_or(0);
        if seen.len() < need {
            seen.resize(need, false);
        }
        for p in ports {
            let idx = p.port as usize;
            if p.power_role == Some("Sink") && idx < seen.len() {
                seen[idx] = true;
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
    // Fast path: avoid write lock when count unchanged and duties sized.
    let last = state.fan.last_fan_count.load(Ordering::Acquire) as usize;
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
    state
        .fan
        .last_fan_count
        .store(fan_count as u64, Ordering::Release);
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
    // Batch of 5 sequential EC reads needs more than the single-op 1.5s budget.
    let batch =
        crate::util::spawn_blocking_with_timeout(std::time::Duration::from_secs(5), move || {
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
                // Last successful PD/expansion scan times for staleness.
                let mut last_pd_success_ms: u64 = 0;
                let mut last_expansion_success_ms: u64 = 0;
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
                            match crate::util::spawn_blocking_with_timeout(
                                crate::util::EC_IO_TIMEOUT,
                                cli::EcClient::new,
                            )
                            .await
                            {
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
                    match crate::util::spawn_blocking_with_timeout(
                        crate::util::EC_IO_TIMEOUT,
                        move || ec_clone.thermal(),
                    )
                    .await
                    {
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
                        match crate::util::spawn_blocking_with_timeout(
                            crate::util::EC_IO_TIMEOUT,
                            move || ec_clone.power(),
                        )
                        .await
                        {
                            Ok(Ok(bat)) => {
                                consecutive_ec_failures = 0;
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
                                consecutive_ec_failures += 1;
                                if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                    reset_ec_after_failures(&bg_state2, consecutive_ec_failures);
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    continue;
                                }
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
                                consecutive_ec_failures += 1;
                                if consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                                    reset_ec_after_failures(&bg_state2, consecutive_ec_failures);
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    continue;
                                }
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
                        match crate::util::spawn_blocking_with_timeout(
                            crate::util::EC_IO_TIMEOUT,
                            move || ec_clone.pd_ports(),
                        )
                        .await
                        {
                            Ok(ports) => {
                                last_pd_success_ms = crate::util::monotonic_ms();
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
                            Err(e) => {
                                warn!("PD ports read failed: {}", e);
                                if last_pd_success_ms != 0
                                    && crate::util::monotonic_ms()
                                        .saturating_sub(last_pd_success_ms)
                                        > 30_000
                                {
                                    with_write_lock(&bg_state2.peripherals.pd_ports, |guard| {
                                        if !guard.is_empty() {
                                            *guard = Arc::new(smallvec::SmallVec::new());
                                            mark_view_dirty(&bg_state2);
                                        }
                                    });
                                }
                            }
                        }
                        let ec_clone = Arc::clone(&ec);
                        match crate::util::spawn_blocking_with_timeout(
                            crate::util::EC_IO_TIMEOUT,
                            move || ec_clone.expansion_cards(),
                        )
                        .await
                        {
                            Ok(cards) => {
                                last_expansion_success_ms = crate::util::monotonic_ms();
                                with_write_lock(&bg_state2.peripherals.expansion_cards, |guard| {
                                    if **guard != cards {
                                        *guard = Arc::new(cards);
                                        mark_view_dirty(&bg_state2);
                                    }
                                });
                            }
                            Err(e) => {
                                warn!("Expansion cards read failed: {}", e);
                                if last_expansion_success_ms != 0
                                    && crate::util::monotonic_ms()
                                        .saturating_sub(last_expansion_success_ms)
                                        > 30_000
                                {
                                    with_write_lock(
                                        &bg_state2.peripherals.expansion_cards,
                                        |guard| {
                                            if !guard.is_empty() {
                                                *guard = Arc::new(smallvec::SmallVec::new());
                                                mark_view_dirty(&bg_state2);
                                            }
                                        },
                                    );
                                }
                            }
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
                        if let Ok(Ok(v)) = crate::util::spawn_blocking_with_timeout(
                            crate::util::EC_IO_TIMEOUT,
                            move || ec_clone.versions(),
                        )
                        .await
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
                        if let Err(e) = crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_IO_TIMEOUT,
                            move || cpu_power.refresh(),
                        )
                        .await
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
                    let mut ctx = FanCtx {
                        state: &bg_state2,
                        ec: &ec,
                        mode,
                        now_ms,
                        manual_duty,
                        curve_hysteresis,
                        curve_rate_limit,
                        curve_rate_limit_down,
                        curve_sensors,
                        consecutive_ec_failures,
                        consecutive_ec_write_failures,
                        last_duty_write_ms,
                        last_curve_failover_ms,
                        last_manual_duty,
                        manual_ramp_current,
                        manual_per_fan_ramp: &mut manual_per_fan_ramp,
                        curve_stepper: &mut curve_stepper,
                        last_fan_mode,
                        just_resumed,
                    };
                    let flow = match &mode {
                        crate::types::FanControlMode::Disabled => fan_disabled::run(&mut ctx).await,
                        crate::types::FanControlMode::Manual => fan_manual::run(&mut ctx).await,
                        crate::types::FanControlMode::Curve => fan_curve::run(&mut ctx).await,
                    };
                    // Write back the owned copies the arms may have mutated.
                    consecutive_ec_failures = ctx.consecutive_ec_failures;
                    consecutive_ec_write_failures = ctx.consecutive_ec_write_failures;
                    last_duty_write_ms = ctx.last_duty_write_ms;
                    last_curve_failover_ms = ctx.last_curve_failover_ms;
                    last_manual_duty = ctx.last_manual_duty;
                    manual_ramp_current = ctx.manual_ramp_current;
                    last_fan_mode = ctx.last_fan_mode;
                    match flow {
                        Flow::Done => {
                            last_fan_mode = Some(mode);
                            just_resumed = false;
                        }
                        Flow::Next => continue 'poll_loop,
                        Flow::Stop => return,
                    }
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
                // Exponential backoff: 3s, 6s, 12s, 24s, ... capped at 60s
                let backoff_secs = (3u64 * (1u64 << (consecutive_failures - 1).min(4))).min(60);
                warn!(
                    "Background polling task crashed: {}, restarting in {}s... ({}/{})",
                    e, backoff_secs, consecutive_failures, MAX_CONSECUTIVE_FAILURES
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
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
