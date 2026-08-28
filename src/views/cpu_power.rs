use crate::Message;
use crate::style::*;
use crate::views::ViewSnapshot;
use iced::widget::{button, column, container, row, space, text, text_input};
use iced::{Element, Length};
pub(crate) fn cpu_power_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let mut content = column![].spacing(2);

    let settings_label = if snap.show_cpu_power_settings {
        "[-] Settings"
    } else {
        "[+] Settings"
    };
    content = content.push(row![
        text("CPU Power")
            .size(FONT_SECTION)
            .style(|_theme| iced::widget::text::Style {
                color: Some(COLOR_HEADER)
            }),
        space::horizontal(),
        if snap.intel_cpu {
            button(text(settings_label).size(FONT_SMALL))
                .on_press(Message::ToggleCpuPowerSettings)
                .style(btn_style)
        } else {
            button(text(settings_label).size(FONT_SMALL)).style(btn_style)
        },
    ]);

    if !snap.intel_cpu {
        content = content.push(text("Not Supported").size(FONT_BODY).style(|_theme| {
            iced::widget::text::Style {
                color: Some(COLOR_NOT_SUPPORTED_TEXT),
            }
        }));
        content = content.push(
            text("CPU Power is available on Intel CPUs only.")
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_GRAY),
                }),
        );
        return content.into();
    }

    let info = &snap.cpu_power;

    if !info.available {
        let msg = info.error_msg.unwrap_or("PawnIO driver not available");
        content =
            content.push(
                text(msg)
                    .size(FONT_BODY)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_GRAY),
                    }),
            );
        if !crate::cpu_power::is_pawnio_installed() {
            content = content.push(
                button(text("Install PawnIO").size(FONT_BODY))
                    .on_press(Message::InstallPawnIO)
                    .style(btn_style),
            );
        } else if !crate::cpu_power::modules_downloaded() {
            content = content.push(text("PawnIO Modules required").size(FONT_SMALL).style(
                |_theme| iced::widget::text::Style {
                    color: Some(COLOR_GRAY),
                },
            ));
            if let Some(ref err) = snap.modules_download_error {
                content = content.push(text(err.as_str()).size(FONT_SMALL).style(|_theme| {
                    iced::widget::text::Style {
                        color: Some(iced::Color::from_rgb(0.9, 0.3, 0.3)),
                    }
                }));
            }
            content = content.push(
                button(text("Download PawnIO Modules").size(FONT_BODY))
                    .on_press(Message::DownloadPawnIOModules)
                    .style(btn_style),
            );
            content = content.push(
                text("Manual: download release_0_2_10.zip from https://github.com/namazso/PawnIO.Modules/releases/tag/0.2.10 and place IntelMSR.bin / IntelMCHBAR.bin into %APPDATA%\\framework-crate\\modules\\")
                    .size(FONT_SMALL)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_GRAY),
                    }),
            );
            content = content.push(
                row![
                    button(text("Open Modules Folder").size(FONT_BODY))
                        .on_press(Message::OpenModulesDir)
                        .style(btn_style),
                    button(text("Redetect Modules").size(FONT_BODY))
                        .on_press(Message::RedetectModules)
                        .style(btn_style),
                ]
                .spacing(8),
            );
        }
        return content.into();
    }

    // Show effective (min) limit; CPU enforces lower of MSR and MMIO.
    let pl1_color = if snap.pl_custom_applied {
        COLOR_GREEN
    } else {
        COLOR_HEADER
    };
    let pl2_color = if snap.pl_custom_applied {
        COLOR_GREEN
    } else {
        COLOR_HEADER
    };
    content = content.push(
        row![
            text("  PL1:".to_string()).size(FONT_BODY),
            text(format!("{:.1}W", info.effective_pl1()))
                .size(FONT_BODY)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pl1_color)
                }),
            text("  PL2:".to_string()).size(FONT_BODY),
            text(format!("{:.1}W", info.effective_pl2()))
                .size(FONT_BODY)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pl2_color)
                }),
            if snap.sync_enabled {
                text("  [Syncing]")
                    .size(FONT_SMALL)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_GREEN),
                    })
            } else {
                text("").size(FONT_SMALL)
            },
        ]
        .spacing(4),
    );

    if snap.show_cpu_power_settings {
        let mut settings_content = column![].spacing(4).padding(4);

        settings_content = settings_content.push(text("MSR (Read-only)").size(FONT_BODY));
        settings_content = settings_content.push(
            row![
                text(format!(
                    "  PL1: {:.1}W ({:.2}s)",
                    info.pl1_msr, info.pl1_time_s
                ))
                .size(FONT_BODY),
                text(if info.pl1_msr_enabled {
                    " [En]"
                } else {
                    " [Dis]"
                })
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(if info.pl1_msr_enabled {
                        COLOR_GREEN
                    } else {
                        COLOR_GRAY
                    })
                }),
                text(if info.pl1_msr_clamped { " [Cl]" } else { "" })
                    .size(FONT_SMALL)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_HEADER)
                    }),
            ]
            .spacing(4),
        );
        settings_content = settings_content.push(
            row![
                text(format!(
                    "  PL2: {:.1}W ({:.2}s)",
                    info.pl2_msr, info.pl2_time_s
                ))
                .size(FONT_BODY),
                text(if info.pl2_msr_enabled {
                    " [En]"
                } else {
                    " [Dis]"
                })
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(if info.pl2_msr_enabled {
                        COLOR_GREEN
                    } else {
                        COLOR_GRAY
                    })
                }),
                text(if info.pl2_msr_clamped { " [Cl]" } else { "" })
                    .size(FONT_SMALL)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_HEADER)
                    }),
            ]
            .spacing(4),
        );

        settings_content = settings_content.push(text("MMIO (Read-only)").size(FONT_BODY));
        settings_content = settings_content.push(
            row![
                text(format!(
                    "  PL1: {:.1}W ({:.2}s)",
                    info.pl1_mmio, info.pl1_mmio_time_s
                ))
                .size(FONT_BODY),
                text(if info.pl1_mmio_enabled {
                    " [En]"
                } else {
                    " [Dis]"
                })
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(if info.pl1_mmio_enabled {
                        COLOR_GREEN
                    } else {
                        COLOR_GRAY
                    })
                }),
                text(if info.pl1_mmio_clamped { " [Cl]" } else { "" })
                    .size(FONT_SMALL)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_HEADER)
                    }),
            ]
            .spacing(4),
        );
        settings_content = settings_content.push(
            row![
                text(format!(
                    "  PL2: {:.1}W ({:.2}s)",
                    info.pl2_mmio, info.pl2_mmio_time_s
                ))
                .size(FONT_BODY),
                text(if info.pl2_mmio_enabled {
                    " [En]"
                } else {
                    " [Dis]"
                })
                .size(FONT_SMALL)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(if info.pl2_mmio_enabled {
                        COLOR_GREEN
                    } else {
                        COLOR_GRAY
                    })
                }),
                text(if info.pl2_mmio_clamped { " [Cl]" } else { "" })
                    .size(FONT_SMALL)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_HEADER)
                    }),
            ]
            .spacing(4),
        );

        // Editable PL1/PL2 writes to MSR 0x610.
        settings_content = settings_content.push(text("PL1/PL2 Control").size(FONT_BODY));
        settings_content = settings_content.push(
            row![
                text("  PL1:").size(FONT_BODY),
                text_input("W", &snap.pl1_edit)
                    .width(Length::Fixed(60.0))
                    .on_input(Message::CpuPowerPl1Changed),
                iced::widget::checkbox(snap.pl1_enabled)
                    .on_toggle(Message::CpuPowerPl1EnabledToggled),
                text("En").size(FONT_SMALL),
                iced::widget::checkbox(snap.pl1_clamped)
                    .on_toggle(Message::CpuPowerPl1ClampedToggled),
                text("Cl").size(FONT_SMALL),
                text("T:").size(FONT_SMALL),
                text_input("s", &snap.pl1_time_edit)
                    .width(Length::Fixed(50.0))
                    .on_input(Message::CpuPowerPl1TimeChanged),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        );
        settings_content = settings_content.push(
            row![
                text("  PL2:").size(FONT_BODY),
                text_input("W", &snap.pl2_edit)
                    .width(Length::Fixed(60.0))
                    .on_input(Message::CpuPowerPl2Changed),
                iced::widget::checkbox(snap.pl2_enabled)
                    .on_toggle(Message::CpuPowerPl2EnabledToggled),
                text("En").size(FONT_SMALL),
                iced::widget::checkbox(snap.pl2_clamped)
                    .on_toggle(Message::CpuPowerPl2ClampedToggled),
                text("Cl").size(FONT_SMALL),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        );

        if let Some(ref err) = snap.cpu_power_error {
            settings_content =
                settings_content.push(text(err.as_str()).size(FONT_SMALL).style(|_theme| {
                    iced::widget::text::Style {
                        color: Some(iced::Color::from_rgb(0.9, 0.3, 0.3)),
                    }
                }));
        }

        if snap.sync_enabled {
            settings_content = settings_content.push(
                text("Syncing MSR 0x610 every 250ms")
                    .size(FONT_SMALL)
                    .style(|_theme| iced::widget::text::Style {
                        color: Some(COLOR_GREEN),
                    }),
            );
        }

        settings_content = settings_content.push(
            row![
                button(text("Apply").size(FONT_BODY))
                    .on_press(Message::CpuPowerApply)
                    .style(btn_style),
                button(
                    text(if snap.sync_enabled {
                        "Stop Sync"
                    } else {
                        "Start Sync"
                    })
                    .size(FONT_BODY)
                )
                .on_press(if snap.sync_enabled {
                    Message::CpuPowerSyncStop
                } else {
                    Message::CpuPowerSyncStart
                })
                .style(btn_style),
                button(text("Reset").size(FONT_BODY))
                    .on_press(Message::CpuPowerSyncReset)
                    .style(btn_style),
            ]
            .spacing(8),
        );

        content = content.push(
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
                }),
        );
    }

    content.into()
}
