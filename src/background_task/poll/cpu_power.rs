//! CPU power telemetry polling.

use std::sync::atomic::Ordering;
use tracing::warn;

use crate::app::AppState;

use super::super::{is_idle_now, mark_view_dirty};
use super::PollState;

/// CPU power poll (PawnIO ioctls off the async worker). Slow while idle;
/// hidden windows skip entirely. `now_ms` arrives freshly sampled.
pub(in crate::background_task) async fn phase_cpu_power(
    state: &AppState,
    poll: &mut PollState,
    now_ms: u64,
) {
    // Poll CPU power every 5s; slow to 30s while idle so the
    // card keeps updating without a 5s PawnIO wake-up. Hidden
    // windows still skip entirely.
    const CPU_POWER_POLL_MS: u64 = 5000;
    const CPU_POWER_IDLE_POLL_MS: u64 = 30_000;
    // Re-evaluate instead of reusing the top-of-loop
    // snapshot: the sleep and EC scans above can take
    // seconds under load, flipping idle state meanwhile.
    let cpu_poll_ms = if is_idle_now(state) {
        CPU_POWER_IDLE_POLL_MS
    } else {
        CPU_POWER_POLL_MS
    };
    let visible = state.lifecycle.visible.load(Ordering::Acquire);
    if visible
        && state.system.intel_cpu.load(Ordering::Acquire)
        && now_ms.saturating_sub(poll.last_cpu_power_poll) >= cpu_poll_ms
    {
        poll.last_cpu_power_poll = now_ms;
        // Run PawnIO ioctls off async worker like other EC I/O.
        let cpu_power = state.cpu_power.clone();
        if let Err(e) =
            crate::util::spawn_blocking_with_timeout(crate::util::PAWNIO_IO_TIMEOUT, move || {
                cpu_power.refresh()
            })
            .await
        {
            warn!("CPU power refresh task failed: {}", e);
        }
        mark_view_dirty(state);
    }
}
