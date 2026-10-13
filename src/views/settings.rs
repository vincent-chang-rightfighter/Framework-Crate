use crate::App;
use crate::Message;
use crate::style::*;
use crate::util::read_lock;
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Element, Length};

/// About page reads `App` live; modal and infrequent so it bypasses snapshot cache.
/// Section header matching the CPU Power card style.
fn section_title(id: crate::style::ThemeId, label: &str) -> Element<'_, Message> {
    text(label)
        .size(FONT_SECTION)
        .style(move |_theme| iced::widget::text::Style {
            color: Some(crate::style::palette_for(id).header),
        })
        .into()
}

pub(crate) fn view_settings(app: &App) -> Element<'_, Message> {
    let versions = read_lock(&app.state.system.versions);

    let title_row = row![
        text("About").size(20),
        space::horizontal(),
        button(text("Close").size(FONT_BODY))
            .on_press(Message::SettingsToggled)
            .style(btn_style),
    ];

    let mut hw_content = column![].spacing(2);
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

    let mut sw_content = column![].spacing(2);
    if let Some(v) = versions.as_ref().as_ref() {
        if let Some(ref bios) = v.uefi_version {
            sw_content = sw_content.push(info_row("BIOS", bios));
        }
        if let Some(ref ec) = v.ec_build_version {
            sw_content = sw_content.push(info_row("EC Firmware", ec));
        }
    }
    sw_content = sw_content.push(info_row("framework_lib", env!("FRAMEWORK_LIB_VERSION")));
    if let Some(ref ver) = crate::cpu_power::pawnio_version() {
        sw_content = sw_content.push(info_row("PawnIO", ver));
    } else {
        sw_content = sw_content.push(info_row("PawnIO", "Not installed"));
    }
    sw_content = sw_content.push(info_row(
        "PawnIO Modules",
        &crate::cpu_power::pawnio_modules_version(),
    ));
    if !app.system_info.os.is_empty() {
        sw_content = sw_content.push(info_row("OS", &app.system_info.os));
    }
    sw_content = sw_content.push(space::vertical().height(8));
    sw_content = sw_content.push(text("Poll Rate:").size(FONT_BODY));
    let config = read_lock(&app.state.lifecycle.config);
    let poll_ms = config.telemetry.poll_ms as u32;
    sw_content = sw_content.push(
        row![
            iced::widget::slider(
                POLL_RATE_MIN_MS..=crate::types::POLL_MS_MAX as u32,
                poll_ms,
                |v| Message::PollRateChanged(v as u64)
            )
            .step(10u32)
            .style(slider_style),
            text(format!("{} ms", poll_ms)).size(FONT_BODY),
        ]
        .spacing(4),
    );

    sw_content = sw_content.push(text("Refresh Interval:").size(FONT_BODY));
    let refresh_ms = config.telemetry.ui_refresh_ms as u32;
    sw_content = sw_content.push(
        row![
            iced::widget::slider(
                crate::types::UI_REFRESH_MS_MIN as u32..=crate::types::UI_REFRESH_MS_MAX as u32,
                refresh_ms,
                |v| Message::UiRefreshRateChanged(v as u64)
            )
            .step(50u32)
            .style(slider_style),
            text(format!("{} ms", refresh_ms)).size(FONT_BODY),
        ]
        .spacing(4),
    );

    let startup_enabled = app.startup_launch_enabled;
    let startup_error = app.startup_launch_error.clone();
    let startup_err_el: Element<'_, Message> = if let Some(err) = startup_error {
        text(err)
            .size(FONT_SMALL)
            .style(|theme: &iced::Theme| iced::widget::text::Style {
                color: Some(palette(theme).gray),
            })
            .into()
    } else {
        iced::widget::Space::new().into()
    };
    sw_content = sw_content.push(
        row![
            text("Launch at Startup:").size(FONT_BODY),
            button(text(if startup_enabled { "ON" } else { "OFF" }).size(FONT_BODY))
                .on_press(Message::StartupLaunchToggled(!startup_enabled))
                .style(move |theme, _status| mode_style(theme, startup_enabled)),
            startup_err_el,
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    );

    let mut content = column![].spacing(8);
    let ec_debug = app.expansion_card_debug;
    let diagnostics = column![
        section_title(app.theme, "Diagnostics"),
        row![
            text("Expansion Card Debug Mode:").size(FONT_BODY),
            button(text(if ec_debug { "ON" } else { "OFF" }).size(FONT_BODY))
                .on_press(Message::ToggleExpansionCardDebug)
                .style(move |theme, _status| mode_style(theme, ec_debug)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
        row![
            text("Tools:").size(FONT_BODY),
            button(text("Debug Info").size(FONT_BODY))
                .on_press(Message::CollectDebugInfo)
                .style(btn_style),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    ]
    .spacing(8);
    // Device and Diagnostics stay side by side as independent cards;
    // uneven bottoms are inherent to dynamic content, accepted here
    // for adjacency.
    content = content.push(
        row![
            card(
                column![section_title(app.theme, "Device"), hw_content]
                    .spacing(4)
                    .into()
            ),
            card(diagnostics.into()),
        ]
        .spacing(8),
    );
    let mut updates = column![
        section_title(app.theme, "Updates"),
        update_check_row(app),
        row![
            text("PawnIO:").size(FONT_BODY),
            button(text("Update PawnIO").size(FONT_BODY))
                .on_press(Message::UpdatePawnIOAll)
                .style(btn_style),
            button(text("Open Folder").size(FONT_BODY))
                .on_press(Message::OpenModulesDir)
                .style(btn_style),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
        text("If update fails, check internet or download manually: PawnIO → https://github.com/namazso/PawnIO/releases | Modules → https://github.com/namazso/PawnIO.Modules/releases/latest")
            .size(FONT_SMALL)
            .style(|theme| iced::widget::text::Style {
                color: Some(palette(theme).gray),
            }),
    ]
    .spacing(8);
    if let Some(ref err) = app.modules_download_error {
        updates = updates.push(text(err.as_str()).size(FONT_SMALL).style(|theme| {
            iced::widget::text::Style {
                color: Some(palette(theme).error),
            }
        }));
    }
    content = content.push(card(
        column![section_title(app.theme, "Software"), sw_content]
            .spacing(4)
            .into(),
    ));
    content = content.push(card(theme_picker(app)));
    content = content.push(card(Element::from(updates)));
    if let Some(ref err) = app.cpu_power_error {
        content = content.push(card(
            text(err.as_str())
                .size(FONT_SMALL)
                .style(|theme| iced::widget::text::Style {
                    color: Some(palette(theme).error),
                })
                .into(),
        ));
    }
    content = content.push(card(
        column![
            section_title(app.theme, "License"),
            text("Framework Crate — MIT License")
                .size(FONT_SMALL)
                .style(|theme| iced::widget::text::Style {
                    color: Some(palette(theme).gray)
                }),
            text("framework_lib — BSD-3-Clause")
                .size(FONT_SMALL)
                .style(|theme| iced::widget::text::Style {
                    color: Some(palette(theme).gray)
                }),
            text("PawnIO (optional) — GPL-2.0")
                .size(FONT_SMALL)
                .style(|theme| iced::widget::text::Style {
                    color: Some(palette(theme).gray)
                }),
            text("PawnIO Modules (optional) — LGPL-2.1")
                .size(FONT_SMALL)
                .style(|theme| iced::widget::text::Style {
                    color: Some(palette(theme).gray)
                }),
            text("iced / tokio / tracing — MIT License")
                .size(FONT_SMALL)
                .style(|theme| iced::widget::text::Style {
                    color: Some(palette(theme).gray)
                }),
            text("serde / windows-sys — MIT OR Apache-2.0")
                .size(FONT_SMALL)
                .style(|theme| iced::widget::text::Style {
                    color: Some(palette(theme).gray)
                }),
        ]
        .spacing(2)
        .into(),
    ));

    // Scrollable: section headers grew the page past some window heights,
    // and autosize stays off while settings are shown, so without this the
    // License block at the bottom is unreachable.
    // Title bar stays fixed like the main header; only the cards scroll.
    // Insets match the main page: header padding [8,12], content 4/12/12/12.
    column![
        container(title_row)
            .padding(iced::Padding::from([8, 12]))
            .width(Length::Fill),
        scrollable(
            container(content)
                .center_x(Length::Fill)
                .padding(iced::Padding {
                    top: 4.0,
                    right: 12.0,
                    bottom: 12.0,
                    left: 12.0,
                }),
        )
        .direction(scrollable::Direction::Vertical(
            scrollable::Scrollbar::new()
                .width(6.0)
                .scroller_width(6.0)
                .spacing(4.0),
        ))
        .height(Length::Fill),
    ]
    .spacing(5)
    .into()
}

/// Theme picker: two rows of scheme buttons with selected highlight.
/// Default preserves the exact long-standing look.
fn theme_picker(app: &App) -> Element<'_, Message> {
    let mut rows = column![section_title(app.theme, "Theme")].spacing(8);
    // Two rows of four: the half-width card cannot fit eight side by side.
    for chunk in crate::style::ThemeId::ALL.chunks(4) {
        let mut row = row![].spacing(8).align_y(iced::Alignment::Center);
        for id in chunk {
            let selected = app.theme == *id;
            row = row.push(
                button(text(id.label()).size(FONT_SMALL))
                    .on_press(Message::ThemeChanged(*id))
                    .style(move |theme, _status| mode_style(theme, selected)),
            );
        }
        rows = rows.push(row);
    }
    rows = rows.push(
        text("Default preserves the current look.")
            .size(FONT_SMALL)
            .style(|theme| iced::widget::text::Style {
                color: Some(palette(theme).gray),
            }),
    );
    rows.into()
}

/// Self update check row: manual check button, result status, and a download
/// shortcut when a newer release exists. Notification only, nothing installs.
fn update_check_row(app: &App) -> Element<'_, Message> {
    let mut row = row![
        text("App Updates:").size(FONT_BODY),
        button(
            text(if app.update_checking {
                "Checking…"
            } else {
                "Check"
            })
            .size(FONT_BODY)
        )
        .on_press(Message::CheckForUpdates)
        .style(btn_style),
        button(text("GitHub").size(FONT_BODY))
            .on_press(Message::OpenUrl(crate::update_check::PROJECT_URL))
            .style(btn_style),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    if let Some(ref tag) = app.update_available {
        row = row.push(
            button(text(format!("Download {tag}")).size(FONT_BODY))
                .on_press(Message::OpenUrl(crate::update_check::RELEASES_URL))
                .style(btn_style),
        );
        row = row.push(
            text(format!("New version {tag} available"))
                .size(FONT_SMALL)
                .style(|theme| iced::widget::text::Style {
                    color: Some(palette(theme).green),
                }),
        );
    } else if let Some(ref err) = app.update_check_error {
        row = row.push(text(err.as_str()).size(FONT_SMALL).style(|theme| {
            iced::widget::text::Style {
                color: Some(palette(theme).error),
            }
        }));
    } else if app.update_checked {
        row = row.push(
            text(format!("Up to date ({})", env!("CARGO_PKG_VERSION")))
                .size(FONT_SMALL)
                .style(|theme| iced::widget::text::Style {
                    color: Some(palette(theme).gray),
                }),
        );
    }
    row.into()
}
