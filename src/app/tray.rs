use super::{App, Message, apply_quit_charge_limit, refresh_cpu_power_task, run_ec_task};
use crate::system_info;
use crate::types::FanControlMode;
use crate::util::read_lock;
use iced::Task;
use std::sync::atomic::Ordering;
use tracing::{debug, warn};

impl App {
    pub(crate) fn handle_tray_message(&mut self, message: &Message) -> Option<Task<Message>> {
        match *message {
            Message::CloseRequested(id) => {
                self.closing_window_id = Some(id);
                // No tray on startup failure; quit directly instead of minimizing.
                if self.startup_error.is_some() {
                    self.tray.shutdown();
                    self.state.lifecycle.shutdown.store(true, Ordering::Release);
                    return Some(self.close_window());
                }
                Some(Task::perform(async {}, |_| Message::MinimizeToTray))
            }
            Message::MinimizeToTray => {
                tracing::info!("MinimizeToTray: tray_initialized={}", self.tray_initialized);
                self.iconic_check_count = 0;
                self.pending_minimize_to_tray = true;
                if !self.tray_initialized {
                    if let Some(hwnd) = system_info::find_window_by_title("Framework Crate") {
                        tracing::info!("Found window HWND: {}", hwnd);
                        self.tray.init(hwnd);
                        self.tray_initialized = true;
                    } else {
                        tracing::warn!("Could not find window by title, will retry on next tick");
                    }
                }
                if self.tray_initialized {
                    let icon_ready = self.tray.check_icon_ready();
                    if !icon_ready && !self.icon_create_in_flight {
                        self.tray.show_icon_async();
                        self.icon_create_in_flight = true;
                    }
                    if icon_ready {
                        self.icon_create_in_flight = false;
                        self.tray.hide_window();
                        self.state.lifecycle.visible.store(false, Ordering::Release);
                        self.pending_minimize_to_tray = false;
                        tracing::info!("Window hidden, tray icon visible");
                    } else {
                        tracing::info!("Tray icon creation in progress, will hide on next tick");
                    }
                } else {
                    tracing::warn!("Cannot minimize to tray: HWND not found, pending retained");
                }
                Some(Task::none())
            }
            Message::RestoreFromTray => {
                self.tray.mark_restored();
                self.pl_fields_dirty = false;
                self.state.lifecycle.visible.store(true, Ordering::Release);
                self.mark_dirty();
                self.icon_create_in_flight = false;
                self.iconic_check_count = 0;
                // Clear pending hide; window is visible again.
                self.pending_minimize_to_tray = false;
                // Immediately refresh CPU power on restore so display is current
                // without waiting for the next 5s poll (which was paused while hidden).
                if self.cpu_power_supported() {
                    let cpu_power = self.state.cpu_power.clone();
                    return Some(refresh_cpu_power_task(cpu_power, || {}));
                }
                Some(Task::none())
            }
            Message::TrayQuit => {
                self.tray.mark_restored();
                self.tray.restore_window();
                self.state.lifecycle.visible.store(true, Ordering::Release);
                let config = read_lock(&self.state.lifecycle.config);
                if matches!(
                    config.fan.mode,
                    FanControlMode::Manual | FanControlMode::Curve
                ) {
                    if config.fan.mode == FanControlMode::Curve {
                        // In Curve mode use actual curve duty, not stale manual value
                        let cur = self.state.fan.last_applied_duty.load(Ordering::Acquire) as u32;
                        self.quit_duty_value = if cur > 0 { cur } else { 50 }.clamp(0, 100);
                    } else {
                        self.quit_duty_value = config
                            .fan
                            .manual
                            .as_ref()
                            .map(|m| m.duty_pct)
                            .unwrap_or(50)
                            .clamp(0, 100);
                    }
                    self.show_quit_warning = true;
                } else {
                    self.tray.shutdown();
                    self.state.lifecycle.shutdown.store(true, Ordering::Release);
                    let limit = read_lock(&self.state.lifecycle.config)
                        .battery
                        .charge_limit_max_pct;
                    return Some(run_ec_task(
                        &self.state.system.ec_client,
                        Message::QuitShutdown,
                        move |ec| apply_quit_charge_limit(&ec, &limit),
                    ));
                }
                Some(Task::none())
            }
            Message::TrayEventReceived(event) => {
                match event {
                    crate::tray::TrayEvent::Show => {
                        Some(Task::perform(async {}, |_| Message::RestoreFromTray))
                    }
                    crate::tray::TrayEvent::MenuShow => {
                        Some(Task::perform(async {}, |_| Message::RestoreFromTray))
                    }
                    crate::tray::TrayEvent::Restored => {
                        self.tray.mark_restored();
                        Some(Task::none())
                    }
                    crate::tray::TrayEvent::MenuQuit => {
                        Some(Task::perform(async {}, |_| Message::TrayQuit))
                    }
                    crate::tray::TrayEvent::PowerResumed => {
                        let now = crate::util::monotonic_ms();
                        self.state
                            .lifecycle
                            .last_resume_ts
                            .store(now, Ordering::Release);
                        tracing::warn!(
                            "[RESUME] System resumed from sleep/hibernate at monotonic tick {}",
                            now
                        );
                        if !self.cpu_power_supported() {
                            return Some(Task::none());
                        }
                        // Preserve desired sync params before hardware state is re-read.
                        let was_sync = self.state.cpu_power.sync_enabled.load(Ordering::Acquire);
                        let desired = self.state.cpu_power.desired_sync_params();
                        // Stop sync before refresh to avoid racing with old thread.
                        if was_sync {
                            self.state.cpu_power.stop_sync();
                        } else {
                            self.pl_custom_applied.store(false, Ordering::Release);
                        }
                        let cpu_power = self.state.cpu_power.clone();
                        let bios = cpu_power.bios_defaults();
                        let custom_applied = self.pl_custom_applied.clone();
                        let after = {
                            let cpu_power = cpu_power.clone();
                            move || {
                                if let Some(params) = desired {
                                    // Resume sync with user-desired params, not post-resume readback.
                                    if was_sync {
                                        let _ = cpu_power.start_sync(params);
                                        return;
                                    }
                                }
                                // Fallback: no desired sync, check current flag.
                                if cpu_power.sync_enabled.load(Ordering::Acquire) {
                                    let info = cpu_power.snapshot();
                                    let _ = cpu_power.start_sync(info.msr_limit_params());
                                } else if custom_applied.load(Ordering::Acquire) {
                                    cpu_power.stop_sync();
                                    if let Some(bios) = bios {
                                        let ac_now = crate::cpu_power::read_ac_present();
                                        let source_matches = bios.captured_on_ac == ac_now;
                                        if !source_matches {
                                            debug!(
                                                "Resume: BIOS snapshot captured on {} but now on {}; skipping restore",
                                                if bios.captured_on_ac { "AC" } else { "battery" },
                                                if ac_now { "AC" } else { "battery" }
                                            );
                                        } else {
                                            match crate::cpu_power::write_bios_defaults(&bios) {
                                                Ok(crate::cpu_power::BiosRestore::Full) => {}
                                                Ok(crate::cpu_power::BiosRestore::Partial(e)) => {
                                                    warn!(
                                                        "Resume restore partial (MSR done): {}",
                                                        e
                                                    )
                                                }
                                                Err(e) => {
                                                    warn!("Resume write failed: {}", e)
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        };
                        Some(refresh_cpu_power_task(cpu_power, after))
                    }
                }
            }
            _ => None,
        }
    }
}
