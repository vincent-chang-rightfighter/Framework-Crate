use crate::App;
use crate::Message;
use crate::style::*;
use crate::types::FanControlMode;
use crate::util::read_lock;
use iced::widget::rule;
use iced::widget::{button, column, container, space, text};
use iced::{Element, Length};
use std::sync::atomic::Ordering;

fn quit_mode_messages(mode: FanControlMode) -> (&'static str, &'static str) {
    match mode {
        FanControlMode::Curve => (
            "Fan is in curve mode (temperature-controlled)",
            "Curve control will stop. Fan will be fixed to current duty.",
        ),
        FanControlMode::Manual => (
            "Fan is in manual mode",
            "Manual control will stop. Fan will be fixed to current duty.",
        ),
        FanControlMode::Disabled => (
            "Fan is in automatic mode (firmware control)",
            "Firmware control will remain active after exit.",
        ),
    }
}

pub(crate) fn view_quit_warning(app: &App) -> Element<'_, Message> {
    let config = read_lock(&app.state.lifecycle.config);
    let current_duty = app.state.fan.last_applied_duty.load(Ordering::Acquire) as u32;
    let set_duty = app.quit_duty_value;

    let mut content = column![].spacing(12).padding(20);
    content = content.push(text("Framework Crate").size(20));
    content = content.push(rule::horizontal(1).style(rule_style));
    let (mode_label, stop_label) = quit_mode_messages(config.fan.mode);
    content = content.push(text(mode_label).size(FONT_BODY));
    content = content.push(text(format!("Current duty: {}%", current_duty)).size(FONT_BODY));
    content = content.push(space::horizontal().height(4));
    content = content.push(text(stop_label).size(FONT_BODY));
    content = content.push(text("Choose how to handle the fan before closing:").size(FONT_BODY));
    content = content.push(rule::horizontal(1).style(rule_style));

    content = content.push(
        iced::widget::row![
            text("Set duty to:").size(FONT_BODY),
            iced::widget::slider(0..=100, set_duty, Message::QuitDutyChanged).style(slider_style),
            text(format!("{}%", set_duty)).size(FONT_BODY),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center),
    );

    let set_label = format!("Fixed {}% & Exit", set_duty);
    let exit_label = "Keep Current Duty & Exit";
    content = content.push(
        iced::widget::row![
            button(text("Restore Auto & Exit").size(14))
                .on_press(Message::QuitWithRestore)
                .style(btn_style),
            button(text(set_label).size(14))
                .on_press(Message::QuitWithDuty)
                .style(btn_style),
            button(text(exit_label).size(14))
                .on_press(Message::QuitWithoutRestore)
                .style(btn_style),
            button(text("Cancel").size(14))
                .on_press(Message::QuitCanceled)
                .style(btn_style),
        ]
        .spacing(8),
    );

    container(content)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_mode_uses_automatic_labels() {
        let (label, stop) = quit_mode_messages(FanControlMode::Disabled);
        assert!(label.contains("automatic"));
        assert!(stop.contains("remain active"));
    }

    #[test]
    fn manual_mode_uses_manual_labels() {
        let (label, stop) = quit_mode_messages(FanControlMode::Manual);
        assert!(label.contains("manual"));
        assert!(stop.contains("Manual"));
    }

    #[test]
    fn curve_mode_uses_curve_labels() {
        let (label, stop) = quit_mode_messages(FanControlMode::Curve);
        assert!(label.contains("curve"));
        assert!(stop.contains("Curve"));
    }
}
