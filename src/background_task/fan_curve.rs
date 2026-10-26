//! Curve fan mode: stepper-driven duty with firmware fail-safe handover.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use super::{
    FAN_REASSERT_INTERVAL_MS, FanCtx, Flow, MAX_EC_WRITE_FAILURES, THERMAL_STALE_MS,
    reset_ec_after_failures, reset_ec_on_panic, shutdown_requested,
};
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
}
