use std::collections::VecDeque;
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::time::Instant;
use iced::{Element, Subscription, Task};
use parking_lot::{Mutex, RwLock};
use tracing::{debug, warn};

use crate::background_task;
use crate::config_save_task;
use crate::types::{Config, FanControlMode};
use crate::cli;
use crate::system_info;
use crate::temp_chart;
use crate::sub_state::{FanState, ThermalState, PeripheralState, BatteryState, SystemState, LifecycleState};
use crate::style::*;
use crate::util::{read_lock, with_write_lock};
use crate::views;

/// Window width (logical px) used for auto-resizing. The width is fixed by
/// the user's preferred layout; only the height follows the content.
const AUTO_WIDTH: f32 = 900.0;
/// Ceiling for the auto-resized window height (logical px), so the window
/// never outgrows the screen work area (fan curve mode can be very tall).
const AUTO_MAX_HEIGHT: f32 = 760.0;

/// Execute a closure on the EC client via spawn_blocking. If the EC client
/// is not available, the task completes silently. Errors from the closure
/// are logged as warnings.
pub(crate) fn run_ec_task(
    ec_client: &Arc<RwLock<Arc<Option<Arc<cli::EcClient>>>>>,
    f: impl FnOnce(Arc<cli::EcClient>) + Send + 'static,
) -> Task<Message> {
    let ec_client = Arc::clone(ec_client);
    Task::perform(
        async move {
            let ec_opt = { read_lock(&ec_client) };
            if let Some(ref ec) = *ec_opt {
                let ec = ec.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || f(ec)).await {
                    warn!("EC task failed: {}", e);
                }
            }
            Message::Tick
        },
        |msg| msg,
    )
}

/// Self-rescheduling UI tick. Sleeping via tokio::time lets the runtime
/// park the thread between ticks, so an idle UI wakes ~1x/sec instead of
/// hammering update()/view() at a fixed 50ms.
fn tick_task(ms: u64) -> Task<Message> {
    Task::perform(
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        },
        |_| Message::Tick,
    )
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    StartupError(String),
    FanModeChanged(FanControlMode),
    FanDutyChanged(u32),
    FanCurvePointTempChanged(usize, u32),
    FanCurvePointDutyChanged(usize, u32),
    FanCurvePollMsChanged(u64),
    FanCurveHysteresisChanged(u32),
    FanCurveRateLimitChanged(u32),
    FanUnifiedDutyToggled(bool),
    FanPerDutyChanged(usize, u32),
    ChargeLimitToggled(bool),
    ChargeLimitChanged(u32),
    ToggleSensorSettings,
    SensorToggled(usize, bool),
    PollRateChanged(u64),
    UiRefreshRateChanged(u64),
    SettingsToggled,
    InitComplete,
    DismissConfigWarning,
    KblightChanged(u32),
    FpLedLevelChanged(&'static str),
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
    DebugInfoCollected(String),
}

pub struct App {
    pub cli_present: bool,
    pub startup_error: Option<String>,
    pub show_sensor_settings: bool,
    pub show_battery_details: bool,
    pub show_settings: bool,
    pub init_complete: bool,
    pub config_save_failed: bool,
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
    pub config_tx: tokio::sync::watch::Sender<Arc<Config>>,
    pub last_hwnd_check_ts: u64,
    pub pending_curve_update: bool,
    pub last_curve_edit_ts: Instant,
    pub last_curve_points: Vec<[u32; 2]>,
    pub icon_create_in_flight: bool,
    /// Consecutive iconic check count – auto-minimize only triggers after
    /// is_iconic() returns true for at least this many consecutive 5-second
    /// check cycles, preventing false positives during the restore transition.
    pub iconic_check_count: u32,
    pub(crate) cached_snapshot: Option<crate::views::ViewSnapshot>,
    /// Laid-out height (logical px) of the main view, reported by the
    /// HeightProbe widget every layout pass. The window is resized to match.
    pub content_height: Arc<Mutex<Option<f32>>>,
    /// Id of the (single) window, learned from the first Resized event.
    pub window_id: Option<iced::window::Id>,
    /// Current window height (logical px), tracked via Resized events.
    pub window_height: Option<f32>,
    /// Whether the window height has been fitted to content. Once set, the
    /// window stays at that height even when the content grows (e.g. battery
    /// details expanded, fan curve mode) — those sections scroll instead.
    pub height_set: bool,
    /// Debug report collected by "Collect Debug Info" button.
    pub debug_report: Option<String>,
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
    pub lifecycle: LifecycleState,
}

impl App {
    pub(crate) fn new() -> (Self, Task<Message>) {
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
                versions: Arc::new(RwLock::new(Arc::new(None))),
                platform: Arc::new(RwLock::new(Arc::new(crate::cli::ec_wrapper::detect_platform()))),
            },
            fan: FanState {
                mode: Arc::new(AtomicU64::new(loaded_config.fan.mode.to_u8() as u64)),
                curve_poll_ms: Arc::new(AtomicU64::new(
                    loaded_config.fan.curve.as_ref().map(|c| c.poll_ms).unwrap_or(1000)
                )),
                last_applied_duty: Arc::new(AtomicU64::new(0)),
                fan_max_rpm: Arc::new(AtomicU64::new(0)),
                last_fan_rpm_reset: Arc::new(AtomicU64::new(crate::util::current_time_ms())),
                curve_full_points: Arc::new(RwLock::new(Arc::new(crate::types::curve_full_points(
                    loaded_config.fan.curve.as_ref().map(|c| c.curve.points.as_slice()).unwrap_or(&[]),
                )))),
                fan_count: Arc::new(AtomicU64::new(0)),
                unified_duty: Arc::new(AtomicBool::new(true)),
                per_fan_duty: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            },
            thermal: ThermalState {
                data: Arc::new(RwLock::new(Arc::new(None))),
                history: Arc::new(RwLock::new(Arc::new(temp_chart::ThermalHistory::new()))),
                sensor_cache: Arc::new(RwLock::new(Arc::new(SensorCache::default()))),
            },
            peripherals: PeripheralState {
                kblight: Arc::new(RwLock::new(Arc::new(None))),
                expansion_cards: Arc::new(RwLock::new(Arc::new(Vec::new()))),
                pd_ports: Arc::new(RwLock::new(Arc::new(Vec::new()))),
                pd_ports_history: Arc::new(RwLock::new(Arc::new(VecDeque::new()))),
            },
            battery: BatteryState {
                info: Arc::new(RwLock::new(Arc::new(None))),
            },
            lifecycle: LifecycleState {
                config: Arc::new(RwLock::new(Arc::new(loaded_config.clone()))),
                poll_ms: Arc::new(AtomicU64::new(poll_ms)),
                shutdown: Arc::new(AtomicBool::new(false)),
                visible: Arc::new(AtomicBool::new(true)),
                last_interaction_ts: Arc::new(AtomicU64::new(crate::util::current_time_ms())),
                bg_config_save_failed: Arc::new(AtomicBool::new(false)),
                view_dirty: Arc::new(AtomicBool::new(true)),
                last_resume_ts: Arc::new(AtomicU64::new(0)),
            },
        };

        let cpu = system_info::cpu_name();
        let mem = system_info::total_memory_gb();
        let os = system_info::os_version();
        let screen = system_info::display_resolution();
        let refresh_rate = system_info::display_refresh_rate();

        let (config_tx, config_rx) = tokio::sync::watch::channel(Arc::new(loaded_config.clone()));
        let state_for_save = state.clone();
        config_save_task::spawn(config_rx, state_for_save);

        let app = App {
            cli_present: false,
            startup_error: None,
            show_sensor_settings: false,
            show_battery_details: false,
            show_settings: false,
            init_complete: false,
            config_save_failed: false,
            config_load_warning,
            show_quit_warning: false,
            closing_window_id: None,
            quit_duty_value: 45,
            system_info: SystemInfo {
                header_device_name: "Framework Crate".to_string(),
                header_info_text: String::new(),
                cpu, mem, os, screen, refresh_rate
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
            content_height: Arc::new(Mutex::new(None)),
            window_id: None,
            window_height: None,
            height_set: false,
            debug_report: None,
        };

        let init_task = Task::perform(async move {
            match tokio::task::spawn_blocking(cli::EcClient::new).await {
                Ok(Ok(ec)) => {
                    state.system.cli_available.store(true, Ordering::Release);
                    let arc_ec = Arc::new(ec);
                    with_write_lock(&state.system.ec_client, |guard| {
                        *guard = Arc::new(Some(Arc::clone(&arc_ec)));
                    });
                    let versions = Arc::clone(&state.system.versions);
                    let ec_cl = Arc::clone(&arc_ec);
                    match tokio::task::spawn_blocking(move || ec_cl.versions()).await {
                        Ok(Ok(v)) => {
                            with_write_lock(&versions, |guard| {
                                *guard = Arc::new(Some(v));
                            });
                        }
                        Ok(Err(e)) => { warn!("versions failed: {}", e); }
                        Err(e) => { warn!("versions spawn failed: {}", e); }
                    }
                    background_task::refresh_all_data(&state, &arc_ec).await;
                    Message::InitComplete
                }
                Ok(Err(e)) => {
                    state.system.cli_available.store(false, Ordering::Release);
                    Message::StartupError(format!("EC initialization failed: {}. Run as administrator.", e))
                }
                Err(e) => {
                    state.system.cli_available.store(false, Ordering::Release);
                    Message::StartupError(format!("EC spawn failed: {}", e))
                }
            }
        }, |msg| msg);

        let bg_state = app.state.clone();
        background_task::spawn(bg_state);

        (app, init_task)
    }

    pub(crate) fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::window::close_requests().map(Message::CloseRequested),
            iced::window::resize_events()
                .map(|(id, size)| Message::WindowResized(id, size)),
        ])
    }

    /// Keeps the window height in sync with the measured content height.
    /// Only active once the main view is up (init complete, not on the
    /// settings / quit-warning screens), and only when the difference is
    /// larger than sub-pixel rounding, so it converges and stays quiet.
    fn autosize_task(&self) -> Option<Task<Message>> {
        if !self.init_complete || self.show_settings || self.show_quit_warning || self.height_set {
            return None;
        }
        let id = self.window_id?;
        let current = self.window_height?;
        let target = *self.content_height.lock();
        let target = target?;
        let target = target.min(AUTO_MAX_HEIGHT) + 25.0;
        if (target - current).abs() > 0.5 {
            Some(iced::window::resize(id, iced::Size::new(AUTO_WIDTH, target)))
        } else {
            None
        }
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        let mut task = self.update_inner(message);
        if let Some(resize) = self.autosize_task() {
            self.height_set = true;
            task = Task::batch([task, resize]);
        }
        task
    }

    fn maybe_rebuild_snapshot(&mut self) {
        if self.init_complete
            && (self.state.lifecycle.view_dirty.load(Ordering::Acquire) || self.cached_snapshot.is_none())
        {
            self.cached_snapshot = Some(crate::views::ViewSnapshot::from_app(self));
            self.state.lifecycle.view_dirty.store(false, Ordering::Release);
        }
    }

    fn handle_tick_message(&mut self) -> Task<Message> {
        let elapsed = self.last_tick.elapsed().as_millis() as u64;
        if elapsed < self.tick_interval_ms {
            return tick_task(self.tick_interval_ms - elapsed);
        }
        self.last_tick = Instant::now();
        // Debounce curve_full_points recomputation (100ms after last slider edit)
        if self.pending_curve_update && self.last_curve_edit_ts.elapsed().as_millis() >= 100 {
            self.pending_curve_update = false;
            self.update_curve_full_points();
        }
        self.cli_present = self.state.system.cli_available.load(Ordering::Acquire);
        self.config_save_failed = self.state.lifecycle.bg_config_save_failed.load(Ordering::Relaxed);

        let now_ms = crate::util::current_time_ms();
        let idle = now_ms.saturating_sub(self.state.lifecycle.last_interaction_ts.load(Ordering::Acquire)) > IDLE_THRESHOLD_MS;
        let visible = self.state.lifecycle.visible.load(Ordering::Acquire);
        let next_ms = if !visible {
            UI_HIDDEN_INTERVAL_MS
        } else if idle {
            UI_IDLE_INTERVAL_MS
        } else {
            self.tick_interval_ms
        };

        if !self.tray_initialized {
            if let Some(hwnd) = system_info::find_window_by_title("Framework Crate") {
                self.tray.init(hwnd);
                self.tray_initialized = true;
                tracing::info!("Tray initialized with HWND: {}", hwnd);
                self.tray.show_icon_async();
            }
        }

        if self.tray_initialized {
            // Only validate HWND every 5 seconds to avoid repeated FindWindowW syscalls
            const HWND_CHECK_INTERVAL_MS: u64 = 5000;
            if now_ms.saturating_sub(self.last_hwnd_check_ts) >= HWND_CHECK_INTERVAL_MS {
                self.last_hwnd_check_ts = now_ms;
                if !system_info::is_window(self.tray.hwnd()) {
                    tracing::warn!("HWND {} invalid, reinitializing tray", self.tray.hwnd());
                    if let Some(hwnd) = system_info::find_window_by_title("Framework Crate") {
                        self.tray.reinit(hwnd);
                        self.tray.show_icon_async();
                    } else {
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

        tick_task(next_ms)
    }

    fn handle_config_message(&mut self, message: Message) -> Option<Task<Message>> {
        match message {
            Message::FanModeChanged(mode) => {
                self.state.fan.mode.store(mode.to_u8() as u64, Ordering::Release);
                let curve_poll = {
                    let mut curve_poll_ms = None;
                    self.mutate_config(|cfg| {
                        if mode == FanControlMode::Curve && cfg.fan.curve.is_none() {
                            cfg.fan.curve = Some(crate::types::GlobalCurveConfig::default());
                        }
                        cfg.fan.mode = mode;
                        curve_poll_ms = cfg.fan.curve.as_ref().map(|c| c.poll_ms);
                    });
                    curve_poll_ms
                };
                if let Some(ms) = curve_poll {
                    self.state.fan.curve_poll_ms.store(ms, Ordering::Release);
                }
                self.update_curve_full_points();
                self.save_config();
                Some(Task::none())
            }
            Message::FanDutyChanged(duty) => {
                let duty = duty.clamp(0, 100);
                self.mutate_config(|cfg| {
                    cfg.fan.manual = Some(crate::types::ManualConfig { duty_pct: duty });
                });
                self.state.fan.last_applied_duty.store(duty as u64, Ordering::Release);
                self.save_config();
                Some(Task::none())
            }
            Message::FanUnifiedDutyToggled(unified) => {
                self.state.fan.unified_duty.store(unified, Ordering::Release);
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
                Some(Task::none())
            }
            Message::FanCurvePointTempChanged(idx, temp) => {
                let temp = temp.clamp(0, 99);
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        if idx < curve.curve.points.len() {
                            curve.curve.points[idx][0] = temp;
                        }
                    }
                });
                self.pending_curve_update = true;
                self.last_curve_edit_ts = Instant::now();
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurvePointDutyChanged(idx, duty) => {
                let duty = duty.clamp(0, 100);
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        if idx < curve.curve.points.len() {
                            curve.curve.points[idx][1] = duty;
                        }
                    }
                });
                self.pending_curve_update = true;
                self.last_curve_edit_ts = Instant::now();
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurvePollMsChanged(ms) => {
                let clamped = ms.max(500);
                self.state.fan.curve_poll_ms.store(clamped, Ordering::Release);
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        curve.poll_ms = clamped;
                    }
                });
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurveHysteresisChanged(h) => {
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        curve.curve.hysteresis_c = h;
                    }
                });
                self.save_config();
                Some(Task::none())
            }
            Message::FanCurveRateLimitChanged(r) => {
                self.mutate_config(|cfg| {
                    if let Some(ref mut curve) = cfg.fan.curve {
                        curve.curve.rate_limit_pct_per_step = r;
                    }
                });
                self.save_config();
                Some(Task::none())
            }
            Message::ChargeLimitToggled(enabled) => {
                self.mutate_config(|cfg| {
                    let limit = cfg.battery.charge_limit_max_pct.get_or_insert(crate::types::SettingU8 { enabled: false, value: CHARGE_LIMIT_MIN as u8 });
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
                    let limit = cfg.battery.charge_limit_max_pct.get_or_insert(crate::types::SettingU8 { enabled: false, value: CHARGE_LIMIT_MIN as u8 });
                    limit.value = value.min(CHARGE_LIMIT_MAX) as u8;
                });
                self.save_config();
                Some(Task::none())
            }
            Message::SensorToggled(idx, enabled) => {
                let name = {
                    let cache = read_lock(&self.state.thermal.sensor_cache);
                    cache.keys.get(idx).cloned()
                };
                let Some(name) = name else {
                    return Some(Task::none());
                };
                self.mutate_config(|cfg| {
                    if cfg.telemetry.selected_sensors.is_empty() {
                        let cache = read_lock(&self.state.thermal.sensor_cache);
                        cfg.telemetry.selected_sensors = cache.keys.clone();
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
            Message::PollRateChanged(ms) => {
                let ms = ms.max(POLL_RATE_MIN_MS as u64);
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

    fn handle_tray_message(&mut self, message: Message) -> Option<Task<Message>> {
        match message {
            Message::CloseRequested(id) => {
                self.closing_window_id = Some(id);
                Some(Task::perform(async {}, |_| Message::MinimizeToTray))
            }
            Message::MinimizeToTray => {
                tracing::info!("MinimizeToTray: tray_initialized={}", self.tray_initialized);
                self.iconic_check_count = 0;
                if !self.tray_initialized {
                    if let Some(hwnd) = system_info::find_window_by_title("Framework Crate") {
                        tracing::info!("Found window HWND: {}", hwnd);
                        self.tray.init(hwnd);
                        self.tray_initialized = true;
                    } else {
                        tracing::warn!("Could not find window by title");
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
                        tracing::info!("Window hidden, tray icon visible");
                    } else {
                        self.pending_minimize_to_tray = true;
                        tracing::info!("Tray icon creation in progress, will hide on next tick");
                    }
                } else {
                    tracing::warn!("Cannot minimize to tray: HWND not found");
                }
                Some(Task::none())
            }
            Message::RestoreFromTray => {
                self.tray.mark_restored();
                self.tray.restore_window();
                self.state.lifecycle.visible.store(true, Ordering::Release);
                self.state.lifecycle.view_dirty.store(true, Ordering::Release);
                self.icon_create_in_flight = false;
                self.iconic_check_count = 0;
                Some(Task::none())
            }
            Message::TrayQuit => {
                self.tray.mark_restored();
                self.tray.restore_window();
                self.state.lifecycle.visible.store(true, Ordering::Release);
                let config = read_lock(&self.state.lifecycle.config);
                if config.fan.mode == FanControlMode::Manual {
                    self.quit_duty_value = config.fan.manual.as_ref().map(|m| m.duty_pct).unwrap_or(45).clamp(10, 100);
                    self.show_quit_warning = true;
                } else {
                    self.tray.shutdown();
                    self.state.lifecycle.shutdown.store(true, Ordering::Release);
                    self.save_config_now();
                    return Some(self.close_window());
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
                    crate::tray::TrayEvent::MenuQuit => {
                        Some(Task::perform(async {}, |_| Message::TrayQuit))
                    }
                    crate::tray::TrayEvent::PowerResumed => {
                        let now = crate::util::current_time_ms();
                        self.state.lifecycle.last_resume_ts.store(now, Ordering::Release);
                        tracing::warn!("[RESUME] System resumed from sleep/hibernate at {}", now);
                        Some(Task::none())
                    }
                }
            }
            _ => None,
        }
    }

    fn handle_quit_message(&mut self, message: Message) -> Option<Task<Message>> {
        match message {
            Message::QuitWithRestore => {
                self.show_quit_warning = false;
                self.state.lifecycle.shutdown.store(true, Ordering::Release);
                Some(Task::batch([
                    run_ec_task(&self.state.system.ec_client, |ec| {
                        if let Err(e) = ec.autofanctrl() {
                            warn!("Failed to restore auto fan control on quit: {}", e);
                        }
                    }),
                    Task::perform(async {}, |_| Message::QuitShutdown),
                ]))
            }
            Message::QuitDutyChanged(duty) => {
                self.quit_duty_value = duty.clamp(0, 100);
                Some(Task::none())
            }
            Message::QuitWithDuty => {
                self.show_quit_warning = false;
                self.state.lifecycle.shutdown.store(true, Ordering::Release);
                let duty = self.quit_duty_value;
                Some(Task::batch([
                    run_ec_task(&self.state.system.ec_client, move |ec| {
                        if let Err(e) = ec.set_fan_duty(duty, None) {
                            warn!("Failed to set quit fan duty: {}", e);
                        }
                    }),
                    Task::perform(async {}, |_| Message::QuitShutdown),
                ]))
            }
            Message::QuitWithoutRestore => {
                self.show_quit_warning = false;
                self.tray.shutdown();
                self.state.lifecycle.shutdown.store(true, Ordering::Release);
                self.save_config_now();
                Some(self.close_window())
            }
            Message::QuitShutdown => {
                self.show_quit_warning = false;
                self.tray.shutdown();
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
            Message::Tick | Message::InitComplete | Message::StartupError(_) | Message::WindowResized(..) => {}
            _ => {
                let now_ms = crate::util::current_time_ms();
                self.state.lifecycle.last_interaction_ts.store(now_ms, Ordering::Release);
            }
        }
        self.maybe_rebuild_snapshot();
        if let Some(task) = self.handle_config_message(message.clone()) {
            return task;
        }
        if let Some(task) = self.handle_tray_message(message.clone()) {
            return task;
        }
        if let Some(task) = self.handle_quit_message(message.clone()) {
            return task;
        }
        match message {
            Message::Tick => self.handle_tick_message(),
            Message::InitComplete => {
                self.init_complete = true;
                self.rebuild_header_info();
                self.rebuild_sensor_cache();
                self.cached_snapshot = Some(crate::views::ViewSnapshot::from_app(self));
                self.state.lifecycle.view_dirty.store(false, Ordering::Release);
                tick_task(0)
            }
            Message::StartupError(msg) => {
                self.startup_error = Some(msg);
                Task::none()
            }
            Message::WindowResized(id, size) => {
                self.window_id = Some(id);
                self.window_height = Some(size.height);
                Task::none()
            }
            Message::ToggleSensorSettings => {
                self.show_sensor_settings = !self.show_sensor_settings;
                Task::none()
            }
            Message::SettingsToggled => {
                self.show_settings = !self.show_settings;
                Task::none()
            }
            Message::KblightChanged(percent) => {
                let kblight = Arc::clone(&self.state.peripherals.kblight);
                run_ec_task(&self.state.system.ec_client, move |ec| {
                    if let Err(e) = ec.kblight_set(percent) {
                        warn!("Failed to set keyboard backlight: {}", e);
                    }
                    if let Ok(kb) = ec.kblight_get() {
                        with_write_lock(&kblight, |guard| {
                            *guard = Arc::new(Some(kb));
                        });
                    }
                })
            }
            Message::FpLedLevelChanged(level) => {
                run_ec_task(&self.state.system.ec_client, move |ec| {
                    if let Err(e) = ec.fp_led_level_set(level) {
                        warn!("Failed to set fingerprint LED: {}", e);
                    }
                })
            }
            Message::ToggleBatteryDetails => {
                self.show_battery_details = !self.show_battery_details;
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
                report.push_str(&format!("Display: {} {}\n", self.system_info.screen, self.system_info.refresh_rate));
                if let Some(v) = read_lock(&self.state.system.versions).as_ref() {
                    report.push_str(&format!("BIOS: {:?}\n", v.uefi_version));
                    report.push_str(&format!("EC Firmware: {:?}\n", v.ec_build_version));
                }
                let config = read_lock(&self.state.lifecycle.config);
                report.push_str(&format!("\nFan Mode: {:?}\n", config.fan.mode));
                report.push_str(&format!("Fan Duty: {}\n", self.state.fan.last_applied_duty.load(Ordering::Acquire)));
                report.push_str(&format!("Fan Count: {}\n", self.state.fan.fan_count.load(Ordering::Acquire)));
                report.push_str(&format!("Unified Duty: {}\n", self.state.fan.unified_duty.load(Ordering::Acquire)));
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
                    report.push_str(&format!("  Voltage: {:?}mV\n", battery.power_info.present_voltage_mv));
                    report.push_str(&format!("  Rate: {:?}mA\n", battery.power_info.present_rate_ma));
                }
                let pd_ports = read_lock(&self.state.peripherals.pd_ports);
                if !pd_ports.is_empty() {
                    report.push_str("\n=== PD Ports ===\n");
                    for port in pd_ports.iter() {
                        report.push_str(&format!("  Port {}: role={:?}, data={:?}, dp_alt={}, watts={:?}\n",
                            port.port, port.power_role, port.data_role, port.dp_alt_mode, port.negotiated_watts));
                    }
                }
                Task::perform(async move { Message::DebugInfoCollected(report) }, |r| r)
            }
            Message::DebugInfoCollected(report) => {
                self.debug_report = Some(report);
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn save_config(&mut self) {
        self.state.lifecycle.view_dirty.store(true, Ordering::Release);
        let cfg = read_lock(&self.state.lifecycle.config);
        if self.config_tx.send(Arc::clone(&cfg)).is_ok() {
            self.config_save_failed = false;
        } else {
            debug!("Config save channel dropped — falling back to sync save");
            // Synchronous save ensures durability when the async channel is gone.
            // On the hot path this is rare (only if config_save_task panicked);
            // during normal operation the channel-based path is used.
            let cfg_owned: Config = (*cfg).clone();
            drop(cfg);
            if let Err(e) = crate::config::save(&cfg_owned) {
                warn!("Fallback sync config save failed: {}", e);
                self.config_save_failed = true;
                self.state.lifecycle.bg_config_save_failed.store(true, Ordering::Relaxed);
            } else {
                self.config_save_failed = false;
                self.state.lifecycle.bg_config_save_failed.store(false, Ordering::Relaxed);
            }
        }
    }

    /// Synchronous config save — called only during shutdown paths
    /// (QuitWithoutRestore / CloseRequested). Using spawn_blocking here risks
    /// the write not completing before process::exit, so sync I/O is acceptable.
    fn save_config_now(&mut self) {
        let cfg = read_lock(&self.state.lifecycle.config);
        if let Err(e) = crate::config::save(&cfg) {
            warn!("Failed to save config: {}", e);
        }
    }

    /// Mutate the config under a write lock. Caller must call `save_config()`
    /// afterwards if the change should be persisted.
    fn mutate_config(&self, f: impl FnOnce(&mut Config)) {
        with_write_lock(&self.state.lifecycle.config, |guard| {
            f(Arc::make_mut(guard));
        });
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
        let pts: &[[u32; 2]] = cfg.fan.curve.as_ref().map(|c| c.curve.points.as_slice()).unwrap_or(&[]);
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
        self.system_info.header_device_name = versions.as_ref().as_ref()
            .and_then(|v| v.mainboard_type.as_deref())
            .unwrap_or("Framework Crate")
            .to_owned();

        let bios = versions.as_ref().as_ref()
            .and_then(|v| v.uefi_version.as_deref())
            .unwrap_or_default();

        let mut info = String::with_capacity(128);
        if !self.system_info.cpu.is_empty() {
            use std::fmt::Write;
            let _ = write!(info, "CPU: {}", self.system_info.cpu);
        }
        if self.system_info.mem != "N/A" {
            if !info.is_empty() { info.push_str("  |  "); }
            use std::fmt::Write;
            let _ = write!(info, "RAM: {}", self.system_info.mem);
        }
        if !self.system_info.os.is_empty() {
            if !info.is_empty() { info.push_str("  |  "); }
            use std::fmt::Write;
            let _ = write!(info, "OS: {}", self.system_info.os);
        }
        if !bios.is_empty() {
            if !info.is_empty() { info.push_str("  |  "); }
            use std::fmt::Write;
            let _ = write!(info, "BIOS: {}", bios);
        }
        if !self.system_info.screen.is_empty() {
            if !info.is_empty() { info.push_str("  |  "); }
            use std::fmt::Write;
            if !self.system_info.refresh_rate.is_empty() {
                let _ = write!(info, "Display: {} {}", self.system_info.screen, self.system_info.refresh_rate);
            } else {
                let _ = write!(info, "Display: {}", self.system_info.screen);
            }
        }
        self.system_info.header_info_text = info;
    }

    pub(crate) fn view(&self) -> Element<'_, Message> {
        views::view_main(self)
    }

    /// Rebuild sensor_cache sorted and colors from current config + keys.
    pub(crate) fn rebuild_sensor_cache(&self) {
        let cache = read_lock(&self.state.thermal.sensor_cache);
        let config = read_lock(&self.state.lifecycle.config);
        let sorted = crate::types::sorted_sensor_list(&config.telemetry.selected_sensors, &cache.keys);
        let colors: Vec<iced::Color> = sorted.iter()
            .map(|name| crate::style::sensor_color(name, &cache.keys))
            .collect();
        // Must drop the read lock on sensor_cache before acquiring a write lock below.
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
}
