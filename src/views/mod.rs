pub mod battery;
pub mod cpu_power;
pub mod fan_control;
pub mod header;
pub mod misc;
pub mod quit_warning;
pub mod sensors;
pub mod settings;

use crate::App;
use crate::Message;
use crate::cli;
use crate::style::*;
use crate::util::read_lock;
use iced::widget::rule;
use iced::widget::space;
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Length};
use smallvec::SmallVec;
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Clone)]
pub(crate) struct ViewSnapshot {
    pub thermal: Arc<Option<cli::ec_wrapper::ThermalData>>,
    pub config: Arc<crate::types::Config>,
    pub sensor_cache: Arc<crate::app::SensorCache>,
    pub temp_history: std::sync::Arc<std::collections::VecDeque<crate::temp_chart::TempSample>>,
    pub battery: Arc<Option<crate::types::BatteryInfo>>,
    pub kblight: Arc<Option<u32>>,
    pub expansion_cards: Arc<SmallVec<[cli::ec_wrapper::ExpansionCard; 4]>>,
    pub pd_ports: Arc<SmallVec<[cli::ec_wrapper::UsbCPort; 4]>>,
    pub pd_ports_history: Arc<crate::sub_state::PdPortsHistory>,
    pub pd_usb_c_seen: Arc<Vec<bool>>,
    pub curve_full_points: Arc<Vec<[u32; 2]>>,
    pub platform: cli::ec_wrapper::PlatformFamily,
    pub fan_count: u64,
    pub unified_duty: bool,
    pub per_fan_duty: Arc<Vec<u32>>,
    pub expansion_card_debug: bool,
    pub cpu_power: Arc<crate::cpu_power::CpuPowerInfo>,
    pub sync_enabled: bool,
    pub show_cpu_power_settings: bool,
    pub show_curve_settings: bool,
    pub pl_custom_applied: bool,
    pub modules_download_error: Option<String>,
    pub pl1_edit: String,
    pub pl2_edit: String,
    pub pl1_time_edit: String,
    pub pl1_enabled: bool,
    pub pl2_enabled: bool,
    pub pl1_clamped: bool,
    pub pl2_clamped: bool,
    pub cpu_power_error: Option<String>,
    pub ec_op_error: Option<String>,
    pub intel_cpu: bool,
    pub curve_points: Arc<[[u32; 2]]>,
    pub curve_marks: Arc<Vec<crate::curve_canvas::SensorMark>>,
}

impl ViewSnapshot {
    pub fn from_app(app: &App) -> Self {
        let now_ms = crate::util::monotonic_ms() as i64;
        let thermal_snap = app.state.thermal.snapshot(now_ms);
        let peripheral_snap = app.state.peripherals.snapshot();
        let platform = *read_lock(&app.state.system.platform);
        let fan_count = app.state.fan.fan_count.load(Ordering::Acquire);
        let unified_duty = app.state.fan.unified_duty.load(Ordering::Acquire);
        let per_fan_duty = Arc::clone(&read_lock(&app.state.fan.per_fan_duty));
        let cpu_power = app.state.cpu_power.snapshot();
        let sync_enabled = app.state.cpu_power.sync_enabled.load(Ordering::Acquire);
        let config = Arc::clone(&read_lock(&app.state.lifecycle.config));
        let curve_points: Arc<[[u32; 2]]> = config
            .fan
            .curve
            .as_ref()
            .map(|c| Arc::from(c.curve.points.as_slice()))
            .unwrap_or_else(|| Arc::from(Vec::<[u32; 2]>::new() as Vec<[u32; 2]>));
        let curve_marks: Arc<Vec<crate::curve_canvas::SensorMark>> =
            if let Some(thermal) = thermal_snap.data.as_ref().as_ref() {
                if let Some(curve) = config.fan.curve.as_ref() {
                    let keys = &thermal_snap.sensor_cache.keys;
                    let sensors: Vec<&str> = {
                        let configured: Vec<&str> =
                            curve.curve.sensors.iter().map(|s| s.as_str()).collect();
                        let has_reading = configured.iter().any(|s| thermal.temps.contains_key(*s));
                        if has_reading {
                            configured
                        } else {
                            let mut best: Option<(&str, i32)> = None;
                            for (name, t) in thermal.temps.iter() {
                                let name: &str = name;
                                if crate::types::is_battery_sensor(name) {
                                    continue;
                                }
                                if best.is_none_or(|(_, bt)| *t > bt) {
                                    best = Some((name, *t));
                                }
                            }
                            if best.is_none() {
                                best = thermal
                                    .temps
                                    .iter()
                                    .max_by_key(|(_, t)| **t)
                                    .map(|(name, t)| (name.as_str(), *t));
                            }
                            best.into_iter().map(|(n, _)| n).collect()
                        }
                    };
                    let mut marks = Vec::new();
                    for name in sensors {
                        if let Some(t) = thermal.temps.get(name) {
                            let idx = keys.iter().position(|k| k == name).unwrap_or(0);
                            marks.push(crate::curve_canvas::SensorMark {
                                temp: *t,
                                color: crate::style::SENSOR_COLORS
                                    [idx % crate::style::SENSOR_COLORS.len()],
                            });
                        }
                    }
                    Arc::new(marks)
                } else {
                    Arc::new(Vec::new())
                }
            } else {
                Arc::new(Vec::new())
            };
        Self {
            thermal: thermal_snap.data,
            config,
            sensor_cache: thermal_snap.sensor_cache,
            temp_history: thermal_snap.temp_history,
            battery: Arc::clone(&read_lock(&app.state.battery.info)),
            kblight: peripheral_snap.kblight,
            expansion_cards: peripheral_snap.expansion_cards,
            pd_ports: peripheral_snap.pd_ports,
            pd_ports_history: peripheral_snap.pd_ports_history,
            pd_usb_c_seen: peripheral_snap.pd_usb_c_seen,
            curve_full_points: Arc::clone(&read_lock(&app.state.fan.curve_full_points)),
            platform,
            fan_count,
            unified_duty,
            per_fan_duty,
            expansion_card_debug: app.expansion_card_debug,
            cpu_power,
            sync_enabled,
            show_cpu_power_settings: app.show_cpu_power_settings,
            show_curve_settings: app.show_curve_settings,
            pl_custom_applied: app.pl_custom_applied.load(Ordering::Acquire),
            modules_download_error: app.modules_download_error.clone(),
            pl1_edit: app.pl1_edit.clone(),
            pl2_edit: app.pl2_edit.clone(),
            pl1_time_edit: app.pl1_time_edit.clone(),
            pl1_enabled: app.pl1_enabled,
            pl2_enabled: app.pl2_enabled,
            pl1_clamped: app.pl1_clamped,
            pl2_clamped: app.pl2_clamped,
            cpu_power_error: app.cpu_power_error.clone(),
            ec_op_error: app.ec_op_error.clone(),
            intel_cpu: app.state.system.intel_cpu.load(Ordering::Acquire),
            curve_points,
            curve_marks,
        }
    }
}

pub(crate) fn warning_banner(msg: String) -> Element<'static, Message> {
    container(
        row![
            colored_dot(iced::Color::from_rgb(0.9, 0.6, 0.0), 8.0),
            text(msg).size(FONT_BODY),
            space::horizontal(),
            button(text("Dismiss").size(FONT_SMALL))
                .on_press(Message::DismissConfigWarning)
                .style(btn_style),
        ]
        .align_y(iced::Alignment::Center)
        .spacing(8),
    )
    .padding(iced::Padding::from([6, 12]))
    .width(Length::Fill)
    .style(|_theme| iced::widget::container::Style {
        background: Some(iced::Color::from_rgba(0.9, 0.6, 0.0, 0.15).into()),
        border: iced::Border::default()
            .rounded(4)
            .width(1)
            .color(iced::Color::from_rgb(0.9, 0.6, 0.0)),
        ..Default::default()
    })
    .into()
}

pub(crate) fn not_supported_section(title: &str) -> Element<'_, Message> {
    container(
        column![
            text(title)
                .size(FONT_SECTION)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_NOT_SUPPORTED_TEXT)
                }),
            text("Not Supported")
                .size(FONT_BODY)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_NOT_SUPPORTED_TEXT)
                }),
        ]
        .spacing(4),
    )
    .padding(iced::Padding::from([8, 12]))
    .width(Length::Fill)
    .style(|_theme| iced::widget::container::Style {
        background: Some(COLOR_NOT_SUPPORTED_BG.into()),
        border: iced::Border::default().rounded(4),
        ..Default::default()
    })
    .into()
}

pub fn view_main(app: &App) -> Element<'_, Message> {
    if let Some(ref err) = app.startup_error {
        let mut content = column![].spacing(8).padding(20);
        content = content.push(text("Framework Crate").size(20));
        content = content.push(rule::horizontal(1));
        content = content.push(text(err.as_str()).size(FONT_BODY));
        content = content.push(rule::horizontal(1));
        content = content
            .push(text("Close this window and run the app as administrator.").size(FONT_BODY));
        return container(content)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
    }

    if !app.init_complete {
        let content = column![
            text("Framework Crate").size(20),
            rule::horizontal(1),
            text("Connecting to hardware...").size(FONT_BODY),
        ]
        .spacing(8)
        .padding(20);
        return container(content)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
    }

    if app.show_settings {
        return settings::view_settings(app);
    }

    if app.show_quit_warning {
        return quit_warning::view_quit_warning(app);
    }

    let snap = match &app.cached_snapshot {
        Some(snap) => snap,
        None => {
            // First frame before Tick rebuild: placeholder avoids borrowing temporary.
            return container(text("Preparing view...").size(FONT_BODY))
                .padding(20)
                .into();
        }
    };

    let header = header::view_header(app);
    let config_warning = if app.config_save_failed {
        Some(warning_banner(
            "Config save failed - changes may not persist after restart".to_string(),
        ))
    } else {
        app.config_load_warning
            .as_ref()
            .map(|msg| warning_banner(format!("Config load failed (using defaults): {}", msg)))
    };
    let cli_warning = if app.init_complete && !app.cli_present {
        Some(
            container(
                row![
                    colored_dot(iced::Color::from_rgb(0.9, 0.3, 0.3), 8.0),
                    text("EC unavailable ??hardware control disabled").size(FONT_BODY),
                ]
                .align_y(iced::Alignment::Center)
                .spacing(8),
            )
            .padding(iced::Padding::from([6, 12]))
            .width(Length::Fill)
            .style(|_theme| iced::widget::container::Style {
                background: Some(iced::Color::from_rgba(0.9, 0.2, 0.2, 0.1).into()),
                border: iced::Border::default().rounded(4),
                ..Default::default()
            }),
        )
    } else {
        None
    };
    let right_column = scrollable(
        container(
            column![
                card(fan_control::view_fan_control(snap)),
                card(cpu_power::cpu_power_section(snap)),
                card(misc::view_misc(snap)),
            ]
            .spacing(8),
        )
        .padding(iced::Padding {
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            left: 8.0,
        }),
    )
    // Reserve scrollbar strip to match left inset and avoid overlapping cards.
    .direction(scrollable::Direction::Vertical(
        scrollable::Scrollbar::new()
            .width(6.0)
            .scroller_width(6.0)
            .spacing(4.0),
    ))
    .height(Length::Fill);

    let content = container(
        row![
            container(
                column![card(sensors::view_sensors(app, snap)), card(battery::view_battery(app, snap)),]
                    .width(Length::FillPortion(1))
                    .spacing(8)
            )
            // Match right column inset for symmetric card alignment.
            .padding(iced::Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: 10.0
            })
            .width(Length::FillPortion(1)),
            right_column.width(Length::FillPortion(1)),
        ]
        .spacing(12),
    )
    .padding(iced::Padding {
        top: 4.0,
        right: 12.0,
        bottom: 12.0,
        left: 12.0,
    })
    .width(Length::Fill);

    let mut root = column![header].spacing(5);
    // Keep banner slot always present to preserve Tree state when warnings appear.
    let banner_slot: Element<'_, Message> = match (config_warning, cli_warning) {
        (Some(w), None) => container(w).padding(iced::Padding::from([4, 0])).into(),
        (None, Some(w)) => container(w).padding(iced::Padding::from([4, 0])).into(),
        (Some(w1), Some(w2)) => container(column![w1, w2].spacing(4))
            .padding(iced::Padding::from([4, 0]))
            .into(),
        (None, None) => container(space()).into(),
    };
    root = root.push(banner_slot);
    let root = root.push(content);
    // Probe reports content height so window resizes to fit.
    crate::probe::HeightProbe::wrap(root.into(), Arc::clone(&app.content_height))
}


#[cfg(test)]
mod tests {
    use crate::cli::ec_wrapper::BatteryData;
    use crate::views::battery::{
        charge_limit_display, view_battery_info, view_battery_verbose, view_charge_limit_section,
    };

    fn default_battery_info() -> BatteryData {
        BatteryData::default()
    }

    #[test]
    fn view_charge_limit_section_returns_element() {
        let _el = view_charge_limit_section(true, 80);
    }

    #[test]
    fn charge_limit_display_unset_is_disabled_100() {
        assert_eq!(charge_limit_display(None), (false, 100));
    }

    #[test]
    fn charge_limit_display_configured_uses_values() {
        assert_eq!(
            charge_limit_display(Some(crate::types::SettingU8 {
                enabled: true,
                value: 80
            })),
            (true, 80)
        );
    }

    #[test]
    fn view_battery_info_empty_battery() {
        let bat = default_battery_info();
        let _el = view_battery_info(&bat, false);
    }

    #[test]
    fn view_battery_info_with_health() {
        let mut bat = default_battery_info();
        bat.last_full_charge_capacity_mah = Some(4500);
        bat.design_capacity_mah = Some(5000);
        let _el = view_battery_info(&bat, true);
    }

    #[test]
    fn view_battery_info_without_design_capacity() {
        let mut bat = default_battery_info();
        bat.remaining_capacity_mah = Some(4500);
        bat.design_capacity_mah = None;
        let _el = view_battery_info(&bat, false);
    }

    #[test]
    fn view_battery_info_with_cycles() {
        let mut bat = default_battery_info();
        bat.cycle_count = Some(150);
        let _el = view_battery_info(&bat, false);
    }

    #[test]
    fn view_battery_info_with_voltage_and_current() {
        let mut bat = default_battery_info();
        bat.present_voltage_mv = Some(12000);
        bat.present_rate_ma = Some(2500);
        let _el = view_battery_info(&bat, true);
        let _el2 = view_battery_info(&bat, false);
    }

    #[test]
    fn view_battery_verbose_no_verbose_fields() {
        let bat = default_battery_info();
        assert!(view_battery_verbose(&bat, false).is_none());
        assert!(view_battery_verbose(&bat, true).is_none());
    }

    #[test]
    fn view_battery_verbose_with_manufacturer() {
        let mut bat = default_battery_info();
        bat.manufacturer = Some("Intel".to_string());
        assert!(view_battery_verbose(&bat, false).is_some());
        assert!(view_battery_verbose(&bat, true).is_some());
    }

    #[test]
    fn view_battery_verbose_with_multiple_fields() {
        let mut bat = default_battery_info();
        bat.model_number = Some("ABC123".to_string());
        bat.serial_number = Some("SN001".to_string());
        bat.battery_type = Some("Li-ion".to_string());
        assert!(view_battery_verbose(&bat, false).is_some());
    }

    #[test]
    fn battery_health_calculation() {
        assert_eq!(crate::types::battery_health_pct(4500, 5000), Some(90));
    }

    #[test]
    fn battery_health_full() {
        assert_eq!(crate::types::battery_health_pct(5000, 5000), Some(100));
    }

    #[test]
    fn battery_health_degraded() {
        assert_eq!(crate::types::battery_health_pct(3000, 5000), Some(60));
    }

    #[test]
    fn voltage_formatting() {
        let mv = 12340u32;
        let v = mv as f32 / 1000.0;
        assert!((v - 12.34).abs() < 0.01);
    }

    #[test]
    fn current_prefix_charging() {
        let charging = true;
        let prefix = if charging { "" } else { "-" };
        assert_eq!(prefix, "");
    }

    #[test]
    fn current_prefix_discharging() {
        let charging = false;
        let prefix = if charging { "" } else { "-" };
        assert_eq!(prefix, "-");
    }
}
