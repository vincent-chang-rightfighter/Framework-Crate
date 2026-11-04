//! Poll-round state and phase functions (P2 follow-up).
//!
//! Owns `PollState`, `Phase`, the eight `phase_*` round steps, and the
//! thermal hello-tiebreak verdict. Shared helpers (view dirty, idle, PD
//! history, thermal sampling, EC reset) stay in the parent so the fan-mode
//! arms and `spawn`/`refresh_all_data` keep one home.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use crate::app::AppState;
use crate::cli;
use crate::fan_control::CurveStepper;
use crate::style::{EXPANSION_SCAN_MS, VERSIONS_REFRESH_MS};
use crate::util::{read_lock, with_write_lock};

use super::{
    BATTERY_STALE_MS, EC_INIT_SEM, FanCtx, Flow, MAX_EC_IO_FAILURES, ac_transition_pending,
    estimate_duty_from_thermal, fan_curve, fan_disabled, fan_manual, is_idle_now,
    mark_pd_usb_c_seen, mark_view_dirty, push_pd_ports_history, record_thermal_sample,
    reset_ec_after_failures, try_ec_init_permit,
};

/// Per-poll mutable state for the background loop (P2). Collects the ~23
/// cross-iteration locals that used to sprawl across the poll body so phases
/// can take `&mut PollState` instead of a dozen parameters. Owned by the
/// inner task; each phase borrows it for one call, never across awaits that
/// outlive the iteration.
pub(super) struct PollState {
    last_fan_mode: Option<crate::types::FanControlMode>,
    last_manual_duty: Option<u32>,
    last_unified_duty: Option<bool>,
    manual_ramp_current: Option<u32>,
    manual_per_fan_ramp: Option<Vec<u32>>,
    curve_stepper: CurveStepper,
    just_resumed: bool,
    consecutive_ec_failures: u32,
    consecutive_battery_failures: u32,
    consecutive_ec_write_failures: u32,
    last_duty_write_ms: u64,
    last_pd_success_ms: u64,
    last_handover_ms: u64,
    last_expansion_scan: u64,
    last_versions_scan: u64,
    versions_scan_attempts: u32,
    last_pd_fw_scan: u64,
    pd_fw_scan_attempts: u32,
    last_smart_scan: u64,
    smart_scan_attempts: u32,
    last_cpu_power_poll: u64,
    last_resume_ts: u64,
    last_wall_ms: u64,
}

impl PollState {
    pub(super) fn new(
        state: &AppState,
        saved_duty: u32,
        init_fan_mode: crate::types::FanControlMode,
        init_is_manual: bool,
        start_ms: u64,
    ) -> Self {
        Self {
            // Seed Disabled as None to force firmware restore on first iteration.
            last_fan_mode: (init_fan_mode != crate::types::FanControlMode::Disabled)
                .then_some(init_fan_mode),
            last_manual_duty: None,
            last_unified_duty: None,
            manual_ramp_current: if saved_duty > 0 {
                Some(saved_duty)
            } else if init_is_manual {
                estimate_duty_from_thermal(state)
            } else {
                None
            },
            manual_per_fan_ramp: None,
            curve_stepper: if saved_duty > 0 {
                CurveStepper::with_last_duty(saved_duty)
            } else {
                CurveStepper::new()
            },
            just_resumed: false,
            consecutive_ec_failures: 0,
            consecutive_battery_failures: 0,
            consecutive_ec_write_failures: 0,
            last_duty_write_ms: 0,
            last_pd_success_ms: 0,
            last_handover_ms: 0,
            last_expansion_scan: start_ms,
            // 0 is the sentinel for "never attempted"; the first poll
            // should try immediately when versions are still missing.
            last_versions_scan: 0,
            versions_scan_attempts: 0,
            last_pd_fw_scan: 0,
            pd_fw_scan_attempts: 0,
            last_smart_scan: 0,
            smart_scan_attempts: 0,
            last_cpu_power_poll: 0,
            last_resume_ts: 0,
            last_wall_ms: crate::util::current_time_ms(),
        }
    }

    /// Clears fan-controller state after resume or EC reset. The caller owns
    /// the parts that differ (EC teardown, rpm bookkeeping, logging,
    /// `just_resumed`).
    pub(super) fn reset_fan_state(&mut self) {
        self.curve_stepper.reset();
        self.last_manual_duty = None;
        self.last_unified_duty = None;
        self.manual_ramp_current = None;
        self.manual_per_fan_ramp = None;
        self.last_fan_mode = None;
        self.last_duty_write_ms = 0;
        self.last_handover_ms = 0;
    }
}

/// How the poll loop proceeds after a phase.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// Run the next phase.
    Continue,
    /// Skip the rest of the round.
    NextRound,
    /// Shutdown was requested.
    Stop,
}

/// Ensures a live EC client, creating one under the init semaphore when
/// missing. Returns `None` when the round should be skipped (init not done,
/// init in flight, or creation failed).
pub(super) async fn phase_ensure_ec(state: &AppState) -> Option<Arc<dyn cli::EcHw>> {
    let ec_opt = { read_lock(&state.system.ec_client) };
    match ec_opt.as_ref().as_ref() {
        Some(c) => Some(Arc::clone(c)),
        None => {
            // Wait for init task before creating own client.
            if !state.system.ec_init_done.load(Ordering::Acquire) {
                return None;
            }
            let sem = EC_INIT_SEM.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)));
            let Some(permit) = try_ec_init_permit(sem) else {
                tracing::debug!("EC init already in flight; waiting");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                return None;
            };
            let state_cl = state.clone();
            match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                let _permit = permit;
                cli::EcClient::new()
            })
            .await
            {
                Ok(Ok(c)) => {
                    let arc_ec: Arc<dyn cli::EcHw> = Arc::new(c);
                    with_write_lock(&state_cl.system.ec_client, |guard| {
                        *guard = Arc::new(Some(Arc::clone(&arc_ec)));
                    });
                    state_cl.system.cli_available.store(true, Ordering::Release);
                    Some(arc_ec)
                }
                Ok(Err(e)) => {
                    warn!("Background loop: failed to init EC: {}", e);
                    state_cl
                        .system
                        .cli_available
                        .store(false, Ordering::Release);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    None
                }
                Err(e) => {
                    warn!("Background loop: EC spawn_blocking panicked: {}", e);
                    state_cl
                        .system
                        .cli_available
                        .store(false, Ordering::Release);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    None
                }
            }
        }
    }
}

/// Thermal read with the hello tiebreak unified across all failure kinds:
/// empty readings, command errors, and dispatch failures all count alike,
/// and the threshold pings the EC once instead of resetting blindly.
pub(super) async fn phase_thermal(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    poll: &mut PollState,
) -> Phase {
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec("thermal", crate::util::EC_IO_TIMEOUT, ec_clone, move |ec| {
        ec.thermal()
    })
    .await
    {
        Ok(Ok(t)) => {
            if t.temps.is_empty() {
                warn!("Thermal read returned empty temps");
                poll.consecutive_ec_failures += 1;
            } else {
                poll.consecutive_ec_failures = 0;
                if record_thermal_sample(state, t) {
                    mark_view_dirty(state);
                }
            }
        }
        Ok(Err(e)) => {
            warn!("Thermal read failed: {}", e);
            poll.consecutive_ec_failures += 1;
        }
        Err(e) if e.is_overloaded() => {
            // Backpressure, not failure: fall through to fan
            // modes below instead of skipping the whole round.
            return Phase::Continue;
        }
        Err(e) => {
            warn!("EC thermal spawn failed: {}", e);
            poll.consecutive_ec_failures += 1;
        }
    }
    if poll.consecutive_ec_failures >= MAX_EC_IO_FAILURES {
        match thermal_failure_verdict(state, ec, poll.consecutive_ec_failures).await {
            ThermalVerdict::Reset => {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                return Phase::NextRound;
            }
            // EC proven alive: count fresh so the next
            // ping is another full run away instead of
            // every round. Falls through so battery/PD
            // stay fresh and the fan arms hand over on
            // staleness by themselves.
            ThermalVerdict::RideThrough => {
                poll.consecutive_ec_failures = 0;
            }
        }
    }
    Phase::Continue
}

/// Wall-clock jump fallback, resume handling, and fan-reset handling.
/// Never skips the round by itself; resume work falls through to polling.
pub(super) fn phase_resume(state: &AppState, poll: &mut PollState) -> Phase {
    let resume_ts = state.lifecycle.last_resume_ts.load(Ordering::Acquire);
    // Skip resume handling if shutdown requested.
    if state.lifecycle.shutdown.load(Ordering::Acquire) {
        return Phase::Stop;
    }
    {
        let wall_now = crate::util::current_time_ms();
        // Sleep/resume: wall jumps forward while this loop
        // was parked in sleep; monotonic sleep time is not
        // visible here, so a large wall delta alone signals
        // a missed resume. Threshold 60s avoids DST noise.
        // NOTE: last_resume_ts and last_fan_rpm_reset are
        // monotonic-domain; store monotonic_ms() here, never
        // the wall epoch, or the 60s re-baseline breaks.
        if wall_now.saturating_sub(poll.last_wall_ms) > 60_000 {
            tracing::warn!(
                "[RESUME] wall-clock jump detected ({}ms), forcing EC re-init as resume fallback",
                wall_now.saturating_sub(poll.last_wall_ms)
            );
            // Synthesize a resume_ts so the block below runs.
            state
                .lifecycle
                .last_resume_ts
                .store(crate::util::monotonic_ms(), Ordering::Release);
        }
        poll.last_wall_ms = wall_now;
    }
    if resume_ts != 0 && resume_ts != poll.last_resume_ts {
        poll.last_resume_ts = resume_ts;
        poll.reset_fan_state();
        poll.just_resumed = true;
        // EC reverts to firmware control during sleep. The client teardown
        // below plus the cleared mode force the next iteration to re-assert
        // the selected mode and re-seed ramps.
        state.system.cli_available.store(false, Ordering::Release);
        with_write_lock(&state.system.ec_client, |guard| {
            *guard = Arc::new(None);
        });
        state
            .fan
            .last_fan_rpm_reset
            .store(resume_ts, Ordering::Release);
        // Keep fan_max_rpm as the real max-RPM baseline for accurate
        // ramp seeding after resume; only defer the rolling re-baseline
        // by 60s via last_fan_rpm_reset.
        state.fan.last_applied_duty.store(0, Ordering::Release);
        // Reset duty write time so the 30s re-assert guard trips
        // immediately on the resume iteration; otherwise Curve duty
        // could stay un-asserted for up to 30s after a short sleep.
        // Retain thermal history so the chart does not flash blank;
        // stale samples are pruned by the normal window retention.
        tracing::warn!(
            "[RESUME] EC client and fan state reset after system resume (history retained)"
        );
    }
    if state
        .lifecycle
        .fan_reset_pending
        .swap(false, Ordering::Acquire)
    {
        poll.reset_fan_state();
        state.fan.last_applied_duty.store(0, Ordering::Release);
        tracing::warn!("[EC RESET] fan controller state invalidated, will re-assert mode");
    }
    Phase::Continue
}

/// Battery read with AC-transition detection and staleness expiry.
/// UI-only: never resets the EC client and never skips the round.
pub(super) async fn phase_battery(state: &AppState, ec: &Arc<dyn cli::EcHw>, poll: &mut PollState) {
    // Battery polling also drives the AC→battery power-limit
    // reset, so it must keep running while idle.
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec("power", crate::util::EC_IO_TIMEOUT, ec_clone, move |ec| {
        ec.power()
    })
    .await
    {
        Ok(Ok(bat)) => {
            poll.consecutive_battery_failures = 0;
            let ac_now = bat.ac_present == Some(true);
            let had_prior_sample = state.battery.last_success_ms.load(Ordering::Acquire) != 0;
            let ac_was = state.battery.prev_ac_present.load(Ordering::Acquire);
            if ac_transition_pending(ac_was, ac_now, had_prior_sample) {
                state
                    .lifecycle
                    .pl_reset_pending
                    .store(true, Ordering::Release);
                tracing::info!("AC→battery transition detected, PL1/PL2 reset pending");
            }
            state
                .battery
                .prev_ac_present
                .store(ac_now, Ordering::Release);
            crate::cpu_power::publish_ac_snapshot(ac_now);
            state
                .battery
                .last_success_ms
                .store(crate::util::monotonic_ms(), Ordering::Release);
            with_write_lock(&state.battery.info, |guard| {
                let new_info = crate::types::BatteryInfo { power_info: bat };
                if guard.as_ref().as_ref() != Some(&new_info) {
                    *guard = Arc::new(Some(new_info));
                }
            });
            mark_view_dirty(state);
        }
        Ok(Err(e)) => {
            tracing::debug!("Battery read failed: {}", e);
            poll.consecutive_battery_failures += 1;
            // Battery is UI-only: never reset the EC
            // client for it (that would needlessly drop
            // healthy fan/thermal state), and never
            // skip the poll loop (that would skip
            // PD/expansion/fan). Fall through to the
            // staleness check and the rest of the round.
            if poll.consecutive_battery_failures >= MAX_EC_IO_FAILURES {
                // No sleep here: sleeping would delay the
                // fan fail-safe below. Staleness expiry
                // below already clears a dead battery UI.
            }
            let last_ok = state.battery.last_success_ms.load(Ordering::Acquire);
            let cur = crate::util::monotonic_ms();
            if last_ok != 0 && cur.saturating_sub(last_ok) > BATTERY_STALE_MS {
                with_write_lock(&state.battery.info, |guard| {
                    if guard.is_some() {
                        *guard = Arc::new(None);
                        mark_view_dirty(state);
                    }
                });
            }
        }
        Err(e) => {
            // Overload skips are backpressure, not
            // failure: fall through to the rest of the
            // round instead of skipping PD/fan.
            if e.is_overloaded() {
            } else {
                warn!("Battery spawn failed: {}", e);
                poll.consecutive_battery_failures += 1;
                // UI-only: no sleep/continue and no EC
                // reset; staleness expiry below handles
                // a dead battery UI.
            }
            let last_ok = state.battery.last_success_ms.load(Ordering::Acquire);
            let cur = crate::util::monotonic_ms();
            if last_ok != 0 && cur.saturating_sub(last_ok) > BATTERY_STALE_MS {
                with_write_lock(&state.battery.info, |guard| {
                    if guard.is_some() {
                        *guard = Arc::new(None);
                        mark_view_dirty(state);
                    }
                });
            }
        }
    }
}

/// Expansion/PD scans plus privacy switches, sharing one throttle gate.
/// `now_ms` arrives freshly sampled by the caller (see the loop).
pub(super) async fn phase_scans(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    poll: &mut PollState,
    now_ms: u64,
) {
    let expansion_interval = if state.lifecycle.visible.load(Ordering::Acquire) {
        EXPANSION_SCAN_MS
    } else {
        30_000
    };
    if now_ms.saturating_sub(poll.last_expansion_scan) < expansion_interval {
        return;
    }
    poll.last_expansion_scan = now_ms;
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec(
        "pd_ports",
        crate::util::EC_IO_TIMEOUT,
        ec_clone,
        move |ec| ec.pd_ports(),
    )
    .await
    {
        Ok(ports) => {
            poll.last_pd_success_ms = crate::util::monotonic_ms();
            let changed = {
                let current = read_lock(&state.peripherals.pd_ports);
                *current != ports
            };
            if changed {
                with_write_lock(&state.peripherals.pd_ports, |guard| {
                    *guard = Arc::new(ports);
                });
                mark_view_dirty(state);
            }
            mark_pd_usb_c_seen(
                &read_lock(&state.peripherals.pd_ports),
                &state.peripherals.pd_usb_c_seen,
            );
            push_pd_ports_history(
                &state.peripherals.pd_ports,
                &state.peripherals.pd_ports_history,
            );
        }
        Err(e) => {
            // Overload skips are backpressure: fall
            // through to expansion/fan instead of
            // skipping the round.
            if e.is_overloaded() {
                // No warning, no staleness change.
            } else {
                warn!("PD ports read failed: {}", e);
                if poll.last_pd_success_ms != 0
                    && crate::util::monotonic_ms().saturating_sub(poll.last_pd_success_ms) > 30_000
                {
                    with_write_lock(&state.peripherals.pd_ports, |guard| {
                        if !guard.is_empty() {
                            *guard = Arc::new(smallvec::SmallVec::new());
                            mark_view_dirty(state);
                        }
                    });
                }
            }
        }
    }
    // Privacy switches move with a physical slider: the
    // 30s peripheral cadence is plenty, and the round
    // itself is the retry (no attempt budget needed).
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec(
        "privacy-switches",
        crate::util::EC_IO_TIMEOUT,
        ec_clone,
        move |ec| ec.privacy_switches(),
    )
    .await
    {
        Ok(Ok(switches)) => {
            with_write_lock(&state.peripherals.privacy, |guard| {
                if guard.as_ref().as_ref() != Some(&switches) {
                    *guard = Arc::new(Some(switches));
                    mark_view_dirty(state);
                }
            });
        }
        Err(e) if e.is_overloaded() => {}
        Ok(Err(e)) => {
            tracing::debug!("Privacy switches read failed: {}", e);
        }
        Err(e) => {
            tracing::debug!("Privacy switches dispatch failed: {}", e);
        }
    }
}

/// Slow firmware reads (versions, PD controller versions, deep
/// smart-battery telemetry) on the shared startup budget. Each has an
/// independent success flag so one failing never blocks the others.
/// `now_ms` arrives freshly sampled by the caller (see the loop).
pub(super) async fn phase_firmware(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    poll: &mut PollState,
    now_ms: u64,
) {
    // Versions rarely change; only fetch a few times at startup.
    // Two quick retries after the immediate first attempt,
    // then the full refresh interval for the last one.
    let versions_retry_ms = if poll.versions_scan_attempts < 2 {
        10_000
    } else {
        VERSIONS_REFRESH_MS
    };
    if poll.versions_scan_attempts < 3
        && read_lock(&state.system.versions).is_none()
        && (poll.last_versions_scan == 0
            || now_ms.saturating_sub(poll.last_versions_scan) >= versions_retry_ms)
    {
        poll.last_versions_scan = now_ms;
        let ec_clone = Arc::clone(ec);
        match crate::ec_serial::exec(
            "versions",
            crate::util::EC_IO_TIMEOUT,
            ec_clone,
            move |ec| ec.versions(),
        )
        .await
        {
            Ok(Ok(v)) => {
                with_write_lock(&state.system.versions, |guard| {
                    if guard.as_ref().as_ref() != Some(&v) {
                        *guard = Arc::new(Some(v));
                        mark_view_dirty(state);
                    }
                });
                // Stop after first successful fetch; versions only change after reboot.
                if read_lock(&state.system.versions).is_some() {
                    poll.versions_scan_attempts = 3;
                }
            }
            // Overload skips are backpressure, not failure:
            // don't consume the 3 startup tries.
            Err(e) if e.is_overloaded() => {}
            _ => {
                poll.versions_scan_attempts += 1;
            }
        }
    }
    // PD controller firmware changes as rarely as versions;
    // best-effort alongside it so one failing never blocks
    // the other.
    if poll.pd_fw_scan_attempts < 3
        && read_lock(&state.system.pd_fw).is_none()
        && (poll.last_pd_fw_scan == 0
            || now_ms.saturating_sub(poll.last_pd_fw_scan) >= versions_retry_ms)
    {
        poll.last_pd_fw_scan = now_ms;
        let ec_clone = Arc::clone(ec);
        match crate::ec_serial::exec(
            "pd-fw-versions",
            crate::util::EC_IO_TIMEOUT,
            ec_clone,
            move |ec| ec.pd_fw_versions(),
        )
        .await
        {
            Ok(Ok(v)) => {
                with_write_lock(&state.system.pd_fw, |guard| {
                    if guard.as_ref().as_ref() != Some(&v) {
                        *guard = Arc::new(Some(v));
                        mark_view_dirty(state);
                    }
                });
                if read_lock(&state.system.pd_fw).is_some() {
                    poll.pd_fw_scan_attempts = 3;
                }
            }
            Err(e) if e.is_overloaded() => {}
            _ => {
                poll.pd_fw_scan_attempts += 1;
            }
        }
    }
    // Deep smart-battery telemetry (~25 EC commands) is far
    // too heavy for the 5s battery poll; fetch it here on the
    // same startup budget instead. Cells and cycle counts go
    // stale slowly, so a rare fetch is plenty.
    if poll.smart_scan_attempts < 3
        && read_lock(&state.battery.smart).is_none()
        && (poll.last_smart_scan == 0
            || now_ms.saturating_sub(poll.last_smart_scan) >= versions_retry_ms)
    {
        poll.last_smart_scan = now_ms;
        let ec_clone = Arc::clone(ec);
        match crate::ec_serial::exec(
            "smart-battery",
            crate::util::EC_IO_TIMEOUT,
            ec_clone,
            move |ec| ec.smart_battery(),
        )
        .await
        {
            Ok(Ok(v)) => {
                with_write_lock(&state.battery.smart, |guard| {
                    if guard.as_ref().as_ref() != Some(&v) {
                        *guard = Arc::new(Some(v));
                        mark_view_dirty(state);
                    }
                });
                if read_lock(&state.battery.smart).is_some() {
                    poll.smart_scan_attempts = 3;
                }
            }
            Err(e) if e.is_overloaded() => {}
            _ => {
                poll.smart_scan_attempts += 1;
            }
        }
    }
}

/// CPU power poll (PawnIO ioctls off the async worker). Slow while idle;
/// hidden windows skip entirely. `now_ms` arrives freshly sampled.
pub(super) async fn phase_cpu_power(state: &AppState, poll: &mut PollState, now_ms: u64) {
    // Poll CPU power every 5s; slow to 30s while idle so the
    // card keeps updating without a 5s PawnIO wake-up. Hidden
    // windows still skip entirely.
    const CPU_POWER_POLL_MS: u64 = 5000;
    const CPU_POWER_IDLE_POLL_MS: u64 = 30_000;
    // Re-evaluate instead of reusing the top-of-loop
    // snapshot: the sleep and EC scans above can take
    // seconds under load, flipping idle state meanwhile.
    let cpu_poll_ms = if is_idle_now(state) {
        CPU_POWER_IDLE_POLL_MS
    } else {
        CPU_POWER_POLL_MS
    };
    let visible = state.lifecycle.visible.load(Ordering::Acquire);
    if visible
        && state.system.intel_cpu.load(Ordering::Acquire)
        && now_ms.saturating_sub(poll.last_cpu_power_poll) >= cpu_poll_ms
    {
        poll.last_cpu_power_poll = now_ms;
        // Run PawnIO ioctls off async worker like other EC I/O.
        let cpu_power = state.cpu_power.clone();
        if let Err(e) =
            crate::util::spawn_blocking_with_timeout(crate::util::PAWNIO_IO_TIMEOUT, move || {
                cpu_power.refresh()
            })
            .await
        {
            warn!("CPU power refresh task failed: {}", e);
        }
        mark_view_dirty(state);
    }
}

/// Fan-mode dispatch: config snapshot, mode-change reseeding, and the
/// mode arm through `FanCtx`. Owns the final clock resample (see below).
pub(super) async fn phase_fan(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    poll: &mut PollState,
) -> Phase {
    // Read needed fields then drop lock to avoid holding across EC I/O.
    let (manual_duty, curve_hysteresis, curve_rate_limit, curve_rate_limit_down, curve_sensors) = {
        let config = read_lock(&state.lifecycle.config);
        let mode_tmp =
            crate::types::FanControlMode::from_u8(state.fan.mode.load(Ordering::Acquire) as u8);
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

    let mode = crate::types::FanControlMode::from_u8(state.fan.mode.load(Ordering::Acquire) as u8);
    if poll.last_fan_mode.as_ref() != Some(&mode) {
        let last_duty = state.fan.last_applied_duty.load(Ordering::Acquire) as u32;
        if matches!(mode, crate::types::FanControlMode::Curve) && last_duty > 0 {
            // Seed stepper with last written duty to ramp instead of jump.
            poll.curve_stepper = CurveStepper::with_last_duty(last_duty);
        } else {
            poll.curve_stepper.reset();
        }
        poll.last_manual_duty = None;
        poll.last_unified_duty = None;
        poll.manual_per_fan_ramp = None;
        if matches!(mode, crate::types::FanControlMode::Manual) {
            // Prefer last written duty; fallback to RPM estimate.
            let last_duty = state.fan.last_applied_duty.load(Ordering::Acquire) as u32;
            poll.manual_ramp_current = if last_duty > 0 {
                Some(last_duty)
            } else {
                estimate_duty_from_thermal(state)
            };
        } else {
            poll.manual_ramp_current = None;
        }
    }
    // Re-sample the clock: PD/expansion/versions/CPU scans above
    // can take seconds under EC load, and a stale now_ms
    // would drift the 30s duty re-assert guard.
    let now_ms = crate::util::monotonic_ms();
    let mut ctx = FanCtx {
        state,
        ec,
        mode,
        now_ms,
        manual_duty,
        curve_hysteresis,
        curve_rate_limit,
        curve_rate_limit_down,
        curve_sensors,
        consecutive_ec_write_failures: poll.consecutive_ec_write_failures,
        last_duty_write_ms: poll.last_duty_write_ms,
        last_handover_ms: poll.last_handover_ms,
        last_manual_duty: poll.last_manual_duty,
        manual_ramp_current: poll.manual_ramp_current,
        manual_per_fan_ramp: &mut poll.manual_per_fan_ramp,
        curve_stepper: &mut poll.curve_stepper,
        last_fan_mode: poll.last_fan_mode,
        last_unified_duty: poll.last_unified_duty,
        just_resumed: poll.just_resumed,
    };
    let flow = match &mode {
        crate::types::FanControlMode::Disabled => fan_disabled::run(&mut ctx).await,
        crate::types::FanControlMode::Manual => fan_manual::run(&mut ctx).await,
        crate::types::FanControlMode::Curve => fan_curve::run(&mut ctx).await,
    };
    // Write back the owned copies the arms may have mutated.
    poll.consecutive_ec_write_failures = ctx.consecutive_ec_write_failures;
    poll.last_duty_write_ms = ctx.last_duty_write_ms;
    poll.last_handover_ms = ctx.last_handover_ms;
    poll.last_manual_duty = ctx.last_manual_duty;
    poll.manual_ramp_current = ctx.manual_ramp_current;
    poll.last_fan_mode = ctx.last_fan_mode;
    poll.last_unified_duty = ctx.last_unified_duty;
    match flow {
        Flow::Done => {
            poll.last_fan_mode = Some(mode);
            poll.just_resumed = false;
            Phase::Continue
        }
        Flow::Next => Phase::NextRound,
        Flow::Stop => Phase::Stop,
    }
}

/// Outcome of the hello tiebreak when thermal reads keep failing.
#[derive(Debug, PartialEq, Eq)]
enum ThermalVerdict {
    /// EC answers: keep the client and ride through. The fan arms hand
    /// control to firmware on staleness by themselves, so no reset, no
    /// backoff sleep, and the rest of the round still runs.
    RideThrough,
    /// EC silent: full client reset with backoff; the caller skips the round.
    Reset,
}

/// Pings the EC once thermal failures hit the threshold. A ~1ms hello
/// separates a dead EC (reset the client) from a flaky thermal path
/// (keep the client: tearing it down would also wipe fan state and stall
/// control for 3s for no reason).
async fn thermal_failure_verdict(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    consecutive_failures: u32,
) -> ThermalVerdict {
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec(
        "ec-hello",
        crate::util::EC_IO_TIMEOUT,
        ec_clone,
        move |ec| ec.hello(),
    )
    .await
    {
        Ok(Ok(())) => {
            warn!(
                "Thermal read failed {} times but EC answers hello; keeping client (no reset)",
                consecutive_failures
            );
            ThermalVerdict::RideThrough
        }
        // Dispatcher saturated: backpressure, not proof of death. Same
        // rule as everywhere else: overload never counts as failure.
        Err(e) if e.is_overloaded() => ThermalVerdict::RideThrough,
        Ok(Err(e)) => {
            warn!(
                "Thermal read failed {} times and EC hello failed ({}); resetting client",
                consecutive_failures, e
            );
            reset_ec_after_failures(state, consecutive_failures);
            ThermalVerdict::Reset
        }
        Err(e) => {
            warn!(
                "Thermal read failed {} times and EC hello dispatch failed ({}); resetting client",
                consecutive_failures, e
            );
            reset_ec_after_failures(state, consecutive_failures);
            ThermalVerdict::Reset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_client_state(ec: Arc<dyn crate::cli::EcHw>) -> AppState {
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
        with_write_lock(&state.system.ec_client, |guard| {
            *guard = Arc::new(Some(ec));
        });
        state.system.cli_available.store(true, Ordering::Release);
        state
    }

    #[tokio::test]
    async fn thermal_verdict_rides_through_when_ec_alive() {
        use crate::cli::ec_wrapper::tests::MockEc;
        let _serial = crate::ec_serial::global_dispatch_test_lock().lock().await;
        let mock = Arc::new(MockEc::new());
        let dyn_ec: Arc<dyn crate::cli::EcHw> = mock.clone();
        let state = live_client_state(dyn_ec.clone());
        let verdict = thermal_failure_verdict(&state, &dyn_ec, 5).await;
        assert_eq!(verdict, ThermalVerdict::RideThrough);
        // Client untouched: no reset happened.
        assert!(state.system.cli_available.load(Ordering::Acquire));
        assert!(
            read_lock(&state.system.ec_client)
                .as_ref()
                .as_ref()
                .is_some()
        );
        assert!(mock.ops.lock().unwrap().contains(&"hello".to_string()));
    }

    #[tokio::test]
    async fn thermal_verdict_resets_when_ec_silent() {
        use crate::cli::ec_wrapper::tests::MockEc;
        let _serial = crate::ec_serial::global_dispatch_test_lock().lock().await;
        let mock = Arc::new(MockEc::new());
        mock.fail_hello.store(true, Ordering::SeqCst);
        let dyn_ec: Arc<dyn crate::cli::EcHw> = mock.clone();
        let state = live_client_state(dyn_ec.clone());
        let verdict = thermal_failure_verdict(&state, &dyn_ec, 5).await;
        assert_eq!(verdict, ThermalVerdict::Reset);
        // reset_ec_after_failures drops the client and flags a fan reset.
        assert!(!state.system.cli_available.load(Ordering::Acquire));
        assert!(
            read_lock(&state.system.ec_client)
                .as_ref()
                .as_ref()
                .is_none()
        );
        assert!(state.lifecycle.fan_reset_pending.load(Ordering::Acquire));
    }

    #[test]
    fn poll_state_reset_fan_state_clears_ramps() {
        use crate::app::AppState;
        use crate::types::FanControlMode;
        let state = AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        let mut poll = PollState::new(&state, 70, FanControlMode::Manual, true, 1_000);
        // Seeded from the saved duty, like the loop start.
        assert_eq!(poll.manual_ramp_current, Some(70));
        assert_eq!(poll.curve_stepper.current_duty(), Some(70));
        assert_eq!(poll.last_fan_mode, Some(FanControlMode::Manual));
        poll.last_manual_duty = Some(60);
        poll.last_duty_write_ms = 5_000;
        poll.just_resumed = true;
        poll.reset_fan_state();
        assert_eq!(poll.manual_ramp_current, None);
        assert!(poll.manual_per_fan_ramp.is_none());
        assert_eq!(poll.curve_stepper.current_duty(), None);
        assert_eq!(poll.last_fan_mode, None);
        assert_eq!(poll.last_manual_duty, None);
        assert_eq!(poll.last_duty_write_ms, 0);
        assert_eq!(poll.last_handover_ms, 0);
        // Counters and clocks are not fan state: untouched.
        assert_eq!(poll.last_cpu_power_poll, 0);
        assert_eq!(poll.consecutive_ec_failures, 0);
    }

    #[test]
    fn poll_state_disabled_mode_seeds_no_fan_mode() {
        use crate::app::AppState;
        use crate::types::FanControlMode;
        let state = AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        let poll = PollState::new(&state, 0, FanControlMode::Disabled, false, 1_000);
        // None forces firmware restore on the first iteration.
        assert_eq!(poll.last_fan_mode, None);
        assert_eq!(poll.manual_ramp_current, None);
    }
}
