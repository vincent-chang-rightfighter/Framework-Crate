#![cfg_attr(not(test), windows_subsystem = "windows")]
#![deny(unsafe_op_in_unsafe_fn)]

mod app;
mod background_task;
mod cli;
mod config;
mod config_save_task;
mod cpu_power;
mod curve_canvas;
mod fan_control;
mod probe;
mod style;
mod sub_state;
mod system_info;
mod temp_chart;
mod tray;
mod types;
mod util;
mod views;

pub use app::{App, AppState, Message, SystemInfo};
pub use style::*;
pub use util::{read_lock, with_write_lock};

include!(concat!(env!("OUT_DIR"), "/icon_rgba.rs"));

/// Tokio executor with 2 workers for tray app; EC calls use blocking pool, workers cover async loop and iced tasks pinned to LP-E core.
struct SmallTokioExecutor {
    rt: tokio::runtime::Runtime,
}

impl iced::Executor for SmallTokioExecutor {
    fn new() -> Result<Self, iced::futures::io::Error> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(8)
            .enable_all()
            .build()
            .map_err(iced::futures::io::Error::other)
            .map(|rt| Self { rt })
    }

    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        drop(self.rt.spawn(future));
    }

    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.rt.block_on(future)
    }

    fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.rt.enter();
        f()
    }
}

fn fallback_log(msg: &str) {
    // windows_subsystem hides stderr when double-clicked; also write to file with rotation
    eprintln!("{}", msg);
    if let Some(base) = dirs::config_dir().or_else(dirs::data_local_dir) {
        let path = base.join("framework-crate").join("app.log");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // rotate if >1MB, keep up to 3 backups
        if let Ok(meta) = std::fs::metadata(&path)
            && meta.len() > 1024 * 1024
        {
            let dir_path = base.join("framework-crate");
            let _ = std::fs::rename(dir_path.join("app.log.2"), dir_path.join("app.log.3"));
            let _ = std::fs::rename(dir_path.join("app.log.1"), dir_path.join("app.log.2"));
            let _ = std::fs::rename(&path, dir_path.join("app.log.1"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(f, "[{}] {}", chrono_like_timestamp(), msg);
        }
    }
}

fn chrono_like_timestamp() -> String {
    // lightweight ISO 8601 timestamp without chrono dep
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;
    // days since 1970-01-01 to Y-M-D (simplified leap year calc)
    let mut y = 1970u32;
    let mut remaining = days;
    loop {
        let days_in_year =
            if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) {
                366
            } else {
                365
            };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let month_days: [u32; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 1u32;
    for &d in &month_days {
        if remaining < d as u64 {
            break;
        }
        remaining -= d as u64;
        mo += 1;
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        y,
        mo,
        remaining + 1,
        h,
        m,
        s
    )
}

/// Single-instance guard; prevents parallel EC I/O and tray/config races from second process.
fn acquire_single_instance(minimized: bool) -> Option<system_info::SingleInstanceGuard> {
    match system_info::SingleInstanceGuard::acquire("FrameworkCrateSingleInstance") {
        Ok(guard) => Some(guard),
        Err(()) => {
            fallback_log("Framework Crate is already running.");
            // Only foreground existing window on manual launch; schtasks fires on unlock so stay silent.
            if !minimized {
                // Signal running instance to restore; second process cannot restore parked window itself.
                system_info::request_show_running_instance();
            }
            std::process::exit(0);
        }
    }
}

fn main() {
    let minimized = std::env::args().any(|a| a == "--minimized");
    // Hold guard for process lifetime.
    let _single_instance = acquire_single_instance(minimized);

    #[cfg(not(test))]
    {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(std::io::stderr)
            .init();
    }

    let window_icon =
        match iced::window::icon::from_rgba(ICON_RGBA.to_vec(), ICON_WIDTH, ICON_HEIGHT) {
            Ok(icon) => Some(icon),
            Err(e) => {
                // Run without icon instead of panicking before UI shows.
                tracing::error!("Failed to load window icon: {}", e);
                None
            }
        };

    fn app_title(_app: &App) -> String {
        "Framework Crate".to_string()
    }

    fn app_theme(_app: &App) -> iced::Theme {
        iced::Theme::Dark
    }

    iced::application(move || App::new(minimized), App::update, App::view)
        .title(app_title)
        .subscription(App::subscription)
        .theme(app_theme)
        .executor::<SmallTokioExecutor>()
        .antialiasing(false)
        .window(iced::window::Settings {
            // NOTE: .window overrides earlier size calls; set size here.
            size: iced::Size::new(900.0, 613.0),
            resizable: false,
            icon: window_icon,
            exit_on_close_request: false,
            ..iced::window::Settings::default()
        })
        .run()
        .unwrap_or_else(|e| {
            eprintln!("Failed to start application: {}", e);
            std::process::exit(1);
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FanControlMode, sorted_sensor_list};
    use std::sync::atomic::Ordering;

    #[test]
    fn sorted_sensor_list_empty_selected_uses_all_keys() {
        let selected: Vec<String> = vec![];
        let keys = vec!["C".into(), "A".into(), "B".into()];
        let result = sorted_sensor_list(&selected, &keys);
        assert_eq!(result, vec!["C", "A", "B"]);
    }

    #[test]
    fn sorted_sensor_list_filters_stale_names() {
        let selected = vec!["A".into(), "Gone".into(), "C".into()];
        let keys = vec!["A".into(), "B".into(), "C".into()];
        let result = sorted_sensor_list(&selected, &keys);
        assert_eq!(result, vec!["A", "C"]);
    }

    #[test]
    fn sorted_sensor_list_preserves_key_order() {
        let selected = vec!["C".into(), "A".into(), "B".into()];
        let keys = vec!["A".into(), "B".into(), "C".into()];
        let result = sorted_sensor_list(&selected, &keys);
        assert_eq!(result, vec!["A", "B", "C"]);
    }

    #[test]
    fn config_round_trip_toml() {
        let config = types::Config {
            fan: types::FanControlConfig {
                mode: FanControlMode::Manual,
                manual: Some(types::ManualConfig { duty_pct: 60 }),
                curve: None,
                ..Default::default()
            },
            battery: types::BatteryConfig::default(),
            telemetry: types::TelemetryConfig::default(),
        };
        let serialized = toml::to_string(&config).expect("serialize");
        let deserialized: types::Config = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(config, deserialized);
    }

    #[test]
    fn config_round_trip_with_curve() {
        let config = types::Config {
            fan: types::FanControlConfig {
                mode: FanControlMode::Curve,
                manual: None,
                curve: Some(types::GlobalCurveConfig {
                    poll_ms: 1000,
                    curve: types::CurveConfig {
                        sensors: vec![],
                        points: vec![[30, 10], [50, 50], [70, 90]],
                        hysteresis_c: 2,
                        rate_limit_pct_per_step: 5,
                        rate_limit_down_pct_per_step: None,
                    },
                }),
                ..Default::default()
            },
            battery: types::BatteryConfig::default(),
            telemetry: types::TelemetryConfig::default(),
        };
        let serialized = toml::to_string(&config).expect("serialize");
        let deserialized: types::Config = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(config, deserialized);
    }

    #[test]
    fn config_save_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("test_config.toml");

        let original = types::Config {
            fan: types::FanControlConfig {
                mode: FanControlMode::Manual,
                manual: Some(types::ManualConfig { duty_pct: 45 }),
                curve: None,
                ..Default::default()
            },
            battery: types::BatteryConfig {
                charge_limit_max_pct: Some(types::SettingU8 {
                    enabled: true,
                    value: 80,
                }),
            },
            telemetry: types::TelemetryConfig {
                poll_ms: 1000,
                ui_refresh_ms: 200,
                selected_sensors: vec!["CPU".into(), "Battery".into()],
            },
        };

        let serialized = toml::to_string(&original).expect("serialize");
        std::fs::write(&config_path, &serialized).expect("write");
        let content = std::fs::read_to_string(&config_path).expect("read");
        let deserialized: types::Config = toml::from_str(&content).expect("deserialize");
        assert_eq!(original, deserialized);
    }

    // Serialize tests touching config to avoid cross-test pollution.
    static APP_CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn app_config_lock() -> std::sync::MutexGuard<'static, ()> {
        APP_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[tokio::test]
    async fn settings_toggle_flips_flag() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        assert!(!app.show_settings);
        let _ = app.update(Message::SettingsToggled);
        assert!(app.show_settings);
        let _ = app.update(Message::SettingsToggled);
        assert!(!app.show_settings);
    }

    #[tokio::test]
    async fn sensor_settings_toggle_flips_flag() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        assert!(!app.show_sensor_settings);
        let _ = app.update(Message::ToggleSensorSettings);
        assert!(app.show_sensor_settings);
        let _ = app.update(Message::ToggleSensorSettings);
        assert!(!app.show_sensor_settings);
    }

    #[tokio::test]
    async fn chart_window_changed_validates_option() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        assert_eq!(app.chart_window_seconds, 30, "default window is 30s");
        let _ = app.update(Message::ChartWindowChanged(60));
        assert_eq!(app.chart_window_seconds, 60);
        let _ = app.update(Message::ChartWindowChanged(45));
        assert_eq!(
            app.chart_window_seconds, 60,
            "invalid windows must be rejected"
        );
    }

    #[tokio::test]
    async fn battery_details_toggle_flips_flag() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        assert!(!app.show_battery_details);
        let _ = app.update(Message::ToggleBatteryDetails);
        assert!(app.show_battery_details);
        let _ = app.update(Message::ToggleBatteryDetails);
        assert!(!app.show_battery_details);
    }

    #[tokio::test]
    async fn fan_mode_curve_creates_default_config() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanModeChanged(FanControlMode::Curve));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.mode, FanControlMode::Curve);
        assert!(cfg.fan.curve.is_some());
    }

    #[tokio::test]
    async fn fan_mode_disabled_sets_disabled() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanModeChanged(FanControlMode::Curve));
        let _ = app.update(Message::FanModeChanged(FanControlMode::Disabled));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.mode, FanControlMode::Disabled);
    }

    #[tokio::test]
    async fn fan_manual_duty_is_clamped() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanDutyChanged(5));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.manual.as_ref().map(|m| m.duty_pct), Some(5));
    }

    #[tokio::test]
    async fn fan_manual_duty_above_max_clamps() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanDutyChanged(200));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.manual.as_ref().map(|m| m.duty_pct), Some(100));
    }

    #[tokio::test]
    async fn fan_manual_duty_in_range_unchanged() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanDutyChanged(60));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.manual.as_ref().map(|m| m.duty_pct), Some(60));
    }

    #[tokio::test]
    async fn charge_limit_toggle_creates_default() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::ChargeLimitToggled(true));
        let cfg = read_lock(&app.state.lifecycle.config);
        let limit = cfg.battery.charge_limit_max_pct;
        assert!(limit.is_some());
        assert!(limit.unwrap().enabled);
    }

    #[tokio::test]
    async fn charge_limit_changes_value() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::ChargeLimitChanged(80));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.battery.charge_limit_max_pct.map(|l| l.value), Some(80));
    }

    #[tokio::test]
    async fn poll_rate_changes_config_and_atomic() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::PollRateChanged(1000));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.telemetry.poll_ms, 1000);
        assert_eq!(app.state.lifecycle.poll_ms.load(Ordering::Relaxed), 1000);
    }

    #[tokio::test]
    async fn ui_refresh_rate_changes_interval() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::UiRefreshRateChanged(200));
        assert_eq!(app.tick_interval_ms, 200);
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.telemetry.ui_refresh_ms, 200);
    }

    #[test]
    fn validate_battery_charge_limit_clamps_high() {
        let mut cfg = types::Config::default();
        cfg.battery.charge_limit_max_pct = Some(types::SettingU8 {
            enabled: true,
            value: 150,
        });
        cfg.validate();
        assert_eq!(cfg.battery.charge_limit_max_pct.unwrap().value, 100);
    }

    #[test]
    fn validate_battery_charge_limit_clamps_low() {
        let mut cfg = types::Config::default();
        cfg.battery.charge_limit_max_pct = Some(types::SettingU8 {
            enabled: true,
            value: 10,
        });
        cfg.validate();
        assert_eq!(cfg.battery.charge_limit_max_pct.unwrap().value, 25);
    }

    #[tokio::test]
    async fn close_request_in_non_manual_does_not_show_warning() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanModeChanged(FanControlMode::Disabled));
        let id = iced::window::Id::unique();
        let _task = app.update(Message::CloseRequested(id));
        assert!(!app.show_quit_warning);
    }

    #[tokio::test]
    async fn quit_cancel_hides_warning() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::QuitCanceled);
        assert!(!app.show_quit_warning);
    }

    #[tokio::test]
    async fn quit_duty_changes_clamped() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::QuitDutyChanged(5));
        assert_eq!(app.quit_duty_value, 5);
        let _ = app.update(Message::QuitDutyChanged(150));
        assert_eq!(app.quit_duty_value, 100);
        let _ = app.update(Message::QuitDutyChanged(50));
        assert_eq!(app.quit_duty_value, 50);
    }
}
