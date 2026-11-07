//! Poll-round state and phase orchestration.
//!
//! `PollState` holds the cross-iteration locals; the phases live in sibling
//! modules (EC, thermal, lifecycle, scans, CPU power, fan) so each concern
//! stays independently testable.

use crate::app::AppState;
use crate::fan_control::CurveStepper;

use super::estimate_duty_from_thermal;

mod cpu_power;
mod ec;
mod fan;
mod lifecycle;
mod scans;
mod thermal;

pub(in crate::background_task) use cpu_power::phase_cpu_power;
pub(in crate::background_task) use ec::phase_ensure_ec;
pub(in crate::background_task) use fan::phase_fan;
pub(in crate::background_task) use lifecycle::{phase_battery, phase_resume};
pub(in crate::background_task) use scans::{phase_firmware, phase_scans};
pub(in crate::background_task) use thermal::phase_thermal;

/// Per-poll mutable state for the background loop (P2). Collects the ~23
/// cross-iteration locals that used to sprawl across the poll body so phases
/// can take `&mut PollState` instead of a dozen parameters. Owned by the
/// inner task; each phase borrows it for one call, never across awaits that
/// outlive the iteration.
pub(super) struct PollState {
    last_fan_mode: Option<crate::types::FanControlMode>,
    last_manual_duty: Option<u32>,
    last_unified_duty: Option<bool>,
    manual_ramp_current: Option<u32>,
    manual_per_fan_ramp: Option<Vec<u32>>,
    curve_stepper: CurveStepper,
    /// One stepper per fan for non-unified curves; grown (never truncated)
    /// as fans appear, mirroring `manual_per_fan_ramp`.
    per_fan_curve_steppers: Vec<CurveStepper>,
    just_resumed: bool,
    consecutive_ec_failures: u32,
    consecutive_battery_failures: u32,
    consecutive_ec_write_failures: u32,
    last_duty_write_ms: u64,
    last_pd_success_ms: u64,
    last_handover_ms: u64,
    last_expansion_scan: u64,
    last_versions_scan: u64,
    versions_scan_attempts: u32,
    last_pd_fw_scan: u64,
    pd_fw_scan_attempts: u32,
    last_smart_scan: u64,
    smart_scan_attempts: u32,
    last_cpu_power_poll: u64,
    last_resume_ts: u64,
    last_wall_ms: u64,
}

impl PollState {
    pub(super) fn new(
        state: &AppState,
        saved_duty: u32,
        init_fan_mode: crate::types::FanControlMode,
        init_is_manual: bool,
        start_ms: u64,
    ) -> Self {
        Self {
            // Seed Disabled as None to force firmware restore on first iteration.
            last_fan_mode: (init_fan_mode != crate::types::FanControlMode::Disabled)
                .then_some(init_fan_mode),
            last_manual_duty: None,
            last_unified_duty: None,
            manual_ramp_current: if saved_duty > 0 {
                Some(saved_duty)
            } else if init_is_manual {
                estimate_duty_from_thermal(state)
            } else {
                None
            },
            manual_per_fan_ramp: None,
            curve_stepper: if saved_duty > 0 {
                CurveStepper::with_last_duty(saved_duty)
            } else {
                CurveStepper::new()
            },
            per_fan_curve_steppers: Vec::new(),
            just_resumed: false,
            consecutive_ec_failures: 0,
            consecutive_battery_failures: 0,
            consecutive_ec_write_failures: 0,
            last_duty_write_ms: 0,
            last_pd_success_ms: 0,
            last_handover_ms: 0,
            last_expansion_scan: start_ms,
            // 0 is the sentinel for "never attempted"; the first poll
            // should try immediately when versions are still missing.
            last_versions_scan: 0,
            versions_scan_attempts: 0,
            last_pd_fw_scan: 0,
            pd_fw_scan_attempts: 0,
            last_smart_scan: 0,
            smart_scan_attempts: 0,
            last_cpu_power_poll: 0,
            last_resume_ts: 0,
            last_wall_ms: crate::util::current_time_ms(),
        }
    }

    /// Clears fan-controller state after resume or EC reset. The caller owns
    /// the parts that differ (EC teardown, rpm bookkeeping, logging,
    /// `just_resumed`).
    pub(super) fn reset_fan_state(&mut self) {
        self.curve_stepper.reset();
        self.per_fan_curve_steppers.clear();
        self.last_manual_duty = None;
        self.last_unified_duty = None;
        self.manual_ramp_current = None;
        self.manual_per_fan_ramp = None;
        self.last_fan_mode = None;
        self.last_duty_write_ms = 0;
        self.last_handover_ms = 0;
    }
}

/// How the poll loop proceeds after a phase.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// Run the next phase.
    Continue,
    /// Skip the rest of the round.
    NextRound,
    /// Shutdown was requested.
    Stop,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_state_reset_fan_state_clears_ramps() {
        use crate::types::FanControlMode;
        let state = AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        let mut poll = PollState::new(&state, 70, FanControlMode::Manual, true, 1_000);
        // Seeded from the saved duty, like the loop start.
        assert_eq!(poll.manual_ramp_current, Some(70));
        assert_eq!(poll.curve_stepper.current_duty(), Some(70));
        assert_eq!(poll.last_fan_mode, Some(FanControlMode::Manual));
        poll.last_manual_duty = Some(60);
        poll.last_duty_write_ms = 5_000;
        poll.just_resumed = true;
        poll.reset_fan_state();
        assert_eq!(poll.manual_ramp_current, None);
        assert!(poll.manual_per_fan_ramp.is_none());
        assert_eq!(poll.curve_stepper.current_duty(), None);
        assert_eq!(poll.last_fan_mode, None);
        assert_eq!(poll.last_manual_duty, None);
        assert_eq!(poll.last_duty_write_ms, 0);
        assert_eq!(poll.last_handover_ms, 0);
        // Counters and clocks are not fan state: untouched.
        assert_eq!(poll.last_cpu_power_poll, 0);
        assert_eq!(poll.consecutive_ec_failures, 0);
    }

    #[test]
    fn poll_state_disabled_mode_seeds_no_fan_mode() {
        use crate::types::FanControlMode;
        let state = AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        let poll = PollState::new(&state, 0, FanControlMode::Disabled, false, 1_000);
        // None forces firmware restore on the first iteration.
        assert_eq!(poll.last_fan_mode, None);
        assert_eq!(poll.manual_ramp_current, None);
    }
}
