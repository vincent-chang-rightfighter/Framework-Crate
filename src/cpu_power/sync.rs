//! Background thread that re-asserts PL1/PL2 against firmware and EC overwrites.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, warn};

use super::ffi::open_handle;
use super::limits::{PowerLimitParams, write_msr_pl1_pl2};
use super::modules::load_intel_msr_blob;

/// Sync thread that continuously re-asserts MSR 0x610 to counter
/// firmware/EC overwrites. Runs until `external_alive` is dropped.
pub(super) struct SyncThread {
    pub(super) running: Arc<AtomicBool>,
    pub(super) alive: Arc<AtomicBool>,
    pub(super) handle: Option<std::thread::JoinHandle<()>>,
}

impl SyncThread {
    pub(super) fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Starts sync thread; `external_alive` tracks liveness without locking.
    /// `broken` is set by the worker itself when it hits a permanent
    /// failure (MSR breaker), so auto-restart stops respawning it.
    pub(super) fn start(
        &mut self,
        params: PowerLimitParams,
        external_alive: Arc<AtomicBool>,
        broken: Arc<AtomicBool>,
    ) -> Result<(), &'static str> {
        // The caller has already joined any previous worker while holding the
        // lock, so there is nothing left to tear down here.
        self.join_running();

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let alive = Arc::new(AtomicBool::new(false));
        let alive_clone = alive.clone();
        let alive_for_thread = alive.clone();
        let external_clone = external_alive.clone();
        let external_for_thread = external_alive.clone();
        let broken_for_thread = broken.clone();

        let handle = std::thread::Builder::new()
            .name("cpu-power-sync".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    sync_thread_main(
                        running_clone,
                        alive_for_thread,
                        external_for_thread,
                        params,
                        broken_for_thread,
                    );
                }));
                if result.is_err() {
                    warn!("Sync thread panicked");
                }
                // Clear BOTH liveness flags on exit so the UI cannot show a
                // stale "Syncing" state after an early return or panic.
                alive_clone.store(false, Ordering::Release);
                external_clone.store(false, Ordering::Release);
            })
            .map_err(|_| "failed to spawn sync thread")?;

        self.running = running;
        self.alive = alive;
        self.handle = Some(handle);
        // Liveness will be set to true by the thread after successful handle init.
        Ok(())
    }

    /// Stops the running worker and joins it, leaving `alive` untouched.
    ///
    /// Used when a new worker is about to take its place: liveness belongs to
    /// the new worker, so clearing it here would make a freshly started sync
    /// look dead. The worker wakes within ~100ms (interruptible sleep), so
    /// joining under the caller's lock cannot stall the UI.
    pub(super) fn join_running(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Stops the worker, joins it, and marks the thread not alive.
    pub(super) fn shutdown(&mut self) {
        self.join_running();
        self.alive.store(false, Ordering::Release);
    }
}

impl Drop for SyncThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// How loudly to report the `count`-th consecutive write failure.
///
/// `Some(true)` logs the error itself, `Some(false)` logs the one-time
/// "keeps failing" summary, and `None` stays silent. A register that is
/// BIOS-locked fails on every pass, so without the cutoff it would emit
/// four log lines per second forever.
fn failure_log_level(count: u32) -> Option<bool> {
    match count {
        1..=5 => Some(true),
        6 => Some(false),
        _ => None,
    }
}

/// Counts consecutive write failures and reports them at a decaying volume.
struct FailureCounter {
    label: &'static str,
    consecutive: u32,
}

impl FailureCounter {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            consecutive: 0,
        }
    }

    fn succeeded(&mut self) {
        self.consecutive = 0;
    }

    fn failed(&mut self, err: &str) {
        self.consecutive += 1;
        match failure_log_level(self.consecutive) {
            Some(true) => warn!("Sync thread {} write failed: {}", self.label, err),
            Some(false) => warn!(
                "Sync thread {} write keeps failing ({}), suppressing further warnings",
                self.label, err
            ),
            None => {}
        }
    }

    fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

/// Consecutive-failure limit for the sync loop breaker.
const MSR_BREAKER_LIMIT: u32 = 30;

/// Sync thread main loop; re-asserts MSR 0x610 every 250ms.
///
/// The official IntelMCHBAR module is read-only, so this thread intentionally
/// does not attempt MMIO writes; the mirror is still read for status/effective
/// limit display.
fn sync_thread_main(
    running: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    external_alive: Arc<AtomicBool>,
    params: PowerLimitParams,
    broken: Arc<AtomicBool>,
) {
    // Load IntelMSR module and open a persistent handle.
    let msr_blob = match load_intel_msr_blob() {
        Ok(b) => b,
        Err(e) => {
            warn!("Sync thread: failed to load IntelMSR module: {}", e);
            broken.store(true, Ordering::Release);
            return;
        }
    };

    let msr_handle = match open_handle(&msr_blob) {
        Ok(h) => h,
        Err(e) => {
            warn!("Sync thread: failed to open MSR handle: {}", e);
            broken.store(true, Ordering::Release);
            return;
        }
    };

    alive.store(true, Ordering::Release);
    external_alive.store(true, Ordering::Release);

    debug!(
        "Sync thread started: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s) (MSR only)",
        params.pl1_watts, params.pl1_time_s, params.pl2_watts, params.pl2_time_s
    );

    let mut msr_failures = FailureCounter::new("MSR");
    while running.load(Ordering::Relaxed) {
        // Unconditionally re-assert limits every 250ms. The whole point of
        // this thread is to win against firmware/EC overwrites.
        match write_msr_pl1_pl2(&msr_handle, &params) {
            Ok(()) => msr_failures.succeeded(),
            Err(e) => msr_failures.failed(&e),
        }
        // Circuit breaker: a BIOS-locked register will never succeed; stop
        // after 30 consecutive failures.
        let write_failures = msr_failures.consecutive();
        if write_failures >= MSR_BREAKER_LIMIT {
            warn!("Sync thread giving up after 30 consecutive MSR failures (likely BIOS-locked)");
            // Permanent: re-spawning would fail identically, so raise the
            // flag auto-restart watches instead of dying silently.
            broken.store(true, Ordering::Release);
            break;
        }
        // Back off on persistent failures; cap at 30s.
        // BIOS-locked register will otherwise spin forever; 30s reduces thermal contention.
        let interval_ms = if write_failures >= 20 {
            30_000
        } else if write_failures >= 10 {
            5000
        } else if write_failures >= 4 {
            1000
        } else {
            250
        };
        // Sleep in short chunks so stop_sync()/join() never blocks on the
        // full backoff interval (previously up to 30s of UI freeze).
        let mut slept_ms: u64 = 0;
        while slept_ms < interval_ms {
            if !running.load(Ordering::Relaxed) {
                break;
            }
            let chunk = (interval_ms - slept_ms).min(100);
            std::thread::sleep(std::time::Duration::from_millis(chunk));
            slept_ms += chunk;
        }
    }

    debug!("Sync thread stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_log_level_logs_each_error_for_the_first_five() {
        for count in 1..=5 {
            assert_eq!(
                failure_log_level(count),
                Some(true),
                "count {count} should log the error"
            );
        }
    }

    #[test]
    fn failure_log_level_summarises_once_at_the_switchover() {
        assert_eq!(failure_log_level(6), Some(false));
    }

    #[test]
    fn failure_log_level_is_silent_after_the_switchover() {
        // A BIOS-locked register fails on every pass, so anything logged past
        // the switchover would be four lines per second forever.
        for count in 7..10_000 {
            assert_eq!(
                failure_log_level(count),
                None,
                "count {count} should be silent"
            );
        }
    }

    #[test]
    fn failure_log_level_is_silent_before_any_failure() {
        assert_eq!(failure_log_level(0), None);
    }

    #[test]
    fn failure_counter_resets_on_success() {
        let mut c = FailureCounter::new("MSR");
        for _ in 0..25 {
            c.failed("boom");
        }
        assert_eq!(c.consecutive(), 25);
        c.succeeded();
        assert_eq!(c.consecutive(), 0);
        // After a reset the detail level applies again rather than staying muted.
        c.failed("boom");
        assert_eq!(c.consecutive(), 1);
    }

    #[test]
    fn failure_counter_counts_consecutively_not_totally() {
        let mut c = FailureCounter::new("MMIO");
        c.failed("a");
        c.failed("b");
        assert_eq!(c.consecutive(), 2);
        c.succeeded();
        c.failed("c");
        assert_eq!(c.consecutive(), 1, "a success must break the run");
    }
}
