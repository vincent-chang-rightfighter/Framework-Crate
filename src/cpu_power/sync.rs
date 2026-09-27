//! Background thread that re-asserts PL1/PL2 against firmware and EC overwrites.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, warn};

use super::limits::{PowerLimitParams, write_mmio_pl1_pl2, write_msr_pl1_pl2};
use super::modules::{load_intel_mchbar_blob, load_intel_msr_blob};
use super::open_handle;

/// Sync thread that continuously writes MSR 0x610 and MMIO to counter
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
    pub(super) fn start(
        &mut self,
        params: PowerLimitParams,
        external_alive: Arc<AtomicBool>,
    ) -> Result<(), &'static str> {
        self.stop();

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let alive = Arc::new(AtomicBool::new(false));
        let alive_clone = alive.clone();
        let alive_for_thread = alive.clone();
        let external_clone = external_alive.clone();
        let external_for_thread = external_alive.clone();

        let handle = std::thread::Builder::new()
            .name("cpu-power-sync".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    sync_thread_main(running_clone, alive_for_thread, external_for_thread, params);
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

    /// Stops sync thread (single shutdown helper; Drop reuses it).
    /// The worker wakes within ~100ms (interruptible sleep), so joining
    /// under the outer `sync_thread` lock cannot stall the UI.
    fn shutdown(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.alive.store(false, Ordering::Release);
    }

    /// Stops sync thread.
    fn stop(&mut self) {
        self.shutdown();
    }
}

impl Drop for SyncThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Sync thread main loop; writes MSR and MMIO every 250ms.
fn sync_thread_main(
    running: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    external_alive: Arc<AtomicBool>,
    params: PowerLimitParams,
) {
    // Load IntelMSR module and open a persistent handle.
    let msr_blob = match load_intel_msr_blob() {
        Ok(b) => b,
        Err(e) => {
            warn!("Sync thread: failed to load IntelMSR module: {}", e);
            return;
        }
    };

    let msr_handle = match open_handle(&msr_blob) {
        Ok(h) => h,
        Err(e) => {
            warn!("Sync thread: failed to open MSR handle: {}", e);
            return;
        }
    };

    // Load IntelMCHBAR for MMIO write (may fail).
    let mchbar_handle = match load_intel_mchbar_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            warn!(
                "Sync thread: MMIO write unavailable ({}), will write MSR only",
                e
            );
            None
        }
    };
    alive.store(true, Ordering::Release);
    external_alive.store(true, Ordering::Release);

    debug!(
        "Sync thread started: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s) mmio={}",
        params.pl1_watts,
        params.pl1_time_s,
        params.pl2_watts,
        params.pl2_time_s,
        mchbar_handle.is_some()
    );

    // Track consecutive failures; keep retrying despite overrides.
    let mut write_failures: u32 = 0;
    let mut mmio_write_failures: u32 = 0;
    while running.load(Ordering::Relaxed) {
        // Unconditionally re-assert limits every 250ms. The whole point of
        // this thread is to win against firmware/EC overwrites.
        match write_msr_pl1_pl2(&msr_handle, &params) {
            Ok(()) => {
                write_failures = 0;
            }
            Err(e) => {
                write_failures += 1;
                if write_failures <= 5 {
                    warn!("Sync thread MSR write failed: {}", e);
                } else if write_failures == 6 {
                    warn!(
                        "Sync thread MSR write keeps failing ({}), suppressing further warnings",
                        e
                    );
                }
            }
        }
        if let Some(ref mchbar) = mchbar_handle {
            match write_mmio_pl1_pl2(mchbar, &params) {
                Ok(()) => {
                    mmio_write_failures = 0;
                }
                Err(e) => {
                    mmio_write_failures += 1;
                    if mmio_write_failures <= 5 {
                        warn!("Sync thread MMIO write failed: {}", e);
                    } else if mmio_write_failures == 6 {
                        warn!(
                            "Sync thread MMIO write keeps failing ({}), suppressing further warnings",
                            e
                        );
                    }
                }
            }
        }
        // Circuit breaker: BIOS-locked register will never succeed; stop after 30 consecutive failures
        if write_failures >= 30 {
            warn!("Sync thread giving up after 30 consecutive MSR failures (likely BIOS-locked)");
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
