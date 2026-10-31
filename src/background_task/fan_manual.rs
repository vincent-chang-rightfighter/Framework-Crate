//! Manual fan mode: unified duty ramp and per-fan ramps.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use super::{
    FAN_REASSERT_INTERVAL_MS, FanCtx, Flow, MAX_EC_WRITE_FAILURES, THERMAL_STALE_MS,
    estimate_duty_from_thermal, reset_ec_after_failures, reset_ec_on_panic, shutdown_requested,
};
use crate::util::read_lock;

fn unified_transition_resets(prev: Option<bool>, now: bool) -> bool {
    prev != Some(now)
}

fn active_manual_duty_low(
    unified: bool,
    manual_duty: Option<u32>,
    per_fan: &[u32],
    fan_count: usize,
    min_duty: u32,
) -> bool {
    if unified {
        manual_duty.is_none_or(|d| d < min_duty)
    } else {
        fan_count > 0 && per_fan.iter().take(fan_count).any(|&d| d < min_duty)
    }
}

/// Drives Manual mode: unified single ramp, or one ramp per fan.
pub(super) async fn run(ctx: &mut FanCtx<'_>) -> Flow {
    let unified_now = ctx.state.fan.unified_duty.load(Ordering::Acquire);
    if unified_transition_resets(ctx.last_unified_duty, unified_now) {
        ctx.last_unified_duty = Some(unified_now);
        ctx.last_manual_duty = None;
        ctx.manual_ramp_current = None;
        *ctx.manual_per_fan_ramp = None;
        ctx.last_duty_write_ms = 0;
    }
    // High-temp guard: Manual low duty with fresh hot readings would cook
    // the machine while stale detection stays quiet. Hand back to firmware.
    // Checked before the stale path so fresh-hot still trips. Below 60% at
    // 80C+ (non-battery max) is unsafe. Battery sensors are excluded so
    // charging heat can't trip it.
    {
        if shutdown_requested(ctx.state) {
            return Flow::Stop;
        }
        let thermal = crate::util::read_lock(&ctx.state.thermal.data);
        if let Some(info) = thermal.as_ref().as_ref() {
            let max_temp = info
                .temps
                .iter()
                .filter(|(n, _)| !crate::types::is_battery_sensor(n))
                .map(|(_, t)| *t)
                .max()
                .unwrap_or(0);
            if max_temp >= 80 {
                // 80C requires at least 60%, 90C requires at least 80%.
                let min_duty = if max_temp >= 90 { 80 } else { 60 };
                let unified = ctx.state.fan.unified_duty.load(Ordering::Acquire);
                let low = if unified {
                    active_manual_duty_low(true, ctx.manual_duty, &[], 0, min_duty)
                } else {
                    let per_fan = crate::util::read_lock(&ctx.state.fan.per_fan_duty);
                    let fan_count = ctx.state.fan.fan_count.load(Ordering::Acquire) as usize;
                    active_manual_duty_low(false, ctx.manual_duty, &per_fan, fan_count, min_duty)
                };
                if low {
                    return ctx
                        .handover_to_firmware(
                            "manual-high-temp-handover",
                            &format!(
                                "Manual mode: {}C (non-battery max) with sub-{}% duty, handed to firmware",
                                max_temp, min_duty
                            ),
                        )
                        .await;
                }
            }
        }
    }
    // Same stale-thermal handover as Curve: driving blind on dead readings
    // risks holding a low duty into rising heat, so hand back to firmware.
    let last_ok = ctx.state.thermal.last_success_ms.load(Ordering::Acquire);
    if last_ok == 0 || ctx.now_ms.saturating_sub(last_ok) > THERMAL_STALE_MS {
        // Read failures are counted by the background loop that performs
        // the thermal read; this arm only hands over. The shared helper
        // throttles repeated handovers and resets the write-failure run.
        let reason = format!(
            "Manual mode: stale thermal ({}ms), handed fan control to firmware",
            ctx.now_ms.saturating_sub(last_ok)
        );
        return ctx
            .handover_to_firmware("manual-stale-handover", &reason)
            .await;
    }
    let unified = ctx.state.fan.unified_duty.load(Ordering::Acquire);
    if unified {
        if let Some(target) = ctx.manual_duty {
            let current = ctx
                .manual_ramp_current
                .or_else(|| {
                    let last = ctx.state.fan.last_applied_duty.load(Ordering::Acquire) as u32;
                    (last > 0)
                        .then_some(last)
                        .or_else(|| estimate_duty_from_thermal(ctx.state))
                })
                .unwrap_or(target);
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
                    let ec_clone = Arc::clone(ctx.ec);
                    match crate::ec_serial::exec(
                        "manual-set-fan-duty",
                        crate::util::EC_IO_TIMEOUT,
                        ec_clone,
                        move |ec| ec.set_fan_duty(next, None),
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
                                    backoff_ec = true;
                                }
                            }
                        },
                        Err(e) if e.is_overloaded() => {
                            // Backpressure, not failure: skip the rest of
                            // this round like the old lock-contention path.
                            return Flow::Next;
                        }
                        Err(e) => {
                            warn!("EC write failed (set_fan_duty): {}", e);
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
                    let ec_clone = Arc::clone(ctx.ec);
                    let fan_idx = idx as u32;
                    match crate::ec_serial::exec(
                        "manual-set-per-fan-duty",
                        crate::util::EC_IO_TIMEOUT,
                        ec_clone,
                        move |ec| ec.set_fan_duty(next_i, Some(fan_idx)),
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
                                write_backoff = true;
                            }
                        }
                        Err(e) if e.is_overloaded() => {
                            // Backpressure, not failure: abandon this round
                            // like the old lock-contention path did.
                            break;
                        }
                        Err(e) => {
                            warn!("EC write failed (set_fan_duty fan {}): {}", fan_idx, e);
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

#[cfg(test)]
mod tests {
    use super::active_manual_duty_low;
    use super::run;
    use super::unified_transition_resets;
    use crate::app::AppState;
    use crate::background_task::{FanCtx, Flow};
    use crate::cli::EcHw;
    use crate::cli::ec_wrapper::tests::MockEc;
    use crate::fan_control::CurveStepper;
    use crate::types::FanControlMode;
    use crate::util::with_write_lock;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn unified_toggle_forces_a_fan_write() {
        let _serial = crate::ec_serial::global_dispatch_test_lock().lock().await;
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
            .fan
            .mode
            .store(FanControlMode::Manual.to_u8() as u64, Ordering::Release);
        state.fan.unified_duty.store(false, Ordering::Release);
        state.fan.fan_count.store(1, Ordering::Release);
        state
            .thermal
            .last_success_ms
            .store(60_000, Ordering::Release);
        with_write_lock(&state.fan.per_fan_duty, |guard| {
            *guard = Arc::new(vec![100]);
        });
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        let mut ramp = Some(vec![100]);
        let mut stepper = CurveStepper::new();
        let mut ctx = FanCtx {
            state: &state,
            ec: &ec,
            mode: FanControlMode::Manual,
            now_ms: 60_000,
            manual_duty: Some(100),
            curve_hysteresis: None,
            curve_rate_limit: None,
            curve_rate_limit_down: None,
            curve_sensors: Vec::new(),
            consecutive_ec_write_failures: 0,
            last_duty_write_ms: 60_000,
            last_handover_ms: 0,
            last_manual_duty: Some(100),
            manual_ramp_current: Some(100),
            manual_per_fan_ramp: &mut ramp,
            curve_stepper: &mut stepper,
            last_fan_mode: Some(FanControlMode::Manual),
            last_unified_duty: Some(true),
            just_resumed: false,
        };
        assert!(matches!(run(&mut ctx).await, Flow::Done));
        let ops = mock.ops.lock().unwrap();
        assert!(
            ops.iter().any(|op| op == "set_fan_duty(100,Some(0))"),
            "unified transition must re-write the per-fan duty: {ops:?}"
        );
    }

    #[test]
    fn unified_toggle_resets_manual_bookkeeping() {
        assert!(unified_transition_resets(None, true));
        assert!(unified_transition_resets(Some(true), false));
        assert!(unified_transition_resets(Some(false), true));
    }

    #[test]
    fn unchanged_unified_mode_keeps_ramps() {
        assert!(!unified_transition_resets(Some(true), true));
        assert!(!unified_transition_resets(Some(false), false));
    }

    #[test]
    fn high_temp_guard_uses_only_the_active_unified_path() {
        assert!(active_manual_duty_low(true, Some(50), &[0], 1, 60));
        assert!(!active_manual_duty_low(true, Some(100), &[0], 1, 60));
        // >=90C tier raises the floor to 80%.
        assert!(active_manual_duty_low(true, Some(70), &[0], 1, 80));
        assert!(!active_manual_duty_low(true, Some(80), &[0], 1, 80));
    }

    #[test]
    fn high_temp_guard_uses_only_active_per_fan_entries() {
        assert!(active_manual_duty_low(false, Some(100), &[50, 100], 2, 60));
        assert!(!active_manual_duty_low(false, Some(50), &[100, 50], 1, 60));
        assert!(!active_manual_duty_low(false, Some(50), &[], 0, 60));
    }
}
