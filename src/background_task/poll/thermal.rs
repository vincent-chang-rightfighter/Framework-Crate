//! Thermal read phase and the EC hello tiebreak.

use std::sync::Arc;
use tracing::warn;

use crate::app::AppState;
use crate::cli;

use super::super::{
    MAX_EC_IO_FAILURES, mark_view_dirty, record_thermal_sample, reset_ec_after_failures,
};
use super::{Phase, PollState};

/// Thermal read with the hello tiebreak unified across all failure kinds:
/// empty readings, command errors, and dispatch failures all count alike,
/// and the threshold pings the EC once instead of resetting blindly.
pub(in crate::background_task) async fn phase_thermal(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    poll: &mut PollState,
) -> Phase {
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec("thermal", crate::util::EC_IO_TIMEOUT, ec_clone, move |ec| {
        ec.thermal()
    })
    .await
    {
        Ok(Ok(t)) => {
            if t.temps.is_empty() {
                warn!("Thermal read returned empty temps");
                poll.consecutive_ec_failures += 1;
            } else {
                poll.consecutive_ec_failures = 0;
                if record_thermal_sample(state, t) {
                    mark_view_dirty(state);
                }
            }
        }
        Ok(Err(e)) => {
            warn!("Thermal read failed: {}", e);
            poll.consecutive_ec_failures += 1;
        }
        Err(e) if e.is_overloaded() => {
            // Backpressure, not failure: fall through to fan
            // modes below instead of skipping the whole round.
            return Phase::Continue;
        }
        Err(e) => {
            warn!("EC thermal spawn failed: {}", e);
            poll.consecutive_ec_failures += 1;
        }
    }
    if poll.consecutive_ec_failures >= MAX_EC_IO_FAILURES {
        match thermal_failure_verdict(state, ec, poll.consecutive_ec_failures).await {
            ThermalVerdict::Reset => {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                return Phase::NextRound;
            }
            // EC proven alive: count fresh so the next
            // ping is another full run away instead of
            // every round. Falls through so battery/PD
            // stay fresh and the fan arms hand over on
            // staleness by themselves.
            ThermalVerdict::RideThrough => {
                poll.consecutive_ec_failures = 0;
            }
        }
    }
    Phase::Continue
}

/// Outcome of the hello tiebreak when thermal reads keep failing.
#[derive(Debug, PartialEq, Eq)]
enum ThermalVerdict {
    /// EC answers: keep the client and ride through. The fan arms hand
    /// control to firmware on staleness by themselves, so no reset, no
    /// backoff sleep, and the rest of the round still runs.
    RideThrough,
    /// EC silent: full client reset with backoff; the caller skips the round.
    Reset,
}

/// Pings the EC once thermal failures hit the threshold. A ~1ms hello
/// separates a dead EC (reset the client) from a flaky thermal path
/// (keep the client: tearing it down would also wipe fan state and stall
/// control for 3s for no reason).
async fn thermal_failure_verdict(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    consecutive_failures: u32,
) -> ThermalVerdict {
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec(
        "ec-hello",
        crate::util::EC_IO_TIMEOUT,
        ec_clone,
        move |ec| ec.hello(),
    )
    .await
    {
        Ok(Ok(())) => {
            warn!(
                "Thermal read failed {} times but EC answers hello; keeping client (no reset)",
                consecutive_failures
            );
            ThermalVerdict::RideThrough
        }
        // Dispatcher saturated: backpressure, not proof of death. Same
        // rule as everywhere else: overload never counts as failure.
        Err(e) if e.is_overloaded() => ThermalVerdict::RideThrough,
        Ok(Err(e)) => {
            warn!(
                "Thermal read failed {} times and EC hello failed ({}); resetting client",
                consecutive_failures, e
            );
            reset_ec_after_failures(state, consecutive_failures);
            ThermalVerdict::Reset
        }
        Err(e) => {
            warn!(
                "Thermal read failed {} times and EC hello dispatch failed ({}); resetting client",
                consecutive_failures, e
            );
            reset_ec_after_failures(state, consecutive_failures);
            ThermalVerdict::Reset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{read_lock, with_write_lock};
    use std::sync::atomic::Ordering;

    fn live_client_state(ec: Arc<dyn crate::cli::EcHw>) -> AppState {
        let state = AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        with_write_lock(&state.system.ec_client, |guard| {
            *guard = Arc::new(Some(ec));
        });
        state.system.cli_available.store(true, Ordering::Release);
        state
    }

    #[tokio::test]
    async fn thermal_verdict_rides_through_when_ec_alive() {
        use crate::cli::ec_wrapper::tests::MockEc;
        let _serial = crate::ec_serial::global_dispatch_test_lock().lock().await;
        let mock = Arc::new(MockEc::new());
        let dyn_ec: Arc<dyn crate::cli::EcHw> = mock.clone();
        let state = live_client_state(dyn_ec.clone());
        let verdict = thermal_failure_verdict(&state, &dyn_ec, 5).await;
        assert_eq!(verdict, ThermalVerdict::RideThrough);
        // Client untouched: no reset happened.
        assert!(state.system.cli_available.load(Ordering::Acquire));
        assert!(
            read_lock(&state.system.ec_client)
                .as_ref()
                .as_ref()
                .is_some()
        );
        assert!(mock.ops.lock().unwrap().contains(&"hello".to_string()));
    }

    #[tokio::test]
    async fn thermal_verdict_resets_when_ec_silent() {
        use crate::cli::ec_wrapper::tests::MockEc;
        let _serial = crate::ec_serial::global_dispatch_test_lock().lock().await;
        let mock = Arc::new(MockEc::new());
        mock.fail_hello.store(true, Ordering::SeqCst);
        let dyn_ec: Arc<dyn crate::cli::EcHw> = mock.clone();
        let state = live_client_state(dyn_ec.clone());
        let verdict = thermal_failure_verdict(&state, &dyn_ec, 5).await;
        assert_eq!(verdict, ThermalVerdict::Reset);
        // reset_ec_after_failures drops the client and flags a fan reset.
        assert!(!state.system.cli_available.load(Ordering::Acquire));
        assert!(
            read_lock(&state.system.ec_client)
                .as_ref()
                .as_ref()
                .is_none()
        );
        assert!(state.lifecycle.fan_reset_pending.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn phase_thermal_routes_repeated_command_errors_through_verdict() {
        use crate::cli::ec_wrapper::tests::MockEc;
        use crate::types::FanControlMode;
        let _serial = crate::ec_serial::global_dispatch_test_lock().lock().await;
        let mock = Arc::new(MockEc::new());
        mock.fail_thermal.store(true, Ordering::SeqCst);
        let dyn_ec: Arc<dyn crate::cli::EcHw> = mock.clone();
        let state = live_client_state(dyn_ec.clone());
        let mut poll = PollState::new(&state, 0, FanControlMode::Disabled, false, 1_000);
        // Below threshold: counted, no hello ping yet.
        for _ in 0..MAX_EC_IO_FAILURES - 1 {
            assert!(matches!(
                phase_thermal(&state, &dyn_ec, &mut poll).await,
                Phase::Continue
            ));
        }
        assert_eq!(poll.consecutive_ec_failures, MAX_EC_IO_FAILURES - 1);
        assert!(
            !mock.ops.lock().unwrap().contains(&"hello".to_string()),
            "no verdict ping below threshold"
        );
        // At threshold the unified verdict pings hello (alive here):
        // RideThrough resets the count and keeps the client.
        assert!(matches!(
            phase_thermal(&state, &dyn_ec, &mut poll).await,
            Phase::Continue
        ));
        assert_eq!(poll.consecutive_ec_failures, 0);
        assert!(state.system.cli_available.load(Ordering::Acquire));
        assert!(mock.ops.lock().unwrap().contains(&"hello".to_string()));
    }
}
