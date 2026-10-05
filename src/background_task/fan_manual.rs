//! Manual fan mode: unified duty ramp and per-fan ramps.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use super::{
    FAN_REASSERT_INTERVAL_MS, FanCtx, Flow, MAX_EC_WRITE_FAILURES, estimate_duty_from_thermal,
    reset_ec_after_failures, reset_ec_on_panic, shutdown_requested,
};
use crate::util::read_lock;

/// Drives Manual mode: unified single ramp, or one ramp per fan.
pub(super) async fn run(ctx: &mut FanCtx<'_>) -> Flow {
    let unified = ctx.state.fan.unified_duty.load(Ordering::Acquire);
    if unified {
        if let Some(target) = ctx.manual_duty {
            let current = ctx.manual_ramp_current.unwrap_or(target);
            let next = crate::fan_control::apply_rate_limit(current, target, 10);
            let converged = ctx.last_manual_duty == Some(next);
            // Re-assert periodically even when converged to recover missed resume.
            let reassert = converged
                && ctx.now_ms.saturating_sub(ctx.last_duty_write_ms) >= FAN_REASSERT_INTERVAL_MS;
            if converged && !reassert {
                ctx.manual_ramp_current = Some(next);
            } else {
                if shutdown_requested(ctx.state) {
                    return Flow::Stop;
                }
                let mut backoff_ec = false;
                {
                    let Some(_ec_guard) = crate::util::acquire_ec_write().await else {
                        warn!("EC write lock contended, skipping this cycle");
                        return Flow::Next;
                    };
                    let ec_clone = Arc::clone(ctx.ec);
                    match crate::util::spawn_blocking_with_timeout(
                        crate::util::EC_IO_TIMEOUT,
                        move || ec_clone.set_fan_duty(next, None),
                    )
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
                                ctx.last_manual_duty = Some(next);
                                ctx.manual_ramp_current = Some(next);
                            }
                            Err(e) => {
                                warn!("Failed to set manual fan duty: {}", e);
                                ctx.consecutive_ec_write_failures += 1;
                                if ctx.consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                                    reset_ec_after_failures(
                                        ctx.state,
                                        ctx.consecutive_ec_write_failures,
                                    );
                                    // Drop the EC write lock before backing off.
                                    backoff_ec = true;
                                }
                            }
                        },
                        Err(join_err) => {
                            warn!("EC spawn panicked (set_fan_duty): {}", join_err);
                            reset_ec_on_panic(ctx.state);
                            return Flow::Next;
                        }
                    }
                }
                if backoff_ec {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    return Flow::Next;
                }
            }
        }
    } else {
        // Per-fan manual ramps independently; not gated by unified ramp state.
        let per_fan = read_lock(&ctx.state.fan.per_fan_duty);
        if !per_fan.is_empty() {
            let last_duty = ctx.state.fan.last_applied_duty.load(Ordering::Acquire) as u32;
            let seed = if last_duty > 0 {
                last_duty
            } else {
                estimate_duty_from_thermal(ctx.state).unwrap_or_else(|| per_fan[0])
            };
            let ramp = ctx
                .manual_per_fan_ramp
                .get_or_insert_with(|| vec![seed; per_fan.len()]);
            // Grow ramp when fan count increases; otherwise new fans never get written.
            if ramp.len() < per_fan.len() {
                ramp.resize(per_fan.len(), seed);
            }
            let mut wrote_any = false;
            let fan_count_now = ctx.state.fan.fan_count.load(Ordering::Acquire) as usize;
            // Re-assert once per pass, not per fan, to ensure all fans recover.
            let reassert =
                ctx.now_ms.saturating_sub(ctx.last_duty_write_ms) >= FAN_REASSERT_INTERVAL_MS;
            let mut backoff_ec = false;
            for (idx, &target) in per_fan.iter().enumerate() {
                // Skip indices beyond physical fan count (vector retains docked fans).
                if idx >= fan_count_now {
                    continue;
                }
                let current = ramp.get(idx).copied().unwrap_or(target);
                let next_i = crate::fan_control::apply_rate_limit(current, target, 10);
                // Write even when converged on resume or periodic re-assert.
                if next_i == current && !ctx.just_resumed && !reassert {
                    continue;
                }
                if shutdown_requested(ctx.state) {
                    return Flow::Stop;
                }
                // Re-check mode between fans to avoid writes after mode switch.
                let mode_check = crate::types::FanControlMode::from_u8(
                    ctx.state.fan.mode.load(Ordering::Acquire) as u8,
                );
                if mode_check != ctx.mode {
                    break;
                }
                let mut write_backoff = false;
                {
                    let Some(_ec_guard) = crate::util::acquire_ec_write().await else {
                        warn!("EC write lock contended, skipping this cycle");
                        return Flow::Next;
                    };
                    let ec_clone = Arc::clone(ctx.ec);
                    let fan_idx = idx as u32;
                    match crate::util::spawn_blocking_with_timeout(
                        crate::util::EC_IO_TIMEOUT,
                        move || ec_clone.set_fan_duty(next_i, Some(fan_idx)),
                    )
                    .await
                    {
                        Ok(Ok(())) => {
                            if shutdown_requested(ctx.state) {
                                return Flow::Stop;
                            }
                            ramp[idx] = next_i;
                            wrote_any = true;
                            ctx.consecutive_ec_write_failures = 0;
                            ctx.last_duty_write_ms = ctx.now_ms;
                        }
                        Ok(Err(e)) => {
                            warn!("Failed to set fan {} duty: {}", fan_idx, e);
                            ctx.consecutive_ec_write_failures += 1;
                            if ctx.consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                                reset_ec_after_failures(
                                    ctx.state,
                                    ctx.consecutive_ec_write_failures,
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
                            reset_ec_on_panic(ctx.state);
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
                return Flow::Next;
            }
            // Update on any write, including 0% convergence.
            if wrote_any {
                let max_applied = ramp.iter().copied().max().unwrap_or(0);
                ctx.state
                    .fan
                    .last_applied_duty
                    .store(max_applied as u64, Ordering::Release);
            }
            // Sync unified ramp seed with per-fan max for mode switch.
            ctx.manual_ramp_current = ramp.iter().copied().max();
        }
    }
    Flow::Done
}
