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
        {
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
                    } else {
                        ctx.consecutive_ec_write_failures = 0;
                        ctx.last_duty_write_ms = ctx.now_ms;
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
        let mode_now =
            crate::types::FanControlMode::from_u8(ctx.state.fan.mode.load(Ordering::Acquire) as u8);
        if mode_now != ctx.mode {
            ctx.last_fan_mode = Some(ctx.mode);
            return Flow::Next;
        }
    }
    Flow::Done
}
