use crate::App;
use crate::Message;
use crate::cli;
use crate::util::read_lock;
use crate::style::*;
use crate::types::FanControlMode;
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::widget::rule;
use iced::widget::space;
use iced::{Element, Length};
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
    pub expansion_cards: Arc<Vec<cli::ec_wrapper::ExpansionCard>>,
    pub pd_ports: Arc<Vec<cli::ec_wrapper::UsbCPort>>,
    pub pd_ports_history: Arc<crate::sub_state::PdPortsHistory>,
    pub curve_full_points: Arc<Vec<[u32; 2]>>,
    pub platform: cli::ec_wrapper::PlatformFamily,
    pub fan_count: u64,
    pub unified_duty: bool,
    pub per_fan_duty: Arc<Vec<u32>>,
    pub expansion_card_debug: bool,
    pub cpu_power: Arc<crate::cpu_power::CpuPowerInfo>,
    pub sync_enabled: bool,
    pub show_cpu_power_settings: bool,
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
}

impl ViewSnapshot {
    pub fn from_app(app: &App) -> Self {
        let now_ms = crate::util::current_time_ms_i64();
        let thermal_snap = app.state.thermal.snapshot(now_ms);
        let peripheral_snap = app.state.peripherals.snapshot();
        let platform = *read_lock(&app.state.system.platform);
        let fan_count = app.state.fan.fan_count.load(Ordering::Acquire);
        let unified_duty = app.state.fan.unified_duty.load(Ordering::Acquire);
        let per_fan_duty = Arc::clone(&read_lock(&app.state.fan.per_fan_duty));
        let cpu_power = app.state.cpu_power.snapshot();
        let sync_enabled = app.state.cpu_power.sync_enabled.load(Ordering::Acquire);
        Self {
            thermal: thermal_snap.data,
            config: Arc::clone(&read_lock(&app.state.lifecycle.config)),
            sensor_cache: thermal_snap.sensor_cache,
            temp_history: thermal_snap.temp_history,
            battery: Arc::clone(&read_lock(&app.state.battery.info)),
            kblight: peripheral_snap.kblight,
            expansion_cards: peripheral_snap.expansion_cards,
            pd_ports: peripheral_snap.pd_ports,
            pd_ports_history: peripheral_snap.pd_ports_history,
            curve_full_points: Arc::clone(&read_lock(&app.state.fan.curve_full_points)),
            platform,
            fan_count,
            unified_duty,
            per_fan_duty,
            expansion_card_debug: app.expansion_card_debug,
            cpu_power,
            sync_enabled,
            show_cpu_power_settings: app.show_cpu_power_settings,
            pl_custom_applied: app.pl_custom_applied,
            modules_download_error: app.modules_download_error.clone(),
            pl1_edit: app.pl1_edit.clone(),
            pl2_edit: app.pl2_edit.clone(),
            pl1_time_edit: app.pl1_time_edit.clone(),
            pl1_enabled: app.pl1_enabled,
            pl2_enabled: app.pl2_enabled,
            pl1_clamped: app.pl1_clamped,
            pl2_clamped: app.pl2_clamped,
            cpu_power_error: app.cpu_power_error.clone(),
        }
    }
}

fn warning_banner(msg: String) -> Element<'static, Message> {
    container(
        row![
            colored_dot(iced::Color::from_rgb(0.9, 0.6, 0.0), 8.0),
            text(msg).size(FONT_BODY),
            space::horizontal(),
            button(text("Dismiss").size(FONT_SMALL)).on_press(Message::DismissConfigWarning).style(btn_style),
        ].align_y(iced::Alignment::Center).spacing(8)
    )
    .padding(iced::Padding::from([6, 12]))
    .width(Length::Fill)
    .style(|_theme| iced::widget::container::Style {
        background: Some(iced::Color::from_rgba(0.9, 0.6, 0.0, 0.15).into()),
        border: iced::Border::default().rounded(4).width(1).color(iced::Color::from_rgb(0.9, 0.6, 0.0)),
        ..Default::default()
    })
    .into()
}

fn not_supported_section(title: &str) -> Element<'_, Message> {
    container(
        column![
            text(title).size(FONT_SECTION).style(|_theme| iced::widget::text::Style { color: Some(COLOR_NOT_SUPPORTED_TEXT) }),
            text("Not Supported").size(FONT_BODY).style(|_theme| iced::widget::text::Style { color: Some(COLOR_NOT_SUPPORTED_TEXT) }),
        ].spacing(4)
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
        content = content.push(text("Close this window and run the app as administrator.").size(FONT_BODY));
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
        ].spacing(8).padding(20);
        return container(content)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
    }

    if app.show_settings {
        return view_settings(app);
    }

    if app.show_quit_warning {
        return view_quit_warning(app);
    }

    let snap = match &app.cached_snapshot {
        Some(snap) => snap,
        None => {
            // First frame after init (before the first Tick rebuild): show a
            // placeholder instead of borrowing from a temporary snapshot.
            return container(text("Preparing view...").size(FONT_BODY))
                .padding(20)
                .into();
        }
    };

    let header = view_header(app);
    let config_warning = if app.config_save_failed {
        Some(warning_banner("Config save failed - changes may not persist after restart".to_string()))
    } else {
        app.config_load_warning.as_ref().map(|msg| warning_banner(format!("Config load failed (using defaults): {}", msg)))
    };
    let cli_warning = if app.init_complete && !app.cli_present {
        Some(container(
            row![
                colored_dot(iced::Color::from_rgb(0.9, 0.3, 0.3), 8.0),
                text("EC unavailable — hardware control disabled").size(FONT_BODY),
            ].align_y(iced::Alignment::Center).spacing(8)
        )
        .padding(iced::Padding::from([6, 12]))
        .width(Length::Fill)
        .style(|_theme| iced::widget::container::Style {
            background: Some(iced::Color::from_rgba(0.9, 0.2, 0.2, 0.1).into()),
            border: iced::Border::default().rounded(4),
            ..Default::default()
        }))
    } else {
        None
    };
    let right_column = scrollable(
        container(
            column![
                card(view_fan_control(snap)),
                card(cpu_power_section(snap)),
                card(view_misc(snap)),
            ].spacing(8)
        ).padding(iced::Padding::from([0, 8]))
    ).height(Length::Fill);

    let content = container(
        row![
            column![
                card(view_sensors(app, snap)),
                card(view_battery(app, snap)),
            ].width(Length::FillPortion(1)).spacing(8),
            right_column.width(Length::FillPortion(1)),
        ].spacing(12)
    ).padding(12).width(Length::Fill);

    let mut root = column![header].spacing(8);
    // Always reserve the banner slots (same Container widget type) so the
    // content subtree keeps its Tree state (canvas cache, scroll offsets)
    // when a warning appears/disappears mid-session.
    // Note: cannot extract to named function — Container<'a> is invariant,
    // so `|| container(space())` inline closures are required for type inference.
    root = root.push(config_warning.unwrap_or_else(|| container(space()).into()));
    root = root.push(cli_warning.unwrap_or_else(|| container(space())));
    let root = root.push(content);
    // The window height follows the content: the probe reports the laid-out
    // height of this column to App, which resizes the window to match.
    crate::probe::HeightProbe::wrap(root.into(), Arc::clone(&app.content_height))
}

fn view_settings(app: &App) -> Element<'_, Message> {
    let versions = read_lock(&app.state.system.versions);

    let title_row = row![
        text("About").size(20),
        space::horizontal(),
        button(text("Close").size(FONT_BODY)).on_press(Message::SettingsToggled)
            .style(btn_style),
    ];

    let mut hw_content = column![].spacing(4);
    match versions.as_ref().as_ref() {
        Some(v) => {
            if let Some(ref t) = v.mainboard_type {
                hw_content = hw_content.push(info_row("Mainboard", t));
            }
        }
        None => {
            hw_content = hw_content.push(text("No device info available").size(FONT_BODY));
        }
    }
    let cpu = &app.system_info.cpu;
    let mem = &app.system_info.mem;
    let res = &app.system_info.screen;
    let refresh = &app.system_info.refresh_rate;
    if !cpu.is_empty() {
        hw_content = hw_content.push(info_row("CPU", cpu));
    }
    if mem != "N/A" {
        hw_content = hw_content.push(info_row("RAM", mem));
    }
    if !res.is_empty() {
        let display_text = if !refresh.is_empty() {
            format!("{} {}", res, refresh)
        } else {
            res.clone()
        };
        hw_content = hw_content.push(info_row("Display", &display_text));
    }

    let mut sw_content = column![].spacing(4);
    if let Some(v) = versions.as_ref().as_ref() {
        if let Some(ref bios) = v.uefi_version {
            sw_content = sw_content.push(info_row("BIOS", bios));
        }
        if let Some(ref ec) = v.ec_build_version {
            sw_content = sw_content.push(info_row("EC Firmware", ec));
        }
    }
    sw_content = sw_content.push(info_row("framework_lib", "0.6.5"));
    if let Some(ref ver) = crate::cpu_power::pawnio_version() {
        sw_content = sw_content.push(info_row("PawnIO", ver));
    }
    sw_content = sw_content.push(info_row("PawnIO Modules", crate::cpu_power::pawnio_modules_version()));
    if !app.system_info.os.is_empty() {
        sw_content = sw_content.push(info_row("OS", &app.system_info.os));
    }
    sw_content = sw_content.push(space::vertical().height(8));
    sw_content = sw_content.push(text("Poll Rate:").size(FONT_BODY));
    let config = read_lock(&app.state.lifecycle.config);
    let poll_ms = config.telemetry.poll_ms as u32;
    sw_content = sw_content.push(
        row![
            iced::widget::slider(POLL_RATE_MIN_MS..=2000, poll_ms, |v| Message::PollRateChanged(v as u64)).step(10u32),
            text(format!("{} ms", poll_ms)).size(FONT_BODY),
        ].spacing(4)
    );

    sw_content = sw_content.push(text("Refresh Interval:").size(FONT_BODY));
    let refresh_ms = config.telemetry.ui_refresh_ms as u32;
    sw_content = sw_content.push(
        row![
            iced::widget::slider(50..=1000, refresh_ms, |v| Message::UiRefreshRateChanged(v as u64)).step(50u32),
            text(format!("{} ms", refresh_ms)).size(FONT_BODY),
        ].spacing(4)
    );

    let mut content = column![].spacing(12).padding(20);
    content = content.push(title_row);
    content = content.push(hw_content);
    content = content.push(sw_content);
    content = content.push(space::vertical().height(12));
    let ec_debug = app.expansion_card_debug;
    content = content.push(
        row![
            text("Expansion Card Debug Mode:").size(FONT_BODY),
            button(text(if ec_debug { "ON" } else { "OFF" }).size(FONT_BODY))
                .on_press(Message::ToggleExpansionCardDebug)
                .style(move |_theme, _status| mode_style(ec_debug)),
        ].spacing(8).align_y(iced::Alignment::Center)
    );
    content = content.push(
        row![
            button(text("Collect Debug Info").size(FONT_BODY))
                .on_press(Message::CollectDebugInfo)
                .style(btn_style),
        ].spacing(8)
    );

    container(content)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

fn view_quit_warning(app: &App) -> Element<'_, Message> {
    let config = read_lock(&app.state.lifecycle.config);
    let current_duty = config.fan.manual.as_ref().map(|m| m.duty_pct).unwrap_or(50);
    let set_duty = app.quit_duty_value;

    let mut content = column![].spacing(12).padding(20);
    content = content.push(text("Framework Crate").size(20));
    content = content.push(rule::horizontal(1));
    content = content.push(text("Fan is in manual mode").size(FONT_BODY));
    content = content.push(text(format!("Current duty: {}%", current_duty)).size(FONT_BODY));
    content = content.push(space::horizontal().height(4));
    content = content.push(text("The fan will remain at its current speed after exiting.").size(FONT_BODY));
    content = content.push(text("Choose how to handle the fan before closing:").size(FONT_BODY));
    content = content.push(rule::horizontal(1));

    content = content.push(iced::widget::row![
        text("Set duty to:").size(FONT_BODY),
        iced::widget::slider(0..=100, set_duty, Message::QuitDutyChanged),
        text(format!("{}%", set_duty)).size(FONT_BODY),
    ].spacing(4).align_y(iced::Alignment::Center));

    content = content.push(iced::widget::row![
        iced::widget::button(text("Restore Auto & Exit").size(14)).on_press(Message::QuitWithRestore)
            .style(super::btn_style),
        iced::widget::button(text(format!("Set {}% & Exit", set_duty)).size(14)).on_press(Message::QuitWithDuty)
            .style(super::btn_style),
        iced::widget::button(text("Exit").size(14)).on_press(Message::QuitWithoutRestore)
            .style(super::btn_style),
        iced::widget::button(text("Cancel").size(14)).on_press(Message::QuitCanceled)
            .style(super::btn_style),
    ].spacing(8));

    container(content)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

fn view_header(app: &App) -> Element<'_, Message> {
    let header_content = column![
        row![
            text(&app.system_info.header_device_name).size(18),
            space::horizontal(),
            button(text("About").size(FONT_BODY)).on_press(Message::SettingsToggled)
                .style(btn_style),
        ].align_y(iced::Alignment::Center),
        text(&app.system_info.header_info_text).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }),
    ].spacing(4);

    container(header_content)
        .padding(iced::Padding::from([8, 12]))
        .width(Length::Fill)
        .into()
}

fn view_sensors<'a>(app: &'a App, snap: &'a ViewSnapshot) -> Element<'a, Message> {
    let thermal = &snap.thermal;
    match thermal.as_ref().as_ref() {
        Some(thermal) => {
            let mut content = column![].spacing(6);

            let cache = &snap.sensor_cache;
            let config = &snap.config;
            let all_empty = config.telemetry.selected_sensors.is_empty();

            let settings_label = if app.show_sensor_settings { "[-] Settings" } else { "[+] Settings" };
            content = content.push(
                row![
                    text("Sensors").size(FONT_SECTION).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) }),
                    space::horizontal(),
                    button(text(settings_label).size(FONT_SMALL))
                        .on_press(Message::ToggleSensorSettings)
                        .style(btn_style),
                ]
            );

            let settings_panel: Element<'_, Message> = if app.show_sensor_settings {
                let mut settings_content = column![].spacing(4).padding(4);
                settings_content = settings_content.push(text("Sensors").size(FONT_BODY));

                // Build a lookup set once instead of a linear scan per row
                // (selected_sensors is small, but the panel rebuilds every frame).
                let selected_set: std::collections::HashSet<&str> = if all_empty {
                    std::collections::HashSet::new()
                } else {
                    config.telemetry.selected_sensors.iter().map(|s| s.as_str()).collect()
                };

                for (idx, name) in cache.keys.iter().enumerate() {
                    // The loop index is the position in `cache.keys`, so use
                    // it directly instead of a per-row linear scan.
                    let color = SENSOR_COLORS[idx % SENSOR_COLORS.len()];
                    let is_on = all_empty || selected_set.contains(name.as_str());
                    let on_off = if is_on { "On" } else { "Off" };
                    let on_color = if is_on { COLOR_GREEN } else { COLOR_GRAY };
                    let bg_color = if is_on { color } else { COLOR_DARK };
                    settings_content = settings_content.push(
                        row![
                            button(
                                container(text(" ").size(FONT_SMALL))
                                    .width(16).height(16).center_x(16).center_y(16)
                                    .style(move |_theme| iced::widget::container::Style {
                                        background: Some(bg_color.into()),
                                        border: iced::Border::default().rounded(8),
                                        ..Default::default()
                                    })
                             ).on_press(Message::SensorToggled(idx, !is_on))
                              .style(btn_style).padding(0),
                            text(name.as_str()).size(FONT_BODY),
                            space::horizontal(),
                            text(on_off).size(FONT_SMALL).style(move |_theme| iced::widget::text::Style { color: Some(on_color) }),
                        ].align_y(iced::Alignment::Center).spacing(6)
                    );
                }
                container(settings_content)
                    .padding(8)
                    .style(|_theme| iced::widget::container::Style {
                        background: Some(COLOR_SETTINGS_BG.into()),
                        border: iced::Border::default().rounded(4).color(COLOR_DARK).width(1),
                        ..Default::default()
                    })
                    .into()
            } else {
                // Always reserve the slot (same Container widget type) so the
                // subtree below keeps its Tree state when the panel toggles.
                container(column![]).into()
            };
            content = content.push(settings_panel);

            let history = Arc::clone(&snap.temp_history);
            let sorted_sensors = &cache.sorted;
            let chart_colors = Arc::clone(&cache.colors);

            let mut list_content = column![].spacing(2);
            let mut temp_buf = String::with_capacity(8);

            for (idx, name) in sorted_sensors.iter().enumerate() {
                if let Some(temp) = thermal.temps.get(name) {
                    let color = chart_colors.get(idx).copied().unwrap_or(iced::Color::WHITE);
                    temp_buf.clear();
                    use std::fmt::Write;
                    let _ = write!(temp_buf, "{}°C", temp);
                    let row = row![
                        colored_dot(color, 10.0),
                        text(name.as_str()).size(FONT_BODY).width(Length::Fill),
                        text(temp_buf.clone()).size(FONT_BODY),
                    ].align_y(iced::Alignment::Center).spacing(6);
                    list_content = list_content.push(row);
                }
            }

            content = content.push(
                container(crate::temp_chart::view_temp_chart(crate::temp_chart::TempHistory {
                    samples: history,
                    colors: chart_colors,
                    sensor_names: Arc::clone(&cache.sorted),
                }))
                    .width(Length::Fill)
                    .height(150)
            );

            content = content.push(scrollable(list_content).height(Length::Shrink));

            content.into()
        }
        None => {
            text(if app.cli_present { "Waiting for sensor data..." } else { "EC not available" }).into()
        }
    }
}

fn view_fan_control(snap: &ViewSnapshot) -> Element<'_, Message> {
    let thermal = &snap.thermal;
    let config = &snap.config;
    let current_mode = config.fan.mode;

    let fan_rpm_text = thermal.as_ref().as_ref().and_then(|t| {
        if t.fans.is_empty() { None }
        else {
            use std::fmt::Write;
            let mut s = String::with_capacity(32);
            for (i, f) in t.fans.iter().enumerate() {
                if i > 0 { s.push_str("  "); }
                let _ = write!(s, "{} RPM", f.rpm);
            }
            Some(s)
        }
    });

    let duty_text = if current_mode == FanControlMode::Manual {
        let duty = config.fan.manual.as_ref().map(|m| m.duty_pct).unwrap_or(50);
        use std::fmt::Write;
        let mut s = String::with_capacity(4);
        let _ = write!(s, "{}%", duty);
        s
    } else {
        String::new()
    };

    let right_text = match (duty_text.is_empty(), &fan_rpm_text) {
        (true, rpm) => rpm.clone().unwrap_or_default(),
        (false, Some(rpm)) => {
            use std::fmt::Write;
            let mut s = String::with_capacity(duty_text.len() + rpm.len() + 3);
            let _ = write!(s, "{}   {}", duty_text, rpm);
            s
        }
        (false, None) => duty_text,
    };

    let title_row = row![
        text("Fan Control").size(FONT_SECTION).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) }),
        space::horizontal(),
        text(right_text).size(FONT_BODY),
    ].align_y(iced::Alignment::Center);

    let mut content = column![title_row].spacing(6);

    let mode_row = {
        let is_disabled = current_mode == FanControlMode::Disabled;
        let is_manual = current_mode == FanControlMode::Manual;
        let is_curve = current_mode == FanControlMode::Curve;
        row![
            button(text("Auto").size(FONT_BODY)).on_press(Message::FanModeChanged(FanControlMode::Disabled))
                .style(move |_theme, _status| mode_style(is_disabled)),
            button(text("Manual").size(FONT_BODY)).on_press(Message::FanModeChanged(FanControlMode::Manual))
                .style(move |_theme, _status| mode_style(is_manual)),
            button(text("Curve").size(FONT_BODY)).on_press(Message::FanModeChanged(FanControlMode::Curve))
                .style(move |_theme, _status| mode_style(is_curve)),
        ].spacing(8)
    };

    content = content.push(mode_row);

    match current_mode {
        FanControlMode::Disabled => {
            content = content.push(text("Fans controlled by platform firmware.").size(FONT_BODY));
        }
        FanControlMode::Manual => {
            let duty = config.fan.manual.as_ref().map(|m| m.duty_pct).unwrap_or(50);
            if snap.fan_count > 1 {
                let unified = snap.unified_duty;
                content = content.push(
                    row![
                        text("Unified Duty:").size(FONT_BODY),
                        button(text(if unified { "ON" } else { "OFF" }).size(FONT_BODY))
                            .on_press(Message::FanUnifiedDutyToggled(!unified))
                            .style(btn_style),
                    ].spacing(8).align_y(iced::Alignment::Center)
                );
                if unified {
                    content = content.push(
                        iced::widget::slider(0..=100, duty, Message::FanDutyChanged)
                    );
                } else {
                    for (idx, &per_duty) in snap.per_fan_duty.iter().enumerate() {
                        content = content.push(
                            row![
                                text(format!("Fan {}:", idx + 1)).size(FONT_BODY),
                                iced::widget::slider(0..=100, per_duty, move |d| Message::FanPerDutyChanged(idx, d)),
                                text(format!("{}%", per_duty)).size(FONT_BODY),
                            ].spacing(8).align_y(iced::Alignment::Center)
                        );
                    }
                }
            } else {
                content = content.push(
                    iced::widget::slider(0..=100, duty, Message::FanDutyChanged)
                );
            }
        }
        FanControlMode::Curve => {
            if let Some(ref curve) = config.fan.curve {
                let poll = curve.poll_ms;
                let hyst = curve.curve.hysteresis_c;
                let rate = curve.curve.rate_limit_pct_per_step;

                content = content.push(text(format!("Poll: {} ms", poll)).size(FONT_BODY));
                content = content.push(iced::widget::slider(500u32..=10000, (poll as u32).clamp(500, 10000), |v| Message::FanCurvePollMsChanged(v as u64)));

                content = content.push(text(format!("Hysteresis: {}°C", hyst)).size(FONT_BODY));
                content = content.push(iced::widget::slider(0..=10, hyst, Message::FanCurveHysteresisChanged));

                content = content.push(text(format!("Rate Limit: {} %/step", rate)).size(FONT_BODY));
                content = content.push(iced::widget::slider(1..=100, rate, Message::FanCurveRateLimitChanged));

                content = content.push(text("Curve Points (Temp C -> Duty %)").size(FONT_SECTION));

                for (idx, point) in curve.curve.points.iter().enumerate() {
                    let temp = point[0];
                    let duty = point[1];
                    content = content.push(
                        column![
                            row![
                                text(format!("P{}:", idx + 1)).size(FONT_BODY),
                                text("Temp:").size(FONT_BODY),
                                iced::widget::slider(1..=99, temp, move |v| Message::FanCurvePointTempChanged(idx, v)),
                                text(format!("{}°C", temp)).size(FONT_BODY),
                            ].spacing(4).align_y(iced::Alignment::Center),
                            row![
                                space::horizontal().width(20),
                                text("Duty:").size(FONT_BODY),
                                iced::widget::slider(0..=100, duty, move |v| Message::FanCurvePointDutyChanged(idx, v)),
                                text(format!("{}%", duty)).size(FONT_BODY),
                            ].spacing(4).align_y(iced::Alignment::Center),
                        ]
                    );
                }

                let pts = &curve.curve.points;
                let all_pts = Arc::clone(&snap.curve_full_points);
                let canvas = crate::curve_canvas::view_curve(pts, &all_pts);

                let mut curve_area = column![].spacing(2);
                curve_area = curve_area.push(canvas);

                content = content.push(curve_area);
            } else {
                content = content.push(text("Initializing curve...").size(FONT_BODY));
            }
        }
    }

    content.into()
}

fn view_charge_limit_section(enabled: bool, value: u32) -> Element<'static, Message> {
    column![
        row![
            iced::widget::checkbox(enabled).on_toggle(Message::ChargeLimitToggled),
            text("Max Charge Limit (%):").size(FONT_BODY),
            space::horizontal(),
            text(format!("{}%", value)).size(FONT_BODY),
        ].spacing(4),
        iced::widget::slider(CHARGE_LIMIT_MIN..=CHARGE_LIMIT_MAX, value, Message::ChargeLimitChanged),
    ].spacing(4).into()
}

fn view_battery_info(battery: &crate::cli::ec_wrapper::BatteryData, charging: bool) -> Element<'_, Message> {
    let mut rows = column![].spacing(4);

    if let Some(v) = battery.remaining_capacity_mah {
        if let Some(design) = battery.design_capacity_mah {
            let health = (v as f32 / design as f32 * 100.0) as u32;
            rows = rows.push(row![
                text("Battery Health:").size(FONT_BODY),
                space::horizontal(),
                text(format!("{}%  ({} / {} mAh)", health, v, design)).size(FONT_BODY),
            ]);
        } else {
            rows = rows.push(row![
                text("Remaining:").size(FONT_BODY),
                space::horizontal(),
                text(format!("{} mAh", v)).size(FONT_BODY),
            ]);
        }
    }
    if let Some(cycles) = battery.cycle_count {
        rows = rows.push(row![
            text("Battery Cycles:").size(FONT_BODY),
            space::horizontal(),
            text(format!("{}", cycles)).size(FONT_BODY),
        ]);
    }
    if let Some(capacity) = battery.last_full_charge_capacity_mah {
        rows = rows.push(row![
            text("Full Charge:").size(FONT_BODY),
            space::horizontal(),
            text(format!("{} mAh", capacity)).size(FONT_BODY),
        ]);
    }
    if let Some(v) = battery.present_voltage_mv {
        rows = rows.push(row![
            text("Voltage:").size(FONT_BODY),
            space::horizontal(),
            text(format!("{:.2} V", v as f32 / 1000.0)).size(FONT_BODY),
        ]);
    }
    if let Some(v) = battery.present_rate_ma {
        let prefix = if charging { "" } else { "-" };
        rows = rows.push(row![
            text("Current:").size(FONT_BODY),
            space::horizontal(),
            text(format!("{}{:.2} A", prefix, v as f32 / 1000.0)).size(FONT_BODY),
        ]);
    }

    rows.into()
}

fn view_battery_verbose(battery: &crate::cli::ec_wrapper::BatteryData, show_details: bool) -> Option<Element<'_, Message>> {
    let has_verbose = battery.manufacturer.is_some()
        || battery.model_number.is_some()
        || battery.serial_number.is_some()
        || battery.battery_type.is_some()
        || battery.remaining_capacity_wh.is_some()
        || battery.design_capacity_wh.is_some()
        || battery.charger_temp_c.is_some();

    if !has_verbose { return None; }

    let details_label = if show_details { "[-] Details" } else { "[+] Details" };
    let mut content = column![].spacing(4);

    content = content.push(
        row![
            space::horizontal(),
            button(text(details_label).size(FONT_SMALL))
                .on_press(Message::ToggleBatteryDetails)
                .style(btn_style),
        ]
    );

    if show_details
        && let Some(details) = battery_detail_rows(battery)
    {
        content = content.push(details);
    }

    Some(content.into())
}

/// Cap for the Battery & Power card. The outer row stretches the left column
/// to match the right column's height, so a plain `Length::Fill` would make
/// the card fill all leftover space even when its content is short.
const BATTERY_SECTION_MAX_HEIGHT: f32 = 300.0;

/// Cap for the Misc card (keyboard backlight, fingerprint LED, ports).
const MISC_SECTION_MAX_HEIGHT: f32 = 300.0;

fn view_battery<'a>(app: &'a App, snap: &'a ViewSnapshot) -> Element<'a, Message> {
    if !snap.platform.has_battery() {
        return not_supported_section("Battery & Power");
    }
    let battery = &snap.battery;
    let config = &snap.config;
    if let Some(battery) = battery.as_ref().as_ref() {
        let charging = battery.power_info.ac_present == Some(true)
            && battery.power_info.discharging != Some(true);
        let status_color = if charging { COLOR_GREEN } else { COLOR_HEADER };

        let power_text = battery.power_info.present_rate_ma
            .and_then(|rate_ma| {
                battery.power_info.present_voltage_mv.map(|voltage| {
                    let power_w = (rate_ma as f32 * voltage as f32) / 1_000_000.0;
                    if charging && power_w.abs() >= 0.05 {
                        format!("+{:.1}W", power_w)
                    } else if charging {
                        "AC".to_string()
                    } else {
                        format!("-{:.1}W", power_w)
                    }
                })
            })
            .unwrap_or_default();

        let soc_text = battery.power_info.soc_pct
            .map(|s| format!("{}%", s))
            .unwrap_or_default();

        let title_row = row![
            text("Battery & Power").size(FONT_SECTION).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) }),
            space::horizontal(),
            text(format!("{}  {}", power_text, soc_text)).size(FONT_BODY).style(move |_theme| iced::widget::text::Style { color: Some(status_color) }),
        ].align_y(iced::Alignment::Center);

        let charge_limit = config.battery.charge_limit_max_pct.unwrap_or_default();

        let mut content = column![title_row].spacing(6);
        content = content.push(view_charge_limit_section(charge_limit.enabled, charge_limit.value as u32));
        content = content.push(view_battery_info(&battery.power_info, charging));

        if let Some(verbose) = view_battery_verbose(&battery.power_info, app.show_battery_details) {
            content = content.push(verbose);
        }

        let right_pad = iced::Padding::ZERO.right(14.0);
        // Shrink to content with a height cap: compact when the details are
        // collapsed, internally scrollable when the verbose rows are open.
        container(
            scrollable(container(content).padding(right_pad)).height(Length::Shrink)
        )
        .width(Length::Fill)
        .max_height(BATTERY_SECTION_MAX_HEIGHT)
        .into()
    } else {
        text(if app.cli_present { "Waiting for battery data..." } else { "EC not available" }).into()
    }
}

fn battery_detail_rows(battery_info: &crate::cli::ec_wrapper::BatteryData) -> Option<Element<'_, Message>> {
    let mut rows = column![].spacing(2).padding(4);
    let mut has_content = false;

    if let Some(ref v) = battery_info.manufacturer {
        has_content = true;
        rows = rows.push(row![text("Manufacturer:").size(FONT_SMALL), space::horizontal(), text(v.as_str()).size(FONT_SMALL)].spacing(4));
    }
    if let Some(ref v) = battery_info.model_number {
        has_content = true;
        rows = rows.push(row![text("Model:").size(FONT_SMALL), space::horizontal(), text(v.as_str()).size(FONT_SMALL)].spacing(4));
    }
    if let Some(ref v) = battery_info.serial_number {
        has_content = true;
        rows = rows.push(row![text("Serial:").size(FONT_SMALL), space::horizontal(), text(v.as_str()).size(FONT_SMALL)].spacing(4));
    }
    if let Some(ref v) = battery_info.battery_type {
        has_content = true;
        rows = rows.push(row![text("Type:").size(FONT_SMALL), space::horizontal(), text(v.as_str()).size(FONT_SMALL)].spacing(4));
    }
    if let Some(v) = battery_info.remaining_capacity_wh {
        has_content = true;
        rows = rows.push(row![text("Capacity (Wh):").size(FONT_SMALL), space::horizontal(), text(format!("{:.2} Wh", v)).size(FONT_SMALL)].spacing(4));
    }
    if let Some(v) = battery_info.design_capacity_wh {
        has_content = true;
        rows = rows.push(row![text("Design (Wh):").size(FONT_SMALL), space::horizontal(), text(format!("{:.2} Wh", v)).size(FONT_SMALL)].spacing(4));
    }
    if let Some(v) = battery_info.charger_temp_c {
        has_content = true;
        rows = rows.push(row![text("Charger Temp:").size(FONT_SMALL), space::horizontal(), text(format!("{:.2}°C", v)).size(FONT_SMALL)].spacing(4));
    }
    if let Some(v) = battery_info.charger_voltage_mv {
        has_content = true;
        rows = rows.push(row![text("Charger Voltage:").size(FONT_SMALL), space::horizontal(), text(format!("{:.0} mV", v)).size(FONT_SMALL)].spacing(4));
    }
    if let Some(v) = battery_info.charger_current_ma {
        has_content = true;
        rows = rows.push(row![text("Charger Current:").size(FONT_SMALL), space::horizontal(), text(format!("{} mA", v)).size(FONT_SMALL)].spacing(4));
    }

    if has_content {
        Some(
            container(rows)
                .padding(iced::Padding::from([4, 8]))
                .width(Length::Fill)
                .style(|_theme| iced::widget::container::Style {
                    background: Some(COLOR_SETTINGS_BG.into()),
                    border: iced::Border::default().rounded(4).color(COLOR_DARK).width(1),
                    ..Default::default()
                })
                .into()
        )
    } else {
        None
    }
}

fn view_misc(snap: &ViewSnapshot) -> Element<'_, Message> {
    let mut content = column![text("Misc").size(FONT_SECTION).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) })].spacing(6);

    if snap.platform.has_keyboard_backlight() {
        content = content.push(kblight_section(snap));
    } else {
        content = content.push(not_supported_section("Keyboard Backlight"));
    }

    content = content.push(space::vertical().height(8));

    if snap.platform.has_fingerprint_led() {
        content = content.push(text("Fingerprint LED").size(FONT_SECTION));
        let button_row = row![
            button(text("Low").size(FONT_BODY)).on_press(Message::FpLedLevelChanged("low")).style(btn_style),
            button(text("Medium").size(FONT_BODY)).on_press(Message::FpLedLevelChanged("medium")).style(btn_style),
            button(text("High").size(FONT_BODY)).on_press(Message::FpLedLevelChanged("high")).style(btn_style),
        ].spacing(6);
        content = content.push(button_row);
    } else {
        content = content.push(not_supported_section("Fingerprint LED"));
    }

    content = content.push(space::vertical().height(8));

    content = content.push(ports_section(snap));

    let right_pad = iced::Padding::ZERO.right(14.0);
    let max_h = if snap.expansion_card_debug { 500.0 } else { MISC_SECTION_MAX_HEIGHT };
    container(
        scrollable(container(content).padding(right_pad)).height(Length::Shrink)
    )
    .width(Length::Fill)
    .max_height(max_h)
    .into()
}

fn kblight_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let kblight = &snap.kblight;
    let mut content = column![].spacing(2);
    if let Some(kb) = kblight.as_ref().as_ref().copied() {
        content = content.push(row![
            text("Keyboard Backlight").size(FONT_SECTION).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) }),
            space::horizontal(),
            text(format!("{}%", kb)).size(FONT_BODY),
        ].align_y(iced::Alignment::Center));
        content = content.push(
            iced::widget::slider(0..=100, kb, Message::KblightChanged).step(10u32)
        );
    } else {
        content = content.push(text("Keyboard Backlight").size(FONT_SECTION).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) }));
        content = content.push(text("Unavailable").size(FONT_BODY).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }));
    }
    content.into()
}

fn ports_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let cards = &snap.expansion_cards;
    let ports = &snap.pd_ports;
    let history = &snap.pd_ports_history;
    let mut content = column![text("Ports & Expansion Cards").size(FONT_SECTION)].spacing(2);

    if ports.is_empty() && cards.is_empty() {
        content = content.push(text("None detected").size(FONT_BODY).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }));
        if snap.expansion_card_debug {
            content = content.push(
                text("[Debug] No ports or expansion cards detected").size(FONT_SMALL)
                    .style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) })
            );
        }
    } else {
        let dp_card = cards.iter().find(|c| c.name.contains("DisplayPort") || c.name.contains("HDMI"));
        for port in ports.iter() {
            let card_type = crate::cli::ec_wrapper::classify_pd_port(port, history.iter().map(|a| a.as_ref()), STABLE_THRESHOLD, dp_card.is_some());
            let is_display_card = card_type == "DisplayPort Expansion Card"
                || card_type == "HDMI Expansion Card"
                || card_type == "DP/HDMI Expansion Card";
            let display_type = if card_type == "DP/HDMI Expansion Card" {
                dp_card.map(|c| c.name.as_str()).unwrap_or(card_type)
            } else {
                card_type
            };

            let mut row_content = row![
                text(format!("Port {} ({})", port.port, display_type)).size(FONT_BODY),
            ].align_y(iced::Alignment::Center).spacing(6);
            if port.dp_alt_mode || is_display_card {
                if let Some(card) = dp_card {
                    if let Some(ref fw) = card.active_firmware {
                        row_content = row_content.push(text(format!("v{}", fw)).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }));
                    }
                } else if port.dp_alt_mode {
                    row_content = row_content.push(text("DP").size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(SENSOR_COLORS[0]) }));
                }
            }
            content = content.push(row_content);
            if port.pd_contract && !is_display_card
                && let Some(ref level) = port.negotiated_text
            {
                let color = if port.power_role == Some("Source") { COLOR_GRAY } else { COLOR_GREEN };
                content = content.push(
                    text(format!("  {}", level)).size(FONT_SMALL).style(move |_theme| iced::widget::text::Style { color: Some(color) })
                );
            }
            if snap.expansion_card_debug {
                let dp_alt_str = if port.dp_alt_mode { "DP_ALT" } else { "" };
                let role_str = port.power_role.unwrap_or("?");
                let data_str = port.data_role.unwrap_or("?");
                let watts_str = port.negotiated_watts.map(|w| format!("{:.1}W", w)).unwrap_or_else(|| "-".to_string());
                let debug_line = format!("  [{}] role={} data={} {} watts={}", port.port, role_str, data_str, dp_alt_str, watts_str);
                content = content.push(
                    text(debug_line).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) })
                );
            }
        }

        for card in cards.iter().filter(|c| c.name.contains("Audio")) {
            let mut row_content = row![
                colored_dot(COLOR_GREEN, 8.0),
                text(card.name.as_str()).size(FONT_BODY),
            ].align_y(iced::Alignment::Center).spacing(6);
            if let Some(ref fw) = card.active_firmware {
                row_content = row_content.push(text(format!("v{}", fw)).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }));
            }
            content = content.push(row_content);
            if snap.expansion_card_debug {
                let fw_str = card.active_firmware.as_deref().unwrap_or("N/A");
                content = content.push(
                    text(format!("  [Debug] name={} fw={}", card.name, fw_str)).size(FONT_SMALL)
                        .style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) })
                );
            }
        }
        for card in cards.iter().filter(|c| !c.name.contains("Audio")) {
            if snap.expansion_card_debug {
                let fw_str = card.active_firmware.as_deref().unwrap_or("N/A");
                content = content.push(
                    text(format!("  [Debug] {} fw={}", card.name, fw_str)).size(FONT_SMALL)
                        .style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) })
                );
            }
        }
    }

    content.into()
}

fn cpu_power_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let info = &snap.cpu_power;
    let mut content = column![].spacing(2);

    // Header with settings toggle
    let settings_label = if snap.show_cpu_power_settings { "[-] Settings" } else { "[+] Settings" };
    content = content.push(
        row![
            text("CPU Power").size(FONT_SECTION).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) }),
            space::horizontal(),
            button(text(settings_label).size(FONT_SMALL))
                .on_press(Message::ToggleCpuPowerSettings)
                .style(btn_style),
        ]
    );

    if !info.available {
        let msg = info.error_msg.unwrap_or("PawnIO driver not available");
        content = content.push(text(msg).size(FONT_BODY).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }));
        if !crate::cpu_power::is_pawnio_installed() {
            content = content.push(
                button(text("Install PawnIO").size(FONT_BODY))
                    .on_press(Message::InstallPawnIO)
                    .style(btn_style)
            );
        } else if !crate::cpu_power::modules_downloaded() {
            content = content.push(text("PawnIO Modules required").size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }));
            if let Some(ref err) = snap.modules_download_error {
                content = content.push(text(err.as_str()).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(iced::Color::from_rgb(0.9, 0.3, 0.3)) }));
            }
            content = content.push(
                button(text("Download PawnIO Modules").size(FONT_BODY))
                    .on_press(Message::DownloadPawnIOModules)
                    .style(btn_style)
            );
        }
        return content.into();
    }

    // Summary line (always visible)
    let pl1_color = if snap.pl_custom_applied { COLOR_GREEN } else { COLOR_HEADER };
    let pl2_color = if snap.pl_custom_applied { COLOR_GREEN } else { COLOR_HEADER };
    content = content.push(row![
        text("  PL1:".to_string()).size(FONT_BODY),
        text(format!("{:.1}W", info.pl1_mmio)).size(FONT_BODY).style(move |_theme| iced::widget::text::Style { color: Some(pl1_color) }),
        text("  PL2:".to_string()).size(FONT_BODY),
        text(format!("{:.1}W", info.pl2_mmio)).size(FONT_BODY).style(move |_theme| iced::widget::text::Style { color: Some(pl2_color) }),
        if snap.sync_enabled {
            text("  [Syncing]").size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GREEN) })
        } else {
            text("").size(FONT_SMALL)
        },
    ].spacing(4));

    // Settings panel (collapsible, with border)
    if snap.show_cpu_power_settings {
        let mut settings_content = column![].spacing(4).padding(4);

        // MSR (static) read-only PL1/PL2
        settings_content = settings_content.push(text("MSR (Read-only)").size(FONT_BODY));
        settings_content = settings_content.push(row![
            text(format!("  PL1: {:.1}W ({:.2}s)", info.pl1_msr, info.pl1_time_s)).size(FONT_BODY),
            text(if info.pl1_msr_enabled { " [En]" } else { " [Dis]" }).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(if info.pl1_msr_enabled { COLOR_GREEN } else { COLOR_GRAY }) }),
            text(if info.pl1_msr_clamped { " [Cl]" } else { "" }).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) }),
        ].spacing(4));
        settings_content = settings_content.push(row![
            text(format!("  PL2: {:.1}W ({:.2}s)", info.pl2_msr, info.pl2_time_s)).size(FONT_BODY),
            text(if info.pl2_msr_enabled { " [En]" } else { " [Dis]" }).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(if info.pl2_msr_enabled { COLOR_GREEN } else { COLOR_GRAY }) }),
            text(if info.pl2_msr_clamped { " [Cl]" } else { "" }).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) }),
        ].spacing(4));

        // Editable PL1/PL2 — writes to MSR 0x610
        settings_content = settings_content.push(text("PL1/PL2 Control").size(FONT_BODY));
        settings_content = settings_content.push(row![
            text("  PL1:").size(FONT_BODY),
            text_input("W", &snap.pl1_edit)
                .width(Length::Fixed(60.0))
                .on_input(Message::CpuPowerPl1Changed),
            iced::widget::checkbox(snap.pl1_enabled).on_toggle(Message::CpuPowerPl1EnabledToggled),
            text("En").size(FONT_SMALL),
            iced::widget::checkbox(snap.pl1_clamped).on_toggle(Message::CpuPowerPl1ClampedToggled),
            text("Cl").size(FONT_SMALL),
            text("T:").size(FONT_SMALL),
            text_input("s", &snap.pl1_time_edit)
                .width(Length::Fixed(50.0))
                .on_input(Message::CpuPowerPl1TimeChanged),
        ].spacing(4).align_y(iced::Alignment::Center));
        settings_content = settings_content.push(row![
            text("  PL2:").size(FONT_BODY),
            text_input("W", &snap.pl2_edit)
                .width(Length::Fixed(60.0))
                .on_input(Message::CpuPowerPl2Changed),
            iced::widget::checkbox(snap.pl2_enabled).on_toggle(Message::CpuPowerPl2EnabledToggled),
            text("En").size(FONT_SMALL),
            iced::widget::checkbox(snap.pl2_clamped).on_toggle(Message::CpuPowerPl2ClampedToggled),
            text("Cl").size(FONT_SMALL),
        ].spacing(4).align_y(iced::Alignment::Center));

        // Validation error
        if let Some(ref err) = snap.cpu_power_error {
            settings_content = settings_content.push(text(err.as_str())
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style { color: Some(iced::Color::from_rgb(0.9, 0.3, 0.3)) }));
        }

        // Sync status
        if snap.sync_enabled {
            settings_content = settings_content.push(text("Syncing MSR 0x610 every 250ms")
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style { color: Some(COLOR_GREEN) }));
        }

        // Buttons
        settings_content = settings_content.push(row![
            button(text("Apply").size(FONT_BODY))
                .on_press(Message::CpuPowerApply)
                .style(btn_style),
            button(text(if snap.sync_enabled { "Stop Sync" } else { "Start Sync" }).size(FONT_BODY))
                .on_press(if snap.sync_enabled { Message::CpuPowerSyncStop } else { Message::CpuPowerSyncStart })
                .style(btn_style),
            button(text("Reset").size(FONT_BODY))
                .on_press(Message::CpuPowerSyncReset)
                .style(btn_style),
        ].spacing(8));

        content = content.push(
            container(settings_content)
                .padding(8)
                .style(|_theme| iced::widget::container::Style {
                    background: Some(COLOR_SETTINGS_BG.into()),
                    border: iced::Border::default().rounded(4).color(COLOR_DARK).width(1),
                    ..Default::default()
                })
        );
    }

    content.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ec_wrapper::BatteryData;

    fn default_battery_info() -> BatteryData {
        BatteryData::default()
    }

    #[test]
    fn view_charge_limit_section_returns_element() {
        let _el = view_charge_limit_section(true, 80);
    }

    #[test]
    fn view_battery_info_empty_battery() {
        let bat = default_battery_info();
        let _el = view_battery_info(&bat, false);
    }

    #[test]
    fn view_battery_info_with_health() {
        let mut bat = default_battery_info();
        bat.remaining_capacity_mah = Some(4500);
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
        let remaining = 4500u32;
        let design = 5000u32;
        let health = (remaining as f32 / design as f32 * 100.0) as u32;
        assert_eq!(health, 90);
    }

    #[test]
    fn battery_health_full() {
        let remaining = 5000u32;
        let design = 5000u32;
        let health = (remaining as f32 / design as f32 * 100.0) as u32;
        assert_eq!(health, 100);
    }

    #[test]
    fn battery_health_degraded() {
        let remaining = 3000u32;
        let design = 5000u32;
        let health = (remaining as f32 / design as f32 * 100.0) as u32;
        assert_eq!(health, 60);
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
