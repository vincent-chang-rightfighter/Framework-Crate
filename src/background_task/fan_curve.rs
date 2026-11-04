//! Curve fan mode: stepper-driven duty with firmware fail-safe handover.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use super::{
    FAN_REASSERT_INTERVAL_MS, FanCtx, Flow, MAX_EC_WRITE_FAILURES, THERMAL_STALE_MS,
    reset_ec_after_failures, reset_ec_on_panic, shutdown_requested, unified_transition_resets,
};
use crate::fan_control::CurveStepper;
use crate::util::read_lock;

/// Drives Curve mode from the thermal control temperature, handing back to
/// firmware whenever readings or config are missing.
pub(super) async fn run(ctx: &mut FanCtx<'_>) -> Flow {
    // Staleness check before using cached thermal.
    let last_ok = ctx.state.thermal.last_success_ms.load(Ordering::Acquire);
    let is_stale = last_ok == 0 || ctx.now_ms.saturating_sub(last_ok) > THERMAL_STALE_MS;
    if is_stale {
        // Read failures are counted by the background loop that performs
        // the thermal read; this arm only hands over. The shared helper
        // throttles repeated handovers and resets the write-failure run.
        let reason = format!(
            "Curve mode: stale thermal ({}ms), handed fan control to firmware",
            ctx.now_ms.saturating_sub(last_ok)
        );
        return ctx
            .handover_to_firmware("curve-stale-handover", &reason)
            .await;
    }
    // Snapshot the sample and drop the lock: holding it across the EC
    // awaits below blocks the thermal writer. ThermalData clones cheap
    // (temps is already an Arc).
    let thermal = read_lock(&ctx.state.thermal.data).as_ref().clone();
    let Some(ref thermal) = thermal else {
        return ctx
            .handover_to_firmware(
                "curve-missing-sample-handover",
                "Curve mode: no thermal sample, handed fan control to firmware",
            )
            .await;
    };
    // avoid unwrap panic if curve config is corrupted/missing
    let Some(hyst) = ctx.curve_hysteresis else {
        return ctx
            .handover_to_firmware(
                "curve-missing-hysteresis-handover",
                "Curve mode with missing hysteresis, handed back to firmware",
            )
            .await;
    };
    let Some(rate) = ctx.curve_rate_limit else {
        return ctx
            .handover_to_firmware(
                "curve-missing-rate-handover",
                "Curve mode with missing rate_limit, handed back to firmware",
            )
            .await;
    };
    // Fail-safe: no trustworthy temperature means the curve cannot safely drive
    // the fans; hand back to firmware immediately.
    let Some(control_temp) = crate::types::curve_control_temp(&thermal.temps, &ctx.curve_sensors)
    else {
        let reason = if thermal.temps.is_empty() {
            "EC temps read failed"
        } else {
            "no non-battery temperature available"
        };
        return ctx
            .handover_to_firmware(
                "curve-invalid-temp-handover",
                &format!("Curve mode: {}, handed fan control to firmware", reason),
            )
            .await;
    };
    // Snapshot the points and drop the lock before any EC await below.
    let full_pts: Vec<[u32; 2]> = read_lock(&ctx.state.fan.curve_full_points).as_ref().clone();
    // Non-unified curves on multi-fan machines: one stepper per fan, each
    // following its own points. Falls back to the unified path when the
    // per-fan table is undersized (a fan appeared this same round, before
    // the seeder caught up).
    let fan_count = ctx.state.fan.fan_count.load(Ordering::Acquire) as usize;
    let unified_now = ctx.state.fan.unified_duty.load(Ordering::Acquire);
    if unified_transition_resets(ctx.last_unified_duty, unified_now) {
        ctx.last_unified_duty = Some(unified_now);
        // Re-seed from the live duty instead of clearing: a bare reset()
        // drops to None and the next `next()` jumps straight to target with
        // no rate-limit (Manual ramps the same transition via
        // last_applied_duty/estimate). Seeding keeps the toggle bumpless.
        let seed = ctx.state.fan.last_applied_duty.load(Ordering::Acquire) as u32;
        *ctx.curve_stepper = CurveStepper::with_last_duty(seed);
        ctx.per_fan_curve_steppers.clear();
        ctx.last_duty_write_ms = 0;
    }
    let per_fan_full: Vec<Vec<[u32; 2]>> = read_lock(&ctx.state.fan.per_fan_curve_full_points)
        .as_ref()
        .clone();
    if !unified_now && fan_count > 1 && per_fan_full.len() >= fan_count {
        return run_per_fan(ctx, control_temp, hyst, rate, &per_fan_full, fan_count).await;
    }
    let mut next = ctx.curve_stepper.next(
        control_temp,
        hyst,
        rate,
        ctx.curve_rate_limit_down,
        &full_pts,
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
                let ec_clone = Arc::clone(ctx.ec);
                match crate::ec_serial::exec(
                    "curve-set-per-fan-duty",
                    crate::util::EC_IO_TIMEOUT,
                    ec_clone,
                    move |ec| ec.set_fan_duty(next, Some(fan_idx)),
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
                    Err(e) if e.is_overloaded() => {
                        // Backpressure, not failure: skip the rest of this
                        // round like the old lock-contention path did.
                        return Flow::Next;
                    }
                    Err(e) => {
                        warn!(
                            "EC write failed (set_fan_duty curve fan {}): {}",
                            fan_idx, e
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
            let ec_clone = Arc::clone(ctx.ec);
            match crate::ec_serial::exec(
                "curve-set-fan-duty",
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
                Err(e) if e.is_overloaded() => {
                    return Flow::Next;
                }
                Err(e) => {
                    warn!("EC write failed (set_fan_duty curve): {}", e);
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

/// Drives one curve per fan. Mirrors the unified write-each skeleton
/// (shutdown/mode re-checks, failure accounting, 30s re-assert) but steps
/// each fan through its own points with its own stepper.
async fn run_per_fan(
    ctx: &mut FanCtx<'_>,
    control_temp: i32,
    hyst: u32,
    rate: u32,
    per_fan_full: &[Vec<[u32; 2]>],
    fan_count: usize,
) -> Flow {
    // Grow steppers for new fans; never truncate (undock keeps state).
    // Seed from live duties so a new fan ramps instead of jumping: prefer
    // the max of already-running per-fan steppers (unified stepper goes
    // stale in per-fan mode), then the unified stepper, then last applied.
    if ctx.per_fan_curve_steppers.len() < fan_count {
        let per_fan_max = ctx
            .per_fan_curve_steppers
            .iter()
            .filter_map(|s| s.current_duty())
            .max();
        let seed = per_fan_max
            .or_else(|| ctx.curve_stepper.current_duty())
            .or_else(|| {
                let last = ctx.state.fan.last_applied_duty.load(Ordering::Acquire) as u32;
                (last > 0).then_some(last)
            })
            .unwrap_or(0);
        ctx.per_fan_curve_steppers
            .resize_with(fan_count, || CurveStepper::with_last_duty(seed));
    }
    let reassert_all =
        ctx.now_ms.saturating_sub(ctx.last_duty_write_ms) >= FAN_REASSERT_INTERVAL_MS;
    let mut targets: Vec<Option<u32>> = Vec::with_capacity(fan_count);
    for full in per_fan_full.iter().take(fan_count) {
        let idx = targets.len();
        let mut next = ctx.per_fan_curve_steppers[idx].next(
            control_temp,
            hyst,
            rate,
            ctx.curve_rate_limit_down,
            full,
        );
        if next.is_none() && reassert_all {
            next = ctx.per_fan_curve_steppers[idx].current_duty();
        }
        targets.push(next);
    }
    // Converged everywhere with no re-assert due: nothing to write and
    // nothing to count (mirrors the unified `if let Some(next)` guard).
    if targets.iter().all(|t| t.is_none()) {
        return Flow::Done;
    }
    let mut any_succeeded = false;
    let mut failed_fans: Vec<u32> = Vec::new();
    let mut max_duty: Option<u32> = None;
    for (fan_idx, next) in targets.into_iter().enumerate() {
        let Some(next) = next else { continue };
        if shutdown_requested(ctx.state) {
            return Flow::Stop;
        }
        let mode_check =
            crate::types::FanControlMode::from_u8(ctx.state.fan.mode.load(Ordering::Acquire) as u8);
        if mode_check != ctx.mode {
            break;
        }
        let ec_clone = Arc::clone(ctx.ec);
        let idx_u32 = fan_idx as u32;
        match crate::ec_serial::exec(
            "curve-set-per-fan-duty",
            crate::util::EC_IO_TIMEOUT,
            ec_clone,
            move |ec| ec.set_fan_duty(next, Some(idx_u32)),
        )
        .await
        {
            Ok(result) => {
                if let Err(e) = result {
                    // Transient per-fan failure (e.g. dock unplugged); only all-fail counts toward reset.
                    failed_fans.push(idx_u32);
                    tracing::debug!("Failed to set fan {} duty (curve): {}", idx_u32, e);
                } else {
                    any_succeeded = true;
                    ctx.last_duty_write_ms = ctx.now_ms;
                    ctx.per_fan_curve_steppers[fan_idx].note_applied(next);
                    max_duty = Some(max_duty.map_or(next, |m: u32| m.max(next)));
                }
            }
            Err(e) if e.is_overloaded() => {
                // Backpressure, not failure: skip the rest of this
                // round like the old lock-contention path did.
                return Flow::Next;
            }
            Err(e) => {
                warn!(
                    "EC write failed (set_fan_duty curve fan {}): {}",
                    idx_u32, e
                );
                reset_ec_on_panic(ctx.state);
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
            "Failed to set fan duties (curve per-fan) for fans {:?}",
            failed_fans,
        );
    }
    // Single representative for seeding/restart ramps; per-fan truth lives
    // in the steppers, which outlive any single round.
    if let Some(max) = max_duty {
        ctx.state
            .fan
            .last_applied_duty
            .store(max as u64, Ordering::Release);
    }
    Flow::Done
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::app::AppState;
    use crate::background_task::{FanCtx, Flow};
    use crate::cli::EcHw;
    use crate::cli::ec_wrapper::ThermalData;
    use crate::cli::ec_wrapper::tests::MockEc;
    use crate::fan_control::CurveStepper;
    use crate::types::FanControlMode;
    use crate::util::with_write_lock;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn battery_only_readings_hand_back_to_firmware() {
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
            .store(FanControlMode::Curve.to_u8() as u64, Ordering::Release);
        state
            .thermal
            .last_success_ms
            .store(60_000, Ordering::Release);
        let mut temps = BTreeMap::new();
        temps.insert("Battery".to_string(), 30);
        with_write_lock(&state.thermal.data, |guard| {
            *guard = Arc::new(Some(ThermalData {
                temps: Arc::new(temps),
                fans: smallvec::SmallVec::new(),
            }));
        });
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        let mut ramp = None;
        let mut stepper = CurveStepper::new();
        let mut per_fan_steppers: Vec<CurveStepper> = Vec::new();
        let mut ctx = FanCtx {
            state: &state,
            ec: &ec,
            mode: FanControlMode::Curve,
            now_ms: 60_000,
            manual_duty: None,
            curve_hysteresis: Some(2),
            curve_rate_limit: Some(10),
            curve_rate_limit_down: None,
            curve_sensors: Vec::new(),
            // A success must clear a prior (possibly scattered) failure run.
            consecutive_ec_write_failures: 4,
            last_duty_write_ms: 0,
            last_handover_ms: 0,
            last_manual_duty: None,
            manual_ramp_current: None,
            manual_per_fan_ramp: &mut ramp,
            curve_stepper: &mut stepper,
            per_fan_curve_steppers: &mut per_fan_steppers,
            last_fan_mode: Some(FanControlMode::Curve),
            last_unified_duty: None,
            just_resumed: false,
        };
        assert!(matches!(run(&mut ctx).await, Flow::Next));
        assert_eq!(ctx.consecutive_ec_write_failures, 0);
        assert_eq!(*mock.ops.lock().unwrap(), vec!["autofanctrl".to_string()]);
    }

    #[tokio::test]
    async fn failover_is_throttled_within_one_second() {
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
            .store(FanControlMode::Curve.to_u8() as u64, Ordering::Release);
        state
            .thermal
            .last_success_ms
            .store(60_000, Ordering::Release);
        let mut temps = BTreeMap::new();
        temps.insert("Battery".to_string(), 30);
        with_write_lock(&state.thermal.data, |guard| {
            *guard = Arc::new(Some(ThermalData {
                temps: Arc::new(temps),
                fans: smallvec::SmallVec::new(),
            }));
        });
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        let mut ramp = None;
        let mut stepper = CurveStepper::new();
        let mut per_fan_steppers: Vec<CurveStepper> = Vec::new();
        let mut ctx = FanCtx {
            state: &state,
            ec: &ec,
            mode: FanControlMode::Curve,
            now_ms: 60_000,
            manual_duty: None,
            curve_hysteresis: Some(2),
            curve_rate_limit: Some(10),
            curve_rate_limit_down: None,
            curve_sensors: Vec::new(),
            consecutive_ec_write_failures: 0,
            last_duty_write_ms: 0,
            // 500ms ago: inside the one-second handover throttle window.
            last_handover_ms: 59_500,
            last_manual_duty: None,
            manual_ramp_current: None,
            manual_per_fan_ramp: &mut ramp,
            curve_stepper: &mut stepper,
            per_fan_curve_steppers: &mut per_fan_steppers,
            last_fan_mode: Some(FanControlMode::Curve),
            last_unified_duty: None,
            just_resumed: false,
        };
        assert!(matches!(run(&mut ctx).await, Flow::Next));
        assert!(
            mock.ops.lock().unwrap().is_empty(),
            "handover must be throttled within one second"
        );
    }

    #[tokio::test]
    async fn per_fan_curves_drive_each_fan_from_its_own_points() {
        use crate::cli::ec_wrapper::FanReading;
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
            .store(FanControlMode::Curve.to_u8() as u64, Ordering::Release);
        state.fan.unified_duty.store(false, Ordering::Release);
        state.fan.fan_count.store(2, Ordering::Release);
        state
            .thermal
            .last_success_ms
            .store(60_000, Ordering::Release);
        let mut temps = BTreeMap::new();
        temps.insert("CPU".to_string(), 60);
        with_write_lock(&state.thermal.data, |guard| {
            *guard = Arc::new(Some(ThermalData {
                temps: Arc::new(temps),
                fans: smallvec::smallvec![
                    FanReading {
                        name: "Fan 1".to_string(),
                        rpm: 2000
                    },
                    FanReading {
                        name: "Fan 2".to_string(),
                        rpm: 2000
                    },
                ],
            }));
        });
        // Same 60C control temp, different per-fan curves.
        with_write_lock(&state.fan.per_fan_curve_full_points, |guard| {
            *guard = Arc::new(vec![
                vec![[0, 0], [60, 20], [100, 100], [110, 100]],
                vec![[0, 0], [60, 80], [100, 100], [110, 100]],
            ]);
        });
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        let mut ramp = None;
        let mut stepper = CurveStepper::new();
        // Pre-seeded near each fan's target so one round lands on two
        // distinct duties (a cold start would rate-limit both from 0).
        let mut per_fan_steppers = vec![
            CurveStepper::with_last_duty(15),
            CurveStepper::with_last_duty(75),
        ];
        let mut ctx = FanCtx {
            state: &state,
            ec: &ec,
            mode: FanControlMode::Curve,
            now_ms: 60_000,
            manual_duty: None,
            curve_hysteresis: Some(2),
            curve_rate_limit: Some(10),
            curve_rate_limit_down: None,
            curve_sensors: vec!["CPU".to_string()],
            consecutive_ec_write_failures: 0,
            last_duty_write_ms: 0,
            last_handover_ms: 0,
            last_manual_duty: None,
            manual_ramp_current: None,
            manual_per_fan_ramp: &mut ramp,
            curve_stepper: &mut stepper,
            per_fan_curve_steppers: &mut per_fan_steppers,
            last_fan_mode: Some(FanControlMode::Curve),
            last_unified_duty: Some(false),
            just_resumed: false,
        };
        assert!(matches!(run(&mut ctx).await, Flow::Done));
        let ops = mock.ops.lock().unwrap();
        assert!(
            ops.iter().any(|op| op == "set_fan_duty(20,Some(0))"),
            "fan 1 must follow its own curve: {ops:?}"
        );
        assert!(
            ops.iter().any(|op| op == "set_fan_duty(80,Some(1))"),
            "fan 2 must follow its own curve: {ops:?}"
        );
        assert_eq!(ctx.per_fan_curve_steppers.len(), 2);
    }

    #[tokio::test]
    async fn unified_toggle_seeds_instead_of_jumping() {
        use crate::cli::ec_wrapper::FanReading;
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
            .store(FanControlMode::Curve.to_u8() as u64, Ordering::Release);
        // Toggle per-fan -> unified: the stepper must ramp from live duty.
        state.fan.unified_duty.store(true, Ordering::Release);
        state.fan.fan_count.store(1, Ordering::Release);
        state.fan.last_applied_duty.store(50, Ordering::Release);
        state
            .thermal
            .last_success_ms
            .store(60_000, Ordering::Release);
        let mut temps = BTreeMap::new();
        temps.insert("CPU".to_string(), 60);
        with_write_lock(&state.thermal.data, |guard| {
            *guard = Arc::new(Some(ThermalData {
                temps: Arc::new(temps),
                fans: smallvec::smallvec![FanReading {
                    name: "Fan 1".to_string(),
                    rpm: 2000
                },],
            }));
        });
        with_write_lock(&state.fan.curve_full_points, |guard| {
            *guard = Arc::new(vec![[0, 0], [60, 80], [100, 100], [110, 100]]);
        });
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        let mut ramp = None;
        let mut stepper = CurveStepper::new();
        let mut per_fan_steppers: Vec<CurveStepper> = Vec::new();
        let mut ctx = FanCtx {
            state: &state,
            ec: &ec,
            mode: FanControlMode::Curve,
            now_ms: 60_000,
            manual_duty: None,
            curve_hysteresis: Some(2),
            curve_rate_limit: Some(10),
            curve_rate_limit_down: None,
            curve_sensors: vec!["CPU".to_string()],
            consecutive_ec_write_failures: 0,
            last_duty_write_ms: 0,
            last_handover_ms: 0,
            last_manual_duty: None,
            manual_ramp_current: None,
            manual_per_fan_ramp: &mut ramp,
            curve_stepper: &mut stepper,
            per_fan_curve_steppers: &mut per_fan_steppers,
            last_fan_mode: Some(FanControlMode::Curve),
            last_unified_duty: Some(false),
            just_resumed: false,
        };
        assert!(matches!(run(&mut ctx).await, Flow::Done));
        let ops = mock.ops.lock().unwrap();
        assert!(
            ops.iter().any(|op| op == "set_fan_duty(60,None)"),
            "toggle must ramp 50->60, not jump to 80: {ops:?}"
        );
        assert!(
            ops.iter().all(|op| op != "set_fan_duty(80,None)"),
            "no jump on unified toggle: {ops:?}"
        );
    }
}
