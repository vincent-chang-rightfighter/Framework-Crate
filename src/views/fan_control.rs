use crate::Message;
use crate::style::*;
use crate::types::FanControlMode;
use crate::views::ViewSnapshot;
use iced::widget::{button, column, container, row, space, text};
use iced::{Element, Length};
use std::sync::Arc;
pub(crate) fn view_fan_control(snap: &ViewSnapshot) -> Element<'_, Message> {
    let thermal = &snap.thermal;
    let config = &snap.config;
    let current_mode = config.fan.mode;

    let fan_rpm_text = thermal.as_ref().as_ref().and_then(|t| {
        if t.fans.is_empty() {
            None
        } else {
            use std::fmt::Write;
            let mut s = String::with_capacity(32);
            for (i, f) in t.fans.iter().enumerate() {
                if i > 0 {
                    s.push_str("  ");
                }
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
        text("Fan Control")
            .size(FONT_SECTION)
            .style(|_theme| iced::widget::text::Style {
                color: Some(COLOR_HEADER)
            }),
        space::horizontal(),
        text(right_text).size(FONT_BODY),
    ]
    .align_y(iced::Alignment::Center);

    let mut content = column![title_row].spacing(6);

    let mode_row = {
        let is_disabled = current_mode == FanControlMode::Disabled;
        let is_manual = current_mode == FanControlMode::Manual;
        let is_curve = current_mode == FanControlMode::Curve;
        row![
            button(text("Auto").size(FONT_BODY))
                .on_press(Message::FanModeChanged(FanControlMode::Disabled))
                .style(move |_theme, _status| mode_style(is_disabled)),
            button(text("Manual").size(FONT_BODY))
                .on_press(Message::FanModeChanged(FanControlMode::Manual))
                .style(move |_theme, _status| mode_style(is_manual)),
            button(text("Curve").size(FONT_BODY))
                .on_press(Message::FanModeChanged(FanControlMode::Curve))
                .style(move |_theme, _status| mode_style(is_curve)),
        ]
        .spacing(8)
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
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                );
                if unified {
                    content = content.push(
                        iced::widget::slider(0..=100, duty, Message::FanDutyChanged)
                            .style(slider_style),
                    );
                } else {
                    for (idx, &per_duty) in snap.per_fan_duty.iter().enumerate() {
                        content = content.push(
                            row![
                                text(format!("Fan {}:", idx + 1)).size(FONT_BODY),
                                iced::widget::slider(0..=100, per_duty, move |d| {
                                    Message::FanPerDutyChanged(idx, d)
                                })
                                .style(slider_style),
                                text(format!("{}%", per_duty)).size(FONT_BODY),
                            ]
                            .spacing(8)
                            .align_y(iced::Alignment::Center),
                        );
                    }
                }
            } else {
                content = content.push(
                    iced::widget::slider(0..=100, duty, Message::FanDutyChanged)
                        .style(slider_style),
                );
            }
        }
        FanControlMode::Curve => {
            if let Some(ref curve) = config.fan.curve {
                let hyst = curve.curve.hysteresis_c;
                let rate = curve.curve.rate_limit_pct_per_step;

                let settings_label = if snap.show_curve_settings {
                    "[-] Settings"
                } else {
                    "[+] Settings"
                };
                content = content.push(
                    row![
                        text("Curve").size(FONT_SECTION).style(|_theme| {
                            iced::widget::text::Style {
                                color: Some(COLOR_HEADER),
                            }
                        }),
                        space::horizontal(),
                        button(text(settings_label).size(FONT_SMALL))
                            .on_press(Message::ToggleCurveSettings)
                            .style(btn_style),
                    ]
                    .align_y(iced::Alignment::Center),
                );

                let settings_panel: Element<'_, Message> = if snap.show_curve_settings {
                    let mut settings_content = column![].spacing(4).padding(4);

                    settings_content = settings_content.push(text("Hysteresis").size(FONT_BODY));
                    settings_content = settings_content.push(
                        row![
                            iced::widget::slider(0..=10, hyst, Message::FanCurveHysteresisChanged)
                                .style(slider_style),
                            text(format!("{}°C", hyst)).size(FONT_BODY),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                    );

                    settings_content = settings_content.push(text("Rate Limit").size(FONT_BODY));
                    settings_content = settings_content.push(
                        row![
                            iced::widget::slider(1..=100, rate, Message::FanCurveRateLimitChanged)
                                .style(slider_style),
                            text(format!("{} %/step", rate)).size(FONT_BODY),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                    );

                    let curve_poll_ms = curve.poll_ms as u32;
                    settings_content = settings_content.push(text("Curve Poll").size(FONT_BODY));
                    settings_content = settings_content.push(
                        row![
                            iced::widget::slider(
                                crate::types::CURVE_POLL_MS_MIN as u32
                                    ..=crate::types::CURVE_POLL_MS_MAX as u32,
                                curve_poll_ms,
                                |v| Message::CurvePollMsChanged(v as u64)
                            )
                            .step(100u32)
                            .style(slider_style),
                            text(format!("{} ms", curve_poll_ms)).size(FONT_BODY),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                    );

                    settings_content =
                        settings_content.push(text("Temperature sensor").size(FONT_BODY));
                    let cache = &snap.sensor_cache;
                    let curve_sensor: Option<&str> =
                        curve.curve.sensors.first().map(|s| s.as_str());
                    if cache.keys.is_empty() {
                        settings_content = settings_content.push(
                            text("No sensors detected yet")
                                .size(FONT_SMALL)
                                .style(|_theme| iced::widget::text::Style {
                                    color: Some(COLOR_GRAY),
                                }),
                        );
                    } else {
                        for (idx, name) in cache.keys.iter().enumerate() {
                            let is_on = curve_sensor == Some(name.as_str());
                            let color = SENSOR_COLORS[idx % SENSOR_COLORS.len()];
                            let on_off = if is_on { "On" } else { "Off" };
                            let on_color = if is_on { COLOR_GREEN } else { COLOR_GRAY };
                            let bg_color = if is_on { color } else { COLOR_DARK };
                            settings_content = settings_content.push(
                                row![
                                    button(
                                        container(text(" ").size(FONT_SMALL))
                                            .width(14)
                                            .height(14)
                                            .center_x(14)
                                            .center_y(14)
                                            .style(move |_theme| iced::widget::container::Style {
                                                background: Some(bg_color.into()),
                                                // White ring matches sensor toggles.
                                                border: iced::Border::default()
                                                    .rounded(7)
                                                    .color(iced::Color::WHITE)
                                                    .width(1),
                                                ..Default::default()
                                            })
                                    )
                                    .on_press(Message::CurveSensorSelected(idx))
                                    .style(btn_style)
                                    .padding(0),
                                    text(name.as_str()).size(FONT_BODY),
                                    space::horizontal(),
                                    text(on_off).size(FONT_SMALL).style(move |_theme| {
                                        iced::widget::text::Style {
                                            color: Some(on_color),
                                        }
                                    }),
                                ]
                                .align_y(iced::Alignment::Center)
                                .spacing(6),
                            );
                        }
                    }

                    container(settings_content)
                        .width(Length::Fill)
                        .padding(8)
                        .style(|_theme| iced::widget::container::Style {
                            background: Some(COLOR_SETTINGS_BG.into()),
                            border: iced::Border::default()
                                .rounded(4)
                                .color(COLOR_DARK)
                                .width(1),
                            ..Default::default()
                        })
                        .into()
                } else {
                    iced::widget::Space::new().into()
                };
                content = content.push(settings_panel);

                // Sort by temperature so labels follow canvas left-to-right order.
                let mut points_sorted = curve.curve.points.clone();
                points_sorted.sort_by_key(|p| p[0]);
                for (chunk_idx, chunk) in points_sorted.chunks(3).enumerate() {
                    let mut r = iced::widget::Row::new().spacing(12);
                    for (offset, point) in chunk.iter().enumerate() {
                        let idx = chunk_idx * 3 + offset;
                        let label = format!("P{}: {}°C -> {}%", idx + 1, point[0], point[1]);
                        let txt = if point[0] >= crate::types::CURVE_TEMP_LOCK_START {
                            text(label)
                                .size(FONT_BODY)
                                .style(|_theme| iced::widget::text::Style {
                                    color: Some(COLOR_GRAY),
                                })
                        } else {
                            text(label).size(FONT_BODY)
                        };
                        r = r.push(txt);
                    }
                    content = content.push(r);
                }

                let canvas = crate::curve_canvas::view_curve(
                    Arc::clone(&snap.curve_points),
                    Arc::clone(&snap.curve_full_points),
                    Arc::clone(&snap.curve_marks),
                );

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

