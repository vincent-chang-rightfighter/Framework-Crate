#![cfg_attr(not(test), windows_subsystem = "windows")]
#![deny(unsafe_op_in_unsafe_fn)]

mod cli;
mod config;
mod cpu_power;
mod types;
mod curve_canvas;
mod temp_chart;
mod probe;
mod system_info;
mod fan_control;
mod style;
mod views;
mod app;
mod background_task;
mod config_save_task;
mod sub_state;
mod tray;
mod util;

pub use app::{App, AppState, SystemInfo, Message};
pub use util::{read_lock, with_write_lock};
pub use style::*;

include!(concat!(env!("OUT_DIR"), "/icon_rgba.rs"));

fn main() {
    // Keep the async runtime small for a tray app. The Win32 tray message pump
    // already owns a native thread; Tokio should not spin up a large worker pool.
    // Respect an explicit override from the environment but default to two workers.
    if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
        // SAFETY: This is called at the very start of main(), before any user
        // threads are created and before Tokio initializes its worker pool.
        // The only code that has run is core_affinity::get_core_ids() which
        // may create threads on Windows, but they complete before this line.
        // No other thread can observe the environment change because Tokio
        // has not started yet. After this point, set_var is never called again.
        unsafe {
            std::env::set_var("TOKIO_WORKER_THREADS", "2");
        }
    }

    background_task::pin_to_slowest_core();

    #[cfg(not(test))]
    {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
            )
            .with_writer(std::io::stderr)
            .init();
    }

    let window_icon = iced::window::icon::from_rgba(
        ICON_RGBA.to_vec(),
        ICON_WIDTH,
        ICON_HEIGHT,
    )
    .expect("Failed to load window icon");

    fn app_title(_app: &App) -> String {
        "Framework Crate".to_string()
    }

    fn app_theme(_app: &App) -> iced::Theme {
        iced::Theme::Dark
    }

    iced::application(App::new, App::update, App::view)
        .title(app_title)
        .subscription(App::subscription)
        .theme(app_theme)
        .window(iced::window::Settings {
            // NOTE: `.window(...)` overrides any earlier `.window_size()`
            // / `.resizable()` calls, so the size must be set here.
            size: iced::Size::new(900.0, 613.0),
            resizable: false,
            icon: Some(window_icon),
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
    use crate::types::{sorted_sensor_list, FanControlMode};
    use std::sync::atomic::Ordering;

    // Tests: sorted_sensor_list ordering

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

    // Tests: Config serialization round-trip

    #[test]
    fn config_round_trip_toml() {
        let config = types::Config {
            fan: types::FanControlConfig {
                mode: FanControlMode::Manual,
                manual: Some(types::ManualConfig { duty_pct: 60 }),
                curve: None,
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
            },
            battery: types::BatteryConfig {
                charge_limit_max_pct: Some(types::SettingU8 { enabled: true, value: 80 }),
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

    // --- Integration tests: UI state toggles ---

    // App::new() loads/saves the config file (test-isolated via config_path),
    // so serialize tests that touch it to avoid cross-test state pollution.
    static APP_CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn app_config_lock() -> std::sync::MutexGuard<'static, ()> {
        APP_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[tokio::test]
    async fn settings_toggle_flips_flag() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        assert!(!app.show_settings);
        let _ = app.update(Message::SettingsToggled);
        assert!(app.show_settings);
        let _ = app.update(Message::SettingsToggled);
        assert!(!app.show_settings);
    }

    #[tokio::test]
    async fn sensor_settings_toggle_flips_flag() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        assert!(!app.show_sensor_settings);
        let _ = app.update(Message::ToggleSensorSettings);
        assert!(app.show_sensor_settings);
        let _ = app.update(Message::ToggleSensorSettings);
        assert!(!app.show_sensor_settings);
    }

    #[tokio::test]
    async fn battery_details_toggle_flips_flag() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        assert!(!app.show_battery_details);
        let _ = app.update(Message::ToggleBatteryDetails);
        assert!(app.show_battery_details);
        let _ = app.update(Message::ToggleBatteryDetails);
        assert!(!app.show_battery_details);
    }

    // --- Integration tests: Config changes through messages ---

    #[tokio::test]
    async fn fan_mode_curve_creates_default_config() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::FanModeChanged(FanControlMode::Curve));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.mode, FanControlMode::Curve);
        assert!(cfg.fan.curve.is_some());
    }

    #[tokio::test]
    async fn fan_mode_disabled_sets_disabled() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::FanModeChanged(FanControlMode::Curve));
        let _ = app.update(Message::FanModeChanged(FanControlMode::Disabled));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.mode, FanControlMode::Disabled);
    }

    #[tokio::test]
    async fn fan_manual_duty_is_clamped() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::FanDutyChanged(5));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.manual.as_ref().map(|m| m.duty_pct), Some(5));
    }

    #[tokio::test]
    async fn fan_manual_duty_above_max_clamps() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::FanDutyChanged(200));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.manual.as_ref().map(|m| m.duty_pct), Some(100));
    }

    #[tokio::test]
    async fn fan_manual_duty_in_range_unchanged() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::FanDutyChanged(60));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.manual.as_ref().map(|m| m.duty_pct), Some(60));
    }

    #[tokio::test]
    async fn charge_limit_toggle_creates_default() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::ChargeLimitToggled(true));
        let cfg = read_lock(&app.state.lifecycle.config);
        let limit = cfg.battery.charge_limit_max_pct;
        assert!(limit.is_some());
        assert!(limit.unwrap().enabled);
    }

    #[tokio::test]
    async fn charge_limit_changes_value() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::ChargeLimitChanged(80));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.battery.charge_limit_max_pct.map(|l| l.value), Some(80));
    }

    #[tokio::test]
    async fn poll_rate_changes_config_and_atomic() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::PollRateChanged(1000));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.telemetry.poll_ms, 1000);
        assert_eq!(app.state.lifecycle.poll_ms.load(Ordering::Relaxed), 1000);
    }

    #[tokio::test]
    async fn ui_refresh_rate_changes_interval() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::UiRefreshRateChanged(200));
        assert_eq!(app.tick_interval_ms, 200);
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.telemetry.ui_refresh_ms, 200);
    }

    // --- Integration tests: Config validation ---

    #[test]
    fn validate_battery_charge_limit_clamps_high() {
        let mut cfg = types::Config::default();
        cfg.battery.charge_limit_max_pct = Some(types::SettingU8 { enabled: true, value: 150 });
        cfg.validate();
        assert_eq!(cfg.battery.charge_limit_max_pct.unwrap().value, 100);
    }

    #[test]
    fn validate_battery_charge_limit_clamps_low() {
        let mut cfg = types::Config::default();
        cfg.battery.charge_limit_max_pct = Some(types::SettingU8 { enabled: true, value: 10 });
        cfg.validate();
        assert_eq!(cfg.battery.charge_limit_max_pct.unwrap().value, 25);
    }

    // --- Integration tests: Quit flow ---

    #[tokio::test]
    async fn close_request_in_non_manual_does_not_show_warning() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::FanModeChanged(FanControlMode::Disabled));
        let id = iced::window::Id::unique();
        let _task = app.update(Message::CloseRequested(id));
        assert!(!app.show_quit_warning);
    }

    #[tokio::test]
    async fn quit_cancel_hides_warning() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::QuitCanceled);
        assert!(!app.show_quit_warning);
    }

    #[tokio::test]
    async fn quit_duty_changes_clamped() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new();
        let _ = app.update(Message::QuitDutyChanged(5));
        assert_eq!(app.quit_duty_value, 5);
        let _ = app.update(Message::QuitDutyChanged(150));
        assert_eq!(app.quit_duty_value, 100);
        let _ = app.update(Message::QuitDutyChanged(50));
        assert_eq!(app.quit_duty_value, 50);
    }
}
