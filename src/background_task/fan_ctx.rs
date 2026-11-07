//! Per-iteration fan context and firmware handover shared by the mode arms.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::warn;

use crate::app::AppState;
use crate::cli;
use crate::fan_control::CurveStepper;

use super::{
    CURVE_TEMP_FAILOVER_MS, Flow, MAX_EC_WRITE_FAILURES, reset_ec_after_failures,
    reset_ec_on_panic, shutdown_requested,
};

/// Full warning for a persistent handover condition is emitted at most this
/// often; throttled repeats go to debug so a stale sensor cannot fill the log.
const HANDOVER_WARN_INTERVAL_MS: u64 = 60_000;
static LAST_HANDOVER_WARN_MS: AtomicU64 = AtomicU64::new(0);

/// Per-iteration fan state shared with the mode submodules.
///
/// Plain scalars are owned copies, written back by the parent after the
/// call; the stepper and per-fan ramp stay borrowed so no state is lost.
pub(super) struct FanCtx<'a> {
    pub state: &'a AppState,
    pub ec: &'a Arc<dyn cli::EcHw>,
    pub mode: crate::types::FanControlMode,
    pub now_ms: u64,
    pub manual_duty: Option<u32>,
    pub curve_hysteresis: Option<u32>,
    pub curve_rate_limit: Option<u32>,
    pub curve_rate_limit_down: Option<u32>,
    pub curve_sensors: Vec<String>,
    // NOTE: no read-failure counter here on purpose: the background loop
    // owns thermal read accounting, so mode arms must not double-count one
    // bad poll. Write failures use consecutive_ec_write_failures below.
    pub consecutive_ec_write_failures: u32,
    pub last_duty_write_ms: u64,
    pub last_handover_ms: u64,
    pub last_manual_duty: Option<u32>,
    pub manual_ramp_current: Option<u32>,
    pub manual_per_fan_ramp: &'a mut Option<Vec<u32>>,
    pub curve_stepper: &'a mut CurveStepper,
    /// Per-fan steppers for non-unified curves; the arm grows (never
    /// truncates) this as fans appear, mirroring `manual_per_fan_ramp`.
    pub per_fan_curve_steppers: &'a mut Vec<CurveStepper>,
    pub last_fan_mode: Option<crate::types::FanControlMode>,
    pub last_unified_duty: Option<bool>,
    pub just_resumed: bool,
}

impl FanCtx<'_> {
    /// Hands fan control back to firmware with shared throttling and
    /// write-failure accounting. Every failover path (stale thermal, missing
    /// config, no usable temperature, manual high-temp guard) goes through
    /// this so a persistent condition cannot spam `autofanctrl` every poll,
    /// and a successful handover clears the consecutive-write-failure run.
    pub(super) async fn handover_to_firmware(&mut self, op: &'static str, reason: &str) -> Flow {
        if shutdown_requested(self.state) {
            return Flow::Stop;
        }
        if self.now_ms.saturating_sub(self.last_handover_ms) < CURVE_TEMP_FAILOVER_MS {
            return Flow::Next;
        }
        // Stamp the attempt before the write so failures are throttled too.
        self.last_handover_ms = self.now_ms;
        let ec_clone = Arc::clone(self.ec);
        match crate::ec_serial::exec(op, crate::util::EC_IO_TIMEOUT, ec_clone, move |ec| {
            ec.autofanctrl()
        })
        .await
        {
            Ok(Ok(())) => {
                self.consecutive_ec_write_failures = 0;
                let last = LAST_HANDOVER_WARN_MS.load(Ordering::Relaxed);
                if self.now_ms.saturating_sub(last) >= HANDOVER_WARN_INTERVAL_MS {
                    LAST_HANDOVER_WARN_MS.store(self.now_ms, Ordering::Relaxed);
                    warn!("{}", reason);
                } else {
                    tracing::debug!("{}", reason);
                }
            }
            Ok(Err(e)) => {
                warn!("{} failed: {}", op, e);
                self.consecutive_ec_write_failures += 1;
                if self.consecutive_ec_write_failures >= MAX_EC_WRITE_FAILURES {
                    reset_ec_after_failures(self.state, self.consecutive_ec_write_failures);
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
            Err(join_err) => {
                warn!("EC spawn panicked ({}): {}", op, join_err);
                reset_ec_on_panic(self.state);
            }
        }
        Flow::Next
    }
}
