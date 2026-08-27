use iced::{Element, Subscription, Task};
use parking_lot::{Mutex, RwLock};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;
use tracing::{debug, warn};

use crate::background_task;
use crate::cli;
use crate::config_save_task;
use crate::style::*;
use crate::sub_state::{
    BatteryState, FanState, LifecycleState, PeripheralState, SystemState, ThermalState,
};
use crate::system_info;
use crate::temp_chart;
use crate::types::{Config, FanControlMode};
use crate::util::{read_lock, with_write_lock};
use crate::views;

/// Fixed window width (logical px) for auto-resizing; height follows content.
const AUTO_WIDTH: f32 = 900.0;
/// Maximum auto-resized window height (logical px) to fit screen work area.
const AUTO_MAX_HEIGHT: f32 = 1100.0;
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

/// Executes closure on EC client via spawn_blocking; no-ops silently if unavailable.
pub(crate) fn run_ec_task(
    ec_client: &Arc<RwLock<Arc<Option<Arc<cli::EcClient>>>>>,
    done: Message,
    f: impl FnOnce(Arc<cli::EcClient>) + Send + 'static,
) -> Task<Message> {
    let ec_client = Arc::clone(ec_client);
    Task::perform(
        async move {
            let _guard = crate::util::ec_write_mutex().lock().await;
            let ec_opt = { read_lock(&ec_client) };
            if let Some(ref ec) = *ec_opt {
                let ec = ec.clone();
                if let Err(e) = crate::util::spawn_blocking_with_timeout(
                    crate::util::EC_IO_TIMEOUT,
                    move || f(ec),
                )
                .await
                {
                    warn!("EC task failed: {}", e);
                }
            }
            done
        },
        |msg| msg,
    )
}

/// Flushes persisted charge limit to EC on quit (config_save_task skips EC writes during shutdown).
fn apply_quit_charge_limit(ec: &cli::EcClient, limit: &Option<crate::types::SettingU8>) {
    if let Some(limit) = limit {
        let pct = if limit.enabled { limit.value } else { 100 };
        if let Err(e) = ec.charge_limit_set(0, pct) {
            warn!("Failed to set charge limit on quit: {}", e);
        }
    }
}

/// Like `run_ec_task` but reports `Result` via `Message::EcOpResult` to surface EC write failures.
pub(crate) fn run_ec_task_result(
    ec_client: &Arc<RwLock<Arc<Option<Arc<cli::EcClient>>>>>,
    f: impl FnOnce(Arc<cli::EcClient>) -> Result<(), String> + Send + 'static,
) -> Task<Message> {
    let ec_client = Arc::clone(ec_client);
    Task::perform(
        async move {
            let _guard = crate::util::ec_write_mutex().lock().await;
            let ec_opt = { read_lock(&ec_client) };
            let res = if let Some(ref ec) = *ec_opt {
                let ec = ec.clone();
                match crate::util::spawn_blocking_with_timeout(
                    crate::util::EC_IO_TIMEOUT,
                    move || f(ec),
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => Err(e),
                }
            } else {
                // No EC client: silently no-op to avoid spurious error.
                Ok(())
            };
            Message::EcOpResult(res.err())
        },
        |msg| msg,
    )
}

/// Self-rescheduling UI tick via tokio::time to avoid busy polling.
fn tick_task(ms: u64) -> Task<Message> {
    Task::perform(
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        },
        |_| Message::Tick,
    )
}

/// Refreshes CPU power data off UI thread; `after` runs after refresh and completion sends `Message::CpuPowerDataRefreshed`.
fn refresh_cpu_power_task(
    state: crate::cpu_power::CpuPowerState,
    after: impl FnOnce() + Send + 'static,
) -> Task<Message> {
    let task_state = state.clone();
    Task::perform(
        async move {
            let _ = crate::util::spawn_blocking_with_timeout(
                crate::util::PAWNIO_IO_TIMEOUT,
                move || {
                    task_state.refresh();
                    after();
                },
            )
            .await;
            Message::CpuPowerDataRefreshed
        },
        |msg| msg,
    )
}

/// Stops CPU power sync thread off UI thread (join may block up to 250ms).
fn stop_sync_task(state: crate::cpu_power::CpuPowerState) -> Task<Message> {
    Task::perform(
        async move {
            let _guard = crate::util::cpu_power_mutex().lock().await;
            let _ = crate::util::spawn_blocking_with_timeout(
                crate::util::PAWNIO_IO_TIMEOUT,
                move || state.stop_sync(),
            )
            .await;
            Message::CpuPowerSyncStopped
        },
        |msg| msg,
    )
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
    InstallPawnIO,
    PawnIOInstalled(Result<(), String>),
    DownloadPawnIOModules,
    PawnIOModulesDownloaded(Result<(), String>),
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
                                .unwrap_or_else(|e| Err(e))
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

    /// Syncs window height to measured content height when active.
    fn autosize_task(&self) -> Option<Task<Message>> {
        if !self.init_complete || self.show_settings || self.show_quit_warning || self.height_set {
            return None;
        }
        let id = self.window_id?;
        let current = self.window_height?;
        let target = *self.content_height.lock();
        let target = target?;
        let mut target = target.min(AUTO_MAX_HEIGHT + 25.0);
        if let Some((_, work_h)) = crate::system_info::work_area_size() {
            let max_h = (work_h as f32 - 40.0).max(400.0);
            target = target.min(max_h);
        }
        let width = if let Some((work_w, _)) = crate::system_info::work_area_size() {
            AUTO_WIDTH.min(work_w as f32 - 20.0)
        } else {
            AUTO_WIDTH
        };
        if (target - current).abs() > 0.5 {
            Some(iced::window::resize(id, iced::Size::new(width, target)))
        } else {
            None
        }
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

    fn maybe_rebuild_snapshot(&mut self) {
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

    fn handle_tick_message(&mut self) -> Task<Message> {
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
            && read_lock(&self.state.system.ec_client).is_some()
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

    fn handle_config_message(&mut self, message: &Message) -> Option<Task<Message>> {
        match *message {
            Message::FanModeChanged(mode) => {
                self.state
                    .fan
                    .mode
                    .store(mode.to_u8() as u64, Ordering::Release);
                self.mutate_config(|cfg| {
                    if mode == FanControlMode::Curve && cfg.fan.curve.is_none() {
                        cfg.fan.curve = Some(crate::types::GlobalCurveConfig::default());
                    }
                    if mode == FanControlMode::Manual && cfg.fan.manual.is_none() {
                        cfg.fan.manual = Some(crate::types::ManualConfig { duty_pct: 50 });
                    }
                    cfg.fan.mode = mode;
                });
                self.update_curve_full_points();
                self.save_config();
                Some(Task::none())
            }
            Message::FanDutyChanged(duty) => {
                let duty = duty.clamp(0, 100);
                self.mutate_config(|cfg| {
                    cfg.fan.manual = Some(crate::types::ManualConfig { duty_pct: duty });
                });
                // Do not update last_applied_duty here; it tracks actual EC writes.
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::FanUnifiedDutyToggled(unified) => {
                self.state
                    .fan
                    .unified_duty
                    .store(unified, Ordering::Release);
                self.mutate_config(|cfg| {
                    cfg.fan.unified_duty = unified;
                });
                self.save_config();
                Some(Task::none())
            }
            Message::FanPerDutyChanged(fan_idx, duty) => {
                let duty = duty.clamp(0, 100);
                with_write_lock(&self.state.fan.per_fan_duty, |guard| {
                    let duties = Arc::make_mut(guard);
                    if fan_idx < duties.len() {
                        duties[fan_idx] = duty;
                    }
                });
                self.mutate_config(|cfg| {
                    // Copy live duties to preserve values for newly added fans.
                    let live = read_lock(&self.state.fan.per_fan_duty);
                    cfg.fan.per_fan_duty = (*live).clone();
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurvePointMoved(idx, temp, duty) => {
                let mut duty = duty.clamp(0, 100);
                if temp >= crate::types::CURVE_TEMP_LOCK_START {
                    duty = 100;
                }
                // Locked points (100,110) are not editable.
                {
                    let cfg = read_lock(&self.state.lifecycle.config);
                    if let Some(curve) = cfg.fan.curve.as_ref()
                        && let Some(&[t, _]) = curve.curve.points.get(idx)
                        && t >= crate::types::CURVE_TEMP_LOCK_START
                    {
                        return Some(Task::none());
                    }
                }
                // Clamp temperature between neighbors to avoid duplicate temps collapsing control points.
                // Editable range is 0..99; 100–110 is locked 100%.
                let temp = {
                    let cfg = read_lock(&self.state.lifecycle.config);
                    let points = cfg
                        .fan
                        .curve
                        .as_ref()
                        .map(|c| c.curve.points.as_slice())
                        .unwrap_or(&[]);
                    let orig_temp = points.get(idx).map(|p| p[0] as i64).unwrap_or(temp as i64);
                    // Locked zone points are fixed; keep original if trying to edit them (already returned).
                    let max_t = crate::types::CURVE_TEMP_EDIT_MAX as i64;
                    let mut others: Vec<i64> = points
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != idx)
                        .map(|(_, p)| p[0] as i64)
                        .filter(|&t| t < crate::types::CURVE_TEMP_LOCK_START as i64)
                        .collect();
                    others.sort_unstable();
                    let mut lo: i64 = -1; // nothing below → allow down to 0
                    let mut hi: i64 = max_t + 1; // editable max is 99, locked 100 above
                    for &t in &others {
                        if t < temp as i64 {
                            lo = lo.max(t);
                        } else {
                            hi = hi.min(t);
                        }
                    }
                    // No valid slot between neighbors (gap <=1) would force a
                    // duplicate that validate would silently drop; keep original.
                    if hi - lo <= 1 {
                        orig_temp.clamp(0, max_t) as u32
                    } else {
                        let min_t = (lo + 1).max(0);
                        let max_allowed = (hi - 1).min(max_t);
                        (temp.clamp(0, crate::types::CURVE_TEMP_EDIT_MAX) as i64)
                            .clamp(min_t, max_allowed) as u32
                    }
                };
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve
                        && idx < curve.curve.points.len()
                    {
                        curve.curve.points[idx] = [temp, duty];
                    }
                });
                self.pending_curve_update = true;
                self.last_curve_edit_ts = Instant::now();
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurveHysteresisChanged(h) => {
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        curve.curve.hysteresis_c = h;
                    }
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurveRateLimitChanged(r) => {
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        curve.curve.rate_limit_pct_per_step = r;
                    }
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::CurvePollMsChanged(ms) => {
                let ms = ms.clamp(
                    crate::types::CURVE_POLL_MS_MIN,
                    crate::types::CURVE_POLL_MS_MAX,
                );
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        curve.poll_ms = ms;
                    }
                });
                // Background loop reads poll interval directly; no tick reschedule needed.
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::ChargeLimitToggled(enabled) => {
                self.mutate_config(|cfg| {
                    let limit =
                        cfg.battery
                            .charge_limit_max_pct
                            .get_or_insert(crate::types::SettingU8 {
                                enabled: false,
                                value: CHARGE_LIMIT_MIN as u8,
                            });
                    limit.enabled = enabled;
                    if limit.value < CHARGE_LIMIT_MIN as u8 {
                        limit.value = CHARGE_LIMIT_MIN as u8;
                    }
                });
                self.save_config();
                Some(Task::none())
            }
            Message::ChargeLimitChanged(value) => {
                self.mutate_config(|cfg| {
                    let limit =
                        cfg.battery
                            .charge_limit_max_pct
                            .get_or_insert(crate::types::SettingU8 {
                                enabled: false,
                                value: CHARGE_LIMIT_MIN as u8,
                            });
                    limit.value = value.clamp(CHARGE_LIMIT_MIN, CHARGE_LIMIT_MAX) as u8;
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::SensorToggled(idx, enabled) => {
                let (name, all_keys) = {
                    let cache = read_lock(&self.state.thermal.sensor_cache);
                    (cache.keys.get(idx).cloned(), cache.keys.clone())
                };
                let Some(name) = name else {
                    return Some(Task::none());
                };
                self.mutate_config(|cfg| {
                    if cfg.telemetry.selected_sensors.is_empty() {
                        cfg.telemetry.selected_sensors = all_keys.clone();
                    }
                    if enabled {
                        if !cfg.telemetry.selected_sensors.contains(&name) {
                            cfg.telemetry.selected_sensors.push(name);
                        }
                    } else {
                        cfg.telemetry.selected_sensors.retain(|s| s != &name);
                    }
                });
                self.rebuild_sensor_cache();
                self.save_config();
                Some(Task::none())
            }
            Message::CurveSensorSelected(idx) => {
                let name = {
                    let cache = read_lock(&self.state.thermal.sensor_cache);
                    cache.keys.get(idx).cloned()
                };
                let Some(name) = name else {
                    return Some(Task::none());
                };
                self.mutate_config(|cfg| {
                    if let Some(curve) = cfg.fan.curve.as_mut() {
                        // Curve is driven by single selected sensor.
                        curve.curve.sensors = vec![name];
                    }
                });
                self.mark_dirty();
                self.save_config();
                Some(Task::none())
            }
            Message::PollRateChanged(ms) => {
                let ms = ms.clamp(POLL_RATE_MIN_MS as u64, crate::types::POLL_MS_MAX);
                self.mutate_config(|cfg| {
                    cfg.telemetry.poll_ms = ms;
                });
                self.state.lifecycle.poll_ms.store(ms, Ordering::Relaxed);
                self.save_config();
                Some(Task::none())
            }
            Message::UiRefreshRateChanged(ms) => {
                let ms = ms.clamp(50, 1000);
                self.mutate_config(|cfg| {
                    cfg.telemetry.ui_refresh_ms = ms;
                });
                self.tick_interval_ms = ms;
                self.last_tick = Instant::now();
                self.save_config();
                Some(Task::none())
            }
            _ => None,
        }
    }

    fn handle_tray_message(&mut self, message: &Message) -> Option<Task<Message>> {
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
                                        let _ = cpu_power.start_sync(
                                            params.pl1_watts,
                                            params.pl1_enabled,
                                            params.pl1_clamped,
                                            params.pl1_time_s,
                                            params.pl2_watts,
                                            params.pl2_enabled,
                                            params.pl2_clamped,
                                            params.pl2_time_s,
                                            params.power_unit,
                                            params.time_unit,
                                        );
                                        return;
                                    }
                                }
                                // Fallback: no desired sync, check current flag.
                                if cpu_power.sync_enabled.load(Ordering::Acquire) {
                                    let info = cpu_power.snapshot();
                                    let _ = cpu_power.start_sync(
                                        info.pl1_msr,
                                        info.pl1_msr_enabled,
                                        info.pl1_msr_clamped,
                                        info.pl1_time_s,
                                        info.pl2_msr,
                                        info.pl2_msr_enabled,
                                        info.pl2_msr_clamped,
                                        info.pl2_time_s,
                                        info.power_unit,
                                        info.time_unit,
                                    );
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
                                        } else if let Err(e) =
                                            crate::cpu_power::write_bios_defaults(&bios)
                                        {
                                            warn!("Resume write failed: {}", e);
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

    fn handle_quit_message(&mut self, message: &Message) -> Option<Task<Message>> {
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
        if let Some(task) = self.handle_tray_message(&message) {
            return task;
        }
        if let Some(task) = self.handle_quit_message(&message) {
            return task;
        }
        match message {
            Message::Tick => self.handle_tick_message(),
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
                    return Task::batch([
                        Task::perform(async {}, |_| Message::MinimizeToTray),
                        tick_task(0),
                    ]);
                }
                tick_task(0)
            }
            Message::StartupError(msg) => {
                self.startup_error = Some(msg);
                // Keep tick loop running to initialize tray despite startup error.
                tick_task(0)
            }
            Message::WindowResized(id, size) => {
                self.window_id = Some(id);
                self.window_height = Some(size.height);
                Task::none()
            }
            Message::ToggleSensorSettings => {
                self.show_sensor_settings = !self.show_sensor_settings;
                self.height_set = false;
                Task::none()
            }
            Message::ChartWindowChanged(secs) => {
                if crate::temp_chart::HISTORY_WINDOW_OPTIONS.contains(&secs) {
                    self.chart_window_seconds = secs;
                    with_write_lock(&self.state.thermal.history, |hist| {
                        Arc::make_mut(hist).set_window(secs);
                    });
                    self.mark_dirty();
                }
                Task::none()
            }
            Message::ToggleCurveSettings => {
                self.show_curve_settings = !self.show_curve_settings;
                self.height_set = false;
                self.mark_dirty();
                Task::none()
            }
            Message::ToggleCpuPowerSettings => {
                self.show_cpu_power_settings = !self.show_cpu_power_settings;
                self.height_set = false;
                // Mark dirty; flag is snapshot-backed.
                self.mark_dirty();
                Task::none()
            }
            Message::SettingsToggled => {
                self.show_settings = !self.show_settings;
                self.height_set = false;
                Task::none()
            }
            Message::KblightChanged(percent) => {
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
                task
            }
            Message::FpLedLevelChanged(level) => {
                run_ec_task_result(&self.state.system.ec_client, move |ec| {
                    ec.fp_led_level_set(level)
                })
            }
            Message::EcOpResult(err) => {
                // Surface peripheral EC write failure to UI.
                self.ec_op_error = err;
                self.mark_dirty();
                Task::none()
            }
            Message::ToggleBatteryDetails => {
                self.show_battery_details = !self.show_battery_details;
                self.height_set = false;
                Task::none()
            }
            Message::ToggleExpansionCardDebug => {
                self.expansion_card_debug = !self.expansion_card_debug;
                // Mark dirty; snapshot-backed value.
                self.mark_dirty();
                Task::none()
            }
            Message::StartupLaunchToggled(enabled) => {
                self.startup_launch_error = None;
                match crate::system_info::set_startup_launch(enabled) {
                    Ok(()) => self.startup_launch_enabled = enabled,
                    Err(e) => {
                        warn!(
                            "Failed to {} startup launch: {}",
                            if enabled { "enable" } else { "disable" },
                            e
                        );
                        self.startup_launch_error = Some(e);
                    }
                }
                Task::none()
            }
            Message::DismissConfigWarning => {
                self.config_save_failed = false;
                self.config_load_warning = None;
                Task::none()
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
                Task::none()
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
                Task::none()
            }
            Message::InstallPawnIO => {
                if !self.cpu_power_supported() {
                    return Task::none();
                }
                Task::perform(
                    async {
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_IO_TIMEOUT,
                            || crate::cpu_power::install_pawnio().map_err(|e| e.to_string()),
                        )
                        .await
                        .unwrap_or_else(|e| Err(e))
                    },
                    Message::PawnIOInstalled,
                )
            }
            Message::PawnIOInstalled(result) => {
                if let Err(e) = result {
                    tracing::error!("PawnIO install failed: {}", e);
                    self.cpu_power_error = Some(format!("PawnIO install failed: {}", e));
                    self.mark_dirty();
                    Task::none()
                } else {
                    // Refresh PawnIO version and re-read MSR/MMIO off UI thread.
                    self.cpu_power_error = None;
                    self.pl_fields_dirty = false;
                    crate::cpu_power::invalidate_pawnio_version();
                    refresh_cpu_power_task(self.state.cpu_power.clone(), || {
                        crate::cpu_power::pawnio_version();
                    })
                }
            }
            Message::DownloadPawnIOModules => {
                if !self.cpu_power_supported() {
                    return Task::none();
                }
                Task::perform(
                    async {
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_IO_TIMEOUT,
                            || {
                                crate::cpu_power::download_and_extract_modules()
                                    .map_err(|e| e.to_string())
                            },
                        )
                        .await
                        .unwrap_or_else(|e| Err(e))
                    },
                    Message::PawnIOModulesDownloaded,
                )
            }
            Message::PawnIOModulesDownloaded(result) => match result {
                Ok(()) => {
                    self.modules_download_error = None;
                    self.pl_fields_dirty = false;
                    refresh_cpu_power_task(self.state.cpu_power.clone(), || {})
                }
                Err(e) => {
                    tracing::error!("PawnIO Modules download failed: {}", e);
                    self.modules_download_error = Some(e);
                    self.mark_dirty();
                    Task::none()
                }
            },
            Message::OpenModulesDir => {
                if let Err(e) = crate::cpu_power::open_modules_dir() {
                    self.modules_download_error = Some(e);
                    self.mark_dirty();
                }
                Task::none()
            },
            Message::RedetectModules => {
                let ok = crate::cpu_power::redetect_modules();
                if ok {
                    self.modules_download_error = None;
                    self.pl_fields_dirty = false;
                    refresh_cpu_power_task(self.state.cpu_power.clone(), || {})
                } else {
                    self.modules_download_error =
                        Some("Modules not found or hash mismatch".into());
                    self.mark_dirty();
                    Task::none()
                }
            },
            Message::CpuPowerPl1Changed(val) => {
                self.pl1_edit = val;
                self.pl_fields_dirty = true;
                self.cpu_power_error = None;
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerPl2Changed(val) => {
                self.pl2_edit = val;
                self.pl_fields_dirty = true;
                self.cpu_power_error = None;
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerPl1TimeChanged(val) => {
                self.pl1_time_edit = val;
                self.pl_fields_dirty = true;
                self.cpu_power_error = None;
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerPl1EnabledToggled(v) => {
                self.pl1_enabled = v;
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerPl2EnabledToggled(v) => {
                self.pl2_enabled = v;
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerPl1ClampedToggled(v) => {
                self.pl1_clamped = v;
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerPl2ClampedToggled(v) => {
                self.pl2_clamped = v;
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerApply => {
                if !self.cpu_power_supported() {
                    return Task::none();
                }
                let (pl1, pl2, pl1_time) = match self.validate_cpu_power_inputs() {
                    Ok(v) => v,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Task::none();
                    }
                };
                self.pl_fields_dirty = false;
                self.cpu_power_error = None;
                let pl1_en = self.pl1_enabled;
                let pl2_en = self.pl2_enabled;
                let pl1_cl = self.pl1_clamped;
                let pl2_cl = self.pl2_clamped;
                let info = self.state.cpu_power.snapshot();
                let power_unit = info.power_unit;
                let time_unit = info.time_unit;
                let pl2_time = info.pl2_time_s;
                Task::perform(
                    async move {
                        let _guard = crate::util::cpu_power_mutex().lock().await;
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_IO_TIMEOUT,
                            move || {
                                crate::cpu_power::write_msr_pl1_pl2_public(
                                    pl1, pl1_en, pl1_cl, pl1_time, pl2, pl2_en, pl2_cl, pl2_time,
                                    power_unit, time_unit,
                                )
                                .map_err(|e| e.to_string())
                            },
                        )
                        .await
                        .unwrap_or_else(|e| Err(e))
                    },
                    Message::CpuPowerApplied,
                )
            }
            Message::CpuPowerApplied(result) => {
                match result {
                    Ok(()) => {
                        self.pl_custom_applied.store(true, Ordering::Release);
                        self.pl_fields_dirty = false;
                        self.cpu_power_error = None;
                        // Refresh readback and restart sync with fresh values if enabled.
                        let cpu_power = self.state.cpu_power.clone();
                        let after = {
                            let cpu_power = cpu_power.clone();
                            move || {
                                // Re-read flag; user may have toggled sync during refresh.
                                if !cpu_power.sync_enabled.load(Ordering::Acquire) {
                                    return;
                                }
                                let info = cpu_power.snapshot();
                                let _ = cpu_power.start_sync(
                                    info.pl1_msr,
                                    info.pl1_msr_enabled,
                                    info.pl1_msr_clamped,
                                    info.pl1_time_s,
                                    info.pl2_msr,
                                    info.pl2_msr_enabled,
                                    info.pl2_msr_clamped,
                                    info.pl2_time_s,
                                    info.power_unit,
                                    info.time_unit,
                                );
                            }
                        };
                        return refresh_cpu_power_task(cpu_power, after);
                    }
                    Err(e) => {
                        tracing::error!("Failed to write PL1/PL2: {}", e);
                        self.cpu_power_error = Some(e);
                    }
                }
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerSyncStart => {
                if !self.cpu_power_supported() {
                    return Task::none();
                }
                let (pl1, pl2, pl1_time) = match self.validate_cpu_power_inputs() {
                    Ok(v) => v,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Task::none();
                    }
                };
                self.pl_fields_dirty = false;
                self.cpu_power_error = None;
                let pl1_en = self.pl1_enabled;
                let pl2_en = self.pl2_enabled;
                let pl1_cl = self.pl1_clamped;
                let pl2_cl = self.pl2_clamped;
                let info = self.state.cpu_power.snapshot();
                let power_unit = info.power_unit;
                let time_unit = info.time_unit;
                let pl2_time = info.pl2_time_s;
                let cpu_power = self.state.cpu_power.clone();
                Task::perform(
                    async move {
                        let _guard = crate::util::cpu_power_mutex().lock().await;
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_IO_TIMEOUT,
                            move || {
                                cpu_power
                                    .start_sync(
                                        pl1, pl1_en, pl1_cl, pl1_time, pl2, pl2_en, pl2_cl, pl2_time,
                                        power_unit, time_unit,
                                    )
                                    .map_err(|e| e.to_string())
                            },
                        )
                        .await
                        .unwrap_or_else(|e| Err(e))
                    },
                    Message::CpuPowerSyncStarted,
                )
            }
            Message::CpuPowerSyncStarted(result) => {
                match result {
                    Ok(()) => {
                        self.state
                            .cpu_power
                            .sync_enabled
                            .store(true, Ordering::Release);
                        // Ensure AC->battery reset triggers for sync users.
                        self.pl_custom_applied.store(true, Ordering::Release);
                        self.cpu_power_error = None;
                        tracing::info!("CPU power sync started");
                    }
                    Err(e) => {
                        tracing::error!("Failed to start CPU power sync: {}", e);
                        self.cpu_power_error = Some(format!("Sync start failed: {}", e));
                    }
                }
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerSyncStop => {
                if !self.cpu_power_supported() {
                    return Task::none();
                }
                stop_sync_task(self.state.cpu_power.clone())
            }
            Message::CpuPowerSyncReset => {
                self.pl_fields_dirty = false;
                self.handle_cpu_power_sync_reset()
            }
            Message::CpuPowerResetDone(result) => {
                match result {
                    Ok(()) => {
                        self.cpu_power_error = None;
                    }
                    Err(e) => {
                        warn!("CPU power reset failed: {}", e);
                        self.cpu_power_error = Some(format!("Reset failed: {}", e));
                    }
                }
                self.mark_dirty();
                refresh_cpu_power_task(self.state.cpu_power.clone(), || {})
            }
            Message::CpuPowerDataRefreshed => {
                let info = self.state.cpu_power.snapshot();
                self.apply_edit_fields_from_snapshot(&info);
                self.mark_dirty();
                Task::none()
            }
            Message::CpuPowerSyncStopped => {
                self.mark_dirty();
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn save_config(&mut self) {
        self.mark_dirty();
        let cfg = read_lock(&self.state.lifecycle.config);
        let ver = next_config_version();
        if self.config_tx.send((Arc::clone(&cfg), ver)).is_ok() {
            self.config_save_failed = false;
        } else {
            debug!("Config save channel dropped — falling back to sync save");
            // Fallback synchronous save when channel is dropped.
            let cfg_owned: Config = (*cfg).clone();
            drop(cfg);
            if let Err(e) = crate::config::save_versioned(&cfg_owned, ver, true) {
                warn!("Fallback sync config save failed: {}", e);
                self.config_save_failed = true;
                self.state
                    .lifecycle
                    .bg_config_save_failed
                    .store(true, Ordering::Relaxed);
            } else {
                self.config_save_failed = false;
                self.state
                    .lifecycle
                    .bg_config_save_failed
                    .store(false, Ordering::Relaxed);
            }
        }
    }

    /// Synchronous config save for shutdown paths.
    fn save_config_now(&mut self) {
        let cfg = read_lock(&self.state.lifecycle.config);
        let ver = next_config_version();
        if let Err(e) = crate::config::save_versioned(&cfg, ver, true) {
            warn!("Failed to save config: {}", e);
        }
    }

    /// Mutates config under write lock; caller must persist with save_config().
    fn mutate_config(&self, f: impl FnOnce(&mut Config)) {
        with_write_lock(&self.state.lifecycle.config, |guard| {
            f(Arc::make_mut(guard));
        });
    }

    fn cpu_power_supported(&self) -> bool {
        self.state.system.intel_cpu.load(Ordering::Acquire)
    }

    fn handle_cpu_power_sync_reset(&mut self) -> Task<Message> {
        if !self.cpu_power_supported() {
            return Task::none();
        }
        self.pl_custom_applied.store(false, Ordering::Release);
        self.cpu_power_error = None;
        let bios = match self.state.cpu_power.bios_defaults() {
            Some(b) => b,
            None => {
                self.cpu_power_error = Some(
                    "Cannot reset: BIOS defaults not available (PawnIO may not be running)".into(),
                );
                self.mark_dirty();
                return Task::none();
            }
        };
        let cpu_power = self.state.cpu_power.clone();
        Task::perform(
            async move {
                let _guard = crate::util::cpu_power_mutex().lock().await;
                let write_result = crate::util::spawn_blocking_with_timeout(
                    crate::util::PAWNIO_IO_TIMEOUT,
                    move || {
                        cpu_power.stop_sync();
                        crate::cpu_power::write_bios_defaults(&bios).map_err(|e| e.to_string())
                    },
                )
                .await;
                let result = match write_result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(e),
                };
                Message::CpuPowerResetDone(result)
            },
            |msg| msg,
        )
    }

    /// Validates CPU power inputs; returns Ok((pl1, pl2, pl1_time)) or Err.
    fn validate_cpu_power_inputs(&self) -> Result<(f64, f64, f64), String> {
        let pl1: f64 = self
            .pl1_edit
            .parse()
            .map_err(|_| "PL1 is not a valid number".to_string())?;
        let pl2: f64 = self
            .pl2_edit
            .parse()
            .map_err(|_| "PL2 is not a valid number".to_string())?;
        let pl1_time: f64 = self
            .pl1_time_edit
            .parse()
            .map_err(|_| "PL1 time is not a valid number".to_string())?;
        if !pl1.is_finite() {
            return Err("PL1 must be a finite number".to_string());
        }
        if !pl2.is_finite() {
            return Err("PL2 must be a finite number".to_string());
        }
        if !pl1_time.is_finite() {
            return Err("PL1 time must be a finite number".to_string());
        }
        if pl1 <= 0.0 {
            return Err("PL1 must be greater than 0W".to_string());
        }
        if pl2 <= 0.0 {
            return Err("PL2 must be greater than 0W".to_string());
        }
        if pl1_time <= 0.0 {
            return Err("PL1 time must be greater than 0s".to_string());
        }
        if pl1 > pl2 {
            return Err(format!(
                "PL1 ({:.1}W) must not exceed PL2 ({:.1}W)",
                pl1, pl2
            ));
        }
        Ok((pl1, pl2, pl1_time))
    }

    /// Populates edit fields from CPU power snapshot.
    fn apply_edit_fields_from_snapshot(&mut self, info: &crate::cpu_power::CpuPowerInfo) {
        // Do not clobber uncommitted user edits with a fresh readback (#7).
        if self.pl_fields_dirty {
            return;
        }
        let (pl1, pl2, p1en, p2en, p1cl, p2cl, t1, _t2) = info.init_edit_fields();
        self.pl1_edit = pl1;
        self.pl2_edit = pl2;
        self.pl1_time_edit = t1;
        self.pl1_enabled = p1en;
        self.pl2_enabled = p2en;
        self.pl1_clamped = p1cl;
        self.pl2_clamped = p2cl;
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

    fn update_curve_full_points(&mut self) {
        let cfg = read_lock(&self.state.lifecycle.config);
        let pts: &[[u32; 2]] = cfg
            .fan
            .curve
            .as_ref()
            .map(|c| c.curve.points.as_slice())
            .unwrap_or(&[]);
        if self.last_curve_points.as_slice() == pts {
            return;
        }
        self.last_curve_points = pts.to_vec();
        let new_full = Arc::new(crate::types::curve_full_points(pts));
        with_write_lock(&self.state.fan.curve_full_points, |guard| {
            *guard = new_full;
        });
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
