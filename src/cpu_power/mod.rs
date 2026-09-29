mod bios;
mod ffi;
mod limits;
mod modules;
mod read;
mod sync;

pub use bios::{BiosDefaults, publish_ac_snapshot, read_ac_present};
pub use ffi::{
    install_pawnio, invalidate_pawnio_version, is_pawnio_installed, pawnio_version, reset_dll_fns,
    update_pawnio, update_pawnio_modules,
};
pub use limits::{
    BiosRestore, CpuPowerInfo, CpuPowerUnavailable, PowerLimitParams, write_bios_defaults,
    write_msr_pl1_pl2_public,
};
pub use modules::{
    download_and_extract_modules, modules_downloaded, open_modules_dir, pawnio_modules_version,
    redetect_modules,
};

use bios::{PersistedBios, load_persisted_bios_defaults, persist_bios_defaults};
use read::read_cpu_power;
use sync::SyncThread;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

use crate::util::with_write_lock;

/// Shared CPU power state.
#[derive(Clone)]
pub struct CpuPowerState {
    pub info: Arc<parking_lot::RwLock<Arc<CpuPowerInfo>>>,
    pub available: Arc<AtomicBool>,
    pub sync_enabled: Arc<AtomicBool>,
    sync_thread: Arc<parking_lot::Mutex<SyncThread>>,
    /// Outside mutex so liveness check never blocks on join.
    sync_alive: Arc<AtomicBool>,
    sync_start_ms: Arc<std::sync::atomic::AtomicU64>,
    bios: Arc<parking_lot::RwLock<Arc<Option<BiosDefaults>>>>,
    desired_sync: Arc<parking_lot::RwLock<Option<PowerLimitParams>>>,
}

impl Default for CpuPowerState {
    fn default() -> Self {
        Self {
            info: Arc::new(parking_lot::RwLock::new(Arc::new(CpuPowerInfo::default()))),
            available: Arc::new(AtomicBool::new(false)),
            sync_enabled: Arc::new(AtomicBool::new(false)),
            sync_thread: Arc::new(parking_lot::Mutex::new(SyncThread::new())),
            sync_alive: Arc::new(AtomicBool::new(false)),
            sync_start_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            bios: Arc::new(parking_lot::RwLock::new(Arc::new(None))),
            desired_sync: Arc::new(parking_lot::RwLock::new(None)),
        }
    }
}

impl CpuPowerState {
    pub fn refresh(&self) {
        let info = read_cpu_power();
        let is_available = info.available;
        self.available.store(is_available, Ordering::Release);
        with_write_lock(&self.info, |guard| {
            *guard = Arc::new(info);
        });
        // If we still lack the original BIOS snapshot and now have live data,
        // capture it — on first ever run this persists the true factory values
        // so Reset stays correct after the user has modified them.
        if is_available && self.bios_defaults().is_none() {
            self.init_bios_defaults();
        }
    }

    /// Captures BIOS defaults; persists on first run for Reset.
    pub fn init_bios_defaults(&self) {
        // Already in memory — nothing to do.
        if self.bios_defaults().is_some() {
            return;
        }
        // Try loading persisted originals. Each outcome decides whether the
        // live values below may be written over them, so they are not
        // collapsed into "no file".
        match load_persisted_bios_defaults() {
            PersistedBios::Loaded(persisted) => {
                with_write_lock(&self.bios, |guard| {
                    *guard = Arc::new(Some(persisted));
                });
                info!(
                    "Loaded persisted BIOS defaults: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s)",
                    persisted.pl1_watts,
                    persisted.pl1_time_s,
                    persisted.pl2_watts,
                    persisted.pl2_time_s
                );
                return;
            }
            // Corrupt, and already copied aside: overwriting would destroy the
            // only remaining record of the factory limits.
            PersistedBios::Unusable => {
                warn!(
                    "bios_defaults.toml exists but could not be loaded; refusing to overwrite with live values"
                );
                return;
            }
            // The file can be neither read nor written, so there is nothing to
            // preserve and nothing to capture into.
            PersistedBios::Unavailable => {
                warn!("cannot resolve the config directory; skipping BIOS defaults capture");
                return;
            }
            // First run, or the user deleted the file to re-capture.
            PersistedBios::Missing => {}
        }
        let info = self.snapshot();
        if !info.available {
            return;
        }
        // Record power source to prevent cross-source restore on resume.
        let captured_on_ac = read_ac_present();
        let defaults = BiosDefaults {
            pl1_watts: info.pl1_msr,
            pl1_enabled: info.pl1_msr_enabled,
            pl1_clamped: info.pl1_msr_clamped,
            pl1_time_s: info.pl1_time_s,
            pl2_watts: info.pl2_msr,
            pl2_enabled: info.pl2_msr_enabled,
            pl2_clamped: info.pl2_msr_clamped,
            pl2_time_s: info.pl2_time_s,
            pl1_mmio_watts: info.pl1_mmio,
            pl1_mmio_enabled: info.pl1_mmio_enabled,
            pl1_mmio_clamped: info.pl1_mmio_clamped,
            pl1_mmio_time_s: info.pl1_mmio_time_s,
            pl2_mmio_watts: info.pl2_mmio,
            pl2_mmio_enabled: info.pl2_mmio_enabled,
            pl2_mmio_clamped: info.pl2_mmio_clamped,
            pl2_mmio_time_s: info.pl2_mmio_time_s,
            power_unit: info.power_unit,
            time_unit: info.time_unit,
            captured_on_ac,
        };
        if let Err(e) = persist_bios_defaults(&defaults) {
            warn!("Failed to persist BIOS defaults: {}", e);
        } else {
            info!(
                "Persisted original BIOS defaults: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s)",
                defaults.pl1_watts, defaults.pl1_time_s, defaults.pl2_watts, defaults.pl2_time_s
            );
        }
        with_write_lock(&self.bios, |guard| {
            *guard = Arc::new(Some(defaults));
        });
        info!(
            "BIOS defaults captured: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s)",
            defaults.pl1_watts, defaults.pl1_time_s, defaults.pl2_watts, defaults.pl2_time_s
        );
    }

    /// Returns BIOS defaults captured at startup.
    pub fn bios_defaults(&self) -> Option<BiosDefaults> {
        *crate::util::read_lock(&self.bios)
    }

    pub fn snapshot(&self) -> Arc<CpuPowerInfo> {
        crate::util::read_lock(&self.info).clone()
    }

    /// Starts the sync thread that continuously re-asserts PL1/PL2.
    ///
    /// Holds the `sync_thread` lock across the entire stop-old + join + start
    /// sequence so a concurrent start (e.g. `CpuPowerSyncStart` racing
    /// `CpuPowerApplied`, or a resume handler) cannot slip into the gap between
    /// taking the old handle and starting a new one and spawn two sync threads
    /// writing MSR/MMIO at once. The sync loop never takes this lock, so
    /// joining under it cannot deadlock.
    pub fn start_sync(&self, params: PowerLimitParams) -> Result<(), &'static str> {
        let mut thread = self.sync_thread.lock();
        thread.join_running();
        thread.start(params, Arc::clone(&self.sync_alive))?;
        self.sync_enabled.store(true, Ordering::Release);
        self.sync_start_ms
            .store(crate::util::monotonic_ms(), Ordering::Release);
        *self.desired_sync.write() = Some(params);
        Ok(())
    }

    /// Checks if sync thread is still alive (lock-free).
    pub fn is_sync_alive(&self) -> bool {
        self.sync_alive.load(Ordering::Acquire)
    }

    /// Whether sync is considered dead, with startup grace period.
    pub fn is_sync_dead(&self) -> bool {
        if !self.sync_enabled.load(Ordering::Acquire) {
            return false;
        }
        if self.sync_alive.load(Ordering::Acquire) {
            return false;
        }
        let start = self.sync_start_ms.load(Ordering::Acquire);
        // Grace period after start_sync before declaring dead (thread startup/handshake)
        if start != 0 && crate::util::monotonic_ms().saturating_sub(start) < 1500 {
            return false;
        }
        true
    }

    /// Attempts to revive a dead sync thread using the last desired params.
    /// Returns true if a restart was actually initiated on this call. Throttled
    /// to once per 5s so a persistently failing handle init does not spawn a
    /// new thread every UI tick. No-ops (returns false) during the 1.5s startup
    /// grace period or while a restart is still in progress.
    pub fn try_restart_if_dead(&self) -> bool {
        if !self.is_sync_dead() {
            return false;
        }
        let now = crate::util::monotonic_ms();
        let last = self.sync_start_ms.load(Ordering::Acquire);
        if last != 0 && now.saturating_sub(last) < 5000 {
            return false;
        }
        let params = match *self.desired_sync.read() {
            Some(p) => p,
            None => return false,
        };
        match self.start_sync(params) {
            Ok(()) => {
                tracing::info!("Restarted dead CPU power sync thread");
                true
            }
            Err(e) => {
                warn!("Failed to restart CPU power sync thread: {}", e);
                false
            }
        }
    }

    /// Stops sync thread. Holds the `sync_thread` lock across the whole teardown
    /// so a concurrent `start_sync` cannot re-spawn a thread in the gap and then
    /// be incorrectly marked stopped (which would disable a freshly started sync).
    pub fn stop_sync(&self) {
        let mut thread = self.sync_thread.lock();
        thread.shutdown();
        drop(thread);
        self.sync_alive.store(false, Ordering::Release);
        self.sync_enabled.store(false, Ordering::Release);
        // Keep desired_sync for resume restore.
    }

    pub(crate) fn desired_sync_params(&self) -> Option<PowerLimitParams> {
        *self.desired_sync.read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exercises the start_sync/stop_sync locking path: the calls must serialize
    // cleanly with no panic and no duplicate live threads.
    //
    // Ignored by default because it is not hermetic. With PawnIO installed,
    // start_sync opens a real MSR handle and the sync thread re-asserts
    // PL1/PL2 against the host CPU every 250ms until stop_sync, so a plain
    // `cargo test` would rewrite the machine's power limits as a side effect.
    //
    // The assertions only carry weight on a machine where start_sync actually
    // succeeds; without PawnIO every call returns Err and the state stays at
    // its default, so they hold trivially. Run it deliberately with:
    //     cargo test -- --ignored concurrent_start_stop_sync_is_safe
    #[test]
    #[ignore = "writes real PL1/PL2 to the host CPU through PawnIO"]
    fn concurrent_start_stop_sync_is_safe() {
        let state = CpuPowerState::default();
        let p = PowerLimitParams {
            pl1_watts: 15.0,
            pl1_enabled: true,
            pl1_clamped: false,
            pl1_time_s: 28.0,
            pl2_watts: 35.0,
            pl2_enabled: true,
            pl2_clamped: false,
            pl2_time_s: 28.0,
            power_unit: 0.125,
            time_unit: 0.0009765625,
        };
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = state.clone();
            handles.push(std::thread::spawn(move || {
                let _ = s.start_sync(p);
                s.stop_sync();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(!state.is_sync_alive());
        assert!(
            !state
                .sync_enabled
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn try_restart_if_dead_safe_without_params() {
        let state = CpuPowerState::default();
        // sync_enabled has to be set, or is_sync_dead() returns false and the
        // missing-params guard below is never reached. sync_start_ms stays 0 so
        // the startup grace period does not short-circuit either.
        state
            .sync_enabled
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(state.is_sync_dead(), "precondition: sync must look dead");
        // Dead-looking but with nothing to restart from: must not restart, and
        // must not panic.
        assert!(!state.try_restart_if_dead());
        assert!(
            state
                .sync_enabled
                .load(std::sync::atomic::Ordering::Acquire),
            "a no-op restart must leave the flag alone"
        );
    }

    // The 5s restart throttle has no test. Both guards compare against
    // monotonic_ms(), which counts from the first call in the process, so
    // reaching the window between the 1.5s liveness grace period and the 5s
    // throttle would need the suite to still be running 1.5s in. It finishes in
    // well under a second, and a saturating_sub on a clock that has not reached
    // 2000ms yet yields 0, which trips the grace check instead. Covering this
    // needs an injected clock, which is more machinery than the guard is worth.
}
