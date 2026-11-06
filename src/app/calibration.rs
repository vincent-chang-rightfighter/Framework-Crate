//! Fan calibration wizard: measures per-fan RPM at fixed duties and stores
//! the tables used to seed ramps.
//!
//! Tick-driven: each step writes one duty to all fans, dwells while
//! sampling RPM, then records the per-fan median. The background fan arm
//! yields while `calibration_active` is set, so the wizard owns EC writes
//! in that window. Aborts (nothing saved) on user cancel, over-temperature,
//! topology change, missing readings, or EC errors; a finished run saves a
//! complete table per fan.

use super::{App, Message, run_ec_task_result};
use crate::cli;
use crate::util::read_lock;
use iced::Task;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::warn;

/// Calibration duties, high to low: cool the machine first, stop last so
/// the hot window is as short as possible.
pub(crate) const CALIBRATION_STEPS: &[u32] = &[100, 75, 50, 25, 0];
/// Dwell per step; the median is taken over the trailing window below.
const STEP_DWELL: Duration = Duration::from_secs(10);
/// Trailing window whose RPM samples form the recorded median.
const SAMPLE_WINDOW: Duration = Duration::from_secs(3);
/// Abort when non-battery readings reach this (same philosophy as the
/// Manual 95C floor: open-loop fixed duties must not cook the machine).
const ABORT_TEMP_C: i32 = 95;
/// Refuse to start a run when already this hot: the 0% step would begin
/// with no cooling headroom. 80C leaves margin below the 95C abort.
const START_MAX_TEMP_C: i32 = 80;
/// Guard debounce: a single glitch tick (dropped EC sample, dock wobble)
/// must not discard a ~1min run. Guards must trip consecutively this many
/// ticks before the run aborts; the progress line shows "retrying" meanwhile.
const GUARD_DEBOUNCE_TICKS: u32 = 3;
/// EC write budget for step writes (user may take a while to notice).
const STEP_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap sample history; ticks run at 200ms so this never grows past a few
/// hundred tiny entries.
const SAMPLE_RETAIN: Duration = Duration::from_secs(60);

/// In-progress wizard run.
pub(crate) struct CalibrationRun {
    /// Monotonic id; late results from a cancelled run are ignored.
    seq: u64,
    step_idx: usize,
    step_started: Instant,
    started: Instant,
    awaiting_write: bool,
    fan_total: usize,
    samples: Vec<(Instant, Vec<u32>)>,
    results: Vec<Vec<[u32; 2]>>,
    /// Consecutive guard trips (hot / topology / no-data). Reset on any
    /// clean tick; the run aborts only at `GUARD_DEBOUNCE_TICKS`.
    unstable_streak: u32,
    /// Consecutive dwells that produced no median for some fan. Kept apart
    /// from `unstable_streak` because a clean guard tick must not erase it,
    /// otherwise the median-miss abort can never be reached.
    median_misses: u32,
}

impl CalibrationRun {
    pub(crate) fn progress_line(&self) -> String {
        let duty = CALIBRATION_STEPS.get(self.step_idx).copied().unwrap_or(0);
        let elapsed = self.started.elapsed().as_secs();
        if self.unstable_streak > 0 {
            format!(
                "Step {}/{}: {}% ({}s elapsed, retrying…)",
                self.step_idx + 1,
                CALIBRATION_STEPS.len(),
                duty,
                elapsed
            )
        } else {
            format!(
                "Step {}/{}: {}% ({}s elapsed)",
                self.step_idx + 1,
                CALIBRATION_STEPS.len(),
                duty,
                elapsed
            )
        }
    }
}

/// Median of RPM samples; `None` when empty.
fn median_u32(values: &mut [u32]) -> Option<u32> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

/// Inverts a calibration table (duty→rpm) to the duty producing `rpm`.
/// `None` when the table cannot answer (fewer than 2 distinct duties):
/// callers fall back to the max-RPM heuristic.
pub(crate) fn duty_for_rpm(table: &[[u32; 2]], rpm: u32) -> Option<u32> {
    if rpm == 0 {
        return Some(0);
    }
    let mut pts: Vec<[u32; 2]> = table.to_vec();
    pts.sort_by_key(|p| p[0]);
    pts.dedup_by_key(|p| p[0]);
    if pts.len() < 2 {
        return None;
    }
    if rpm <= pts[0][1] {
        return Some(pts[0][0]);
    }
    if rpm >= pts[pts.len() - 1][1] {
        return Some(100);
    }
    for w in pts.windows(2) {
        let ([d1, r1], [d2, r2]) = (w[0], w[1]);
        if rpm >= r1 && rpm <= r2 {
            if r2 == r1 {
                return Some(d2);
            }
            let ratio = (rpm - r1) as f64 / (r2 - r1) as f64;
            let duty = d1 as f64 + ratio * (d2 as f64 - d1 as f64);
            return Some((duty.round() as u32).clamp(0, 100));
        }
    }
    // Non-monotonic table with rpm strictly inside the range: no honest
    // answer, let the caller fall back to the heuristic.
    None
}

fn emit_step_write(ec_client: &cli::SharedEc, duty: u32, seq: u64) -> Task<Message> {
    let ec_client = Arc::clone(ec_client);
    Task::perform(
        async move {
            let ec_opt = { read_lock(&ec_client) };
            let res: Result<(), String> = if let Some(ref ec) = *ec_opt {
                let ec = Arc::clone(ec);
                match crate::ec_serial::exec(
                    "calibration-set-duty",
                    STEP_WRITE_TIMEOUT,
                    ec,
                    move |ec| ec.set_fan_duty(duty, None),
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => Err(e.to_string()),
                }
            } else {
                Err("no EC client".to_string())
            };
            Message::CalibrationStepDone(seq, res)
        },
        |msg| msg,
    )
}

impl App {
    pub(crate) fn calibration_start(&mut self) -> Option<Task<Message>> {
        if self.calibration.is_some() {
            return Some(Task::none());
        }
        let fan_total = self.state.fan.fan_count.load(Ordering::Acquire) as usize;
        if fan_total == 0 {
            self.calibration_status =
                Some("No fans detected; calibration needs live fan readings".to_string());
            self.mark_dirty();
            return Some(Task::none());
        }
        // Refuse to start hot: the run ends at 0% with no active cooling,
        // so starting at 80C+ leaves no headroom before the 95C abort.
        // Close heavy workloads and let the machine cool first.
        let start_temp: Option<i32> = read_lock(&self.state.thermal.data)
            .as_ref()
            .as_ref()
            .and_then(|t| {
                t.temps
                    .iter()
                    .filter(|(n, _)| !crate::types::is_battery_sensor(n))
                    .map(|(_, v)| *v)
                    .max()
            });
        if start_temp.is_some_and(|t| t >= START_MAX_TEMP_C) {
            self.calibration_status = Some(format!(
                "Too hot to start ({}C); close heavy workloads, let it cool below {}C, then retry",
                start_temp.unwrap_or(0),
                START_MAX_TEMP_C
            ));
            self.mark_dirty();
            return Some(Task::none());
        }
        self.calibration_status = None;
        self.state
            .fan
            .calibration_active
            .store(true, Ordering::Release);
        self.calibration_seq = self.calibration_seq.wrapping_add(1);
        let seq = self.calibration_seq;
        self.calibration = Some(CalibrationRun {
            seq,
            step_idx: 0,
            step_started: Instant::now(),
            started: Instant::now(),
            awaiting_write: true,
            fan_total,
            samples: Vec::new(),
            results: vec![Vec::new(); fan_total],
            unstable_streak: 0,
            median_misses: 0,
        });
        self.mark_dirty();
        Some(emit_step_write(
            &self.state.system.ec_client,
            CALIBRATION_STEPS[0],
            seq,
        ))
    }

    pub(crate) fn calibration_cancel(&mut self) -> Option<Task<Message>> {
        if self.calibration.is_none() {
            return Some(Task::none());
        }
        Some(self.abort_calibration("cancelled by user".to_string()))
    }

    pub(crate) fn calibration_clear_table(&mut self) -> Option<Task<Message>> {
        // Refuse mid-run; cancel first so a live run never loses its baseline.
        if self.calibration.is_some() {
            return Some(Task::none());
        }
        self.mutate_config(|cfg| {
            cfg.fan.calibration_tables.clear();
        });
        self.save_config();
        self.calibration_status =
            Some("Calibration table cleared; using live estimates".to_string());
        self.mark_dirty();
        Some(Task::none())
    }

    pub(crate) fn calibration_step_done(
        &mut self,
        seq: u64,
        result: &Result<(), String>,
    ) -> Option<Task<Message>> {
        let Some(run) = self.calibration.as_mut() else {
            return Some(Task::none());
        };
        // A result from a cancelled run must not touch the current one.
        if run.seq != seq {
            return Some(Task::none());
        }
        match result {
            Ok(()) => {
                run.awaiting_write = false;
                run.step_started = Instant::now();
                run.samples.clear();
                self.mark_dirty();
                Some(Task::none())
            }
            Err(e) => {
                let reason = format!("EC write failed: {e}");
                let restore = self.abort_calibration(reason);
                self.mark_dirty();
                Some(restore)
            }
        }
    }

    /// Advances the wizard one tick: samples RPM, enforces guards, and
    /// emits the next step write when a dwell completes. Returns the write
    /// task, if any; the tick caller dispatches it ahead of everything else.
    pub(crate) fn poll_calibration(&mut self) -> Option<Task<Message>> {
        self.calibration.as_ref()?;
        // Live progress line.
        self.mark_dirty();
        if self.calibration.as_ref().is_some_and(|r| r.awaiting_write) {
            // A step write can take up to STEP_WRITE_TIMEOUT; the heat guard
            // must still fire inside that window (unconditional, no debounce).
            let hot = read_lock(&self.state.thermal.data)
                .as_ref()
                .as_ref()
                .and_then(|t| {
                    t.temps
                        .iter()
                        .filter(|(n, _)| !crate::types::is_battery_sensor(n))
                        .map(|(_, v)| *v)
                        .max()
                })
                .is_some_and(|max| max >= ABORT_TEMP_C);
            if hot {
                let restore = self.abort_calibration(format!(
                    "temperature reached {ABORT_TEMP_C}C, stopping for safety"
                ));
                return Some(restore);
            }
            return None;
        }
        let now = Instant::now();
        let snapshot = read_lock(&self.state.thermal.data);
        let (has_data, fan_len, rpms, hot) = match snapshot.as_ref().as_ref() {
            Some(t) => {
                let hot = t
                    .temps
                    .iter()
                    .filter(|(n, _)| !crate::types::is_battery_sensor(n))
                    .map(|(_, v)| *v)
                    .max()
                    .is_some_and(|m| m >= ABORT_TEMP_C);
                let rpms: Vec<u32> = t.fans.iter().map(|f| f.rpm).collect();
                (true, rpms.len(), rpms, hot)
            }
            None => (false, 0, Vec::new(), false),
        };
        drop(snapshot);
        let duty = CALIBRATION_STEPS
            .get(self.calibration.as_ref().map(|r| r.step_idx).unwrap_or(0))
            .copied()
            .unwrap_or(0);
        if let Some(run) = self.calibration.as_mut() {
            run.samples.push((now, rpms));
            run.samples
                .retain(|(t, _)| now.duration_since(*t) < SAMPLE_RETAIN);
        }
        // Debounced guards: one glitch tick must not kill a ~1min run.
        // Each guard must trip consecutively GUARD_DEBOUNCE_TICKS times;
        // meanwhile the progress line shows "retrying…" (see progress_line).
        let guard_reason: Option<String> = if !has_data {
            Some("no fan readings".to_string())
        } else {
            let fan_total = self.calibration.as_ref().map(|r| r.fan_total).unwrap_or(0);
            if fan_len != fan_total {
                Some(format!("fan topology changed ({fan_total}→{fan_len})"))
            } else if hot {
                Some(format!(
                    "temperature reached {ABORT_TEMP_C}C, stopping for safety"
                ))
            } else {
                None
            }
        };
        if let Some(reason) = guard_reason {
            let streak = self
                .calibration
                .as_mut()
                .map(|r| {
                    r.unstable_streak = r.unstable_streak.saturating_add(1);
                    r.unstable_streak
                })
                .unwrap_or(GUARD_DEBOUNCE_TICKS);
            if streak >= GUARD_DEBOUNCE_TICKS {
                let restore = self.abort_calibration(reason);
                return Some(restore);
            }
            return None;
        }
        if let Some(run) = self.calibration.as_mut() {
            run.unstable_streak = 0;
        }
        let fan_total = self.calibration.as_ref().map(|r| r.fan_total).unwrap_or(0);
        let dwell_done = self
            .calibration
            .as_ref()
            .is_some_and(|r| now.duration_since(r.step_started) >= STEP_DWELL);
        if !dwell_done {
            return None;
        }
        // Finalize the step: median over the trailing window per fan.
        let cutoff = now - SAMPLE_WINDOW;
        let mut medians = Vec::with_capacity(fan_total);
        for i in 0..fan_total {
            let mut vals: Vec<u32> = self
                .calibration
                .as_ref()
                .map(|r| {
                    r.samples
                        .iter()
                        .filter(|(t, _)| *t >= cutoff)
                        .filter_map(|(_, r)| r.get(i).copied())
                        .collect()
                })
                .unwrap_or_default();
            match median_u32(&mut vals) {
                Some(m) => medians.push(m),
                None => {
                    // A clean guard tick must not clear this counter, or the
                    // abort below could never fire (see `median_misses`).
                    let misses = self
                        .calibration
                        .as_mut()
                        .map(|r| {
                            r.median_misses = r.median_misses.saturating_add(1);
                            r.median_misses
                        })
                        .unwrap_or(GUARD_DEBOUNCE_TICKS);
                    if misses >= GUARD_DEBOUNCE_TICKS {
                        let restore = self.abort_calibration("no fan readings".to_string());
                        return Some(restore);
                    }
                    return None;
                }
            }
        }
        if let Some(run) = self.calibration.as_mut() {
            run.median_misses = 0;
        }
        let step_idx = self.calibration.as_ref().map(|r| r.step_idx).unwrap_or(0);
        if let Some(run) = self.calibration.as_mut() {
            for (i, median) in medians.into_iter().enumerate() {
                run.results[i].push([duty, median]);
            }
            run.step_idx += 1;
        }
        if step_idx + 1 >= CALIBRATION_STEPS.len() {
            let restore = self.finish_calibration();
            return Some(restore);
        }
        let seq = self
            .calibration
            .as_ref()
            .map(|run| run.seq)
            .unwrap_or_default();
        if let Some(run) = self.calibration.as_mut() {
            run.awaiting_write = true;
            run.samples.clear();
        }
        Some(emit_step_write(
            &self.state.system.ec_client,
            CALIBRATION_STEPS[step_idx + 1],
            seq,
        ))
    }

    fn finish_calibration(&mut self) -> Task<Message> {
        let (tables, secs) = match self.calibration.take() {
            Some(run) => (run.results, run.started.elapsed().as_secs()),
            None => return Task::none(),
        };
        let fans = tables.len();
        self.mutate_config(|cfg| {
            cfg.fan.calibration_tables = tables;
        });
        self.save_config();
        let restore = self.teardown_calibration();
        self.calibration_status = Some(format!("Calibrated {fans} fan(s) in {secs}s"));
        self.mark_dirty();
        restore
    }

    fn abort_calibration(&mut self, reason: String) -> Task<Message> {
        warn!("Fan calibration aborted: {reason}");
        // Aborts save nothing: a partial table from a hot/cancelled run is
        // exactly what must not get baked in.
        let restore = self.teardown_calibration();
        self.calibration_status = Some(format!("Calibration aborted: {reason}"));
        self.mark_dirty();
        restore
    }

    /// Silent clear for the quit path: the quit handover owns the final fan
    /// write (via `exec_force`, after all queued duties), so no immediate
    /// restore is emitted here. Clears `calibration_active` so no further
    /// step write is issued between quit request and window close.
    pub(crate) fn abort_calibration_for_quit(&mut self) {
        if self.calibration.is_none() {
            return;
        }
        self.calibration = None;
        self.state
            .fan
            .calibration_active
            .store(false, Ordering::Release);
    }

    /// Queues an immediate firmware-handover restore on top of the
    /// `fan_reset_pending` flag. The flag lets the next background round
    /// re-assert the configured mode; the immediate write closes the hot
    /// window (e.g. an abort at 0%) without waiting for that round.
    fn calibration_restore_task(&self) -> Task<Message> {
        run_ec_task_result(&self.state.system.ec_client, move |ec| {
            ec.autofanctrl().map_err(|e| e.to_string())
        })
    }

    fn teardown_calibration(&mut self) -> Task<Message> {
        self.calibration = None;
        self.state
            .fan
            .calibration_active
            .store(false, Ordering::Release);
        // Hand control back: the next round re-asserts the selected mode.
        self.state
            .lifecycle
            .fan_reset_pending
            .store(true, Ordering::Release);
        self.mark_dirty();
        self.calibration_restore_task()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duty_for_rpm_zero_is_zero() {
        let table = [[25u32, 1000u32], [100, 5000]];
        assert_eq!(duty_for_rpm(&table, 0), Some(0));
    }

    #[test]
    fn duty_for_rpm_clamps_outside() {
        let table = [[25u32, 1000u32], [100, 5000]];
        assert_eq!(duty_for_rpm(&table, 500), Some(25));
        assert_eq!(duty_for_rpm(&table, 6000), Some(100));
    }

    #[test]
    fn duty_for_rpm_interpolates() {
        // 3000rpm sits halfway between (25%,1000) and (100%,5000).
        let table = [[25u32, 1000u32], [100, 5000]];
        assert_eq!(duty_for_rpm(&table, 3000), Some(63));
    }

    #[test]
    fn duty_for_rpm_needs_two_points() {
        assert_eq!(duty_for_rpm(&[[50u32, 2000u32]], 2000), None);
        assert_eq!(duty_for_rpm(&[], 2000), None);
    }

    #[test]
    fn median_picks_middle() {
        assert_eq!(median_u32(&mut [3000, 1000, 2000]), Some(2000));
        assert_eq!(median_u32(&mut []), None);
    }

    #[test]
    fn progress_line_counts_steps() {
        let run = CalibrationRun {
            seq: 1,
            step_idx: 2,
            step_started: Instant::now(),
            started: Instant::now(),
            awaiting_write: false,
            fan_total: 1,
            samples: Vec::new(),
            results: vec![Vec::new()],
            unstable_streak: 0,
            median_misses: 0,
        };
        assert!(run.progress_line().starts_with("Step 3/5: 50%"));
    }
}
