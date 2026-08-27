use crate::App;
use crate::Message;
use crate::style::*;
use crate::util::read_lock;
use iced::widget::{button, column, container, row, space, text};
use iced::{Element, Length};

/// About page reads `App` live; modal and infrequent so it bypasses snapshot cache.
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
    }
    sw_content = sw_content.push(info_row(
        "PawnIO Modules",
        crate::cpu_power::pawnio_modules_version(),
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
            .style(|_theme: &iced::Theme| iced::widget::text::Style {
                color: Some(COLOR_GRAY),
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
                .style(move |_theme, _status| mode_style(startup_enabled)),
            startup_err_el,
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
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
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    );
    content = content.push(
        row![
            button(text("Collect Debug Info").size(FONT_BODY))
                .on_press(Message::CollectDebugInfo)
                .style(btn_style),
            button(text("Project on GitHub").size(FONT_BODY))
                .on_press(Message::OpenProjectUrl)
                .style(btn_style),
        ]
        .spacing(8),
    );

    content = content.push(space::vertical().height(8));
    content = content.push(
        column![
            text("Framework Crate — MIT License")
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_GRAY)
                }),
            text("framework_lib — BSD-3-Clause")
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_GRAY)
                }),
            text("PawnIO (optional) — GPL-2.0")
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_GRAY)
                }),
            text("PawnIO Modules (optional) — LGPL-2.1")
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_GRAY)
                }),
            text("iced / tokio / tracing — MIT License")
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_GRAY)
                }),
            text("serde / windows-sys — MIT OR Apache-2.0")
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_GRAY)
                }),
        ]
        .spacing(2),
    );

    container(content)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}
