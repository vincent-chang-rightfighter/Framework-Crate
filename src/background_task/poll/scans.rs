//! PD/expansion/privacy scans and slow firmware reads.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use crate::app::AppState;
use crate::cli;
use crate::style::{EXPANSION_SCAN_MS, VERSIONS_REFRESH_MS};
use crate::util::{read_lock, with_write_lock};

use super::super::{mark_pd_usb_c_seen, mark_view_dirty, push_pd_ports_history};
use super::PollState;

/// Expansion/PD scans plus privacy switches, sharing one throttle gate.
/// `now_ms` arrives freshly sampled by the caller (see the loop).
pub(in crate::background_task) async fn phase_scans(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    poll: &mut PollState,
    now_ms: u64,
) {
    let expansion_interval = if state.lifecycle.visible.load(Ordering::Acquire) {
        EXPANSION_SCAN_MS
    } else {
        30_000
    };
    if now_ms.saturating_sub(poll.last_expansion_scan) < expansion_interval {
        return;
    }
    poll.last_expansion_scan = now_ms;
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec(
        "pd_ports",
        crate::util::EC_IO_TIMEOUT,
        ec_clone,
        move |ec| ec.pd_ports(),
    )
    .await
    {
        Ok(ports) => {
            poll.last_pd_success_ms = crate::util::monotonic_ms();
            let changed = {
                let current = read_lock(&state.peripherals.pd_ports);
                *current != ports
            };
            if changed {
                with_write_lock(&state.peripherals.pd_ports, |guard| {
                    *guard = Arc::new(ports);
                });
                mark_view_dirty(state);
            }
            mark_pd_usb_c_seen(
                &read_lock(&state.peripherals.pd_ports),
                &state.peripherals.pd_usb_c_seen,
            );
            push_pd_ports_history(
                &state.peripherals.pd_ports,
                &state.peripherals.pd_ports_history,
            );
        }
        Err(e) => {
            // Overload skips are backpressure: fall
            // through to expansion/fan instead of
            // skipping the round.
            if e.is_overloaded() {
                // No warning, no staleness change.
            } else {
                warn!("PD ports read failed: {}", e);
                if poll.last_pd_success_ms != 0
                    && crate::util::monotonic_ms().saturating_sub(poll.last_pd_success_ms) > 30_000
                {
                    with_write_lock(&state.peripherals.pd_ports, |guard| {
                        if !guard.is_empty() {
                            *guard = Arc::new(smallvec::SmallVec::new());
                            mark_view_dirty(state);
                        }
                    });
                }
            }
        }
    }
    // Privacy switches move with a physical slider: visible 10s /
    // hidden 30s (same throttle gate as the PD/expansion scans above) is
    // plenty, and the round itself is the retry (no attempt budget needed).
    let ec_clone = Arc::clone(ec);
    match crate::ec_serial::exec(
        "privacy-switches",
        crate::util::EC_IO_TIMEOUT,
        ec_clone,
        move |ec| ec.privacy_switches(),
    )
    .await
    {
        Ok(Ok(switches)) => {
            with_write_lock(&state.peripherals.privacy, |guard| {
                if guard.as_ref().as_ref() != Some(&switches) {
                    *guard = Arc::new(Some(switches));
                    mark_view_dirty(state);
                }
            });
        }
        Err(e) if e.is_overloaded() => {}
        Ok(Err(e)) => {
            tracing::debug!("Privacy switches read failed: {}", e);
        }
        Err(e) => {
            tracing::debug!("Privacy switches dispatch failed: {}", e);
        }
    }
}

/// Slow firmware reads (versions, PD controller versions, deep
/// smart-battery telemetry) on the shared startup budget. Each has an
/// independent success flag so one failing never blocks the others.
/// `now_ms` arrives freshly sampled by the caller (see the loop).
pub(in crate::background_task) async fn phase_firmware(
    state: &AppState,
    ec: &Arc<dyn cli::EcHw>,
    poll: &mut PollState,
    now_ms: u64,
) {
    // Versions rarely change; only fetch a few times at startup.
    // Two quick retries after the immediate first attempt,
    // then the full refresh interval for the last one.
    let versions_retry_ms = if poll.versions_scan_attempts < 2 {
        10_000
    } else {
        VERSIONS_REFRESH_MS
    };
    if poll.versions_scan_attempts < 3
        && read_lock(&state.system.versions).is_none()
        && (poll.last_versions_scan == 0
            || now_ms.saturating_sub(poll.last_versions_scan) >= versions_retry_ms)
    {
        poll.last_versions_scan = now_ms;
        let ec_clone = Arc::clone(ec);
        match crate::ec_serial::exec(
            "versions",
            crate::util::EC_IO_TIMEOUT,
            ec_clone,
            move |ec| ec.versions(),
        )
        .await
        {
            Ok(Ok(v)) => {
                with_write_lock(&state.system.versions, |guard| {
                    if guard.as_ref().as_ref() != Some(&v) {
                        *guard = Arc::new(Some(v));
                        mark_view_dirty(state);
                    }
                });
                // Stop after first successful fetch; versions only change after reboot.
                if read_lock(&state.system.versions).is_some() {
                    poll.versions_scan_attempts = 3;
                }
            }
            // Overload skips are backpressure, not failure:
            // don't consume the 3 startup tries.
            Err(e) if e.is_overloaded() => {}
            _ => {
                poll.versions_scan_attempts += 1;
            }
        }
    }
    // PD controller firmware changes as rarely as versions;
    // best-effort alongside it so one failing never blocks
    // the other.
    if poll.pd_fw_scan_attempts < 3
        && read_lock(&state.system.pd_fw).is_none()
        && (poll.last_pd_fw_scan == 0
            || now_ms.saturating_sub(poll.last_pd_fw_scan) >= versions_retry_ms)
    {
        poll.last_pd_fw_scan = now_ms;
        let ec_clone = Arc::clone(ec);
        match crate::ec_serial::exec(
            "pd-fw-versions",
            crate::util::EC_IO_TIMEOUT,
            ec_clone,
            move |ec| ec.pd_fw_versions(),
        )
        .await
        {
            Ok(Ok(v)) => {
                with_write_lock(&state.system.pd_fw, |guard| {
                    if guard.as_ref().as_ref() != Some(&v) {
                        *guard = Arc::new(Some(v));
                        mark_view_dirty(state);
                    }
                });
                if read_lock(&state.system.pd_fw).is_some() {
                    poll.pd_fw_scan_attempts = 3;
                }
            }
            Err(e) if e.is_overloaded() => {}
            _ => {
                poll.pd_fw_scan_attempts += 1;
            }
        }
    }
    // Deep smart-battery telemetry (~25 EC commands) is far
    // too heavy for the 5s battery poll; fetch it here on the
    // same startup budget instead. Cells and cycle counts go
    // stale slowly, so a rare fetch is plenty.
    if poll.smart_scan_attempts < 3
        && read_lock(&state.battery.smart).is_none()
        && (poll.last_smart_scan == 0
            || now_ms.saturating_sub(poll.last_smart_scan) >= versions_retry_ms)
    {
        poll.last_smart_scan = now_ms;
        let ec_clone = Arc::clone(ec);
        match crate::ec_serial::exec(
            "smart-battery",
            crate::util::EC_IO_TIMEOUT,
            ec_clone,
            move |ec| ec.smart_battery(),
        )
        .await
        {
            Ok(Ok(v)) => {
                with_write_lock(&state.battery.smart, |guard| {
                    if guard.as_ref().as_ref() != Some(&v) {
                        *guard = Arc::new(Some(v));
                        mark_view_dirty(state);
                    }
                });
                if read_lock(&state.battery.smart).is_some() {
                    poll.smart_scan_attempts = 3;
                }
            }
            Err(e) if e.is_overloaded() => {}
            _ => {
                poll.smart_scan_attempts += 1;
            }
        }
    }
}
