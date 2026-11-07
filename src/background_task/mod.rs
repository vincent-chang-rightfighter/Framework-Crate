//! Background poll loop, fan-mode dispatch, and shared helpers.

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use tracing::warn;

use crate::app::AppState;
use crate::cli;
use crate::style::{IDLE_INTERVAL_MS, POLL_RATE_MIN_MS};
use crate::util::{read_lock, with_write_lock};

mod fan_ctx;
mod fan_curve;
mod fan_disabled;
mod fan_manual;
mod helpers;
mod poll;

use poll::{
    Phase, PollState, phase_battery, phase_cpu_power, phase_ensure_ec, phase_fan, phase_firmware,
    phase_resume, phase_scans, phase_thermal,
};

pub(in crate::background_task) use fan_ctx::FanCtx;
pub(in crate::background_task) use helpers::{
    ac_transition_pending, estimate_duty_from_thermal, is_idle_now, mark_pd_usb_c_seen,
    mark_view_dirty, push_pd_ports_history, record_thermal_sample, reset_ec_after_failures,
    reset_ec_on_panic, shutdown_requested, try_ec_init_permit, unified_transition_resets,
};

/// Consecutive thermal read failures before the hello tiebreak.
/// The client is reinitialized only when the EC stays silent; a live EC
/// with a flaky thermal path rides through instead.
pub(super) const MAX_EC_IO_FAILURES: u32 = 5;

/// Consecutive EC write failures before reinitializing client (separate from reads).
pub(super) const MAX_EC_WRITE_FAILURES: u32 = 5;

static EC_INIT_SEM: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

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
