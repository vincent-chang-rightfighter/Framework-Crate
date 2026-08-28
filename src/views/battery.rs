use crate::App;
use crate::Message;
use crate::style::*;
use crate::views::ViewSnapshot;
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Element, Length};
pub(crate) fn view_charge_limit_section(enabled: bool, value: u32) -> Element<'static, Message> {
    column![
        row![
            iced::widget::checkbox(enabled).on_toggle(Message::ChargeLimitToggled),
            text("Max Charge Limit (%):").size(FONT_BODY),
            space::horizontal(),
            text(format!("{}%", value)).size(FONT_BODY),
        ]
        .spacing(4),
        iced::widget::slider(
            CHARGE_LIMIT_MIN..=CHARGE_LIMIT_MAX,
            value,
            Message::ChargeLimitChanged
        )
        .style(slider_style),
    ]
    .spacing(4)
    .into()
}

/// Unset limit means no cap; show as disabled at 100%.
pub(crate) fn charge_limit_display(limit: Option<crate::types::SettingU8>) -> (bool, u32) {
    match limit {
        Some(l) => (l.enabled, l.value as u32),
        None => (false, CHARGE_LIMIT_MAX),
    }
}

pub(crate) fn view_battery_info(
    battery: &crate::cli::ec_wrapper::BatteryData,
    charging: bool,
) -> Element<'_, Message> {
    let mut rows = column![].spacing(4);

    if let (Some(full), Some(design)) = (
        battery.last_full_charge_capacity_mah,
        battery.design_capacity_mah,
    ) {
        if let Some(health) = crate::types::battery_health_pct(full, design) {
            rows = rows.push(row![
                text("Battery Health:").size(FONT_BODY),
                space::horizontal(),
                text(format!("{}%  ({} / {} mAh)", health, full, design)).size(FONT_BODY),
            ]);
        }
    } else if let Some(v) = battery.remaining_capacity_mah {
        rows = rows.push(row![
            text("Remaining:").size(FONT_BODY),
            space::horizontal(),
            text(format!("{} mAh", v)).size(FONT_BODY),
        ]);
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

pub(crate) fn view_battery_verbose(
    battery: &crate::cli::ec_wrapper::BatteryData,
    show_details: bool,
) -> Option<Element<'_, Message>> {
    let has_verbose = battery.manufacturer.is_some()
        || battery.model_number.is_some()
        || battery.serial_number.is_some()
        || battery.battery_type.is_some()
        || battery.remaining_capacity_wh.is_some()
        || battery.design_capacity_wh.is_some()
        || battery.charger_temp_c.is_some();

    if !has_verbose {
        return None;
    }

    let details_label = if show_details {
        "[-] Details"
    } else {
        "[+] Details"
    };
    let mut content = column![].spacing(4);

    content = content.push(row![
        space::horizontal(),
        button(text(details_label).size(FONT_SMALL))
            .on_press(Message::ToggleBatteryDetails)
            .style(btn_style),
    ]);

    if show_details && let Some(details) = battery_detail_rows(battery) {
        content = content.push(details);
    }

    Some(content.into())
}

/// Height cap prevents Battery card from stretching with outer row.
const BATTERY_SECTION_MAX_HEIGHT: f32 = 300.0;

pub(crate) fn view_battery<'a>(app: &'a App, snap: &'a ViewSnapshot) -> Element<'a, Message> {
    if !snap.platform.has_battery() {
        return crate::views::not_supported_section("Battery & Power");
    }
    let battery = &snap.battery;
    let config = &snap.config;
    if let Some(battery) = battery.as_ref().as_ref() {
        let charging = battery.power_info.ac_present == Some(true)
            && battery.power_info.discharging != Some(true);
        let status_color = if charging { COLOR_GREEN } else { COLOR_HEADER };

        let power_text = battery
            .power_info
            .present_rate_ma
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

        let soc_text = battery
            .power_info
            .soc_pct
            .map(|s| format!("{}%", s))
            .unwrap_or_default();

        let title_row = row![
            text("Battery & Power")
                .size(FONT_SECTION)
                .style(|_theme| iced::widget::text::Style {
                    color: Some(COLOR_HEADER)
                }),
            space::horizontal(),
            text(format!("{}  {}", power_text, soc_text))
                .size(FONT_BODY)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(status_color)
                }),
        ]
        .align_y(iced::Alignment::Center);

        let (charge_limit_enabled, charge_limit_value) =
            charge_limit_display(config.battery.charge_limit_max_pct);

        let mut content = column![title_row].spacing(6);
        content = content.push(view_charge_limit_section(
            charge_limit_enabled,
            charge_limit_value,
        ));
        content = content.push(view_battery_info(&battery.power_info, charging));

        if let Some(verbose) = view_battery_verbose(&battery.power_info, app.show_battery_details) {
            content = content.push(verbose);
        }

        let right_pad = iced::Padding::ZERO.right(14.0);
        // Height cap keeps card compact; scrollable when details expand.
        container(scrollable(container(content).padding(right_pad)).height(Length::Shrink))
            .width(Length::Fill)
            .max_height(BATTERY_SECTION_MAX_HEIGHT)
            .into()
    } else {
        text(if app.cli_present {
            "Waiting for battery data..."
        } else {
            "EC not available"
        })
        .into()
    }
}

pub(crate) fn battery_detail_rows(
    battery_info: &crate::cli::ec_wrapper::BatteryData,
) -> Option<Element<'_, Message>> {
    let mut rows = column![].spacing(2).padding(4);
    let mut has_content = false;

    if let Some(ref v) = battery_info.manufacturer {
        has_content = true;
        rows = rows.push(
            row![
                text("Manufacturer:").size(FONT_SMALL),
                space::horizontal(),
                text(v.as_str()).size(FONT_SMALL)
            ]
            .spacing(4),
        );
    }
    if let Some(ref v) = battery_info.model_number {
        has_content = true;
        rows = rows.push(
            row![
                text("Model:").size(FONT_SMALL),
                space::horizontal(),
                text(v.as_str()).size(FONT_SMALL)
            ]
            .spacing(4),
        );
    }
    if let Some(ref v) = battery_info.serial_number {
        has_content = true;
        rows = rows.push(
            row![
                text("Serial:").size(FONT_SMALL),
                space::horizontal(),
                text(v.as_str()).size(FONT_SMALL)
            ]
            .spacing(4),
        );
    }
    if let Some(ref v) = battery_info.battery_type {
        has_content = true;
        rows = rows.push(
            row![
                text("Type:").size(FONT_SMALL),
                space::horizontal(),
                text(v.as_str()).size(FONT_SMALL)
            ]
            .spacing(4),
        );
    }
    if let Some(v) = battery_info.remaining_capacity_wh {
        has_content = true;
        rows = rows.push(
            row![
                text("Capacity (Wh):").size(FONT_SMALL),
                space::horizontal(),
                text(format!("{:.2} Wh", v)).size(FONT_SMALL)
            ]
            .spacing(4),
        );
    }
    if let Some(v) = battery_info.design_capacity_wh {
        has_content = true;
        rows = rows.push(
            row![
                text("Design (Wh):").size(FONT_SMALL),
                space::horizontal(),
                text(format!("{:.2} Wh", v)).size(FONT_SMALL)
            ]
            .spacing(4),
        );
    }
    if let Some(v) = battery_info.charger_temp_c {
        has_content = true;
        rows = rows.push(
            row![
                text("Charger Temp:").size(FONT_SMALL),
                space::horizontal(),
                text(format!("{:.2}°C", v)).size(FONT_SMALL)
            ]
            .spacing(4),
        );
    }
    if let Some(v) = battery_info.charger_voltage_mv {
        has_content = true;
        rows = rows.push(
            row![
                text("Charger Voltage:").size(FONT_SMALL),
                space::horizontal(),
                text(format!("{:.0} mV", v)).size(FONT_SMALL)
            ]
            .spacing(4),
        );
    }
    if let Some(v) = battery_info.charger_current_ma {
        has_content = true;
        rows = rows.push(
            row![
                text("Charger Current:").size(FONT_SMALL),
                space::horizontal(),
                text(format!("{} mA", v)).size(FONT_SMALL)
            ]
            .spacing(4),
        );
    }

    if has_content {
        Some(
            container(rows)
                .padding(iced::Padding::from([4, 8]))
                .width(Length::Fill)
                .style(|_theme| iced::widget::container::Style {
                    background: Some(COLOR_SETTINGS_BG.into()),
                    border: iced::Border::default()
                        .rounded(4)
                        .color(COLOR_DARK)
                        .width(1),
                    ..Default::default()
                })
                .into(),
        )
    } else {
        None
    }
}
