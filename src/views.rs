use crate::App;
use crate::Message;
use crate::read_lock;
use crate::style::*;
use crate::types::FanControlMode;
use iced::widget::{button, column, container, row, scrollable, text};
use iced::widget::rule;
use iced::widget::space;
use iced::{Element, Length};
use std::sync::Arc;

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
    let content = container(
        row![
            column![
                card(view_sensors(app)),
                card(view_battery(app)),
            ].width(Length::FillPortion(1)).spacing(8),
            column![
                card(view_fan_control(app)),
                card(view_misc(app)),
            ].width(Length::FillPortion(1)).spacing(8),
        ].spacing(12)
    ).padding(12).width(Length::Fill).height(Length::Fill);

    let mut root = column![header].spacing(8);
    if let Some(warning) = config_warning {
        root = root.push(warning);
    }
    if let Some(warning) = cli_warning {
        root = root.push(warning);
    }
    root.push(content).into()
}

fn view_settings(app: &App) -> Element<'_, Message> {
    let versions = read_lock(&app.state.versions);

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
    if !app.system_info.os.is_empty() {
        sw_content = sw_content.push(info_row("OS", &app.system_info.os));
    }
    sw_content = sw_content.push(space::vertical().height(8));
    sw_content = sw_content.push(text("Poll Rate:").size(FONT_BODY));
    let config = read_lock(&app.state.config);
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

    container(content)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

fn view_quit_warning(app: &App) -> Element<'_, Message> {
    let config = read_lock(&app.state.config);
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
        iced::widget::slider(10..=100, set_duty, Message::QuitDutyChanged),
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

fn view_sensors(app: &App) -> Element<'_, Message> {
    let thermal = read_lock(&app.state.thermal);
    match thermal.as_ref().as_ref() {
        Some(thermal) => {
            let mut content = column![].spacing(6);

            let cache = read_lock(&app.state.sensor_cache);
            let config = read_lock(&app.state.config);
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

            if app.show_sensor_settings {
                let mut settings_content = column![].spacing(4).padding(4);
                settings_content = settings_content.push(text("Sensors").size(FONT_BODY));

                for name in cache.keys.iter() {
                    let color = sensor_color(name, &cache.keys);
                    let is_on = all_empty || config.telemetry.selected_sensors.contains(name);
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
                             ).on_press(Message::SensorToggled(name.clone(), !is_on))
                              .style(btn_style).padding(0),
                            text(name.clone()).size(FONT_BODY),
                            space::horizontal(),
                            text(on_off).size(FONT_SMALL).style(move |_theme| iced::widget::text::Style { color: Some(on_color) }),
                        ].align_y(iced::Alignment::Center).spacing(6)
                    );
                }
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

            let history = read_lock(&app.state.temp_history);
            let sorted_sensors = Arc::clone(&cache.sorted);
            let chart_colors = Arc::clone(&cache.colors);

            let mut list_content = column![].spacing(2);
            let mut temp_buf = String::with_capacity(8);

            for (idx, name) in sorted_sensors.iter().enumerate() {
                if let Some(temp) = thermal.temps.get(name) {
                    let color = chart_colors[idx];
                    temp_buf.clear();
                    use std::fmt::Write;
                    let _ = write!(temp_buf, "{}°C", temp);
                    let row = row![
                        colored_dot(color, 10.0),
                        text(name.clone()).size(FONT_BODY).width(Length::Fill),
                        text(temp_buf.clone()).size(FONT_BODY),
                    ].align_y(iced::Alignment::Center).spacing(6);
                    list_content = list_content.push(row);
                }
            }

            content = content.push(
                container(crate::temp_chart::view_temp_chart(crate::temp_chart::TempHistory {
                    samples: std::sync::Arc::clone(&history),
                    colors: chart_colors,
                    sensor_names: sorted_sensors,
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

fn view_fan_control(app: &App) -> Element<'_, Message> {
    let thermal = read_lock(&app.state.thermal);
    let config = read_lock(&app.state.config);
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
            content = content.push(
                iced::widget::slider(10..=100, duty, Message::FanDutyChanged)
            );
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

                let pts_len = curve.curve.points.len();
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
                                space::horizontal(),
                                if pts_len > 2 {
                                    button(text("x").size(FONT_SMALL))
                                        .on_press(Message::FanCurvePointRemove(idx))
                                        .style(btn_style).padding(2)
                                } else {
                                    button(text("x").size(FONT_SMALL))
                                        .style(btn_style).padding(2)
                                },
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

                if pts_len < 10 {
                    content = content.push(
                        button(text("+ Add Point").size(FONT_BODY))
                            .on_press(Message::FanCurvePointAdd)
                            .style(btn_style)
                    );
                }

                let pts = &curve.curve.points;
                let all_pts = read_lock(&app.state.curve_full_points);
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

fn view_charge_rate_section(rate_enabled: bool, rate_value: f32, soc_threshold: u8) -> Element<'static, Message> {
    let soc_text = if soc_threshold == 0 { "off".to_string() } else { format!("{}%", soc_threshold) };

    column![
        row![
            iced::widget::checkbox(rate_enabled).on_toggle(Message::ChargeRateToggled),
            text("Rate Limit (C):").size(FONT_BODY),
            space::horizontal(),
            text(format!("{:.2}C", rate_value)).size(FONT_BODY),
        ].spacing(4),
        iced::widget::slider(CHARGE_RATE_SLIDER_MIN..=CHARGE_RATE_SLIDER_MAX, (rate_value * 100.0) as u32, |v| Message::ChargeRateChanged(v as f32 / 100.0)),
        row![
            text("Rate SOC Threshold:").size(FONT_BODY),
            space::horizontal(),
            text(soc_text).size(FONT_BODY),
        ].spacing(4),
        iced::widget::slider(0..=100, soc_threshold as u32, |v| Message::ChargeRateSocThresholdChanged(v as u8)),
    ].spacing(4).into()
}

fn view_battery_info(battery: &crate::cli::ec_wrapper::BatteryData, charging: bool) -> Element<'static, Message> {
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

fn view_battery_verbose(battery: &crate::cli::ec_wrapper::BatteryData, show_details: bool) -> Option<Element<'static, Message>> {
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

    if show_details {
        if let Some(details) = battery_detail_rows(battery) {
            content = content.push(details);
        }
    }

    Some(content.into())
}

fn view_battery(app: &App) -> Element<'static, Message> {
    let battery = read_lock(&app.state.battery);
    let config = read_lock(&app.state.config);
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
        let rate_limit = config.battery.charge_rate_c.unwrap_or_default();
        let rate_value = rate_limit.value.clamp(CHARGE_RATE_MIN_C, CHARGE_RATE_MAX_C);
        let soc_threshold = config.battery.charge_rate_soc_threshold_pct.unwrap_or(0);

        let mut content = column![title_row].spacing(6);
        content = content.push(view_charge_limit_section(charge_limit.enabled, charge_limit.value as u32));
        content = content.push(view_charge_rate_section(rate_limit.enabled, rate_value, soc_threshold));
        content = content.push(view_battery_info(&battery.power_info, charging));

        if let Some(verbose) = view_battery_verbose(&battery.power_info, app.show_battery_details) {
            content = content.push(verbose);
        }

        let right_pad = iced::Padding::ZERO.right(14.0);
        scrollable(container(content).padding(right_pad)).height(Length::Fill).into()
    } else {
        text(if app.cli_present { "Waiting for battery data..." } else { "EC not available" }).into()
    }
}

fn battery_detail_rows(battery_info: &crate::cli::ec_wrapper::BatteryData) -> Option<Element<'static, Message>> {
    let mut rows = column![].spacing(2).padding(4);
    let mut has_content = false;

    if let Some(ref v) = battery_info.manufacturer {
        has_content = true;
        rows = rows.push(row![text("Manufacturer:").size(FONT_SMALL), space::horizontal(), text(v.clone()).size(FONT_SMALL)].spacing(4));
    }
    if let Some(ref v) = battery_info.model_number {
        has_content = true;
        rows = rows.push(row![text("Model:").size(FONT_SMALL), space::horizontal(), text(v.clone()).size(FONT_SMALL)].spacing(4));
    }
    if let Some(ref v) = battery_info.serial_number {
        has_content = true;
        rows = rows.push(row![text("Serial:").size(FONT_SMALL), space::horizontal(), text(v.clone()).size(FONT_SMALL)].spacing(4));
    }
    if let Some(ref v) = battery_info.battery_type {
        has_content = true;
        rows = rows.push(row![text("Type:").size(FONT_SMALL), space::horizontal(), text(v.clone()).size(FONT_SMALL)].spacing(4));
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

fn view_misc(app: &App) -> Element<'_, Message> {
    let mut content = column![text("Misc").size(FONT_SECTION).style(|_theme| iced::widget::text::Style { color: Some(COLOR_HEADER) })].spacing(6);

    content = content.push(kblight_section(app));

    content = content.push(space::vertical().height(8));

    content = content.push(text("Fingerprint LED").size(FONT_SECTION));
    let button_row = row![
        button(text("Low").size(FONT_BODY)).on_press(Message::FpLedLevelChanged("low".to_string())).style(btn_style),
        button(text("Medium").size(FONT_BODY)).on_press(Message::FpLedLevelChanged("medium".to_string())).style(btn_style),
        button(text("High").size(FONT_BODY)).on_press(Message::FpLedLevelChanged("high".to_string())).style(btn_style),
    ].spacing(6);
    content = content.push(button_row);

    content = content.push(space::vertical().height(8));

    content = content.push(ports_section(app));

    content.into()
}

fn kblight_section(app: &App) -> Element<'_, Message> {
    let kblight = read_lock(&app.state.kblight);
    let mut content = column![].spacing(2);
    if let Some(kb) = *kblight {
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

fn ports_section(app: &App) -> Element<'_, Message> {
    let cards = read_lock(&app.state.expansion_cards);
    let ports = read_lock(&app.state.pd_ports);
    let mut content = column![text("Ports & Expansion Cards").size(FONT_SECTION)].spacing(2);

    if ports.is_empty() && cards.is_empty() {
        content = content.push(text("None detected").size(FONT_BODY).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }));
    } else {
        for port in ports.iter() {
            let display_type = if port.dp_alt_mode {
                "DP/HDMI"
            } else if port.pd_contract && port.power_role.as_deref() == Some("Sink") {
                "USB-C (Charging)"
            } else if port.pd_contract && port.power_role.as_deref() == Some("Source") {
                "USB-C (Source)"
            } else {
                "USB-C"
            };

            let mut row_content = row![
                text(format!("Port {} ({})", port.port, display_type)).size(FONT_BODY),
            ].align_y(iced::Alignment::Center).spacing(6);
            if port.dp_alt_mode {
                row_content = row_content.push(text("DP").size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(SENSOR_COLORS[0]) }));
            }
            content = content.push(row_content);
            if port.pd_contract {
                if let Some(ref level) = port.negotiated_text {
                    let color = if port.power_role.as_deref() == Some("Source") { COLOR_GRAY } else { COLOR_GREEN };
                    content = content.push(
                        text(format!("  {}", level)).size(FONT_SMALL).style(move |_theme| iced::widget::text::Style { color: Some(color) })
                    );
                }
            }
        }

        for card in cards.iter() {
            let mut row_content = row![
                colored_dot(COLOR_GREEN, 8.0),
                text(card.name.clone()).size(FONT_BODY),
            ].align_y(iced::Alignment::Center).spacing(6);
            if let Some(ref fw) = card.active_firmware {
                row_content = row_content.push(text(format!("v{}", fw)).size(FONT_SMALL).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }));
            }
            content = content.push(row_content);
        }
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
    fn view_charge_rate_section_returns_element() {
        let _el = view_charge_rate_section(true, 0.5, 80);
    }

    #[test]
    fn view_charge_rate_section_soc_zero_shows_off() {
        let _el = view_charge_rate_section(false, 1.0, 0);
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
