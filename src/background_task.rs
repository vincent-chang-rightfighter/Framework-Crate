use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
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

/// Consecutive thermal read failures before the hello tiebreak.
/// The client is reinitialized only when the EC stays silent; a live EC
/// with a flaky thermal path rides through instead.
pub(super) const MAX_EC_IO_FAILURES: u32 = 5;

/// Consecutive EC write failures before reinitializing client (separate from reads).
pub(super) const MAX_EC_WRITE_FAILURES: u32 = 5;

static EC_INIT_SEM: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

fn try_ec_init_permit(
    sem: &Arc<tokio::sync::Semaphore>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    sem.clone().try_acquire_owned().ok()
}

/// Re-assert interval for converged fan duty; recovers fans if resume event was missed.
pub(super) const FAN_REASSERT_INTERVAL_MS: u64 = 30_000;

/// Rate-limit for curve fail-safe handover to firmware when temps are empty.
pub(super) const CURVE_TEMP_FAILOVER_MS: u64 = 1_000;

/// Full warning for a persistent handover condition is emitted at most this
/// often; throttled repeats go to debug so a stale sensor cannot fill the log.
const HANDOVER_WARN_INTERVAL_MS: u64 = 60_000;
static LAST_HANDOVER_WARN_MS: AtomicU64 = AtomicU64::new(0);

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
    pub ec: &'a Arc<dyn cli::EcHw>,
    pub mode: crate::types::FanControlMode,
    pub now_ms: u64,
    pub manual_duty: Option<u32>,
    pub curve_hysteresis: Option<u32>,
    pub curve_rate_limit: Option<u32>,
    pub curve_rate_limit_down: Option<u32>,
    pub curve_sensors: Vec<String>,
    // NOTE: no read-failure counter here on purpose: the background loop
    // owns thermal read accounting, so mode arms must not double-count one
    // bad poll. Write failures use consecutive_ec_write_failures below.
    pub consecutive_ec_write_failures: u32,
    pub last_duty_write_ms: u64,
    pub last_handover_ms: u64,
    pub last_manual_duty: Option<u32>,
    pub manual_ramp_current: Option<u32>,
    pub manual_per_fan_ramp: &'a mut Option<Vec<u32>>,
    pub curve_stepper: &'a mut CurveStepper,
    pub last_fan_mode: Option<crate::types::FanControlMode>,
    pub last_unified_duty: Option<bool>,
    pub just_resumed: bool,
}

impl FanCtx<'_> {
    /// Hands fan control back to firmware with shared throttling and
    /// write-failure accounting. Every failover path (stale thermal, missing
    /// config, no usable temperature, manual high-temp guard) goes through
    /// this so a persistent condition cannot spam `autofanctrl` every poll,
    /// and a successful handover clears the consecutive-write-failure run.
    pub(super) async fn handover_to_firmware(&mut self, op: &'static str, reason: &str) -> Flow {
        if shutdown_requested(self.state) {
            return Flow::Stop;
        }
        if self.now_ms.saturating_sub(self.last_handover_ms) < CURVE_TEMP_FAILOVER_MS {
            return Flow::Next;
        }
        // Stamp the attempt before the write so failures are throttled too.
        self.last_handover_ms = self.now_ms;
        let ec_clone = Arc::clone(self.ec);
        match crate::ec_serial::exec(op, crate::util::EC_IO_TIMEOUT, ec_clone, move |ec| {
            ec.autofanctrl()
        })
        .await
        {
            Ok(Ok(())) => {
                self.consecutive_ec_write_failures = 0;
                let last = LAST_HANDOVER_WARN_MS.load(Ordering::Relaxed);
                if self.now_ms.saturating_sub(last) >= HANDOVER_WARN_INTERVAL_MS {
                    LAST_HANDOVER_WARN_MS.store(self.now_ms, Ordering::Relaxed);
                    warn!("{}", reason);
                } else {
                    tracing::debug!("{}", reason);
                }
            }
            Ok(Err(e)) => {
                warn!("{} failed: {}", op, e);
                self.consecutive_ec_write_failures += 1;
                if self.consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                    reset_ec_after_failures(self.state, self.consecutive_ec_write_failures);
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
            Err(join_err) => {
                warn!("EC spawn panicked ({}): {}", op, join_err);
                reset_ec_on_panic(self.state);
            }
        }
        Flow::Next
    }
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

/// Per-poll mutable state for the background loop (P2). Collects the ~23
/// cross-iteration locals that used to sprawl across the poll body so phases
/// can take `&mut PollState` instead of a dozen parameters. Owned by the
/// inner task; each phase borrows it for one call, never across awaits that
/// outlive the iteration.
struct PollState {
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
    fn new(
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
    fn reset_fan_state(&mut self) {
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
enum Phase {
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
async fn phase_ensure_ec(state: &AppState) -> Option<Arc<dyn cli::EcHw>> {
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
async fn phase_thermal(state: &AppState, ec: &Arc<dyn cli::EcHw>, poll: &mut PollState) -> Phase {
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
fn phase_resume(state: &AppState, poll: &mut PollState) -> Phase {
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
async fn phase_battery(state: &AppState, ec: &Arc<dyn cli::EcHw>, poll: &mut PollState) {
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
async fn phase_scans(state: &AppState, ec: &Arc<dyn cli::EcHw>, poll: &mut PollState, now_ms: u64) {
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
async fn phase_firmware(
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
async fn phase_cpu_power(state: &AppState, poll: &mut PollState, now_ms: u64) {
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
async fn phase_fan(state: &AppState, ec: &Arc<dyn cli::EcHw>, poll: &mut PollState) -> Phase {
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
        let mut changed = seen.len() < need;
        if changed {
            seen.resize(need, false);
        }
        for p in ports {
            let idx = p.port as usize;
            if p.power_role == Some("Sink") && idx < seen.len() && !seen[idx] {
                seen[idx] = true;
                changed = true;
            }
        }
        if changed {
            *guard = Arc::new(seen);
        }
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

fn ac_transition_pending(prev_ac: bool, ac_now: bool, had_prior_sample: bool) -> bool {
    had_prior_sample && prev_ac && !ac_now
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

/// Fresh idle verdict. `last_interaction_ts` can change while the loop
/// sleeps or runs slow EC scans, so code after an await must call this
/// instead of reusing the snapshot taken at the top of the iteration.
fn is_idle_now(state: &AppState) -> bool {
    is_idle_at(
        state.lifecycle.last_interaction_ts.load(Ordering::Acquire),
        crate::util::monotonic_ms(),
    )
}

/// Pure idle comparison, split out for deterministic tests (`monotonic_ms`
/// counts from process start, so a fresh test process is never idle).
fn is_idle_at(last_interaction_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_interaction_ms) > IDLE_THRESHOLD_MS
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
    // Per-fan duties are only editable/used in Manual mode; Curve writes one
    // global duty, so resizing the vector there is pointless write churn.
    if crate::types::FanControlMode::from_u8(state.fan.mode.load(Ordering::Acquire) as u8)
        == crate::types::FanControlMode::Manual
    {
        ensure_per_fan_duty(state, t.fans.len());
    }
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
        // EC is alive even when temps are stable: record liveness before
        // throttling the chart push, otherwise a stable machine starves
        // last_success_ms and trips THERMAL_STALE_MS failover.
        state
            .thermal
            .last_success_ms
            .store(now as u64, Ordering::Release);
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

/// Runs one startup read unless a previous dispatch failure already proved the
/// EC unavailable. Command-level errors keep the dispatcher usable; a timeout
/// or driver failure skips the remaining reads instead of paying the timeout
/// once per sensor.
async fn read_ec<T, F, Fut>(alive: &mut bool, label: &'static str, f: F) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Result<T, String>, crate::ec_serial::DispatchError>>,
{
    if !*alive {
        return None;
    }
    match f().await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            tracing::debug!("refresh_all_data {} read failed: {}", label, e);
            None
        }
        Err(e) if e.is_overloaded() => {
            tracing::debug!("refresh_all_data {} dispatch overloaded", label);
            None
        }
        Err(e) => {
            *alive = false;
            warn!("refresh_all_data {} dispatch failed: {}", label, e);
            None
        }
    }
}

pub(crate) async fn refresh_all_data(state: &AppState, ec: &std::sync::Arc<dyn cli::EcHw>) {
    let battery_ref = Arc::clone(&state.battery.info);
    let kblight_ref = Arc::clone(&state.peripherals.kblight);
    let pd_ports_ref = Arc::clone(&state.peripherals.pd_ports);
    let pd_history_ref = Arc::clone(&state.peripherals.pd_ports_history);
    let mut ec_alive = true;
    let thermal = read_ec(&mut ec_alive, "thermal", || {
        crate::ec_serial::exec(
            "refresh-thermal",
            crate::util::EC_IO_TIMEOUT,
            Arc::clone(ec),
            |ec| ec.thermal(),
        )
    })
    .await;
    let power = read_ec(&mut ec_alive, "battery", || {
        crate::ec_serial::exec(
            "refresh-power",
            crate::util::EC_IO_TIMEOUT,
            Arc::clone(ec),
            |ec| ec.power(),
        )
    })
    .await;
    let kblight = read_ec(&mut ec_alive, "keyboard backlight", || {
        crate::ec_serial::exec(
            "refresh-kblight",
            crate::util::EC_IO_TIMEOUT,
            Arc::clone(ec),
            |ec| ec.kblight_get(),
        )
    })
    .await;
    let pd_ports = read_ec(&mut ec_alive, "PD ports", || {
        crate::ec_serial::exec(
            "refresh-pd-ports",
            crate::util::EC_IO_TIMEOUT,
            Arc::clone(ec),
            |ec| Ok::<_, String>(ec.pd_ports()),
        )
    })
    .await;

    if let Some(t) = thermal {
        record_thermal_sample(state, t);
    }
    if let Some(bat) = power {
        // Publish AC state here as well as in the poll loop: the startup
        // BIOS-defaults capture runs right after this refresh and must not
        // depend on a later CPU-power poll (which is skipped while hidden).
        if let Some(ac_present) = bat.ac_present {
            crate::cpu_power::publish_ac_snapshot(ac_present);
        }
        with_write_lock(&battery_ref, |guard| {
            *guard = Arc::new(Some(crate::types::BatteryInfo { power_info: bat }));
        });
    }
    if let Some(kb) = kblight {
        with_write_lock(&kblight_ref, |guard| {
            *guard = Arc::new(Some(kb));
        });
    }
    if let Some(ports) = pd_ports {
        with_write_lock(&pd_ports_ref, |guard| {
            *guard = Arc::new(ports);
        });
    }
    mark_pd_usb_c_seen(&read_lock(&pd_ports_ref), &state.peripherals.pd_usb_c_seen);
    push_pd_ports_history(&pd_ports_ref, &pd_history_ref);
    mark_view_dirty(state);
}
pub fn spawn(state: AppState) {
    const MAX_CONSECUTIVE_FAILURES: u32 = 10;
    // Panics months apart must not accumulate into a shutdown: forget
    // failures older than an hour so only a real crash loop trips the
    // firmware handover below.
    const OUTER_FAILURE_WINDOW_MS: u64 = 3_600_000;
    let hook_state = state.clone();
    crate::tray::set_session_end_hook(move || {
        hook_state.lifecycle.shutdown.store(true, Ordering::Release);
        let ec_opt = { read_lock(&hook_state.system.ec_client) };
        let Some(ec) = ec_opt.as_ref().as_ref() else {
            return;
        };
        let limit = read_lock(&hook_state.lifecycle.config)
            .battery
            .charge_limit_max_pct;
        let ec = Arc::clone(ec);
        let _ = crate::ec_serial::exec_blocking(
            "session-end-handover",
            std::time::Duration::from_secs(2),
            ec,
            move |ec| {
                let _ = ec.autofanctrl();
                if let Some(limit) = limit {
                    let pct = if limit.enabled { limit.value } else { 100 };
                    let _ = ec.charge_limit_set(0, pct);
                }
            },
        );
    });
    tokio::spawn(async move {
        let mut consecutive_failures: u32 = 0;
        let mut last_outer_failure_ms: u64 = 0;
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
                let mut poll = PollState::new(
                    &bg_state2,
                    saved_duty,
                    init_fan_mode,
                    init_is_manual,
                    crate::util::monotonic_ms(),
                );
                'poll_loop: loop {
                    match phase_resume(&bg_state2, &mut poll) {
                        Phase::Stop => return,
                        Phase::Continue => {}
                        Phase::NextRound => continue 'poll_loop,
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
                    // No clock read here: `is_idle_now` samples it internally,
                    // and `now_ms` is sampled fresh below so slow EC rounds
                    // cannot skew the poll cadences.
                    let is_idle = is_idle_now(&bg_state2);
                    let hidden = !bg_state2.lifecycle.visible.load(Ordering::Acquire);
                    let effective_interval = match fan_mode {
                        // Curve must keep responding while idle; slowdown only for other modes.
                        // Hidden window caps every mode at 2s to save power while
                        // staying well inside the 5s stale-thermal failover.
                        crate::types::FanControlMode::Curve if !hidden => interval,
                        crate::types::FanControlMode::Curve => interval.max(2_000),
                        _ if is_idle || hidden => IDLE_INTERVAL_MS.max(interval),
                        _ => interval,
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(effective_interval)).await;
                    if bg_state2.lifecycle.shutdown.load(Ordering::Acquire) {
                        return;
                    }

                    let Some(ec) = phase_ensure_ec(&bg_state2).await else {
                        continue 'poll_loop;
                    };

                    match phase_thermal(&bg_state2, &ec, &mut poll).await {
                        Phase::Stop => return,
                        Phase::NextRound => continue 'poll_loop,
                        Phase::Continue => {}
                    }
                    phase_battery(&bg_state2, &ec, &mut poll).await;

                    // PD/privacy scans share one throttle gate (see phase).
                    // Sampled here (not top of loop) so the sleep above and
                    // slow EC rounds cannot skew the poll cadences.
                    let now_ms = crate::util::monotonic_ms();
                    phase_scans(&bg_state2, &ec, &mut poll, now_ms).await;
                    phase_firmware(&bg_state2, &ec, &mut poll, now_ms).await;
                    phase_cpu_power(&bg_state2, &mut poll, now_ms).await;
                    // Re-check shutdown after scans to avoid overwriting quit fan control.
                    if bg_state2.lifecycle.shutdown.load(Ordering::Acquire) {
                        return;
                    }

                    match phase_fan(&bg_state2, &ec, &mut poll).await {
                        Phase::Stop => return,
                        Phase::NextRound => continue 'poll_loop,
                        Phase::Continue => {}
                    }
                }
            });
            if let Some(e) = handle.await.err() {
                let now = crate::util::monotonic_ms();
                if now.saturating_sub(last_outer_failure_ms) > OUTER_FAILURE_WINDOW_MS {
                    consecutive_failures = 0;
                }
                last_outer_failure_ms = now;
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    // Hand the fans back to firmware before dying: without
                    // the loop nothing re-asserts duties, so a stuck manual
                    // duty would persist indefinitely with no UI to fix it.
                    // Single attempt with a generous timeout: the single
                    // writer serializes this after any in-flight duty, so
                    // the handover cannot be overtaken, and retries would
                    // only queue duplicate handovers behind the same hang.
                    let ec_opt = { read_lock(&state.system.ec_client) };
                    let handover: Option<
                        Result<Result<(), String>, crate::ec_serial::DispatchError>,
                    > = match ec_opt.as_ref().as_ref() {
                        Some(ec) => {
                            let ec_clone = Arc::clone(ec);
                            const FINAL_HANDOVER_TIMEOUT: std::time::Duration =
                                std::time::Duration::from_secs(5);
                            Some(
                                crate::ec_serial::exec(
                                    "final-handover",
                                    FINAL_HANDOVER_TIMEOUT,
                                    ec_clone,
                                    move |ec| ec.autofanctrl(),
                                )
                                .await,
                            )
                        }
                        None => None,
                    };
                    match handover {
                        Some(Ok(Ok(()))) => warn!(
                            "Background task failed {} times consecutively, giving up; fans returned to firmware control",
                            consecutive_failures
                        ),
                        Some(Ok(Err(e))) => warn!(
                            "Background task failed {} times consecutively, giving up; firmware handover also failed ({}): fans stuck at last duty",
                            consecutive_failures, e
                        ),
                        Some(Err(e)) => warn!(
                            "Background task failed {} times consecutively, giving up; firmware handover timed out ({}): fans stuck at last duty",
                            consecutive_failures, e
                        ),
                        None => warn!(
                            "Background task failed {} times consecutively, giving up with no EC client; fans stuck at last duty",
                            consecutive_failures
                        ),
                    }
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
    fn first_battery_sample_never_triggers_reset() {
        assert!(!ac_transition_pending(true, false, false));
    }

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
    fn idle_verdict_follows_latest_interaction() {
        // Pure comparison with synthetic clocks (deterministic: the real
        // clock counts from process start, so a fresh test host is young).
        assert!(!is_idle_at(1_000, 1_000));
        assert!(!is_idle_at(0, IDLE_THRESHOLD_MS));
        assert!(is_idle_at(0, IDLE_THRESHOLD_MS + 1));
        assert!(is_idle_at(9_000, 20_000));

        // Live helper: just interacted means active.
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
        state
            .lifecycle
            .last_interaction_ts
            .store(crate::util::monotonic_ms(), Ordering::Release);
        assert!(!is_idle_now(&state));
    }

    #[test]
    fn ac_to_battery_after_first_sample_triggers_reset() {
        assert!(ac_transition_pending(true, false, true));
        assert!(!ac_transition_pending(false, false, true));
        assert!(!ac_transition_pending(true, true, true));
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

    #[test]
    fn ec_init_permits_only_one_attempt_at_a_time() {
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let first = try_ec_init_permit(&sem).expect("first permit");
        assert!(try_ec_init_permit(&sem).is_none());
        drop(first);
        assert!(try_ec_init_permit(&sem).is_some());
    }

    #[tokio::test]
    async fn ec_dispatch_failure_skips_remaining_startup_reads() {
        let mut alive = true;
        let first: Option<u32> = read_ec(&mut alive, "first", || async {
            Err(crate::ec_serial::DispatchError::Failed("dead".into()))
        })
        .await;
        assert!(first.is_none());
        assert!(!alive);

        let mut called = false;
        let second: Option<u32> = read_ec(&mut alive, "second", || {
            called = true;
            async { Ok::<_, crate::ec_serial::DispatchError>(Ok::<_, String>(1u32)) }
        })
        .await;
        assert!(second.is_none());
        assert!(!called);
    }

    #[tokio::test]
    async fn ec_command_error_keeps_startup_reads_alive() {
        let mut alive = true;
        let failed: Option<u32> = read_ec(&mut alive, "first", || async {
            Ok::<_, crate::ec_serial::DispatchError>(Err::<u32, String>("unsupported".into()))
        })
        .await;
        assert!(failed.is_none());
        assert!(alive);

        let value: Option<u32> = read_ec(&mut alive, "second", || async {
            Ok::<_, crate::ec_serial::DispatchError>(Ok::<_, String>(7u32))
        })
        .await;
        assert_eq!(value, Some(7));
        assert!(alive);
    }

    #[tokio::test]
    async fn ec_overload_keeps_startup_reads_alive() {
        let mut alive = true;
        let skipped: Option<u32> = read_ec(&mut alive, "busy", || async {
            Err(crate::ec_serial::DispatchError::Overloaded { op: "busy" })
        })
        .await;
        assert!(skipped.is_none());
        assert!(alive);
    }
}
