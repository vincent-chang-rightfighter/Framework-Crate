//! Disabled fan mode: restores firmware control once when entering the mode.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use super::{
    FanCtx, Flow, MAX_EC_WRITE_FAILURES, reset_ec_after_failures, reset_ec_on_panic,
    shutdown_requested,
};

/// Restores firmware control once when entering Disabled.
pub(super) async fn run(ctx: &mut FanCtx<'_>) -> Flow {
    // Restore firmware control once when entering Disabled.
    if ctx
        .last_fan_mode
        .as_ref()
        .is_none_or(|m| m != &crate::types::FanControlMode::Disabled)
    {
        if shutdown_requested(ctx.state) {
            return Flow::Stop;
        }
        let mut backoff_ec = false;
        let mut restore_failed = false;
        {
            let ec_clone = Arc::clone(ctx.ec);
            match crate::ec_serial::exec(
                "disabled-restore",
                crate::util::EC_IO_TIMEOUT,
                ec_clone,
                move |ec| ec.autofanctrl(),
            )
            .await
            {
                Ok(result) => {
                    if let Err(e) = result {
                        warn!("Failed to restore auto fan control: {}", e);
                        ctx.consecutive_ec_write_failures += 1;
                        if ctx.consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                            reset_ec_after_failures(ctx.state, ctx.consecutive_ec_write_failures);
                            // Drop the EC write lock before backing off
                            // so other writers are not blocked for 3s.
                            backoff_ec = true;
                        }
                        // Do NOT record last_fan_mode below: falling through
                        // to Done would never retry, leaving the fans stuck
                        // at the last manual duty.
                        restore_failed = true;
                    } else {
                        ctx.consecutive_ec_write_failures = 0;
                        ctx.last_duty_write_ms = ctx.now_ms;
                        ctx.state.fan.last_applied_duty.store(0, Ordering::Release);
                    }
                }
                Err(join_err) => {
                    warn!("EC spawn panicked (autofanctrl): {}", join_err);
                    reset_ec_on_panic(ctx.state);
                    return Flow::Next;
                }
            }
        }
        if backoff_ec {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            return Flow::Next;
        }
        if restore_failed {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            return Flow::Next;
        }
        let mode_now =
            crate::types::FanControlMode::from_u8(ctx.state.fan.mode.load(Ordering::Acquire) as u8);
        if mode_now != ctx.mode {
            ctx.last_fan_mode = Some(ctx.mode);
            return Flow::Next;
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
    use crate::cli::ec_wrapper::tests::MockEc;
    use crate::fan_control::CurveStepper;
    use crate::types::FanControlMode;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn firmware_handover_clears_the_last_manual_duty() {
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
            .store(FanControlMode::Disabled.to_u8() as u64, Ordering::Release);
        state.fan.last_applied_duty.store(42, Ordering::Release);
        let mock = Arc::new(MockEc::new());
        let ec: Arc<dyn EcHw> = mock.clone();
        let mut ramp = None;
        let mut stepper = CurveStepper::new();
        let mut ctx = FanCtx {
            state: &state,
            ec: &ec,
            mode: FanControlMode::Disabled,
            now_ms: 1_000,
            manual_duty: None,
            curve_hysteresis: None,
            curve_rate_limit: None,
            curve_rate_limit_down: None,
            curve_sensors: Vec::new(),
            consecutive_ec_write_failures: 0,
            last_duty_write_ms: 0,
            last_curve_failover_ms: 0,
            last_manual_duty: Some(42),
            manual_ramp_current: Some(42),
            manual_per_fan_ramp: &mut ramp,
            curve_stepper: &mut stepper,
            last_fan_mode: Some(FanControlMode::Manual),
            last_unified_duty: None,
            just_resumed: false,
        };
        assert!(matches!(run(&mut ctx).await, Flow::Done));
        assert_eq!(state.fan.last_applied_duty.load(Ordering::Acquire), 0);
        assert_eq!(*mock.ops.lock().unwrap(), vec!["autofanctrl".to_string()]);
    }
}
