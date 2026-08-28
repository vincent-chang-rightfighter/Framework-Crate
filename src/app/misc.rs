use super::{App, MAX_DEBUG_REPORTS, Message, prune_debug_reports, run_ec_task_result, tick_task};
use crate::util::{read_lock, with_write_lock};
use iced::Task;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

impl App {
    /// Handles the remaining one-off UI/dispatch messages not covered by the
    /// config / cpu_power / tray / quit handlers.
    pub(crate) fn handle_misc_message(&mut self, message: &Message) -> Option<Task<Message>> {
        match message {
            Message::InitComplete => {
                self.init_complete = true;
                self.rebuild_header_info();
                self.rebuild_sensor_cache();
                self.cached_snapshot = Some(crate::views::ViewSnapshot::from_app(self));
                self.state
                    .lifecycle
                    .view_dirty
                    .store(false, Ordering::Release);
                // Hide to tray immediately when launched with --minimized.
                if self.start_minimized {
                    self.start_minimized = false;
                    return Some(Task::batch([
                        Task::perform(async {}, |_| Message::MinimizeToTray),
                        tick_task(0),
                    ]));
                }
                Some(tick_task(0))
            }
            Message::StartupError(msg) => {
                self.startup_error = Some(msg.clone());
                // Keep tick loop running to initialize tray despite startup error.
                Some(tick_task(0))
            }
            Message::WindowResized(id, size) => {
                self.window_id = Some(*id);
                self.window_height = Some(size.height);
                Some(Task::none())
            }
            Message::ToggleSensorSettings => {
                self.show_sensor_settings = !self.show_sensor_settings;
                self.height_set = false;
                Some(Task::none())
            }
            Message::ChartWindowChanged(secs) => {
                if crate::temp_chart::HISTORY_WINDOW_OPTIONS.contains(secs) {
                    self.chart_window_seconds = *secs;
                    with_write_lock(&self.state.thermal.history, |hist| {
                        Arc::make_mut(hist).set_window(*secs);
                    });
                    self.mark_dirty();
                }
                Some(Task::none())
            }
            Message::ToggleCurveSettings => {
                self.show_curve_settings = !self.show_curve_settings;
                self.height_set = false;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::ToggleCpuPowerSettings => {
                self.show_cpu_power_settings = !self.show_cpu_power_settings;
                self.height_set = false;
                // Mark dirty; flag is snapshot-backed.
                self.mark_dirty();
                Some(Task::none())
            }
            Message::SettingsToggled => {
                self.show_settings = !self.show_settings;
                self.height_set = false;
                Some(Task::none())
            }
            Message::KblightChanged(percent) => {
                let percent = *percent;
                let kblight = Arc::clone(&self.state.peripherals.kblight);
                let my_gen = self.kblight_write_gen.fetch_add(1, Ordering::Relaxed) + 1;
                let gen_ref = Arc::clone(&self.kblight_write_gen);
                let task = run_ec_task_result(&self.state.system.ec_client, move |ec| {
                    // Coalesce: if newer request superseded this one, skip hardware.
                    if gen_ref.load(Ordering::Acquire) != my_gen {
                        return Ok(());
                    }
                    ec.kblight_set(percent)?;
                    if gen_ref.load(Ordering::Acquire) != my_gen {
                        return Ok(());
                    }
                    if let Ok(kb) = ec.kblight_get()
                        && gen_ref.load(Ordering::Acquire) == my_gen
                    {
                        with_write_lock(&kblight, |guard| {
                            *guard = Arc::new(Some(kb));
                        });
                    }
                    Ok(())
                });
                self.mark_dirty();
                Some(task)
            }
            Message::FpLedLevelChanged(level) => {
                let level = *level;
                Some(run_ec_task_result(
                    &self.state.system.ec_client,
                    move |ec| ec.fp_led_level_set(level),
                ))
            }
            Message::EcOpResult(err) => {
                // Surface peripheral EC write failure to UI.
                self.ec_op_error = err.clone();
                self.mark_dirty();
                Some(Task::none())
            }
            Message::ToggleBatteryDetails => {
                self.show_battery_details = !self.show_battery_details;
                self.height_set = false;
                Some(Task::none())
            }
            Message::ToggleExpansionCardDebug => {
                self.expansion_card_debug = !self.expansion_card_debug;
                // Mark dirty; snapshot-backed value.
                self.mark_dirty();
                Some(Task::none())
            }
            Message::StartupLaunchToggled(enabled) => {
                self.startup_launch_error = None;
                match crate::system_info::set_startup_launch(*enabled) {
                    Ok(()) => self.startup_launch_enabled = *enabled,
                    Err(e) => {
                        warn!(
                            "Failed to {} startup launch: {}",
                            if *enabled { "enable" } else { "disable" },
                            e
                        );
                        self.startup_launch_error = Some(e);
                    }
                }
                Some(Task::none())
            }
            Message::DismissConfigWarning => {
                self.config_save_failed = false;
                self.config_load_warning = None;
                Some(Task::none())
            }
            Message::CollectDebugInfo => {
                let mut report = String::with_capacity(1024);
                report.push_str("=== Framework Crate Debug Report ===\n\n");
                let platform = *read_lock(&self.state.system.platform);
                report.push_str(&format!("Platform: {:?}\n", platform));
                report.push_str(&format!("Mainboard: {}\n", self.system_info.cpu));
                report.push_str(&format!("RAM: {}\n", self.system_info.mem));
                report.push_str(&format!("OS: {}\n", self.system_info.os));
                report.push_str(&format!(
                    "Display: {} {}\n",
                    self.system_info.screen, self.system_info.refresh_rate
                ));
                if let Some(v) = read_lock(&self.state.system.versions).as_ref() {
                    report.push_str(&format!("BIOS: {:?}\n", v.uefi_version));
                    report.push_str(&format!("EC Firmware: {:?}\n", v.ec_build_version));
                }
                report.push_str(&format!(
                    "framework_lib: {}\n",
                    env!("FRAMEWORK_LIB_VERSION")
                ));
                if let Some(ver) = crate::cpu_power::pawnio_version() {
                    report.push_str(&format!("PawnIO: {}\n", ver));
                }
                report.push_str(&format!(
                    "PawnIO Modules: {}\n",
                    crate::cpu_power::pawnio_modules_version()
                ));
                let config = read_lock(&self.state.lifecycle.config);
                report.push_str(&format!("\nFan Mode: {:?}\n", config.fan.mode));
                report.push_str(&format!(
                    "Fan Duty: {}\n",
                    self.state.fan.last_applied_duty.load(Ordering::Acquire)
                ));
                report.push_str(&format!(
                    "Fan Count: {}\n",
                    self.state.fan.fan_count.load(Ordering::Acquire)
                ));
                report.push_str(&format!(
                    "Unified Duty: {}\n",
                    self.state.fan.unified_duty.load(Ordering::Acquire)
                ));
                if let Some(thermal) = read_lock(&self.state.thermal.data).as_ref() {
                    report.push_str("\n=== Thermal Data ===\n");
                    for (name, temp) in thermal.temps.iter() {
                        report.push_str(&format!("  {}: {}°C\n", name, temp));
                    }
                    report.push_str("\n=== Fan RPM ===\n");
                    for fan in &thermal.fans {
                        report.push_str(&format!("  {}: {} RPM\n", fan.name, fan.rpm));
                    }
                }
                if let Some(battery) = read_lock(&self.state.battery.info).as_ref() {
                    report.push_str("\n=== Battery ===\n");
                    report.push_str(&format!("  SOC: {:?}%\n", battery.power_info.soc_pct));
                    report.push_str(&format!("  AC: {:?}\n", battery.power_info.ac_present));
                    report.push_str(&format!(
                        "  Voltage: {:?}mV\n",
                        battery.power_info.present_voltage_mv
                    ));
                    report.push_str(&format!(
                        "  Rate: {:?}mA\n",
                        battery.power_info.present_rate_ma
                    ));
                }
                let pd_ports = read_lock(&self.state.peripherals.pd_ports);
                if !pd_ports.is_empty() {
                    let history = read_lock(&self.state.peripherals.pd_ports_history);
                    let seen = read_lock(&self.state.peripherals.pd_usb_c_seen);
                    let cards = read_lock(&self.state.peripherals.expansion_cards);
                    let dp_card = cards
                        .iter()
                        .find(|c| c.name.contains("DisplayPort") || c.name.contains("HDMI"));
                    report.push_str("\n=== PD Ports ===\n");
                    for port in pd_ports.iter() {
                        let ever_seen_sink = seen.get(port.port as usize).copied().unwrap_or(false);
                        let card_type = crate::cli::ec_wrapper::classify_pd_port(
                            port,
                            history.iter().map(|a| a.as_ref().as_slice()),
                            crate::style::STABLE_THRESHOLD,
                            dp_card.is_some(),
                            ever_seen_sink,
                        );
                        report.push_str(&format!("  Port {}: role={:?}, data={:?}, dp_alt={}, watts={:?}, type=\"{}\", ever_sink={}\n",
                            port.port, port.power_role, port.data_role, port.dp_alt_mode, port.negotiated_watts, card_type, ever_seen_sink));
                    }
                }
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let path = std::env::temp_dir().join(format!("framework_crate_debug_{}.txt", ts));
                if let Err(e) = std::fs::write(&path, &report) {
                    tracing::error!("Failed to write debug report {}: {}", path.display(), e);
                }
                prune_debug_reports(std::env::temp_dir(), MAX_DEBUG_REPORTS);
                if let Err(e) = std::process::Command::new("notepad.exe").arg(&path).spawn() {
                    tracing::error!(
                        "Failed to open debug report {} in notepad: {}",
                        path.display(),
                        e
                    );
                }
                Some(Task::none())
            }
            Message::OpenProjectUrl => {
                const URL: &str = "https://github.com/vincent-chang-rightfighter/Framework-Crate";
                unsafe {
                    use windows_sys::Win32::UI::Shell::ShellExecuteW;
                    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
                    let url_wide: Vec<u16> = URL.encode_utf16().chain(std::iter::once(0)).collect();
                    let open_wide: Vec<u16> =
                        "open".encode_utf16().chain(std::iter::once(0)).collect();
                    let result = ShellExecuteW(
                        std::ptr::null_mut(),
                        open_wide.as_ptr(),
                        url_wide.as_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        SW_SHOWNORMAL,
                    );
                    let result_code = result as isize;
                    if result_code <= 32 {
                        tracing::warn!(
                            "Failed to open project URL (ShellExecuteW error {})",
                            result_code
                        );
                    }
                }
                Some(Task::none())
            }
            _ => None,
        }
    }
}
