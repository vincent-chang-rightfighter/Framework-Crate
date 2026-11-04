use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tracing::warn;

use crate::app::AppState;
use crate::cli;
use crate::fan_control::CurveStepper;
use crate::style::{IDLE_INTERVAL_MS, IDLE_THRESHOLD_MS, POLL_RATE_MIN_MS};
use crate::temp_chart;
use crate::util::{read_lock, with_write_lock};

mod fan_curve;
mod fan_disabled;
mod fan_manual;
mod poll;

use poll::{
    Phase, PollState, phase_battery, phase_cpu_power, phase_ensure_ec, phase_fan, phase_firmware,
    phase_resume, phase_scans, phase_thermal,
};

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
