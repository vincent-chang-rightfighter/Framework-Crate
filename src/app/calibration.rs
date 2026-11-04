//! Fan calibration wizard: measures per-fan RPM at fixed duties and stores
//! the tables used to seed ramps.
//!
//! Tick-driven: each step writes one duty to all fans, dwells while
//! sampling RPM, then records the per-fan median. The background fan arm
//! yields while `calibration_active` is set, so the wizard owns EC writes
//! in that window. Aborts (nothing saved) on user cancel, over-temperature,
//! topology change, missing readings, or EC errors; a finished run saves a
//! complete table per fan.

use super::{App, Message};
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
/// EC write budget for step writes (user may take a while to notice).
const STEP_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap sample history; ticks run at 200ms so this never grows past a few
/// hundred tiny entries.
const SAMPLE_RETAIN: Duration = Duration::from_secs(60);

/// In-progress wizard run.
pub(crate) struct CalibrationRun {
    step_idx: usize,
    step_started: Instant,
    started: Instant,
    awaiting_write: bool,
    fan_total: usize,
    samples: Vec<(Instant, Vec<u32>)>,
    results: Vec<Vec<[u32; 2]>>,
}

impl CalibrationRun {
    pub(crate) fn progress_line(&self) -> String {
        let duty = CALIBRATION_STEPS.get(self.step_idx).copied().unwrap_or(0);
        let elapsed = self.started.elapsed().as_secs();
        format!(
            "Step {}/{}: {}% ({}s elapsed)",
            self.step_idx + 1,
            CALIBRATION_STEPS.len(),
            duty,
            elapsed
        )
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

fn emit_step_write(ec_client: &cli::SharedEc, duty: u32) -> Task<Message> {
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
            Message::CalibrationStepDone(res)
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
        self.calibration_status = None;
        self.state
            .fan
            .calibration_active
            .store(true, Ordering::Release);
        self.calibration = Some(CalibrationRun {
            step_idx: 0,
            step_started: Instant::now(),
            started: Instant::now(),
            awaiting_write: true,
            fan_total,
            samples: Vec::new(),
            results: vec![Vec::new(); fan_total],
        });
        self.mark_dirty();
        Some(emit_step_write(
            &self.state.system.ec_client,
            CALIBRATION_STEPS[0],
        ))
    }

    pub(crate) fn calibration_cancel(&mut self) -> Option<Task<Message>> {
        if self.calibration.is_none() {
            return Some(Task::none());
        }
        self.abort_calibration("cancelled by user".to_string());
        Some(Task::none())
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
        result: &Result<(), String>,
    ) -> Option<Task<Message>> {
        let Some(run) = self.calibration.as_mut() else {
            return Some(Task::none());
        };
        match result {
            Ok(()) => {
                run.awaiting_write = false;
                run.step_started = Instant::now();
                run.samples.clear();
            }
            Err(e) => {
                let reason = format!("EC write failed: {e}");
                self.abort_calibration(reason);
            }
        }
        self.mark_dirty();
        Some(Task::none())
    }

    /// Advances the wizard one tick: samples RPM, enforces guards, and
    /// emits the next step write when a dwell completes. Returns the write
    /// task, if any; the tick caller dispatches it ahead of everything else.
    pub(crate) fn poll_calibration(&mut self) -> Option<Task<Message>> {
        self.calibration.as_ref()?;
        // Live progress line.
        self.mark_dirty();
        if self.calibration.as_ref().is_some_and(|r| r.awaiting_write) {
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
        if !has_data {
            self.abort_calibration("no fan readings".to_string());
            return None;
        }
        let fan_total = self.calibration.as_ref().map(|r| r.fan_total).unwrap_or(0);
        if fan_len != fan_total {
            self.abort_calibration(format!("fan topology changed ({fan_total}→{fan_len})"));
            return None;
        }
        if hot {
            self.abort_calibration(format!(
                "temperature reached {ABORT_TEMP_C}C, stopping for safety"
            ));
            return None;
        }
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
                    self.abort_calibration("no fan readings".to_string());
                    return None;
                }
            }
        }
        let step_idx = self.calibration.as_ref().map(|r| r.step_idx).unwrap_or(0);
        if let Some(run) = self.calibration.as_mut() {
            for (i, median) in medians.into_iter().enumerate() {
                run.results[i].push([duty, median]);
            }
            run.step_idx += 1;
        }
        if step_idx + 1 >= CALIBRATION_STEPS.len() {
            self.finish_calibration();
            return None;
        }
        if let Some(run) = self.calibration.as_mut() {
            run.awaiting_write = true;
            run.samples.clear();
        }
        Some(emit_step_write(
            &self.state.system.ec_client,
            CALIBRATION_STEPS[step_idx + 1],
        ))
    }

    fn finish_calibration(&mut self) {
        let (tables, secs) = match self.calibration.take() {
            Some(run) => (run.results, run.started.elapsed().as_secs()),
            None => return,
        };
        let fans = tables.len();
        self.mutate_config(|cfg| {
            cfg.fan.calibration_tables = tables;
        });
        self.save_config();
        self.teardown_calibration();
        self.calibration_status = Some(format!("Calibrated {fans} fan(s) in {secs}s"));
        self.mark_dirty();
    }

    fn abort_calibration(&mut self, reason: String) {
        warn!("Fan calibration aborted: {reason}");
        // Aborts save nothing: a partial table from a hot/cancelled run is
        // exactly what must not get baked in.
        self.teardown_calibration();
        self.calibration_status = Some(format!("Calibration aborted: {reason}"));
        self.mark_dirty();
    }

    fn teardown_calibration(&mut self) {
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
            step_idx: 2,
            step_started: Instant::now(),
            started: Instant::now(),
            awaiting_write: false,
            fan_total: 1,
            samples: Vec::new(),
            results: vec![Vec::new()],
        };
        assert!(run.progress_line().starts_with("Step 3/5: 50%"));
    }
}
