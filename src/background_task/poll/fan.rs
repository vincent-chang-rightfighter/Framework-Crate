//! Fan-mode dispatch for one poll round.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::app::AppState;
use crate::cli;
use crate::fan_control::CurveStepper;
use crate::util::read_lock;

use super::super::{FanCtx, Flow, estimate_duty_from_thermal, fan_curve, fan_disabled, fan_manual};
use super::{Phase, PollState};

/// Fan-mode dispatch: config snapshot, mode-change reseeding, and the
/// mode arm through `FanCtx`. Owns the final clock resample (see below).
pub(in crate::background_task) async fn phase_fan(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    poll: &mut PollState,
) -> Phase {
    // Calibration wizard owns EC writes while active; skip this round's
    // fan drive (thermal/battery phases already ran above).
    if state.fan.calibration_active.load(Ordering::Acquire) {
        return Phase::Continue;
    }
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
        poll.per_fan_curve_steppers.clear();
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
        per_fan_curve_steppers: &mut poll.per_fan_curve_steppers,
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
