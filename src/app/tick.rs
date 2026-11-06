use super::{App, Message, PL_RESET_RETRY_MS, refresh_cpu_power_task, tick_task};
use crate::style::{IDLE_THRESHOLD_MS, UI_HIDDEN_INTERVAL_MS, UI_IDLE_INTERVAL_MS};
use crate::system_info;
use crate::util::read_lock;
use iced::Task;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tracing::warn;

fn reset_retry_allowed(now_ms: u64, retry_at_ms: u64) -> bool {
    retry_at_ms == 0 || now_ms >= retry_at_ms
}

impl App {
    pub(crate) fn handle_tick_message(&mut self) -> Task<Message> {
        let elapsed = self.last_tick.elapsed().as_millis() as u64;
        if elapsed < self.tick_interval_ms {
            return tick_task(self.tick_interval_ms - elapsed);
        }
        self.last_tick = Instant::now();
        // Calibration wizard owns EC writes while active; dispatch its step
        // writes ahead of everything else (all other tick duties retry, so
        // yielding one tick to it is harmless).
        if let Some(task) = self.poll_calibration() {
            return Task::batch([task, tick_task(self.tick_interval_ms)]);
        }
        // Debounce curve_full_points recomputation 100ms after last edit.
        if self.pending_curve_update && self.last_curve_edit_ts.elapsed().as_millis() >= 100 {
            self.pending_curve_update = false;
            self.update_curve_full_points();
        }
        self.cli_present = self.state.system.cli_available.load(Ordering::Acquire);
        self.config_save_failed = self
            .state
            .lifecycle
            .bg_config_save_failed
            .load(Ordering::Relaxed)
            && !self.config_save_dismissed;

        // Versions can arrive after InitComplete (the background retry path).
        // Rebuild the header so the device name / BIOS line are not stale.
        let header_stale = {
            let versions = crate::util::read_lock(&self.state.system.versions);
            versions.as_ref().as_ref() != self.header_versions.as_ref()
        };
        if header_stale {
            self.rebuild_header_info();
            self.mark_dirty();
        }

        // Startup recovery: background loop may have re-established EC after initial failure.
        if self.startup_error.is_some()
            && self.state.system.cli_available.load(Ordering::Acquire)
            && crate::util::read_lock(&self.state.system.ec_client).is_some()
        {
            tracing::info!("EC recovered after startup failure, clearing error");
            self.startup_error = None;
            self.init_complete = true;
            self.rebuild_header_info();
            self.rebuild_sensor_cache();
            self.infer_pl_custom_applied();
            self.cached_snapshot = Some(crate::views::ViewSnapshot::from_app(self));
            self.state
                .lifecycle
                .view_dirty
                .store(false, Ordering::Release);
        }

        // Ensure CPU power is initialized if needed (non-blocking). Runs
        // every tick (not only startup recovery): PawnIO/modules often become
        // ready after the one-shot startup refresh already filled empties.
        // Note: this must not early-return. The tray init/update, PL-reset,
        // and sync-death checks below must run even while CPU power is
        // unavailable; otherwise a machine without PawnIO starves them every
        // tick. Queue at most one catch-up refresh and continue.
        let source_changed = if self.cpu_power_supported()
            && self.state.cpu_power.snapshot().available
            && !self.pl_capture_in_flight
        {
            match crate::cpu_power::read_ac_present() {
                Some(source) if self.pl_capture_source != Some(source) => {
                    // One refresh per observed source so the MMIO wall can be
                    // recorded for both plugged-in and battery states. The
                    // in-flight flag retries if the refresh cannot complete.
                    self.pl_capture_in_flight = true;
                    // A source change is a fresh start for auto-profile retries.
                    self.pl_profile_retry_failures = 0;
                    true
                }
                _ => false,
            }
        } else {
            false
        };
        let mut cpu_power_refresh: Option<Task<Message>> = if self.cpu_power_supported() {
            let info = self.state.cpu_power.snapshot();
            if !info.available {
                // Keep the latch open while hardware is missing so the
                // catch-up fires again once it appears.
                self.pl_fields_primed = false;
                let cpu_power = self.state.cpu_power.clone();
                Some(refresh_cpu_power_task(cpu_power, || {}))
            } else if !self.pl_fields_primed || source_changed {
                // Hardware just appeared (the background poll brought it
                // up without a refresh message), or the AC/battery source
                // changed: refresh once so the boxes and the MMIO capture
                // match the new state. Later refreshes stay explicit-only,
                // so in-progress typing is never clobbered by the poll loop.
                let cpu_power = self.state.cpu_power.clone();
                Some(refresh_cpu_power_task(cpu_power, || {}))
            } else {
                None
            }
        } else {
            None
        };

        let now_ms = crate::util::monotonic_ms();
        let reset_retry_at = self
            .state
            .lifecycle
            .pl_reset_retry_at_ms
            .load(Ordering::Acquire);
        let idle = now_ms.saturating_sub(
            self.state
                .lifecycle
                .last_interaction_ts
                .load(Ordering::Acquire),
        ) > IDLE_THRESHOLD_MS;
        let visible = self.state.lifecycle.visible.load(Ordering::Acquire);
        let next_ms = if !visible {
            UI_HIDDEN_INTERVAL_MS
        } else if idle {
            UI_IDLE_INTERVAL_MS
        } else {
            self.tick_interval_ms
        };

        if !self.tray_initialized
            && let Some(hwnd) = system_info::find_main_window_by_pid()
        {
            self.tray.init(hwnd);
            self.tray_initialized = true;
            tracing::info!("Tray initialized with HWND: {}", hwnd);
            self.tray.show_icon_async();
        }

        if self.tray_initialized {
            // Complete pending tray reinit before liveness check.
            self.tray.poll_reinit();
            if !self.tray.is_alive() {
                // Tray pump died; reset state and cancel pending minimize.
                tracing::warn!("Tray message pump thread exited unexpectedly");
                self.tray_initialized = false;
                self.pending_minimize_to_tray = false;
                self.tray.reset();
            } else {
                // Retry icon creation until tray thread is ready.
                self.tray.show_icon_async();
                // Hover text carries the hottest non-battery temp; throttled
                // inside update_tip so ticks stay cheap.
                if self.tray.icon_loaded() {
                    let max_temp = read_lock(&self.state.thermal.data)
                        .as_ref()
                        .as_ref()
                        .and_then(|t| crate::types::curve_control_temp(&t.temps, &[]));
                    self.tray
                        .update_tip(&crate::tray::format_tray_tip(max_temp));
                }
                // Validate HWND every 5s to avoid frequent syscalls.
                const HWND_CHECK_INTERVAL_MS: u64 = 5000;
                if now_ms.saturating_sub(self.last_hwnd_check_ts) >= HWND_CHECK_INTERVAL_MS {
                    self.last_hwnd_check_ts = now_ms;
                    if !system_info::is_window(self.tray.hwnd()) {
                        tracing::warn!("HWND {} invalid, reinitializing tray", self.tray.hwnd());
                        if let Some(hwnd) = system_info::find_main_window_by_pid() {
                            self.tray.request_reinit(hwnd);
                            self.tray.show_icon_async();
                        } else {
                            // Reset pump state so next tick re-initializes.
                            self.tray.reset();
                            self.tray_initialized = false;
                            tracing::error!("Cannot find window after HWND invalidation");
                        }
                    }
                    if !self.tray.is_recently_restored()
                        && self.state.lifecycle.visible.load(Ordering::Acquire)
                    {
                        if system_info::is_iconic(self.tray.hwnd()) {
                            self.iconic_check_count += 1;
                            if self.iconic_check_count >= 2 {
                                tracing::info!(
                                    "Window minimized (iconic_check_count={}), auto-minimizing to tray",
                                    self.iconic_check_count
                                );
                                self.iconic_check_count = 0;
                                let task = Task::perform(async {}, |_| Message::MinimizeToTray);
                                let tick = tick_task(next_ms);
                                return match cpu_power_refresh.take() {
                                    Some(refresh) => Task::batch([task, tick, refresh]),
                                    None => Task::batch([task, tick]),
                                };
                            }
                        } else {
                            self.iconic_check_count = 0;
                        }
                    }
                }
                if let Some(event) = self.tray.receive_event() {
                    match &event {
                        crate::tray::TrayEvent::Show | crate::tray::TrayEvent::MenuShow => {
                            self.tray.mark_restored();
                            tracing::info!("Tray show event received, marking restored");
                        }
                        _ => {}
                    }
                    let task = Task::perform(async move { event }, Message::TrayEventReceived);
                    let tick = tick_task(next_ms);
                    return match cpu_power_refresh.take() {
                        Some(refresh) => Task::batch([task, tick, refresh]),
                        None => Task::batch([task, tick]),
                    };
                }
                if self.pending_minimize_to_tray
                    && self.tray.check_icon_ready()
                    && self.state.lifecycle.visible.load(Ordering::Acquire)
                {
                    self.tray.hide_window();
                    self.state.lifecycle.visible.store(false, Ordering::Release);
                    self.pending_minimize_to_tray = false;
                    self.icon_create_in_flight = false;
                    tracing::info!("Tray icon ready, window hidden");
                }
            }
        }

        // Keep-alive profiles: when the About toggle is on, apply the saved
        // profile for the current source on startup and whenever AC↔battery
        // changes. Runs after tray upkeep for the same reason as the reset.
        let mut profile_task = self.maybe_auto_apply_profile(now_ms);

        // AC→battery: reset PL1/PL2 to BIOS defaults. Runs after the tray
        // upkeep so a reset tick never skips tray initialization. The pending
        // flag is cleared by CpuPowerResetDone on success; failures schedule
        // a retry instead of hammering the hardware every tick.
        if self.cpu_power_supported()
            && self
                .state
                .lifecycle
                .pl_reset_pending
                .load(Ordering::Acquire)
            && self.pl_custom_applied.load(Ordering::Acquire)
            && reset_retry_allowed(now_ms, reset_retry_at)
        {
            if self.state.cpu_power.bios_defaults().is_none() {
                self.state
                    .lifecycle
                    .pl_reset_retry_at_ms
                    .store(now_ms.saturating_add(PL_RESET_RETRY_MS), Ordering::Release);
                tracing::warn!(
                    "AC→battery reset pending but BIOS defaults unavailable; retrying in {}s",
                    PL_RESET_RETRY_MS / 1000
                );
            } else {
                tracing::info!("AC→battery: resetting PL1/PL2 to BIOS defaults");
                self.state
                    .lifecycle
                    .pl_reset_retry_at_ms
                    .store(now_ms.saturating_add(PL_RESET_RETRY_MS), Ordering::Release);
                let reset = self.handle_cpu_power_sync_reset();
                let mut tasks = Task::batch([reset, tick_task(self.tick_interval_ms)]);
                if let Some(refresh) = cpu_power_refresh.take() {
                    tasks = Task::batch([tasks, refresh]);
                }
                if let Some(profile) = profile_task.take() {
                    tasks = Task::batch([tasks, profile]);
                }
                return tasks;
            }
        }

        // Detect sync thread death (MSR write failure) with grace period.
        // Restart it from the last desired params; if none are available the
        // death is permanent and we stop reporting it as syncing.
        //
        // While PawnIO/modules are unreadable a restart would fail identically
        // and latch auto-restart off for good, so leave the dead state alone
        // and retry once the hardware becomes readable again.
        if self.state.cpu_power.is_sync_dead() && self.state.cpu_power.snapshot().available {
            let restarted = self.state.cpu_power.try_restart_if_dead();
            if !restarted && self.state.cpu_power.desired_sync_params().is_none() {
                self.state
                    .cpu_power
                    .sync_enabled
                    .store(false, Ordering::Release);
                warn!("Sync thread exited and cannot be restarted (no desired params)");
            }
            // Auto-restart gave up permanently: clear the persisted intent so
            // the About toggle does not claim to be on, then surface it once.
            // Cleared by the next successful apply/sync/reset.
            if self.state.cpu_power.sync_restart_latched() && self.cpu_power_error.is_none() {
                if self.keep_limits_enabled() {
                    self.mutate_config(|cfg| cfg.cpu_power.keep_limits = false);
                    self.save_config();
                    self.pl_active_source = None;
                }
                self.cpu_power_error = Some(
                    "Keep-alive stopped after repeated failures; turn it on in About to retry."
                        .to_string(),
                );
            }
            // Reflect state change (or pending retry) in the UI.
            self.mark_dirty();
        }

        let mut tasks = tick_task(next_ms);
        if let Some(refresh) = cpu_power_refresh {
            tasks = Task::batch([tasks, refresh]);
        }
        if let Some(profile) = profile_task.take() {
            tasks = Task::batch([tasks, profile]);
        }
        tasks
    }

    pub(crate) fn autosize_task(&self) -> Option<Task<Message>> {
        if !self.init_complete || self.show_settings || self.show_quit_warning || self.height_set {
            return None;
        }
        let id = self.window_id?;
        let current = self.window_height?;
        let bits = self.content_height.load(Ordering::Relaxed);
        if bits == crate::probe::NO_HEIGHT {
            return None;
        }
        let target = f32::from_bits(bits);
        let mut target = target.min(super::AUTO_MAX_HEIGHT + super::WINDOW_CHROME);
        let work_area = crate::system_info::work_area_size();
        if let Some((_, work_h)) = work_area {
            let max_h = (work_h as f32 - super::WORK_AREA_MARGIN_H).max(super::WINDOW_MIN_HEIGHT);
            target = target.min(max_h);
        }
        let width = match work_area {
            Some((work_w, _)) => super::AUTO_WIDTH.min(work_w as f32 - super::WORK_AREA_MARGIN_W),
            None => super::AUTO_WIDTH,
        };
        if (target - current).abs() > super::RESIZE_EPSILON {
            Some(iced::window::resize(id, iced::Size::new(width, target)))
        } else {
            None
        }
    }

    pub(crate) fn mark_dirty(&self) {
        self.state
            .lifecycle
            .view_dirty
            .store(true, Ordering::Release);
        self.state
            .lifecycle
            .view_generation
            .fetch_add(1, Ordering::Release);
    }

    pub(crate) fn maybe_rebuild_snapshot(&mut self) {
        if !self.init_complete {
            return;
        }
        let cur_gen = self.state.lifecycle.view_generation.load(Ordering::Acquire);
        let needs_rebuild = cur_gen != self.cached_gen || self.cached_snapshot.is_none();
        if !needs_rebuild {
            return;
        }
        if !self.state.lifecycle.visible.load(Ordering::Acquire) {
            // Keep dirty until visible.
            return;
        }
        let gen_before = cur_gen;
        self.cached_snapshot = Some(crate::views::ViewSnapshot::from_app(self));
        self.cached_gen = gen_before;
        let cur_after = self.state.lifecycle.view_generation.load(Ordering::Acquire);
        if cur_after == gen_before {
            self.state
                .lifecycle
                .view_dirty
                .store(false, Ordering::Release);
        }
        // else: new updates arrived during build, will rebuild next call
    }
}

#[cfg(test)]
mod tests {
    use super::reset_retry_allowed;

    #[test]
    fn reset_retry_honors_the_backoff_deadline() {
        assert!(reset_retry_allowed(1_000, 0));
        assert!(!reset_retry_allowed(1_000, 2_000));
        assert!(reset_retry_allowed(2_000, 2_000));
        assert!(reset_retry_allowed(3_000, 2_000));
    }
}
