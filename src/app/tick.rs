use super::{App, Message, refresh_cpu_power_task};
use crate::style::{IDLE_THRESHOLD_MS, UI_HIDDEN_INTERVAL_MS, UI_IDLE_INTERVAL_MS};
use crate::system_info;
use iced::Task;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tracing::warn;

impl App {
    pub(crate) fn handle_tick_message(&mut self) -> Task<Message> {
        let elapsed = self.last_tick.elapsed().as_millis() as u64;
        if elapsed < self.tick_interval_ms {
            return tick_task(self.tick_interval_ms - elapsed);
        }
        self.last_tick = Instant::now();
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
            .load(Ordering::Relaxed);

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
            self.cached_snapshot = Some(crate::views::ViewSnapshot::from_app(self));
            self.state
                .lifecycle
                .view_dirty
                .store(false, Ordering::Release);
            // Ensure CPU power is initialized if needed (non-blocking)
            if self.cpu_power_supported() {
                let info = self.state.cpu_power.snapshot();
                if !info.available {
                    let cpu_power = self.state.cpu_power.clone();
                    return Task::batch([
                        refresh_cpu_power_task(cpu_power, || {}),
                        tick_task(self.tick_interval_ms),
                    ]);
                }
            }
        }

        // AC→battery: reset PL1/PL2 to BIOS defaults. Only clear pending after successful schedule.
        if self.cpu_power_supported()
            && self
                .state
                .lifecycle
                .pl_reset_pending
                .load(Ordering::Acquire)
            && self.pl_custom_applied.load(Ordering::Acquire)
        {
            if self.state.cpu_power.bios_defaults().is_none() {
                tracing::warn!(
                    "AC→battery reset pending but BIOS defaults unavailable, will retry"
                );
            } else {
                self.state
                    .lifecycle
                    .pl_reset_pending
                    .store(false, Ordering::Release);
                tracing::info!("AC→battery: resetting PL1/PL2 to BIOS defaults");
                return Task::batch([
                    self.handle_cpu_power_sync_reset(),
                    tick_task(self.tick_interval_ms),
                ]);
            }
        }

        let now_ms = crate::util::monotonic_ms();
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
            && let Some(hwnd) = system_info::find_window_by_title("Framework Crate")
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
                // Validate HWND every 5s to avoid frequent syscalls.
                const HWND_CHECK_INTERVAL_MS: u64 = 5000;
                if now_ms.saturating_sub(self.last_hwnd_check_ts) >= HWND_CHECK_INTERVAL_MS {
                    self.last_hwnd_check_ts = now_ms;
                    if !system_info::is_window(self.tray.hwnd()) {
                        tracing::warn!("HWND {} invalid, reinitializing tray", self.tray.hwnd());
                        if let Some(hwnd) = system_info::find_window_by_title("Framework Crate") {
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
                                return Task::batch([
                                    Task::perform(async {}, |_| Message::MinimizeToTray),
                                    tick_task(next_ms),
                                ]);
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
                    return Task::batch([task, tick]);
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

        // Detect sync thread death (MSR write failure) with grace period.
        // Restart it from the last desired params; if none are available the
        // death is permanent and we stop reporting it as syncing.
        if self.state.cpu_power.is_sync_dead() {
            let restarted = self.state.cpu_power.try_restart_if_dead();
            if !restarted && self.state.cpu_power.desired_sync_params().is_none() {
                self.state
                    .cpu_power
                    .sync_enabled
                    .store(false, Ordering::Release);
                warn!("Sync thread exited and cannot be restarted (no desired params)");
            }
            // Reflect state change (or pending retry) in the UI.
            self.mark_dirty();
        }

        tick_task(next_ms)
    }

    pub(crate) fn autosize_task(&self) -> Option<Task<Message>> {
        if !self.init_complete || self.show_settings || self.show_quit_warning || self.height_set {
            return None;
        }
        let id = self.window_id?;
        let current = self.window_height?;
        let target = *self.content_height.lock();
        let target = target?;
        let mut target = target.min(super::AUTO_MAX_HEIGHT + 25.0);
        if let Some((_, work_h)) = crate::system_info::work_area_size() {
            let max_h = (work_h as f32 - 40.0).max(400.0);
            target = target.min(max_h);
        }
        let width = if let Some((work_w, _)) = crate::system_info::work_area_size() {
            super::AUTO_WIDTH.min(work_w as f32 - 20.0)
        } else {
            super::AUTO_WIDTH
        };
        if (target - current).abs() > 0.5 {
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

pub(crate) fn tick_task(ms: u64) -> Task<Message> {
    Task::perform(
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        },
        |_| Message::Tick,
    )
}
