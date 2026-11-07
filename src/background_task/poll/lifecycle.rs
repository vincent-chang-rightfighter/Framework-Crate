//! Resume handling and the battery/AC transition phase.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use crate::app::AppState;
use crate::cli;
use crate::util::{read_lock, with_write_lock};

use super::super::{BATTERY_STALE_MS, ac_transition_pending, mark_view_dirty};
use super::{Phase, PollState};

/// Wall-clock jump fallback, resume handling, and fan-reset handling.
/// Never skips the round by itself; resume work falls through to polling.
pub(in crate::background_task) fn phase_resume(state: &AppState, poll: &mut PollState) -> Phase {
    let mut resume_ts = state.lifecycle.last_resume_ts.load(Ordering::Acquire);
    // Skip resume handling if shutdown requested.
    if state.lifecycle.shutdown.load(Ordering::Acquire) {
        return Phase::Stop;
    }
    {
        let wall_now = crate::util::current_time_ms();
        // Sleep/resume: wall jumps forward while this loop
        // was parked in sleep; monotonic sleep time is not
        // visible here, so a large wall delta alone signals
        // a missed resume. Threshold 60s avoids DST noise.
        // NOTE: last_resume_ts and last_fan_rpm_reset are
        // monotonic-domain; store monotonic_ms() here, never
        // the wall epoch, or the 60s re-baseline breaks.
        if wall_now.saturating_sub(poll.last_wall_ms) > 60_000 {
            tracing::warn!(
                "[RESUME] wall-clock jump detected ({}ms), forcing EC re-init as resume fallback",
                wall_now.saturating_sub(poll.last_wall_ms)
            );
            // Synthesize a resume_ts so the block below runs.
            state
                .lifecycle
                .last_resume_ts
                .store(crate::util::monotonic_ms(), Ordering::Release);
            // Reload: the pre-wall-check load above is stale, otherwise this
            // round is skipped and the reset waits one more iteration.
            resume_ts = state.lifecycle.last_resume_ts.load(Ordering::Acquire);
        }
        poll.last_wall_ms = wall_now;
    }
    if resume_ts != 0 && resume_ts != poll.last_resume_ts {
        poll.last_resume_ts = resume_ts;
        poll.reset_fan_state();
        poll.just_resumed = true;
        // EC reverts to firmware control during sleep. The client teardown
        // below plus the cleared mode force the next iteration to re-assert
        // the selected mode and re-seed ramps.
        state.system.cli_available.store(false, Ordering::Release);
        with_write_lock(&state.system.ec_client, |guard| {
            *guard = Arc::new(None);
        });
        state
            .fan
            .last_fan_rpm_reset
            .store(resume_ts, Ordering::Release);
        // Keep fan_max_rpm as the real max-RPM baseline for accurate
        // ramp seeding after resume; only defer the rolling re-baseline
        // by 60s via last_fan_rpm_reset.
        state.fan.last_applied_duty.store(0, Ordering::Release);
        // Reset duty write time so the 30s re-assert guard trips
        // immediately on the resume iteration; otherwise Curve duty
        // could stay un-asserted for up to 30s after a short sleep.
        // Retain thermal history so the chart does not flash blank;
        // stale samples are pruned by the normal window retention.
        tracing::warn!(
            "[RESUME] EC client and fan state reset after system resume (history retained)"
        );
        // Firmware typically rewrites the RAPL registers across sleep; the
        // keepalive thread only fires every five minutes, so re-assert the
        // user's limits once, immediately, off the poll loop.
        if state.cpu_power.sync_enabled.load(Ordering::Acquire)
            && let Some(params) = state.cpu_power.desired_sync_params()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn_blocking(move || {
                if let Err(e) = crate::cpu_power::write_msr_pl1_pl2_public(params) {
                    tracing::warn!("Post-resume CPU power re-assert failed: {}", e);
                }
            });
        }
    }
    if state
        .lifecycle
        .fan_reset_pending
        .swap(false, Ordering::Acquire)
    {
        poll.reset_fan_state();
        state.fan.last_applied_duty.store(0, Ordering::Release);
        tracing::warn!("[EC RESET] fan controller state invalidated, will re-assert mode");
    }
    Phase::Continue
}

/// Battery read with AC-transition detection and staleness expiry.
/// UI-only: never resets the EC client and never skips the round.
pub(in crate::background_task) async fn phase_battery(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    poll: &mut PollState,
) {
    // Battery polling also drives the AC→battery power-limit
    // reset, so it must keep running while idle.
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec("power", crate::util::EC_IO_TIMEOUT, ec_clone, move |ec| {
        ec.power()
    })
    .await
    {
        Ok(Ok(bat)) => {
            poll.consecutive_battery_failures = 0;
            let ac_now = bat.ac_present == Some(true);
            let had_prior_sample = state.battery.last_success_ms.load(Ordering::Acquire) != 0;
            let ac_was = state.battery.prev_ac_present.load(Ordering::Acquire);
            if ac_transition_pending(ac_was, ac_now, had_prior_sample) {
                // With the keep-alive policy on, the UI layer applies the
                // target AC/Battery profile instead of resetting to BIOS
                // defaults. Only the legacy path sets the reset flag here.
                let keep_limits = read_lock(&state.lifecycle.config).cpu_power.keep_limits;
                if !keep_limits {
                    state
                        .lifecycle
                        .pl_reset_pending
                        .store(true, Ordering::Release);
                    tracing::info!("AC→battery transition detected, PL1/PL2 reset pending");
                }
            }
            state
                .battery
                .prev_ac_present
                .store(ac_now, Ordering::Release);
            crate::cpu_power::publish_ac_snapshot(ac_now);
            state
                .battery
                .last_success_ms
                .store(crate::util::monotonic_ms(), Ordering::Release);
            with_write_lock(&state.battery.info, |guard| {
                let new_info = crate::types::BatteryInfo { power_info: bat };
                if guard.as_ref().as_ref() != Some(&new_info) {
                    *guard = Arc::new(Some(new_info));
                }
            });
            mark_view_dirty(state);
        }
        Ok(Err(e)) => {
            tracing::debug!("Battery read failed: {}", e);
            poll.consecutive_battery_failures += 1;
            // Battery is UI-only: never reset the EC
            // client for it (that would needlessly drop
            // healthy fan/thermal state), and never
            // skip the poll loop (that would skip
            // PD/expansion/fan). Fall through to the
            // staleness check and the rest of the round.
            // No sleep here: sleeping would delay the fan fail-safe below.
            expire_battery_if_stale(state);
        }
        Err(e) => {
            // Overload skips are backpressure, not
            // failure: fall through to the rest of the
            // round instead of skipping PD/fan.
            if e.is_overloaded() {
            } else {
                warn!("Battery spawn failed: {}", e);
                poll.consecutive_battery_failures += 1;
                // UI-only: no sleep/continue and no EC
                // reset; staleness expiry below handles
                // a dead battery UI.
            }
            expire_battery_if_stale(state);
        }
    }
}

/// Clears a dead battery UI after `BATTERY_STALE_MS` without data.
/// Shared by both failure arms above; success arms never call it.
fn expire_battery_if_stale(state: &AppState) {
    let last_ok = state.battery.last_success_ms.load(Ordering::Acquire);
    let cur = crate::util::monotonic_ms();
    if last_ok != 0 && cur.saturating_sub(last_ok) > BATTERY_STALE_MS {
        with_write_lock(&state.battery.info, |guard| {
            if guard.is_some() {
                *guard = Arc::new(None);
                mark_view_dirty(state);
            }
        });
    }
}
