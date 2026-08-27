use super::{App, Message, apply_quit_charge_limit, run_ec_task};
use crate::util::read_lock;
use iced::Task;
use std::sync::atomic::Ordering;
use tracing::warn;

impl App {
    pub(crate) fn handle_quit_message(&mut self, message: &Message) -> Option<Task<Message>> {
        match *message {
            Message::QuitWithRestore => {
                self.show_quit_warning = false;
                self.state.lifecycle.shutdown.store(true, Ordering::Release);
                self.state.cpu_power.stop_sync();
                let limit = read_lock(&self.state.lifecycle.config)
                    .battery
                    .charge_limit_max_pct;
                // Restore fan before quitting; also flush charge limit to EC.
                Some(run_ec_task(
                    &self.state.system.ec_client,
                    Message::QuitShutdown,
                    move |ec| {
                        if let Err(e) = ec.autofanctrl() {
                            warn!("Failed to restore auto fan control on quit: {}", e);
                        }
                        apply_quit_charge_limit(&ec, &limit);
                    },
                ))
            }
            Message::QuitDutyChanged(duty) => {
                self.quit_duty_value = duty.clamp(0, 100);
                Some(Task::none())
            }
            Message::QuitWithDuty => {
                self.show_quit_warning = false;
                self.state.lifecycle.shutdown.store(true, Ordering::Release);
                self.state.cpu_power.stop_sync();
                let duty = self.quit_duty_value;
                let limit = read_lock(&self.state.lifecycle.config)
                    .battery
                    .charge_limit_max_pct;
                // Write quit duty before quitting; also flush charge limit.
                Some(run_ec_task(
                    &self.state.system.ec_client,
                    Message::QuitShutdown,
                    move |ec| {
                        if let Err(e) = ec.set_fan_duty(duty, None) {
                            warn!("Failed to set quit fan duty: {}", e);
                        }
                        apply_quit_charge_limit(&ec, &limit);
                    },
                ))
            }
            Message::QuitWithoutRestore => {
                self.show_quit_warning = false;
                self.tray.shutdown();
                self.state.lifecycle.shutdown.store(true, Ordering::Release);
                self.state.cpu_power.stop_sync();
                let last_duty = self.state.fan.last_applied_duty.load(Ordering::Acquire) as u32;
                let per_fan = read_lock(&self.state.fan.per_fan_duty).clone();
                let unified = self.state.fan.unified_duty.load(Ordering::Acquire);
                let fan_count = self.state.fan.fan_count.load(Ordering::Acquire) as usize;
                let limit = read_lock(&self.state.lifecycle.config)
                    .battery
                    .charge_limit_max_pct;
                // Keep current duty explicitly so it persists after exit (EC duty is volatile)
                Some(run_ec_task(
                    &self.state.system.ec_client,
                    Message::QuitShutdown,
                    move |ec| {
                        if last_duty > 0 {
                            if !unified && fan_count > 1 && !per_fan.is_empty() {
                                for (idx, &duty) in per_fan.iter().enumerate() {
                                    if idx >= fan_count {
                                        continue;
                                    }
                                    let _ = ec.set_fan_duty(duty, Some(idx as u32));
                                }
                            } else {
                                let _ = ec.set_fan_duty(last_duty, None);
                            }
                        }
                        apply_quit_charge_limit(&ec, &limit);
                    },
                ))
            }
            Message::QuitShutdown => {
                self.show_quit_warning = false;
                self.tray.shutdown();
                self.state.cpu_power.stop_sync();
                self.save_config_now();
                Some(self.close_window())
            }
            Message::QuitCanceled => {
                self.show_quit_warning = false;
                Some(Task::none())
            }
            _ => None,
        }
    }
}
