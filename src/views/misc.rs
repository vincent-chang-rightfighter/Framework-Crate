use crate::Message;
use crate::style::*;
use crate::views::ViewSnapshot;
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Element, Length};

const MISC_SECTION_MAX_HEIGHT: f32 = 300.0;
pub(crate) fn view_misc(snap: &ViewSnapshot) -> Element<'_, Message> {
    let mut content =
        column![
            text("Misc")
                .size(FONT_SECTION)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_HEADER)
                })
        ]
        .spacing(6);

    if let Some(ref err) = snap.ec_op_error {
        content = content.push(text(err.as_str()).size(FONT_SMALL).style(|_theme| {
            iced::widget::text::Style {
                color: Some(iced::Color::from_rgb(0.9, 0.3, 0.3)),
            }
        }));
    }

    if snap.platform.has_keyboard_backlight() {
        content = content.push(kblight_section(snap));
    } else {
        content = content.push(crate::views::not_supported_section("Keyboard Backlight"));
    }

    content = content.push(space::vertical().height(8));

    if snap.platform.has_fingerprint_led() {
        content = content.push(text("Fingerprint LED").size(FONT_SECTION));
        let button_row = row![
            button(text("Low").size(FONT_BODY))
                .on_press(Message::FpLedLevelChanged("low"))
                .style(btn_style),
            button(text("Medium").size(FONT_BODY))
                .on_press(Message::FpLedLevelChanged("medium"))
                .style(btn_style),
            button(text("High").size(FONT_BODY))
                .on_press(Message::FpLedLevelChanged("high"))
                .style(btn_style),
        ]
        .spacing(6);
        content = content.push(button_row);
    } else {
        content = content.push(crate::views::not_supported_section("Fingerprint LED"));
    }

    content = content.push(space::vertical().height(8));

    content = content.push(ports_section(snap));

    let right_pad = iced::Padding::ZERO.right(14.0);
    let max_h = if snap.expansion_card_debug {
        500.0
    } else {
        MISC_SECTION_MAX_HEIGHT
    };
    container(scrollable(container(content).padding(right_pad)).height(Length::Shrink))
        .width(Length::Fill)
        .max_height(max_h)
        .into()
}

pub(crate) fn kblight_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let kblight = &snap.kblight;
    let mut content = column![].spacing(2);
    if let Some(kb) = kblight.as_ref().as_ref().copied() {
        content = content.push(
            row![
                text("Keyboard Backlight")
                    .size(FONT_SECTION)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_HEADER)
                    }),
                space::horizontal(),
                text(format!("{}%", kb)).size(FONT_BODY),
            ]
            .align_y(iced::Alignment::Center),
        );
        content = content.push(
            iced::widget::slider(0..=100, kb, Message::KblightChanged)
                .step(10u32)
                .style(slider_style),
        );
    } else {
        content = content.push(
            text("Keyboard Backlight")
                .size(FONT_SECTION)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_HEADER),
                }),
        );
        content = content.push(text("Unavailable").size(FONT_BODY).style(|_theme| {
            iced::widget::text::Style {
                color: Some(COLOR_GRAY),
            }
        }));
    }
    content.into()
}

pub(crate) fn ports_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let cards = &snap.expansion_cards;
    let ports = &snap.pd_ports;
    let history = &snap.pd_ports_history;
    let mut content = column![text("Ports & Expansion Cards").size(FONT_SECTION)].spacing(2);

    if ports.is_empty() && cards.is_empty() {
        content = content.push(text("None detected").size(FONT_BODY).style(|_theme| {
            iced::widget::text::Style {
                color: Some(COLOR_GRAY),
            }
        }));
        if snap.expansion_card_debug {
            content = content.push(
                text("[Debug] No ports or expansion cards detected")
                    .size(FONT_SMALL)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_GRAY),
                    }),
            );
        }
    } else {
        let dp_card = cards
            .iter()
            .find(|c| c.name.contains("DisplayPort") || c.name.contains("HDMI"));
        for port in ports.iter() {
            let ever_seen_sink = snap
                .pd_usb_c_seen
                .get(port.port as usize)
                .copied()
                .unwrap_or(false);
            let card_type = crate::cli::ec_wrapper::classify_pd_port(
                port,
                history.iter().map(|a| a.as_ref().as_slice()),
                STABLE_THRESHOLD,
                dp_card.is_some(),
                ever_seen_sink,
            );
            let is_display_card = card_type == "DisplayPort Expansion Card"
                || card_type == "HDMI Expansion Card"
                || card_type == "DP/HDMI Expansion Card";
            let display_type = if card_type == "DP/HDMI Expansion Card" {
                dp_card.map(|c| c.name.as_str()).unwrap_or(card_type)
            } else {
                card_type
            };

            let mut row_content =
                row![text(format!("Port {} ({})", port.port, display_type)).size(FONT_BODY),]
                    .align_y(iced::Alignment::Center)
                    .spacing(6);
            if port.dp_alt_mode || is_display_card {
                if let Some(card) = dp_card {
                    if let Some(ref fw) = card.active_firmware {
                        row_content =
                            row_content.push(text(format!("v{}", fw)).size(FONT_SMALL).style(
                                |_theme| iced::widget::text::Style {
                                    color: Some(COLOR_GRAY),
                                },
                            ));
                    }
                } else if port.dp_alt_mode {
                    row_content = row_content.push(text("DP").size(FONT_SMALL).style(|_theme| {
                        iced::widget::text::Style {
                            color: Some(SENSOR_COLORS[0]),
                        }
                    }));
                }
            }
            content = content.push(row_content);
            if port.pd_contract
                && !is_display_card
                && let Some(ref level) = port.negotiated_text
            {
                let color = if port.power_role == Some("Source") {
                    COLOR_GRAY
                } else {
                    COLOR_GREEN
                };
                content = content.push(
                    text(format!("  {}", level))
                        .size(FONT_SMALL)
                        .style(move |_theme| iced::widget::text::Style { color: Some(color) }),
                );
            }
            if snap.expansion_card_debug {
                let dp_alt_str = if port.dp_alt_mode { "DP_ALT" } else { "" };
                let role_str = port.power_role.unwrap_or("?");
                let data_str = port.data_role.unwrap_or("?");
                let watts_str = port
                    .negotiated_watts
                    .map(|w| format!("{:.1}W", w))
                    .unwrap_or_else(|| "-".to_string());
                let debug_line = format!(
                    "  [{}] role={} data={} {} watts={}",
                    port.port, role_str, data_str, dp_alt_str, watts_str
                );
                content = content.push(text(debug_line).size(FONT_SMALL).style(|_theme| {
                    iced::widget::text::Style {
                        color: Some(COLOR_GRAY),
                    }
                }));
            }
        }

        for card in cards.iter().filter(|c| c.name.contains("Audio")) {
            let mut row_content = row![
                colored_dot(COLOR_GREEN, 8.0),
                text(card.name.as_str()).size(FONT_BODY),
            ]
            .align_y(iced::Alignment::Center)
            .spacing(6);
            if let Some(ref fw) = card.active_firmware {
                row_content =
                    row_content.push(text(format!("v{}", fw)).size(FONT_SMALL).style(|_theme| {
                        iced::widget::text::Style {
                            color: Some(COLOR_GRAY),
                        }
                    }));
            }
            content = content.push(row_content);
            if snap.expansion_card_debug {
                let fw_str = card.active_firmware.as_deref().unwrap_or("N/A");
                content = content.push(
                    text(format!("  [Debug] name={} fw={}", card.name, fw_str))
                        .size(FONT_SMALL)
                        .style(|_theme| iced::widget::text::Style {
                            color: Some(COLOR_GRAY),
                        }),
                );
            }
        }
        for card in cards.iter().filter(|c| !c.name.contains("Audio")) {
            if snap.expansion_card_debug {
                let fw_str = card.active_firmware.as_deref().unwrap_or("N/A");
                content = content.push(
                    text(format!("  [Debug] {} fw={}", card.name, fw_str))
                        .size(FONT_SMALL)
                        .style(|_theme| iced::widget::text::Style {
                            color: Some(COLOR_GRAY),
                        }),
                );
            }
        }
    }

    content.into()
}

