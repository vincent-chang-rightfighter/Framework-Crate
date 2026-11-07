//! Shared pure/state helpers for the poll loop and fan-mode arms.

use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use crate::app::AppState;
use crate::cli;
use crate::style::IDLE_THRESHOLD_MS;
use crate::temp_chart;
use crate::util::{read_lock, with_write_lock};

/// PD history depth to distinguish USB-A cards from transient USB device states.
const MAX_PD_HISTORY: usize = 3;

pub(super) fn try_ec_init_permit(
    sem: &Arc<tokio::sync::Semaphore>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    sem.clone().try_acquire_owned().ok()
}

/// Unified-toggle edge detector shared by the Manual and Curve arms.
/// `None` (first round) always counts as a transition so ramps reseed
/// deterministically instead of inheriting a stale duty.
pub(super) fn unified_transition_resets(prev: Option<bool>, now: bool) -> bool {
    prev != Some(now)
}

/// Resets EC client after spawn panic so next iteration reinitializes.
pub(super) fn reset_ec_on_panic(state: &AppState) {
    warn!("Resetting EC client after spawn panic");
    state.system.cli_available.store(false, Ordering::Release);
    with_write_lock(&state.system.ec_client, |guard| {
        *guard = Arc::new(None);
    });
    state
        .lifecycle
        .fan_reset_pending
        .store(true, Ordering::Release);
}

/// Forces EC reinitialization after consecutive failures (recovers stale driver after resume).
pub(super) fn reset_ec_after_failures(state: &AppState, failures: u32) {
    warn!(
        "EC unresponsive after {} consecutive read failures, reinitializing client",
        failures
    );
    state.system.cli_available.store(false, Ordering::Release);
    with_write_lock(&state.system.ec_client, |guard| {
        *guard = Arc::new(None);
    });
    state
        .lifecycle
        .fan_reset_pending
        .store(true, Ordering::Release);
}

pub(super) fn push_pd_ports_history(
    pd_ports: &Arc<RwLock<Arc<smallvec::SmallVec<[cli::ec_wrapper::UsbCPort; 4]>>>>,
    history: &Arc<RwLock<Arc<crate::sub_state::PdPortsHistory>>>,
) {
    let snapshot = Arc::clone(&read_lock(pd_ports));
    with_write_lock(history, |hist| {
        let mut h = (**hist).clone();
        h.push_back(snapshot);
        if h.len() > MAX_PD_HISTORY {
            h.pop_front();
        }
        *hist = Arc::new(h);
    });
}

/// Marks ports ever seen as Sink as USB-C (persists beyond history window).
pub(super) fn mark_pd_usb_c_seen(
    ports: &[cli::ec_wrapper::UsbCPort],
    seen_ref: &Arc<RwLock<Arc<Vec<bool>>>>,
) {
    /// Corrupt EC data must not drive a huge allocation.
    const MAX_PD_PORTS: usize = 8;
    if !ports
        .iter()
        .any(|p| p.power_role == Some("Sink") && (p.port as usize) < MAX_PD_PORTS)
    {
        return;
    }
    with_write_lock(seen_ref, |guard| {
        let mut seen = (**guard).clone();
        // Size vec for highest port index; EC skips failed ports so indices are sparse.
        let need = ports
            .iter()
            .filter(|p| (p.port as usize) < MAX_PD_PORTS)
            .map(|p| p.port as usize + 1)
            .max()
            .unwrap_or(0);
        let mut changed = seen.len() < need;
        if changed {
            seen.resize(need, false);
        }
        for p in ports {
            let idx = p.port as usize;
            if p.power_role == Some("Sink") && idx < seen.len() && !seen[idx] {
                seen[idx] = true;
                changed = true;
            }
        }
        if changed {
            *guard = Arc::new(seen);
        }
    });
}

pub(super) fn mark_view_dirty(state: &AppState) {
    state.lifecycle.view_dirty.store(true, Ordering::Release);
    state
        .lifecycle
        .view_generation
        .fetch_add(1, Ordering::Release);
}

/// Returns true if shutdown requested; fan writes must not be issued after quit write.
pub(super) fn shutdown_requested(state: &AppState) -> bool {
    state.lifecycle.shutdown.load(Ordering::Acquire)
}

pub(super) fn ac_transition_pending(prev_ac: bool, ac_now: bool, had_prior_sample: bool) -> bool {
    had_prior_sample && prev_ac && !ac_now
}

/// Fresh idle verdict. `last_interaction_ts` can change while the loop
/// sleeps or runs slow EC scans, so code after an await must call this
/// instead of reusing the snapshot taken at the top of the iteration.
pub(super) fn is_idle_now(state: &AppState) -> bool {
    is_idle_at(
        state.lifecycle.last_interaction_ts.load(Ordering::Acquire),
        crate::util::monotonic_ms(),
    )
}

/// Pure idle comparison, split out for deterministic tests (`monotonic_ms`
/// counts from process start, so a fresh test process is never idle).
fn is_idle_at(last_interaction_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_interaction_ms) > IDLE_THRESHOLD_MS
}

pub(super) fn ensure_per_fan_duty(state: &AppState, fan_count: usize) {
    if fan_count == 0 {
        return;
    }
    // Fast path: avoid write lock when count unchanged and duties sized.
    let last = state.fan.last_fan_count.load(Ordering::Acquire) as usize;
    if fan_count == last {
        let len = read_lock(&state.fan.per_fan_duty).len();
        if len >= fan_count {
            return;
        }
    }
    let fill = {
        let config = read_lock(&state.lifecycle.config);
        config.fan.manual.as_ref().map(|m| m.duty_pct).unwrap_or(50)
    };
    let mut resized = false;
    with_write_lock(&state.fan.per_fan_duty, |guard| {
        let duties = Arc::make_mut(guard);
        if duties.len() >= fan_count {
            // Never truncate; preserves saved per-fan duties across undock/dock.
            return;
        }
        duties.resize(fan_count, fill);
        resized = true;
    });
    state
        .fan
        .last_fan_count
        .store(fan_count as u64, Ordering::Release);
    // NOTE: Resize is not persisted; only tracks hardware changes. Config updates on explicit edits.
    if resized {
        mark_view_dirty(state);
    }
}

/// Grows per-fan curve storage when fans appear on multi-fan machines.
/// New slots clone the unified curve in memory (flushed to disk on the
/// next explicit save, like any other config edit); never truncates, so
/// undock cycles keep every fan's curve. Single-fan machines skip
/// entirely, and without a curve section there is nothing to seed from.
pub(super) fn ensure_per_fan_curve_points(state: &AppState, fan_count: usize) {
    if fan_count <= 1 {
        return;
    }
    // Fast path: avoid write locks once sized.
    if read_lock(&state.fan.per_fan_curve_full_points).len() >= fan_count {
        return;
    }
    if read_lock(&state.lifecycle.config).fan.curve.is_none() {
        return;
    }
    with_write_lock(&state.lifecycle.config, |guard| {
        let cfg = Arc::make_mut(guard);
        if let Some(curve) = cfg.fan.curve.as_mut()
            && curve.per_fan_points.len() < fan_count
        {
            curve
                .per_fan_points
                .resize(fan_count, curve.curve.points.clone());
        }
    });
    let full: Vec<Vec<[u32; 2]>> = {
        let config = read_lock(&state.lifecycle.config);
        config
            .fan
            .curve
            .as_ref()
            .map(|c| {
                c.per_fan_points
                    .iter()
                    .map(|p| crate::types::curve_full_points(p))
                    .collect()
            })
            .unwrap_or_default()
    };
    with_write_lock(&state.fan.per_fan_curve_full_points, |guard| {
        if guard.len() < fan_count && full.len() >= fan_count {
            *guard = Arc::new(full);
            mark_view_dirty(state);
        }
    });
}

pub(super) fn estimate_duty_from_thermal(state: &AppState) -> Option<u32> {
    // The max-RPM fan drives the seed; prefer its calibration table when
    // the wizard measured one, else the max-RPM heuristic below. Reads run
    // in separate scopes (never nested): a seed tolerates a stale sample.
    let (idx, rpm) = {
        let thermal_snap = read_lock(&state.thermal.data);
        let t = thermal_snap.as_ref().as_ref()?;
        t.fans
            .iter()
            .enumerate()
            .map(|(i, f)| (i, f.rpm))
            .max_by_key(|&(_, r)| r)?
    };
    if rpm == 0 {
        // Seed stopped fans at 0.
        return Some(0);
    }
    let table_duty = {
        let config = read_lock(&state.lifecycle.config);
        config
            .fan
            .calibration_tables
            .get(idx)
            .and_then(|table| crate::app::calibration::duty_for_rpm(table, rpm))
    };
    if let Some(duty) = table_duty {
        return Some(duty);
    }
    let max_rpm = state.fan.fan_max_rpm.load(Ordering::Acquire) as u32;
    if max_rpm == 0 {
        Some(10)
    } else {
        // Use u64 to avoid overflow.
        Some(((rpm as u64 * 100) / max_rpm as u64).clamp(10, 100) as u32)
    }
}

#[allow(clippy::collapsible_if)]
pub(super) fn record_thermal_sample(state: &AppState, t: cli::ec_wrapper::ThermalData) -> bool {
    // Periodically reset fan_max_rpm (60s) to adapt to hardware changes.
    const FAN_MAX_RPM_RESET_INTERVAL_MS: u64 = 60_000;
    let now_ts = crate::util::monotonic_ms();
    let fan_count = t.fans.len() as u64;
    state.fan.fan_count.store(fan_count, Ordering::Release);
    // Per-fan duties are only editable/used in Manual mode; Curve drives
    // its own per-fan curves below, so resizing the duty vector there is
    // pointless write churn.
    if crate::types::FanControlMode::from_u8(state.fan.mode.load(Ordering::Acquire) as u8)
        == crate::types::FanControlMode::Manual
    {
        ensure_per_fan_duty(state, t.fans.len());
    }
    if crate::types::FanControlMode::from_u8(state.fan.mode.load(Ordering::Acquire) as u8)
        == crate::types::FanControlMode::Curve
    {
        ensure_per_fan_curve_points(state, t.fans.len());
    }
    if let Some(max_rpm) = t.fans.iter().map(|f| f.rpm).max() {
        let prev = state.fan.fan_max_rpm.load(Ordering::Acquire) as u32;
        let last_reset = state.fan.last_fan_rpm_reset.load(Ordering::Acquire);
        if now_ts.saturating_sub(last_reset) >= FAN_MAX_RPM_RESET_INTERVAL_MS {
            // Rolling re-baseline only raises max; lowering while idle would mis-seed ramp.
            state
                .fan
                .fan_max_rpm
                .store(max_rpm.max(prev) as u64, Ordering::Release);
            state
                .fan
                .last_fan_rpm_reset
                .store(now_ts, Ordering::Release);
        } else if max_rpm > prev {
            state
                .fan
                .fan_max_rpm
                .store(max_rpm as u64, Ordering::Release);
        }
    }
    // Monotonic timestamp for window pruning; wall clock would break on jumps.
    let now = crate::util::monotonic_ms() as i64;

    // Avoid cloning if unchanged; compare whole payload to catch fan RPM changes.
    let changed = {
        let cur = read_lock(&state.thermal.data);
        match cur.as_ref().as_ref() {
            Some(cur) => *cur != t,
            None => true,
        }
    };
    if !changed {
        // EC is alive even when temps are stable: record liveness before
        // throttling the chart push, otherwise a stable machine starves
        // last_success_ms and trips THERMAL_STALE_MS failover.
        state
            .thermal
            .last_success_ms
            .store(now as u64, Ordering::Release);
        // Push periodic sample to keep chart window sliding; throttle to 1/s when idle.
        let history = read_lock(&state.thermal.history);
        if let Some(last_ts) = history.last_timestamp() {
            if now - last_ts < 1_000 {
                return false;
            }
        }
    }

    // Share temps via Arc to avoid deep clone on history reads.
    let temps_for_history = std::sync::Arc::clone(&t.temps);

    // Update sensor cache if key set changed (rare hardware change).
    let keys_changed = {
        let cache = read_lock(&state.thermal.sensor_cache);
        cache.keys.len() != t.temps.len()
            || cache
                .keys
                .iter()
                .zip(t.temps.keys())
                .any(|(a, b)| a.as_str() != b.as_str())
    };
    if keys_changed {
        let new_keys: Vec<String> = t.temps.keys().cloned().collect();
        let config = read_lock(&state.lifecycle.config);
        let sorted =
            crate::types::sorted_sensor_list(&config.telemetry.selected_sensors, &new_keys);
        let colors: Vec<iced::Color> = sorted
            .iter()
            .map(|name| crate::style::sensor_color(name, &new_keys))
            .collect();
        with_write_lock(&state.thermal.sensor_cache, |g| {
            *g = Arc::new(crate::app::SensorCache {
                keys: new_keys,
                sorted: Arc::new(sorted),
                colors: Arc::new(colors),
            });
        });
    }

    with_write_lock(&state.thermal.data, |guard| {
        *guard = Arc::new(Some(t));
    });
    state
        .thermal
        .last_success_ms
        .store(now as u64, Ordering::Release);

    with_write_lock(&state.thermal.history, |hist| {
        let hist = Arc::make_mut(hist);
        hist.push_sample(
            temp_chart::TempSample {
                ts_ms: now,
                temps: temps_for_history,
            },
            now,
        );
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::IDLE_THRESHOLD_MS;

    #[test]
    fn first_battery_sample_never_triggers_reset() {
        assert!(!ac_transition_pending(true, false, false));
    }

    #[test]
    fn idle_verdict_follows_latest_interaction() {
        // Pure comparison with synthetic clocks (deterministic: the real
        // clock counts from process start, so a fresh test host is young).
        assert!(!is_idle_at(1_000, 1_000));
        assert!(!is_idle_at(0, IDLE_THRESHOLD_MS));
        assert!(is_idle_at(0, IDLE_THRESHOLD_MS + 1));
        assert!(is_idle_at(9_000, 20_000));

        // Live helper: just interacted means active.
        let state = crate::app::AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        state
            .lifecycle
            .last_interaction_ts
            .store(crate::util::monotonic_ms(), Ordering::Release);
        assert!(!is_idle_now(&state));
    }

    #[test]
    fn ac_to_battery_after_first_sample_triggers_reset() {
        assert!(ac_transition_pending(true, false, true));
        assert!(!ac_transition_pending(false, false, true));
        assert!(!ac_transition_pending(true, true, true));
    }

    #[test]
    fn record_thermal_sample_updates_on_fan_rpm_change() {
        use crate::cli::ec_wrapper::{FanReading, ThermalData};
        use std::collections::BTreeMap;

        let state = crate::app::AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        let mut temps = BTreeMap::new();
        temps.insert("CPU".to_string(), 60);
        let sample = |rpm: u32| ThermalData {
            temps: Arc::new(temps.clone()),
            fans: smallvec::smallvec![FanReading {
                name: "Fan 1".to_string(),
                rpm
            }],
        };

        assert!(record_thermal_sample(&state, sample(2000)));
        assert_eq!(
            read_lock(&state.thermal.data)
                .as_ref()
                .as_ref()
                .unwrap()
                .fans[0]
                .rpm,
            2000
        );

        // Same temps, different RPM — must still be stored and flagged dirty.
        assert!(record_thermal_sample(&state, sample(3200)));
        assert_eq!(
            read_lock(&state.thermal.data)
                .as_ref()
                .as_ref()
                .unwrap()
                .fans[0]
                .rpm,
            3200
        );

        // Identical data — no write, not dirty.
        assert!(!record_thermal_sample(&state, sample(3200)));
    }

    #[test]
    fn ensure_per_fan_duty_never_truncates_saved_values() {
        let state = crate::app::AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        with_write_lock(&state.fan.per_fan_duty, |guard| {
            *guard = Arc::new(vec![50, 50, 30, 30]);
        });

        // Undock: fan count drops to 2 — values for fans 3-4 must survive.
        ensure_per_fan_duty(&state, 2);
        assert_eq!(*read_lock(&state.fan.per_fan_duty), vec![50, 50, 30, 30]);

        // Re-dock with 4 fans: length unchanged, no fill overwrites.
        ensure_per_fan_duty(&state, 4);
        assert_eq!(*read_lock(&state.fan.per_fan_duty), vec![50, 50, 30, 30]);

        // Genuine growth still pads with the manual duty.
        ensure_per_fan_duty(&state, 6);
        assert_eq!(
            *read_lock(&state.fan.per_fan_duty),
            vec![50, 50, 30, 30, 50, 50]
        );
    }

    #[test]
    fn ensure_per_fan_curve_points_seeds_from_unified() {
        use crate::types::{CurveConfig, GlobalCurveConfig};
        let state = crate::app::AppState {
            system: Default::default(),
            fan: Default::default(),
            thermal: Default::default(),
            peripherals: Default::default(),
            battery: Default::default(),
            cpu_power: Default::default(),
            lifecycle: Default::default(),
        };
        // Single-fan machines never grow per-fan storage.
        ensure_per_fan_curve_points(&state, 1);
        assert!(read_lock(&state.fan.per_fan_curve_full_points).is_empty());
        // No curve section: nothing to seed from, no crash.
        ensure_per_fan_curve_points(&state, 2);
        assert!(read_lock(&state.fan.per_fan_curve_full_points).is_empty());

        with_write_lock(&state.lifecycle.config, |guard| {
            let cfg = Arc::make_mut(guard);
            cfg.fan.curve = Some(GlobalCurveConfig {
                curve: CurveConfig {
                    points: vec![[40, 10]],
                    ..CurveConfig::default()
                },
                poll_ms: 500,
                per_fan_points: Vec::new(),
            });
        });
        ensure_per_fan_curve_points(&state, 2);
        let full = read_lock(&state.fan.per_fan_curve_full_points)
            .as_ref()
            .clone();
        assert_eq!(full.len(), 2);
        // Both slots follow the unified curve (plus lock endpoints).
        let unified_full = crate::types::curve_full_points(&[[40, 10]]);
        assert_eq!(full[0], unified_full);
        assert_eq!(full[1], unified_full);
        // Config slots grew in memory for edits/exports to see.
        assert_eq!(
            read_lock(&state.lifecycle.config)
                .fan
                .curve
                .as_ref()
                .unwrap()
                .per_fan_points
                .len(),
            2
        );

        // Undock to 1 fan: fast path is skipped (<=1) and nothing truncates.
        ensure_per_fan_curve_points(&state, 1);
        assert_eq!(read_lock(&state.fan.per_fan_curve_full_points).len(), 2);
    }

    #[test]
    fn ec_init_permits_only_one_attempt_at_a_time() {
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let first = try_ec_init_permit(&sem).expect("first permit");
        assert!(try_ec_init_permit(&sem).is_none());
        drop(first);
        assert!(try_ec_init_permit(&sem).is_some());
    }
}
