use crate::App;
use crate::Message;
use crate::style::*;
use crate::views::ViewSnapshot;
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Element, Length};
use std::sync::Arc;

pub(crate) fn view_sensors<'a>(app: &'a App, snap: &'a ViewSnapshot) -> Element<'a, Message> {
    let thermal = &snap.thermal;
    let pal = snap.palette;
    match thermal.as_ref().as_ref() {
        Some(thermal) => {
            let mut content = column![].spacing(6);

            let cache = &snap.sensor_cache;
            let config = &snap.config;
            let all_empty = config.telemetry.selected_sensors.is_empty();

            let settings_label = if app.show_sensor_settings {
                "[-] Settings"
            } else {
                "[+] Settings"
            };
            let header = row![
                text("Sensors")
                    .size(FONT_SECTION)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.header)
                    }),
                space::horizontal(),
                button(text(settings_label).size(FONT_SMALL))
                    .on_press(Message::ToggleSensorSettings)
                    .style(btn_style),
            ];
            content = content.push(header);

            let settings_panel: Element<'_, Message> = if app.show_sensor_settings {
                let mut settings_content = column![].spacing(4).padding(4);
                settings_content = settings_content.push(text("Sensors").size(FONT_BODY));

                // Same segmented style as fan mode buttons; highlights active window.
                let mut window_row = row![text("Chart Window:").size(FONT_BODY),]
                    .spacing(8)
                    .align_y(iced::Alignment::Center);
                for opt in crate::temp_chart::HISTORY_WINDOW_OPTIONS {
                    let selected = app.chart_window_seconds == opt;
                    window_row = window_row.push(
                        button(text(format!("{}s", opt)).size(FONT_SMALL))
                            .on_press(Message::ChartWindowChanged(opt))
                            .style(move |theme, _status| mode_style(theme, selected)),
                    );
                }
                settings_content = settings_content.push(window_row);

                for (idx, name) in cache.keys.iter().enumerate() {
                    // Small Vec: linear search is cheaper than HashSet with no allocation.
                    let color = SENSOR_COLORS[idx % SENSOR_COLORS.len()];
                    let is_on = all_empty || config.telemetry.selected_sensors.contains(name);
                    let on_off = if is_on { "On" } else { "Off" };
                    let on_color = if is_on { pal.green } else { pal.gray };
                    let bg_color = if is_on { color } else { pal.dark };
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
                                        // White ring matches slider thumbs and curve points.
                                        border: iced::Border::default()
                                            .rounded(7)
                                            .color(iced::Color::WHITE)
                                            .width(1),
                                        ..Default::default()
                                    })
                            )
                            .on_press(Message::SensorToggled(idx, !is_on))
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
                container(settings_content)
                    .width(Length::Fill)
                    .padding(8)
                    .style(move |_theme| iced::widget::container::Style {
                        background: Some(pal.settings_bg.into()),
                        border: iced::Border::default().rounded(4).color(pal.dark).width(1),
                        ..Default::default()
                    })
                    .into()
            } else {
                iced::widget::Space::new().into()
            };
            content = content.push(settings_panel);

            let history = Arc::clone(&snap.temp_history);
            let sorted_sensors = &cache.sorted;
            let chart_colors = Arc::clone(&cache.colors);

            let mut list_content = column![].spacing(2);

            for (idx, name) in sorted_sensors.iter().enumerate() {
                if let Some(temp) = thermal.temps.get(name) {
                    let color = chart_colors
                        .get(idx)
                        .copied()
                        // Unreachable in practice (colors tracks the sorted
                        // list); keep the legacy fallback verbatim.
                        .unwrap_or(iced::Color::WHITE);
                    let row = row![
                        colored_dot(color, 10.0),
                        text(name.as_str()).size(FONT_BODY).width(Length::Fill),
                        text(format!("{}°C", temp)).size(FONT_BODY),
                    ]
                    .align_y(iced::Alignment::Center)
                    .spacing(6);
                    list_content = list_content.push(row);
                }
            }

            content = content.push(
                container(crate::temp_chart::view_temp_chart(
                    crate::temp_chart::TempHistory {
                        samples: history,
                        colors: chart_colors,
                        sensor_names: Arc::clone(&cache.sorted),
                        window_seconds: app.chart_window_seconds,
                    },
                ))
                .width(Length::Fill)
                .height(150),
            );

            content = content.push(scrollable(list_content).height(Length::Shrink));

            content.into()
        }
        None => text(if app.cli_present {
            "Waiting for sensor data..."
        } else {
            "EC not available"
        })
        .into(),
    }
}
