use crate::Message;
use crate::style::*;
use crate::views::ViewSnapshot;
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Element, Length};

/// Max Misc scroll height: 300px fits the window autosize budget
/// (`AUTO_MAX_HEIGHT` 1100 + chrome); ports-debug rows are taller, so the
/// debug toggle raises the cap to 500px instead of clipping.
const MISC_SECTION_MAX_HEIGHT: f32 = 300.0;
/// Debug-mode cap (see above): PD rows print one line per port plus history.
const MISC_SECTION_MAX_HEIGHT_DEBUG: f32 = 500.0;
pub(crate) fn view_misc(snap: &ViewSnapshot) -> Element<'_, Message> {
    let pal = snap.palette;
    let mut content =
        column![
            text("Misc")
                .size(FONT_SECTION)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pal.header)
                })
        ]
        .spacing(6);

    if let Some(ref err) = snap.ec_op_error {
        content = content.push(text(err.as_str()).size(FONT_SMALL).style(move |_theme| {
            iced::widget::text::Style {
                color: Some(pal.error),
            }
        }));
    }

    content = content.push(privacy_section(snap));

    if snap.platform.has_keyboard_backlight() {
        content = content.push(kblight_section(snap));
    } else {
        content = content.push(crate::views::not_supported_section(
            snap.palette,
            "Keyboard Backlight",
        ));
    }

    if snap.platform.has_fingerprint_led() {
        let pal = snap.palette;
        content = content.push(
            text("Fingerprint LED")
                .size(FONT_SECTION)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pal.header),
                }),
        );
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
        content = content.push(crate::views::not_supported_section(
            snap.palette,
            "Fingerprint LED",
        ));
    }

    content = content.push(ports_section(snap));

    let right_pad = iced::Padding::ZERO.right(14.0);
    let max_h = if snap.ports_debug {
        MISC_SECTION_MAX_HEIGHT_DEBUG
    } else {
        MISC_SECTION_MAX_HEIGHT
    };
    container(
        scrollable(container(content).padding(right_pad))
            .height(Length::Shrink)
            .style(scrollable_style),
    )
    .width(Length::Fill)
    .max_height(max_h)
    .into()
}

pub(crate) fn kblight_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let kblight = &snap.kblight;
    let pal = snap.palette;
    let mut content = column![].spacing(2);
    if let Some(kb) = kblight.as_ref().as_ref().copied() {
        content = content.push(
            row![
                text("Keyboard Backlight")
                    .size(FONT_SECTION)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.header)
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
        content = content.push(text("Keyboard Backlight").size(FONT_SECTION).style(
            move |_theme| iced::widget::text::Style {
                color: Some(pal.header),
            },
        ));
        content = content.push(text("Unavailable").size(FONT_BODY).style(move |_theme| {
            iced::widget::text::Style {
                color: Some(pal.gray),
            }
        }));
    }
    content.into()
}

/// Labels for the privacy-switches row. Polarity contract (pinned by test):
/// `true` means the device is enabled/connected per framework_lib's
/// `get_privacy_info`, so `true` renders On and `false` renders Off.
pub(crate) fn privacy_labels(mic: bool, camera: bool) -> (&'static str, &'static str) {
    (
        if mic { "Mic On" } else { "Mic Off" },
        if camera { "Camera On" } else { "Camera Off" },
    )
}

pub(crate) fn privacy_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let pal = snap.palette;
    let mut content = column![].spacing(2);
    content = content.push(
        text("Privacy Switches")
            .size(FONT_SECTION)
            .style(move |_theme| iced::widget::text::Style {
                color: Some(pal.header),
            }),
    );
    if let Some(switches) = snap.privacy.as_ref().as_ref() {
        let (mic, cam) = privacy_labels(switches.mic, switches.camera);
        content = content.push(
            row![
                text(mic).size(FONT_BODY).style(move |_theme| {
                    iced::widget::text::Style {
                        color: Some(if switches.mic { pal.green } else { pal.gray }),
                    }
                }),
                text(cam).size(FONT_BODY).style(move |_theme| {
                    iced::widget::text::Style {
                        color: Some(if switches.camera { pal.green } else { pal.gray }),
                    }
                }),
            ]
            .spacing(6),
        );
    } else {
        content = content.push(text("Unavailable").size(FONT_BODY).style(move |_theme| {
            iced::widget::text::Style {
                color: Some(pal.gray),
            }
        }));
    }
    content.into()
}

pub(crate) fn ports_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let ports = &snap.pd_ports;
    let history = &snap.pd_ports_history;
    let pal = snap.palette;
    let mut content =
        column![
            text("Ports")
                .size(FONT_SECTION)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pal.header)
                })
        ]
        .spacing(2);

    // Card names have no EC source (the enumeration was always empty), so
    // every label below is inferred from PD state alone.
    if ports.is_empty() {
        content = content.push(text("None detected").size(FONT_BODY).style(move |_theme| {
            iced::widget::text::Style {
                color: Some(pal.gray),
            }
        }));
        if snap.ports_debug {
            content = content.push(text("[Debug] No ports detected").size(FONT_SMALL).style(
                move |_theme| iced::widget::text::Style {
                    color: Some(pal.gray),
                },
            ));
        }
    } else {
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
                ever_seen_sink,
            );
            let is_display_card = card_type == "DisplayPort Expansion Card"
                || card_type == "HDMI Expansion Card"
                || card_type == "DP/HDMI Expansion Card";

            let mut row_content =
                row![text(format!("Port {} ({})", port.port, card_type)).size(FONT_BODY),]
                    .align_y(iced::Alignment::Center)
                    .spacing(6);
            if port.dp_alt_mode {
                let badge = pal.header;
                row_content = row_content.push(text("DP").size(FONT_SMALL).style(move |_theme| {
                    iced::widget::text::Style {
                        // Badge text follows the theme header instead of
                        // fixed sensor blue (low contrast on Light).
                        color: Some(badge),
                    }
                }));
            }
            content = content.push(row_content);
            if port.pd_contract
                && !is_display_card
                && let Some(ref level) = port.negotiated_text
            {
                let color = if port.power_role == Some("Source") {
                    pal.gray
                } else {
                    pal.green
                };
                content = content.push(
                    text(format!("  {}", level))
                        .size(FONT_SMALL)
                        .style(move |_theme| iced::widget::text::Style { color: Some(color) }),
                );
            }
            if snap.ports_debug {
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
                content = content.push(text(debug_line).size(FONT_SMALL).style(move |_theme| {
                    iced::widget::text::Style {
                        color: Some(pal.gray),
                    }
                }));
            }
        }
    }

    content.into()
}

#[cfg(test)]
mod tests {
    use super::privacy_labels;

    #[test]
    fn privacy_labels_match_upstream_polarity() {
        // framework_lib get_privacy_info: true = device enabled/connected.
        assert_eq!(privacy_labels(true, true), ("Mic On", "Camera On"));
        assert_eq!(privacy_labels(true, false), ("Mic On", "Camera Off"));
        assert_eq!(privacy_labels(false, true), ("Mic Off", "Camera On"));
        assert_eq!(privacy_labels(false, false), ("Mic Off", "Camera Off"));
    }
}
