use crate::App;
use crate::Message;
use crate::style::*;
use iced::widget::{button, column, container, row, space, text};
use iced::{Element, Length};

pub(crate) fn view_header(app: &App) -> Element<'_, Message> {
    let header_content = column![
        row![
            text(&app.system_info.header_device_name).size(18),
            space::horizontal(),
            button(text("About").size(FONT_BODY))
                .on_press(Message::SettingsToggled)
                .style(btn_style),
        ]
        .align_y(iced::Alignment::Center),
        text(&app.system_info.header_info_text)
            .size(FONT_SMALL)
            .style(|_theme| iced::widget::text::Style {
                color: Some(COLOR_GRAY)
            }),
    ]
    .spacing(4);

    container(header_content)
        .padding(iced::Padding::from([8, 12]))
        .width(Length::Fill)
        .into()
}
