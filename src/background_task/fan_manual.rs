//! Manual fan mode: unified duty ramp and per-fan ramps.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use super::{
    FAN_REASSERT_INTERVAL_MS, FanCtx, Flow, MAX_EC_WRITE_FAILURES, THERMAL_STALE_MS,
    estimate_duty_from_thermal, reset_ec_after_failures, reset_ec_on_panic, shutdown_requested,
    unified_transition_resets,
};
use crate::util::read_lock;

/// Temperature at or above which Manual duty is floored (non-battery max).
const HOT_FLOOR_TEMP_C: i32 = 95;
/// Forced minimum duty at or above [`HOT_FLOOR_TEMP_C`]. Applied as a floor
/// (`max(user, 90)`), never a ceiling: a user at 100% keeps 100%.
const HOT_FLOOR_DUTY_PCT: u32 = 90;

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
    // Hot floor: at 95C+ (non-battery max) Manual duty is lifted to at
    // least 90%, because open-loop fixed duty has no feedback to save it.
    // Below 95C the user's duty is fully honored. The floor is evaluated
    // fresh every round from live readings and never persisted, so duty
    // eases back (rate-limited) as soon as temperature drops. Battery
    // sensors stay excluded so charging heat can't trip it.
    // Checked before the stale path so fresh-hot still trips; when readings
    // go stale the handover below takes over instead.
    let hot_floor: Option<u32> = {
        if shutdown_requested(ctx.state) {
            return Flow::Stop;
        }
        let thermal = crate::util::read_lock(&ctx.state.thermal.data);
        thermal
            .as_ref()
            .as_ref()
            .and_then(|info| {
                info.temps
                    .iter()
                    .filter(|(n, _)| !crate::types::is_battery_sensor(n))
                    .map(|(_, t)| *t)
                    .max()
            })
            .filter(|max_temp| *max_temp >= HOT_FLOOR_TEMP_C)
            .map(|max_temp| {
                tracing::debug!(
                    "Manual mode: {}C (non-battery max), flooring duty at {}%",
                    max_temp,
                    HOT_FLOOR_DUTY_PCT
                );
                HOT_FLOOR_DUTY_PCT
            })
    };
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
            // Hot floor is a minimum, never a ceiling.
            let target = hot_floor.map_or(target, |floor| target.max(floor));
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
                // Hot floor is a minimum, never a ceiling.
                let target = hot_floor.map_or(target, |floor| target.max(floor));
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
            // Update on any write, including 0% convergence. Only consider
            // physically present fans: the ramp vector retains docked fans
            // beyond fan_count_now, and their stale duties must not leak
            // into the unified seed / quit-handover baseline.
            if wrote_any {
                let max_applied = ramp.iter().take(fan_count_now).copied().max().unwrap_or(0);
                ctx.state
                    .fan
                    .last_applied_duty
                    .store(max_applied as u64, Ordering::Release);
            }
            // Sync unified ramp seed with per-fan max for mode switch.
            // Same dock-residual guard as above: only present fans seed.
            ctx.manual_ramp_current = ramp.iter().take(fan_count_now).copied().max();
        }
    }
    Flow::Done
}

#[cfg(test)]
mod tests {
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
        let mut per_fan_steppers: Vec<CurveStepper> = Vec::new();
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
            per_fan_curve_steppers: &mut per_fan_steppers,
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

    #[tokio::test]
    async fn per_fan_max_ignores_docked_residual() {
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
        // One physical fan; slot 1 retains a docked fan's stale 100.
        state.fan.fan_count.store(1, Ordering::Release);
        state
            .thermal
            .last_success_ms
            .store(60_000, Ordering::Release);
        with_write_lock(&state.fan.per_fan_duty, |guard| {
            *guard = Arc::new(vec![20, 100]);
        });
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        // Ramp retains the docked residual; only the present fan may seed.
        let mut ramp = Some(vec![20, 100]);
        let mut stepper = CurveStepper::new();
        let mut per_fan_steppers: Vec<CurveStepper> = Vec::new();
        let mut ctx = FanCtx {
            state: &state,
            ec: &ec,
            mode: FanControlMode::Manual,
            now_ms: 60_000,
            manual_duty: None,
            curve_hysteresis: None,
            curve_rate_limit: None,
            curve_rate_limit_down: None,
            curve_sensors: Vec::new(),
            consecutive_ec_write_failures: 0,
            // Old write time so the periodic re-assert forces a write.
            last_duty_write_ms: 0,
            last_handover_ms: 0,
            last_manual_duty: None,
            manual_ramp_current: None,
            manual_per_fan_ramp: &mut ramp,
            curve_stepper: &mut stepper,
            per_fan_curve_steppers: &mut per_fan_steppers,
            last_fan_mode: Some(FanControlMode::Manual),
            last_unified_duty: Some(false),
            just_resumed: false,
        };
        assert!(matches!(run(&mut ctx).await, Flow::Done));
        assert_eq!(
            state.fan.last_applied_duty.load(Ordering::Acquire),
            20,
            "docked residual must not leak into the unified seed"
        );
        let ops = mock.ops.lock().unwrap();
        assert!(
            ops.iter().any(|op| op == "set_fan_duty(20,Some(0))"),
            "present fan must be written: {ops:?}"
        );
        assert!(
            ops.iter().all(|op| !op.contains("Some(1)")),
            "docked fan must never be written: {ops:?}"
        );
    }

    fn manual_ctx_with_temp<'a>(
        state: &'a AppState,
        ec: &'a Arc<dyn EcHw>,
        ramp: &'a mut Option<Vec<u32>>,
        stepper: &'a mut CurveStepper,
        per_fan_steppers: &'a mut Vec<CurveStepper>,
        temp_c: i32,
        duty: u32,
    ) -> FanCtx<'a> {
        use crate::cli::ec_wrapper::ThermalData;
        use std::collections::BTreeMap;
        let mut temps = BTreeMap::new();
        temps.insert("CPU".to_string(), temp_c);
        with_write_lock(&state.thermal.data, |guard| {
            *guard = Arc::new(Some(ThermalData {
                temps: Arc::new(temps),
                fans: smallvec::SmallVec::new(),
            }));
        });
        FanCtx {
            state,
            ec,
            mode: FanControlMode::Manual,
            now_ms: 60_000,
            manual_duty: Some(duty),
            curve_hysteresis: None,
            curve_rate_limit: None,
            curve_rate_limit_down: None,
            curve_sensors: Vec::new(),
            consecutive_ec_write_failures: 0,
            last_duty_write_ms: 0,
            last_handover_ms: 0,
            last_manual_duty: None,
            manual_ramp_current: None,
            manual_per_fan_ramp: ramp,
            curve_stepper: stepper,
            per_fan_curve_steppers: per_fan_steppers,
            last_fan_mode: Some(FanControlMode::Manual),
            last_unified_duty: Some(true),
            just_resumed: false,
        }
    }

    fn manual_test_state() -> AppState {
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
        state
            .thermal
            .last_success_ms
            .store(60_000, Ordering::Release);
        state
    }

    #[tokio::test]
    async fn hot_floor_lifts_low_duty_to_90() {
        let _serial = crate::ec_serial::global_dispatch_test_lock().lock().await;
        let state = manual_test_state();
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        let mut ramp = None;
        let mut stepper = CurveStepper::new();
        let mut per_fan_steppers: Vec<CurveStepper> = Vec::new();
        // 30% at 95C is lifted to the 90% floor.
        let mut ctx = manual_ctx_with_temp(
            &state,
            &ec,
            &mut ramp,
            &mut stepper,
            &mut per_fan_steppers,
            95,
            30,
        );
        assert!(matches!(run(&mut ctx).await, Flow::Done));
        let ops = mock.ops.lock().unwrap();
        assert!(
            ops.iter().any(|op| op == "set_fan_duty(90,None)"),
            "hot floor must lift duty to 90: {ops:?}"
        );
        assert!(
            ops.iter().all(|op| op != "autofanctrl"),
            "floor writes duty, never hands over: {ops:?}"
        );
    }

    #[tokio::test]
    async fn hot_floor_never_caps_a_high_duty() {
        let _serial = crate::ec_serial::global_dispatch_test_lock().lock().await;
        let state = manual_test_state();
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        let mut ramp = None;
        let mut stepper = CurveStepper::new();
        let mut per_fan_steppers: Vec<CurveStepper> = Vec::new();
        // 100% at 99C stays 100%: the floor is a minimum, not a ceiling.
        let mut ctx = manual_ctx_with_temp(
            &state,
            &ec,
            &mut ramp,
            &mut stepper,
            &mut per_fan_steppers,
            99,
            100,
        );
        assert!(matches!(run(&mut ctx).await, Flow::Done));
        let ops = mock.ops.lock().unwrap();
        assert!(
            ops.iter().any(|op| op == "set_fan_duty(100,None)"),
            "high duty must be kept: {ops:?}"
        );
    }

    #[tokio::test]
    async fn below_floor_stays_manual_even_at_zero_duty() {
        let _serial = crate::ec_serial::global_dispatch_test_lock().lock().await;
        let state = manual_test_state();
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        let mut ramp = None;
        let mut stepper = CurveStepper::new();
        let mut per_fan_steppers: Vec<CurveStepper> = Vec::new();
        // 0% at 94C is the user's explicit choice: no floor below 95C.
        let mut ctx = manual_ctx_with_temp(
            &state,
            &ec,
            &mut ramp,
            &mut stepper,
            &mut per_fan_steppers,
            94,
            0,
        );
        assert!(matches!(run(&mut ctx).await, Flow::Done));
        let ops = mock.ops.lock().unwrap();
        assert!(
            ops.iter().any(|op| op == "set_fan_duty(0,None)"),
            "manual duty must be written: {ops:?}"
        );
        assert!(
            ops.iter().all(|op| op != "autofanctrl"),
            "no handover below the floor: {ops:?}"
        );
    }
}
