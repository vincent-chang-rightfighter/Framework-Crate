//! Curve fan mode: stepper-driven duty with firmware fail-safe handover.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use super::{
    CURVE_TEMP_FAILOVER_MS, FAN_REASSERT_INTERVAL_MS, FanCtx, Flow, MAX_EC_IO_FAILURES,
    MAX_EC_WRITE_FAILURES, THERMAL_STALE_MS, reset_ec_after_failures, reset_ec_on_panic,
    shutdown_requested,
};
use crate::util::read_lock;

/// Drives Curve mode from the thermal control temperature, handing back to
/// firmware whenever readings or config are missing.
pub(super) async fn run(ctx: &mut FanCtx<'_>) -> Flow {
    // Staleness check before using cached thermal.
    let last_ok = ctx.state.thermal.last_success_ms.load(Ordering::Acquire);
    let is_stale = last_ok == 0 || ctx.now_ms.saturating_sub(last_ok) > THERMAL_STALE_MS;
    if is_stale {
        ctx.consecutive_ec_failures += 1;
        if ctx.consecutive_ec_failures >= MAX_EC_IO_FAILURES {
            reset_ec_after_failures(ctx.state, ctx.consecutive_ec_failures);
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            return Flow::Next;
        }
        // Immediate handover when no fresh sample.
        if shutdown_requested(ctx.state) {
            return Flow::Stop;
        }
        let Some(_ec_guard) = crate::util::acquire_ec_write().await else {
            warn!("EC write lock contended, skipping this cycle");
            return Flow::Next;
        };
        let ec_clone = Arc::clone(ctx.ec);
        match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
            ec_clone.autofanctrl()
        })
        .await
        {
            Ok(Ok(())) => {
                ctx.last_curve_failover_ms = ctx.now_ms;
                tracing::warn!(
                    "Curve mode: stale thermal ({}ms), handed fan control to firmware",
                    ctx.now_ms.saturating_sub(last_ok)
                );
            }
            Ok(Err(e)) => {
                warn!("Curve stale fail-safe autofanctrl failed: {}", e);
                ctx.consecutive_ec_failures += 1;
                if ctx.consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                    reset_ec_after_failures(ctx.state, ctx.consecutive_ec_failures);
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    return Flow::Next;
                }
            }
            Err(join_err) => {
                warn!("EC spawn panicked (curve stale fail-safe): {}", join_err);
                reset_ec_on_panic(ctx.state);
            }
        }
        return Flow::Next;
    }
    let thermal_clone = read_lock(&ctx.state.thermal.data);
    let Some(ref thermal) = *thermal_clone else {
        // No sample yet but not stale-timed out above? immediate handover.
        if shutdown_requested(ctx.state) {
            return Flow::Stop;
        }
        let Some(_ec_guard) = crate::util::acquire_ec_write().await else {
            warn!("EC write lock contended, skipping this cycle");
            return Flow::Next;
        };
        let ec_clone = Arc::clone(ctx.ec);
        let _ = crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
            ec_clone.autofanctrl()
        })
        .await;
        return Flow::Next;
    };
    // avoid unwrap panic if curve config is corrupted/missing
    let Some(hyst) = ctx.curve_hysteresis else {
        warn!("Curve mode with missing hysteresis, handing back to firmware");
        if !shutdown_requested(ctx.state) {
            let Some(_ec_guard) = crate::util::acquire_ec_write().await else {
                warn!("EC write lock contended, skipping this cycle");
                return Flow::Next;
            };
            let ec_clone = Arc::clone(ctx.ec);
            let _ =
                crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                    ec_clone.autofanctrl()
                })
                .await;
        }
        return Flow::Next;
    };
    let Some(rate) = ctx.curve_rate_limit else {
        warn!("Curve mode with missing rate_limit, handing back to firmware");
        if !shutdown_requested(ctx.state) {
            let Some(_ec_guard) = crate::util::acquire_ec_write().await else {
                warn!("EC write lock contended, skipping this cycle");
                return Flow::Next;
            };
            let ec_clone = Arc::clone(ctx.ec);
            let _ =
                crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                    ec_clone.autofanctrl()
                })
                .await;
        }
        return Flow::Next;
    };
    // Fail-safe: empty temps means EC read failed; hand back to firmware immediately.
    // Previously waited 30s (CURVE_TEMP_FAILOVER_MS) holding last duty (could be 0%) → overheat.
    if thermal.temps.is_empty() {
        // Count as read failure toward EC reset.
        ctx.consecutive_ec_failures += 1;
        if ctx.consecutive_ec_failures >= MAX_EC_IO_FAILURES {
            reset_ec_after_failures(ctx.state, ctx.consecutive_ec_failures);
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            return Flow::Next;
        }
        // Throttle autofanctrl to once per second to avoid spamming EC
        if ctx.now_ms.saturating_sub(ctx.last_curve_failover_ms) >= CURVE_TEMP_FAILOVER_MS {
            if shutdown_requested(ctx.state) {
                return Flow::Stop;
            }
            let Some(_ec_guard) = crate::util::acquire_ec_write().await else {
                warn!("EC write lock contended, skipping this cycle");
                return Flow::Next;
            };
            let ec_clone = Arc::clone(ctx.ec);
            match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                ec_clone.autofanctrl()
            })
            .await
            {
                Ok(Ok(())) => {
                    ctx.last_curve_failover_ms = ctx.now_ms;
                    ctx.consecutive_ec_failures = 0;
                    tracing::warn!(
                        "Curve mode: EC temps read failed, handed fan control to firmware"
                    );
                }
                Ok(Err(e)) => {
                    warn!("Curve fail-safe autofanctrl failed: {}", e);
                    // Count failure toward EC reset.
                    ctx.consecutive_ec_failures += 1;
                    if ctx.consecutive_ec_failures >= MAX_EC_IO_FAILURES {
                        reset_ec_after_failures(ctx.state, ctx.consecutive_ec_failures);
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        return Flow::Next;
                    }
                }
                Err(join_err) => {
                    warn!(
                        "EC spawn panicked (curve fail-safe autofanctrl): {}",
                        join_err
                    );
                    reset_ec_on_panic(ctx.state);
                }
            }
        }
        return Flow::Next;
    }
    let control_temp = crate::types::curve_control_temp(&thermal.temps, &ctx.curve_sensors);
    let full_pts_arc = read_lock(&ctx.state.fan.curve_full_points);
    let full_pts: &[[u32; 2]] = &full_pts_arc;
    let mut next = ctx.curve_stepper.next(
        control_temp,
        hyst,
        rate,
        ctx.curve_rate_limit_down,
        full_pts,
    );
    // Even when the stepper has converged, periodically
    // re-assert the last duty. This recovers from a
    // missed resume event that would otherwise leave
    // fans in firmware control.
    if next.is_none()
        && ctx.now_ms.saturating_sub(ctx.last_duty_write_ms) >= FAN_REASSERT_INTERVAL_MS
    {
        next = ctx.curve_stepper.current_duty();
    }
    if let Some(next) = next {
        let fan_count = ctx.state.fan.fan_count.load(Ordering::Acquire) as u32;
        let write_each = !ctx.state.fan.unified_duty.load(Ordering::Acquire) && fan_count > 1;
        // Only advance the stepper when every target fan
        // was updated. Partial success must not distort
        // the ramp state.
        let mut applied = false;
        if write_each {
            let mut all_fans_applied = true;
            let mut any_succeeded = false;
            let mut failed_fans: Vec<u32> = Vec::new();
            for fan_idx in 0..fan_count {
                if shutdown_requested(ctx.state) {
                    return Flow::Stop;
                }
                let mode_check = crate::types::FanControlMode::from_u8(
                    ctx.state.fan.mode.load(Ordering::Acquire) as u8,
                );
                if mode_check != ctx.mode {
                    all_fans_applied = false;
                    break;
                }
                let Some(_ec_guard) = crate::util::acquire_ec_write().await else {
                    warn!("EC write lock contended, skipping this cycle");
                    return Flow::Next;
                };
                let ec_clone = Arc::clone(ctx.ec);
                match crate::util::spawn_blocking_with_timeout(
                    crate::util::EC_IO_TIMEOUT,
                    move || ec_clone.set_fan_duty(next, Some(fan_idx)),
                )
                .await
                {
                    Ok(result) => {
                        if let Err(e) = result {
                            // Transient per-fan failure (e.g. dock unplugged); only all-fail counts toward reset.
                            all_fans_applied = false;
                            failed_fans.push(fan_idx);
                            tracing::debug!("Failed to set fan {} duty (curve): {}", fan_idx, e);
                        } else {
                            any_succeeded = true;
                            ctx.last_duty_write_ms = ctx.now_ms;
                        }
                    }
                    Err(join_err) => {
                        warn!(
                            "EC spawn panicked (set_fan_duty curve fan {}): {}",
                            fan_idx, join_err
                        );
                        reset_ec_on_panic(ctx.state);
                        all_fans_applied = false;
                        break;
                    }
                }
            }
            if any_succeeded {
                ctx.consecutive_ec_write_failures = 0;
            } else {
                ctx.consecutive_ec_write_failures += 1;
                if ctx.consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                    reset_ec_after_failures(ctx.state, ctx.consecutive_ec_write_failures);
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    return Flow::Next;
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
                ctx.state
                    .fan
                    .last_applied_duty
                    .store(next as u64, Ordering::Release);
            }
        } else {
            if shutdown_requested(ctx.state) {
                return Flow::Stop;
            }
            let Some(_ec_guard) = crate::util::acquire_ec_write().await else {
                warn!("EC write lock contended, skipping this cycle");
                return Flow::Next;
            };
            let ec_clone = Arc::clone(ctx.ec);
            match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                ec_clone.set_fan_duty(next, None)
            })
            .await
            {
                Ok(result) => match result {
                    Ok(()) => {
                        if shutdown_requested(ctx.state) {
                            return Flow::Stop;
                        }
                        let mode_now = crate::types::FanControlMode::from_u8(
                            ctx.state.fan.mode.load(Ordering::Acquire) as u8,
                        );
                        if mode_now != ctx.mode {
                            ctx.last_fan_mode = Some(ctx.mode);
                            return Flow::Next;
                        }
                        ctx.consecutive_ec_write_failures = 0;
                        ctx.last_duty_write_ms = ctx.now_ms;
                        ctx.state
                            .fan
                            .last_applied_duty
                            .store(next as u64, Ordering::Release);
                        applied = true;
                    }
                    Err(e) => {
                        warn!("Failed to set fan duty (curve): {}", e);
                        ctx.consecutive_ec_write_failures += 1;
                        if ctx.consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                            reset_ec_after_failures(ctx.state, ctx.consecutive_ec_write_failures);
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                            return Flow::Next;
                        }
                    }
                },
                Err(join_err) => {
                    warn!("EC spawn panicked (set_fan_duty curve): {}", join_err);
                    reset_ec_on_panic(ctx.state);
                    return Flow::Next;
                }
            }
        }
        if applied {
            ctx.curve_stepper.note_applied(next);
        }
    }
    Flow::Done
}
