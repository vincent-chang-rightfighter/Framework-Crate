use iced::{Element, Subscription, Task};
use parking_lot::{Mutex, RwLock};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;
use tracing::warn;

use crate::background_task;
use crate::cli;
use crate::config_save_task;
use crate::sub_state::{
    BatteryState, FanState, LifecycleState, PeripheralState, SystemState, ThermalState,
};
use crate::system_info;
use crate::temp_chart;
use crate::types::{Config, FanControlMode};
use crate::util::{read_lock, with_write_lock};
use crate::views;

mod config;
mod cpu_power;
mod misc;
mod quit;
mod tasks;
mod tick;
mod tray;

pub(crate) use tasks::*;

/// Fixed window width (logical px) for auto-resizing; height follows content.
const AUTO_WIDTH: f32 = 900.0;
/// Maximum auto-resized window height (logical px) to fit screen work area.
const AUTO_MAX_HEIGHT: f32 = 1100.0;
/// Window chrome (title bar + borders) added to content height for outer window.
const WINDOW_CHROME: f32 = 25.0;
/// Margins subtracted from work area when clamping autosize (avoid touching edges).
const WORK_AREA_MARGIN_W: f32 = 20.0;
const WORK_AREA_MARGIN_H: f32 = 40.0;
const WINDOW_MIN_HEIGHT: f32 = 400.0;
const RESIZE_EPSILON: f32 = 0.5;
/// Maximum debug report files kept in temp directory.
const MAX_DEBUG_REPORTS: usize = 5;

/// Monotonic version for config snapshots to prevent stale debounced writes from overwriting newer saves.
fn next_config_version() -> u64 {
    static CONFIG_VERSION: AtomicU64 = AtomicU64::new(0);
    CONFIG_VERSION.fetch_add(1, Ordering::Relaxed) + 1
}

/// Prunes oldest `framework_crate_debug_*.txt` files in `dir` to keep at most `keep`.
fn prune_debug_reports(dir: std::path::PathBuf, keep: usize) {
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut reports: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("framework_crate_debug_")
                .then(|| {
                    e.metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(|t| (t, e.path()))
                })
                .flatten()
        })
        .collect();
    reports.sort_by_key(|(t, _)| *t);
    while reports.len() > keep {
        if let Some((_, oldest)) = reports.first() {
            let _ = std::fs::remove_file(oldest);
            reports.remove(0);
        } else {
            break;
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    StartupError(String),
    FanModeChanged(FanControlMode),
    FanDutyChanged(u32),
    FanCurvePointMoved(usize, u32, u32),
    ToggleCurveSettings,
    CurveSensorSelected(usize),
    FanCurveHysteresisChanged(u32),
    FanCurveRateLimitChanged(u32),
    CurvePollMsChanged(u64),
    FanUnifiedDutyToggled(bool),
    FanPerDutyChanged(usize, u32),
    ChargeLimitToggled(bool),
    ChargeLimitChanged(u32),
    ToggleSensorSettings,
    ToggleCpuPowerSettings,
    SensorToggled(usize, bool),
    ChartWindowChanged(i64),
    PollRateChanged(u64),
    UiRefreshRateChanged(u64),
    SettingsToggled,
    InitComplete,
    DismissConfigWarning,
    KblightChanged(u32),
    FpLedLevelChanged(&'static str),
    EcOpResult(Option<String>),
    ToggleBatteryDetails,
    CloseRequested(iced::window::Id),
    WindowResized(iced::window::Id, iced::Size),
    MinimizeToTray,
    RestoreFromTray,
    TrayQuit,
    TrayEventReceived(crate::tray::TrayEvent),
    QuitWithRestore,
    QuitWithoutRestore,
    QuitWithDuty,
    QuitShutdown,
    QuitDutyChanged(u32),
    QuitCanceled,
    CollectDebugInfo,
    OpenProjectUrl,
    ToggleExpansionCardDebug,
    StartupLaunchToggled(bool),
    StartupLaunchSet(bool, Result<(), String>),
    InstallPawnIO,
    PawnIOInstalled(Result<(), String>),
    DownloadPawnIOModules,
    PawnIOModulesDownloaded(Result<(), String>),
    UpdatePawnIO,
    UpdatePawnIOModules,
    UpdatePawnIOAll,
    UpdatePawnIOAllDone(Result<(), String>),
    OpenModulesDir,
    RedetectModules,
    CpuPowerPl1Changed(String),
    CpuPowerPl2Changed(String),
    CpuPowerPl1TimeChanged(String),
    CpuPowerPl1EnabledToggled(bool),
    CpuPowerPl2EnabledToggled(bool),
    CpuPowerPl1ClampedToggled(bool),
    CpuPowerPl2ClampedToggled(bool),
    CpuPowerApply,
    CpuPowerApplied(Result<(), String>),
    CpuPowerDataRefreshed,
    CpuPowerSyncStopped,
    RefreshCpuPower,
    CpuPowerSyncStart,
    CpuPowerSyncStarted(Result<(), String>),
    CpuPowerSyncStop,
    CpuPowerSyncReset,
    CpuPowerResetDone(Result<(), String>),
}

pub struct App {
    pub cli_present: bool,
    pub startup_error: Option<String>,
    pub show_sensor_settings: bool,
    pub chart_window_seconds: i64,
    pub show_curve_settings: bool,
    pub show_cpu_power_settings: bool,
    pub show_battery_details: bool,
    pub show_settings: bool,
    /// Starts hidden to tray when launched with --minimized.
    pub start_minimized: bool,
    pub init_complete: bool,
    pub config_save_failed: bool,
    pub expansion_card_debug: bool,
    pub startup_launch_enabled: bool,
    pub startup_launch_error: Option<String>,
    pub config_load_warning: Option<String>,
    pub show_quit_warning: bool,
    pub closing_window_id: Option<iced::window::Id>,
    pub quit_duty_value: u32,
    pub system_info: SystemInfo,
    pub state: AppState,
    pub last_tick: Instant,
    pub tick_interval_ms: u64,
    pub tray: crate::tray::TrayManager,
    pub tray_initialized: bool,
    pub pending_minimize_to_tray: bool,
    pub config_tx: tokio::sync::watch::Sender<(Arc<Config>, u64)>,
    pub last_hwnd_check_ts: u64,
    pub pending_curve_update: bool,
    pub last_curve_edit_ts: Instant,
    pub last_curve_points: Vec<[u32; 2]>,
    pub icon_create_in_flight: bool,
    /// Consecutive iconic checks required before auto-minimizing to tray.
    pub iconic_check_count: u32,
    pub(crate) cached_snapshot: Option<crate::views::ViewSnapshot>,
    pub(crate) cached_gen: u64,
    /// Measured main view height (logical px) for window autosizing.
    pub content_height: Arc<Mutex<Option<f32>>>,
    /// Single window ID learned from first Resized event.
    pub window_id: Option<iced::window::Id>,
    /// Current window height (logical px) tracked via Resized events.
    pub window_height: Option<f32>,
    /// Whether window height is fitted to content; resets on layout-changing toggles.
    pub height_set: bool,
    pub modules_download_error: Option<String>,
    pub pl1_edit: String,
    pub pl2_edit: String,
    pub pl1_time_edit: String,
    pub pl1_enabled: bool,
    pub pl2_enabled: bool,
    pub pl1_clamped: bool,
    pub pl2_clamped: bool,
    /// True when the user has uncommitted PL field edits; while set, incoming
    /// CPU power readbacks must not overwrite the edit boxes (#7).
    pub pl_fields_dirty: bool,
    pub cpu_power_error: Option<String>,
    pub ec_op_error: Option<String>,
    pub pl_custom_applied: Arc<std::sync::atomic::AtomicBool>,
    pub kblight_write_gen: Arc<AtomicU64>,
}

pub struct SystemInfo {
    pub cpu: String,
    pub mem: String,
    pub os: String,
    pub screen: String,
    pub refresh_rate: String,
    pub header_device_name: String,
    pub header_info_text: String,
}

#[derive(Clone, Default)]
pub struct SensorCache {
    pub keys: Vec<String>,
    pub sorted: Arc<Vec<String>>,
    pub colors: Arc<Vec<iced::Color>>,
}

#[derive(Clone)]
pub struct AppState {
    pub system: SystemState,
    pub fan: FanState,
    pub thermal: ThermalState,
    pub peripherals: PeripheralState,
    pub battery: BatteryState,
    pub cpu_power: crate::cpu_power::CpuPowerState,
    pub lifecycle: LifecycleState,
}

impl App {
    pub(crate) fn new(start_minimized: bool) -> (Self, Task<Message>) {
        let (loaded_config, config_load_warning) = match crate::config::load() {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                warn!("{}", e);
                (Config::default(), Some(e))
            }
        };
        let poll_ms = loaded_config.telemetry.poll_ms;
        let ui_refresh_ms = loaded_config.telemetry.ui_refresh_ms;

        let state = AppState {
            system: SystemState {
                cli_available: Arc::new(AtomicBool::new(false)),
                ec_client: Arc::new(RwLock::new(Arc::new(None))),
                ec_init_done: Arc::new(AtomicBool::new(false)),
                versions: Arc::new(RwLock::new(Arc::new(None))),
                platform: Arc::new(RwLock::new(Arc::new(
                    crate::cli::ec_wrapper::detect_platform(),
                ))),
                intel_cpu: Arc::new(AtomicBool::new(system_info::is_intel_cpu())),
            },
            fan: FanState {
                mode: Arc::new(AtomicU64::new(loaded_config.fan.mode.to_u8() as u64)),
                last_applied_duty: Arc::new(AtomicU64::new(0)),
                fan_max_rpm: Arc::new(AtomicU64::new(0)),
                last_fan_rpm_reset: Arc::new(AtomicU64::new(crate::util::monotonic_ms())),
                curve_full_points: Arc::new(RwLock::new(Arc::new(
                    crate::types::curve_full_points(
                        loaded_config
                            .fan
                            .curve
                            .as_ref()
                            .map(|c| c.curve.points.as_slice())
                            .unwrap_or(&[]),
                    ),
                ))),
                fan_count: Arc::new(AtomicU64::new(0)),
                unified_duty: Arc::new(AtomicBool::new(loaded_config.fan.unified_duty)),
                per_fan_duty: Arc::new(RwLock::new(Arc::new(
                    loaded_config.fan.per_fan_duty.clone(),
                ))),
                last_fan_count: Arc::new(AtomicU64::new(0)),
            },
            thermal: ThermalState {
                data: Arc::new(RwLock::new(Arc::new(None))),
                history: Arc::new(RwLock::new(Arc::new(temp_chart::ThermalHistory::new()))),
                sensor_cache: Arc::new(RwLock::new(Arc::new(SensorCache::default()))),
                last_success_ms: Arc::new(AtomicU64::new(0)),
            },
            peripherals: PeripheralState {
                kblight: Arc::new(RwLock::new(Arc::new(None))),
                expansion_cards: Arc::new(RwLock::new(Arc::new(smallvec::SmallVec::new()))),
                pd_ports: Arc::new(RwLock::new(Arc::new(smallvec::SmallVec::new()))),
                pd_ports_history: Arc::new(RwLock::new(Arc::new(VecDeque::new()))),
                pd_usb_c_seen: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            },
            battery: BatteryState {
                info: Arc::new(RwLock::new(Arc::new(None))),
                prev_ac_present: Arc::new(AtomicBool::new(true)),
                last_success_ms: Arc::new(AtomicU64::new(0)),
            },
            cpu_power: crate::cpu_power::CpuPowerState::default(),
            lifecycle: LifecycleState {
                config: Arc::new(RwLock::new(Arc::new(loaded_config.clone()))),
                poll_ms: Arc::new(AtomicU64::new(poll_ms)),
                shutdown: Arc::new(AtomicBool::new(false)),
                visible: Arc::new(AtomicBool::new(true)),
                last_interaction_ts: Arc::new(AtomicU64::new(crate::util::monotonic_ms())),
                bg_config_save_failed: Arc::new(AtomicBool::new(false)),
                view_dirty: Arc::new(AtomicBool::new(true)),
                view_generation: Arc::new(AtomicU64::new(1)),
                last_resume_ts: Arc::new(AtomicU64::new(0)),
                pl_reset_pending: Arc::new(AtomicBool::new(false)),
                fan_reset_pending: Arc::new(AtomicBool::new(false)),
            },
        };

        let cpu = system_info::cpu_name();
        let mem = system_info::total_memory_gb();
        let os = system_info::os_version();
        let screen = system_info::display_resolution();
        let refresh_rate = system_info::display_refresh_rate();

        let (config_tx, config_rx) =
            tokio::sync::watch::channel((Arc::new(loaded_config.clone()), 0));
        let state_for_save = state.clone();
        config_save_task::spawn(config_rx, state_for_save);

        let app = App {
            cli_present: false,
            startup_error: None,
            show_sensor_settings: false,
            chart_window_seconds: crate::temp_chart::HISTORY_SECONDS,
            show_curve_settings: false,
            show_cpu_power_settings: false,
            show_battery_details: false,
            show_settings: false,
            init_complete: false,
            config_save_failed: false,
            expansion_card_debug: false,
            startup_launch_enabled: system_info::startup_launch_enabled(),
            startup_launch_error: None,
            start_minimized,
            config_load_warning,
            show_quit_warning: false,
            closing_window_id: None,
            quit_duty_value: 45,
            system_info: SystemInfo {
                header_device_name: "Framework Crate".to_string(),
                header_info_text: String::new(),
                cpu,
                mem,
                os,
                screen,
                refresh_rate,
            },
            state: state.clone(),
            last_tick: Instant::now(),
            tick_interval_ms: ui_refresh_ms,
            tray: crate::tray::TrayManager::new(),
            tray_initialized: false,
            pending_minimize_to_tray: false,
            config_tx,
            last_hwnd_check_ts: 0,
            pending_curve_update: false,
            last_curve_edit_ts: Instant::now(),
            last_curve_points: Vec::new(),
            icon_create_in_flight: false,
            iconic_check_count: 0,
            cached_snapshot: None,
            cached_gen: 0,
            content_height: Arc::new(Mutex::new(None)),
            window_id: None,
            window_height: None,
            height_set: false,
            modules_download_error: None,
            pl1_edit: String::new(),
            pl2_edit: String::new(),
            pl1_time_edit: String::new(),
            pl1_enabled: true,
            pl2_enabled: true,
            pl1_clamped: false,
            pl2_clamped: false,
            pl_fields_dirty: false,
            cpu_power_error: None,
            ec_op_error: None,
            pl_custom_applied: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kblight_write_gen: Arc::new(AtomicU64::new(0)),
        };

        let init_task = Task::perform(
            async move {
                match crate::util::spawn_blocking_with_timeout(
                    crate::util::EC_IO_TIMEOUT,
                    cli::EcClient::new,
                )
                .await
                {
                    Ok(Ok(ec)) => {
                        state.system.cli_available.store(true, Ordering::Release);
                        let arc_ec = Arc::new(ec);
                        with_write_lock(&state.system.ec_client, |guard| {
                            *guard = Arc::new(Some(Arc::clone(&arc_ec)));
                        });
                        // Publish authoritative EC client for background loop.
                        state.system.ec_init_done.store(true, Ordering::Release);
                        let versions = Arc::clone(&state.system.versions);
                        let ec_cl = Arc::clone(&arc_ec);
                        match crate::util::spawn_blocking_with_timeout(
                            crate::util::EC_IO_TIMEOUT,
                            move || ec_cl.versions(),
                        )
                        .await
                        {
                            Ok(Ok(v)) => {
                                with_write_lock(&versions, |guard| {
                                    *guard = Arc::new(Some(v));
                                });
                            }
                            Ok(Err(e)) => {
                                warn!("versions failed: {}", e);
                            }
                            Err(e) => {
                                warn!("versions spawn failed: {}", e);
                            }
                        }
                        background_task::refresh_all_data(&state, &arc_ec).await;
                        {
                            let cfg = read_lock(&state.lifecycle.config);
                            if let Some(ref limit) = cfg.battery.charge_limit_max_pct {
                                let pct = if limit.enabled { limit.value } else { 100 };
                                let ec_clone = Arc::clone(&arc_ec);
                                if let Err(e) = crate::util::spawn_blocking_with_timeout(
                                    crate::util::EC_IO_TIMEOUT,
                                    move || ec_clone.charge_limit_set(0, pct),
                                )
                                .await
                                .unwrap_or_else(Err)
                                {
                                    warn!("Failed to apply saved charge limit: {}", e);
                                }
                            }
                        }
                        // Capture BIOS defaults; do not write MSR without user request.
                        if state.system.intel_cpu.load(Ordering::Acquire) {
                            state.cpu_power.refresh();
                            state.cpu_power.init_bios_defaults();
                        }
                        Message::InitComplete
                    }
                    Ok(Err(e)) => {
                        state.system.cli_available.store(false, Ordering::Release);
                        state.system.ec_init_done.store(true, Ordering::Release);
                        Message::StartupError(format!(
                            "EC initialization failed: {}. Run as administrator.",
                            e
                        ))
                    }
                    Err(e) => {
                        state.system.cli_available.store(false, Ordering::Release);
                        state.system.ec_init_done.store(true, Ordering::Release);
                        Message::StartupError(format!("EC spawn failed: {}", e))
                    }
                }
            },
            |msg| msg,
        );

        let bg_state = app.state.clone();
        background_task::spawn(bg_state);

        (app, init_task)
    }

    pub(crate) fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::window::close_requests().map(Message::CloseRequested),
            iced::window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
        ])
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        let mut task = self.update_inner(message);
        // Rebuild snapshot after handlers that may set view_dirty; no-op if unchanged.
        self.maybe_rebuild_snapshot();
        if let Some(resize) = self.autosize_task() {
            self.height_set = true;
            task = Task::batch([task, resize]);
        }
        task
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        match &message {
            Message::Tick
            | Message::InitComplete
            | Message::StartupError(_)
            | Message::WindowResized(..)
            | Message::CpuPowerDataRefreshed
            | Message::CpuPowerSyncStopped => {}
            _ => {
                let now_ms = crate::util::monotonic_ms();
                self.state
                    .lifecycle
                    .last_interaction_ts
                    .store(now_ms, Ordering::Release);
            }
        }
        self.maybe_rebuild_snapshot();
        if let Some(task) = self.handle_config_message(&message) {
            return task;
        }
        if let Some(task) = self.handle_cpu_power_message(&message) {
            return task;
        }
        if let Some(task) = self.handle_tray_message(&message) {
            return task;
        }
        if let Some(task) = self.handle_quit_message(&message) {
            return task;
        }
        if let Some(task) = self.handle_misc_message(&message) {
            return task;
        }
        match message {
            Message::Tick => self.handle_tick_message(),
            _ => Task::none(),
        }
    }

    fn close_window(&self) -> Task<Message> {
        if let Some(id) = self.closing_window_id {
            return iced::window::close(id);
        }
        iced::window::latest().then(|id| {
            if let Some(id) = id {
                iced::window::close(id)
            } else {
                Task::none()
            }
        })
    }

    fn rebuild_header_info(&mut self) {
        let versions = read_lock(&self.state.system.versions);
        self.system_info.header_device_name = versions
            .as_ref()
            .as_ref()
            .and_then(|v| v.mainboard_type.as_deref())
            .unwrap_or("Framework Crate")
            .to_owned();

        let bios = versions
            .as_ref()
            .as_ref()
            .and_then(|v| v.uefi_version.as_deref())
            .unwrap_or_default();

        let mut info = String::with_capacity(128);
        if !self.system_info.cpu.is_empty() {
            use std::fmt::Write;
            let _ = write!(info, "CPU: {}", self.system_info.cpu);
        }
        if self.system_info.mem != "N/A" {
            if !info.is_empty() {
                info.push_str("  |  ");
            }
            use std::fmt::Write;
            let _ = write!(info, "RAM: {}", self.system_info.mem);
        }
        if !self.system_info.os.is_empty() {
            if !info.is_empty() {
                info.push_str("  |  ");
            }
            use std::fmt::Write;
            let _ = write!(info, "OS: {}", self.system_info.os);
        }
        if !bios.is_empty() {
            if !info.is_empty() {
                info.push_str("  |  ");
            }
            use std::fmt::Write;
            let _ = write!(info, "BIOS: {}", bios);
        }
        if !self.system_info.screen.is_empty() {
            if !info.is_empty() {
                info.push_str("  |  ");
            }
            use std::fmt::Write;
            if !self.system_info.refresh_rate.is_empty() {
                let _ = write!(
                    info,
                    "Display: {} {}",
                    self.system_info.screen, self.system_info.refresh_rate
                );
            } else {
                let _ = write!(info, "Display: {}", self.system_info.screen);
            }
        }
        self.system_info.header_info_text = info;
    }

    pub(crate) fn view(&self) -> Element<'_, Message> {
        views::view_main(self)
    }

    /// Rebuilds sensor cache sorted list and colors from current config.
    pub(crate) fn rebuild_sensor_cache(&self) {
        let cache = read_lock(&self.state.thermal.sensor_cache);
        let config = read_lock(&self.state.lifecycle.config);
        let sorted =
            crate::types::sorted_sensor_list(&config.telemetry.selected_sensors, &cache.keys);
        let colors: Vec<iced::Color> = sorted
            .iter()
            .map(|name| crate::style::sensor_color(name, &cache.keys))
            .collect();
        // Drop read lock before acquiring write lock.
        drop(cache);
        with_write_lock(&self.state.thermal.sensor_cache, |g| {
            let old = Arc::make_mut(g);
            old.sorted = Arc::new(sorted);
            old.colors = Arc::new(colors);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_lock_normal() {
        let lock = Arc::new(RwLock::new(Arc::new(42i32)));
        let val = read_lock(&lock);
        assert_eq!(*val, 42);
    }

    #[test]
    fn with_write_lock_normal() {
        let lock = Arc::new(RwLock::new(Arc::new(10i32)));
        let result = with_write_lock(&lock, |guard| {
            let val = **guard;
            *guard = Arc::new(val + 10);
            **guard
        });
        assert_eq!(result, 20);
        assert_eq!(*read_lock(&lock), 20);
    }

    #[tokio::test]
    async fn validate_cpu_power_rejects_nan_and_inf() {
        let (mut app, _) = App::new(false);
        app.pl1_edit = "nan".into();
        app.pl2_edit = "50".into();
        app.pl1_time_edit = "28".into();
        assert!(app.validate_cpu_power_inputs().is_err());

        app.pl1_edit = "inf".into();
        assert!(app.validate_cpu_power_inputs().is_err());

        app.pl1_edit = "-inf".into();
        assert!(app.validate_cpu_power_inputs().is_err());

        app.pl1_edit = "40".into();
        app.pl2_edit = "nan".into();
        assert!(app.validate_cpu_power_inputs().is_err());

        app.pl2_edit = "40".into();
        app.pl1_time_edit = "nan".into();
        assert!(app.validate_cpu_power_inputs().is_err());
    }

    #[tokio::test]
    async fn validate_cpu_power_accepts_finite_values() {
        let (mut app, _) = App::new(false);
        app.pl1_edit = "40".into();
        app.pl2_edit = "80".into();
        app.pl1_time_edit = "28".into();
        let (pl1, pl2, t) = app.validate_cpu_power_inputs().unwrap();
        assert_eq!((pl1, pl2, t), (40.0, 80.0, 28.0));
    }
}
