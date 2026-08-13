pub const COLOR_GREEN: iced::Color = iced::Color { r: 0.13, g: 0.77, b: 0.37, a: 1.0 };
pub const COLOR_GRAY: iced::Color = iced::Color { r: 0.6, g: 0.6, b: 0.6, a: 1.0 };
pub const COLOR_DARK: iced::Color = iced::Color { r: 0.3, g: 0.3, b: 0.3, a: 1.0 };
pub const COLOR_HEADER: iced::Color = iced::Color { r: 0.9, g: 0.9, b: 0.9, a: 1.0 };
pub const COLOR_SETTINGS_BG: iced::Color = iced::Color { r: 0.15, g: 0.15, b: 0.18, a: 1.0 };
pub const COLOR_CARD_BG: iced::Color = iced::Color { r: 0.14, g: 0.14, b: 0.17, a: 1.0 };
pub const COLOR_CARD_BORDER: iced::Color = iced::Color { r: 0.25, g: 0.25, b: 0.28, a: 1.0 };
pub const COLOR_NOT_SUPPORTED_BG: iced::Color = iced::Color { r: 0.25, g: 0.12, b: 0.12, a: 0.4 };
pub const COLOR_NOT_SUPPORTED_TEXT: iced::Color = iced::Color { r: 0.7, g: 0.4, b: 0.4, a: 1.0 };

/// Minimum hardware poll interval (ms). framework_tool takes ~50-100ms to run,
/// so 200ms prevents overlapping subprocess calls while keeping UI responsive.
pub const POLL_RATE_MIN_MS: u32 = 200;
/// Minimum battery charge limit (%). Framework EC enforces ~25% minimum to
/// preserve battery health; values below are ignored by hardware.
pub const CHARGE_LIMIT_MIN: u32 = 25;
/// Maximum battery charge limit (%). Above 100% is invalid per EC spec.
pub const CHARGE_LIMIT_MAX: u32 = 100;

pub const FONT_SECTION: f32 = 14.0;
pub const FONT_BODY: f32 = 12.0;
pub const FONT_SMALL: f32 = 10.0;

/// Background idle detection: if no user interaction for this many ms,
/// the background poll loop switches to a slower interval to save CPU.
/// Not applied to fan-curve mode, which must stay responsive to temperature.
pub const IDLE_THRESHOLD_MS: u64 = 10_000;
/// Background poll interval when the user is idle and the fan mode is not
/// Curve (ms).
pub const IDLE_INTERVAL_MS: u64 = 2_000;
/// UI tick interval when the user is idle (ms). The UI only needs to
/// rebuild while the user is watching it; at rest it drops to 1Hz.
pub const UI_IDLE_INTERVAL_MS: u64 = 1_000;
/// UI tick interval when the window is hidden (minimized to tray) (ms).
/// Hidden windows receive no WM_PAINT, so no presents/view() run while hidden
/// (each tick is just a trivial update + RedrawWindow syscall). 500ms keeps
/// tray restore / shutdown responsive without meaningful CPU cost.
pub const UI_HIDDEN_INTERVAL_MS: u64 = 500;

/// Expansion card / PD port scan interval (ms). Runs on a fixed wall-clock
/// interval independent of idle state, so hotplug stays detectable while the
/// user is away.
pub const EXPANSION_SCAN_MS: u64 = 10_000;
/// versions scan interval (ms).
pub const VERSIONS_REFRESH_MS: u64 = 60_000;

/// Number of consecutive PD port samples in the same state before it is
/// classified as "stable" (i.e. a USB-A expansion card rather than a
/// transient USB device plugged into a USB-C port).
pub const STABLE_THRESHOLD: usize = 2;

pub const SENSOR_COLORS: [iced::Color; 10] = [
    iced::Color { r: 0.23, g: 0.51, b: 0.96, a: 1.0 },
    iced::Color { r: 0.94, g: 0.27, b: 0.27, a: 1.0 },
    iced::Color { r: 0.06, g: 0.73, b: 0.51, a: 1.0 },
    iced::Color { r: 0.96, g: 0.62, b: 0.04, a: 1.0 },
    iced::Color { r: 0.55, g: 0.36, b: 0.96, a: 1.0 },
    iced::Color { r: 0.96, g: 0.35, b: 0.70, a: 1.0 },
    iced::Color { r: 0.20, g: 0.80, b: 0.80, a: 1.0 },
    iced::Color { r: 0.80, g: 0.80, b: 0.20, a: 1.0 },
    iced::Color { r: 1.00, g: 0.60, b: 0.20, a: 1.0 },
    iced::Color { r: 0.40, g: 0.70, b: 0.30, a: 1.0 },
];

pub fn card<'a>(content: iced::Element<'a, crate::Message>) -> iced::Element<'a, crate::Message> {
    iced::widget::container(content)
        .padding(12)
        .width(iced::Length::Fill)
        .style(|_theme| iced::widget::container::Style {
            background: Some(COLOR_CARD_BG.into()),
            border: iced::Border::default().rounded(8).color(COLOR_CARD_BORDER).width(1),
            ..Default::default()
        })
        .into()
}

pub fn colored_dot<'a>(color: iced::Color, size: f32) -> iced::Element<'a, crate::Message> {
    iced::widget::container(iced::widget::text("").size(FONT_SMALL))
        .width(size).height(size).center_x(size).center_y(size)
        .style(move |_theme| iced::widget::container::Style {
            background: Some(color.into()),
            border: iced::Border::default().rounded(size / 2.0),
            ..Default::default()
        })
        .into()
}

pub fn sensor_color(name: &str, sensor_keys: &[String]) -> iced::Color {
    let idx = sensor_keys.iter().position(|k| k == name).unwrap_or(0);
    SENSOR_COLORS[idx % SENSOR_COLORS.len()]
}

pub fn info_row(label: &str, value: &str) -> iced::Element<'static, crate::Message> {
    iced::widget::row![
        iced::widget::text(format!("{}:", label)).size(FONT_BODY).style(|_theme| iced::widget::text::Style { color: Some(COLOR_GRAY) }),
        iced::widget::text(value.to_owned()).size(FONT_BODY),
    ]
    .spacing(8)
    .into()
}

pub fn mode_style(selected: bool) -> iced::widget::button::Style {
    if selected {
        iced::widget::button::Style {
            background: Some(iced::Color::from_rgb(0.23, 0.51, 0.96).into()),
            text_color: iced::Color::WHITE,
            border: iced::Border::default().rounded(6).width(1).color(iced::Color::from_rgb(0.23, 0.51, 0.96)),
            ..iced::widget::button::Style::default()
        }
    } else {
        iced::widget::button::Style {
            background: Some(iced::Color::TRANSPARENT.into()),
            text_color: iced::Color::from_rgb(0.7, 0.7, 0.7),
            border: iced::Border::default().rounded(6).width(1).color(iced::Color::from_rgb(0.4, 0.4, 0.4)),
            ..iced::widget::button::Style::default()
        }
    }
}

pub fn btn_style(_theme: &iced::Theme, status: iced::widget::button::Status) -> iced::widget::button::Style {
    let (bg, text_color) = match status {
        iced::widget::button::Status::Hovered => (
            Some(iced::Color::from_rgba(0.3, 0.3, 0.3, 0.5).into()),
            iced::Color::WHITE,
        ),
        iced::widget::button::Status::Pressed => (
            Some(iced::Color::from_rgba(0.2, 0.2, 0.2, 0.6).into()),
            iced::Color::WHITE,
        ),
        iced::widget::button::Status::Disabled => (
            Some(iced::Color::TRANSPARENT.into()),
            iced::Color::from_rgb(0.5, 0.5, 0.5),
        ),
        _ => (
            Some(iced::Color::TRANSPARENT.into()),
            iced::Color::from_rgb(0.85, 0.85, 0.85),
        ),
    };
    iced::widget::button::Style {
        background: bg,
        text_color,
        border: iced::Border::default().rounded(6).width(1).color(iced::Color::from_rgb(0.4, 0.4, 0.4)),
        ..iced::widget::button::Style::default()
    }
}
