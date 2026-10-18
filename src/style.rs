pub const COLOR_GREEN: iced::Color = iced::Color {
    r: 0.13,
    g: 0.77,
    b: 0.37,
    a: 1.0,
};
pub const COLOR_GRAY: iced::Color = iced::Color {
    r: 0.6,
    g: 0.6,
    b: 0.6,
    a: 1.0,
};
pub const COLOR_DARK: iced::Color = iced::Color {
    r: 0.3,
    g: 0.3,
    b: 0.3,
    a: 1.0,
};
pub const COLOR_HEADER: iced::Color = iced::Color {
    r: 0.9,
    g: 0.9,
    b: 0.9,
    a: 1.0,
};
pub const COLOR_SETTINGS_BG: iced::Color = iced::Color {
    r: 0.15,
    g: 0.15,
    b: 0.18,
    a: 1.0,
};
pub const COLOR_CARD_BG: iced::Color = iced::Color {
    r: 0.14,
    g: 0.14,
    b: 0.17,
    a: 1.0,
};
pub const COLOR_CARD_BORDER: iced::Color = iced::Color {
    r: 0.25,
    g: 0.25,
    b: 0.28,
    a: 1.0,
};
pub const COLOR_NOT_SUPPORTED_BG: iced::Color = iced::Color {
    r: 0.25,
    g: 0.12,
    b: 0.12,
    a: 0.4,
};
pub const COLOR_NOT_SUPPORTED_TEXT: iced::Color = iced::Color {
    r: 0.7,
    g: 0.4,
    b: 0.4,
    a: 1.0,
};
/// Curve color (#6b75ff) shared by canvas and matching slider thumbs.
pub const COLOR_CURVE: iced::Color = iced::Color {
    r: 107.0 / 255.0,
    g: 117.0 / 255.0,
    b: 1.0,
    a: 1.0,
};

/// UI theme preference. Default preserves the exact long-standing look;
/// the rest map to iced built-in themes. Stock iced Dark is deliberately
/// NOT offered: Default renders on Theme::Dark and the palette resolver
/// keys on the theme value, so the two must never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeId {
    Default,
    Light,
    Dracula,
    Nord,
    Black,
    GruvboxDark,
    CatppuccinMocha,
    Nightfly,
}

impl ThemeId {
    pub const ALL: [ThemeId; 8] = [
        ThemeId::Default,
        ThemeId::Light,
        ThemeId::Dracula,
        ThemeId::Nord,
        ThemeId::Black,
        ThemeId::GruvboxDark,
        ThemeId::CatppuccinMocha,
        ThemeId::Nightfly,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemeId::Default => "Default",
            ThemeId::Light => "Light",
            ThemeId::Dracula => "Dracula",
            ThemeId::Nord => "Nord",
            ThemeId::Black => "Black",
            ThemeId::GruvboxDark => "Gruvbox",
            ThemeId::CatppuccinMocha => "Mocha",
            ThemeId::Nightfly => "Nightfly",
        }
    }

    pub fn parse(s: &str) -> Option<ThemeId> {
        match s.to_lowercase().as_str() {
            "default" => Some(ThemeId::Default),
            "light" => Some(ThemeId::Light),
            "dracula" => Some(ThemeId::Dracula),
            "nord" => Some(ThemeId::Nord),
            "black" | "oxocarbon" => Some(ThemeId::Black),
            "gruvboxdark" | "gruvbox" => Some(ThemeId::GruvboxDark),
            "catppuccinmocha" | "mocha" | "catppuccin mocha" => Some(ThemeId::CatppuccinMocha),
            "nightfly" => Some(ThemeId::Nightfly),
            _ => None,
        }
    }

    pub fn iced(self) -> iced::Theme {
        match self {
            ThemeId::Default => iced::Theme::Dark,
            ThemeId::Light => iced::Theme::Light,
            ThemeId::Dracula => iced::Theme::Dracula,
            ThemeId::Nord => iced::Theme::Nord,
            ThemeId::Black => iced::Theme::Oxocarbon,
            ThemeId::GruvboxDark => iced::Theme::GruvboxDark,
            ThemeId::CatppuccinMocha => iced::Theme::CatppuccinMocha,
            ThemeId::Nightfly => iced::Theme::Nightfly,
        }
    }
}

/// Resolved UI colors for one theme. The Default arm returns the
/// long-standing constants verbatim so the current look cannot drift;
/// other themes derive from iced's own palettes.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub header: iced::Color,
    pub gray: iced::Color,
    pub green: iced::Color,
    pub error: iced::Color,
    pub error_bg: iced::Color,
    pub warning: iced::Color,
    pub warning_bg: iced::Color,
    pub card_bg: iced::Color,
    pub card_border: iced::Color,
    pub settings_bg: iced::Color,
    pub dark: iced::Color,
    pub not_supported_bg: iced::Color,
    pub not_supported_text: iced::Color,
    pub btn_text: iced::Color,
    pub btn_text_dim: iced::Color,
    pub btn_hover_text: iced::Color,
    pub btn_border: iced::Color,
    pub btn_hover_bg: iced::Color,
    pub btn_pressed_bg: iced::Color,
    pub toggle_off_text: iced::Color,
    pub toggle_off_border: iced::Color,
    pub slider_empty: iced::Color,
    /// Selected-state fill and text (mode toggles, theme buttons). Default
    /// is the verbatim legacy blue with white text; other themes use their
    /// own primary pair.
    pub select_bg: iced::Color,
    pub select_text: iced::Color,
    /// Slider filled rail and handle. Default is the verbatim legacy curve
    /// indigo; other themes use their own primary.
    pub slider_fill: iced::Color,
    /// Fan-curve line and draggable points. Same derivation as the slider
    /// accent; sensor-chart curves stay fixed and do not use this slot.
    pub fan_curve: iced::Color,
    /// Temp-chart plot panel. Default is verbatim; other themes use a panel
    /// distinct from cards.
    pub temp_plot_bg: iced::Color,
    /// Curve-canvas plot panel. Same role as above, different legacy value.
    pub curve_plot_bg: iced::Color,
    /// Temp-chart axis labels.
    pub temp_axis_text: iced::Color,
    /// Curve-canvas axis labels.
    pub curve_axis_text: iced::Color,
    /// Grid strokes, plot borders, and light-on-dark rings/dots.
    pub grid_base: iced::Color,
}

/// Secondary text: primary text dimmed so subtitles sit below headers.
/// The contrast regression test composites this alpha before measuring.
fn dim_text(c: iced::Color) -> iced::Color {
    iced::Color { a: 0.7, ..c }
}

pub fn palette(theme: &iced::Theme) -> Palette {
    match theme {
        iced::Theme::Light
        | iced::Theme::Dracula
        | iced::Theme::Nord
        | iced::Theme::Oxocarbon
        | iced::Theme::GruvboxDark
        | iced::Theme::CatppuccinMocha
        | iced::Theme::Nightfly => {
            let p = theme.palette();
            let e = theme.extended_palette();
            // Secondary text must be readable on CARD backgrounds, not on
            // some other swatch: borrowing secondary.weak.text renders
            // near-black on dark cards. Recompute against the card color,
            // then dim so it sits below headers.
            let card_bg = e.background.weak.color;
            let gray = dim_text(iced::theme::palette::Pair::new(card_bg, p.text).text);
            let mut pal = Palette {
                header: p.text,
                gray,
                green: e.success.base.color,
                error: e.danger.base.color,
                error_bg: iced::Color {
                    a: 0.1,
                    ..e.danger.base.color
                },
                warning: p.warning,
                warning_bg: iced::Color {
                    a: 0.15,
                    ..p.warning
                },
                card_bg,
                card_border: e.background.strong.color,
                settings_bg: e.background.weaker.color,
                dark: e.background.strong.color,
                not_supported_bg: iced::Color {
                    a: 0.4,
                    ..e.danger.weak.color
                },
                not_supported_text: e.danger.base.color,
                btn_text: p.text,
                btn_text_dim: gray,
                btn_hover_text: p.text,
                btn_border: e.background.strong.color,
                btn_hover_bg: iced::Color { a: 0.12, ..p.text },
                btn_pressed_bg: iced::Color { a: 0.2, ..p.text },
                toggle_off_text: gray,
                toggle_off_border: e.background.strong.color,
                select_bg: e.primary.strong.color,
                select_text: e.primary.strong.text,
                slider_empty: e.background.strong.color,
                slider_fill: e.primary.base.color,
                fan_curve: e.primary.base.color,
                temp_plot_bg: e.background.weakest.color,
                curve_plot_bg: e.background.weakest.color,
                temp_axis_text: dim_text(p.text),
                curve_axis_text: dim_text(p.text),
                grid_base: p.text,
            };
            // Black goes true-black on the large surfaces; controls keep
            // Oxocarbon grays so they stay visible, and gray is recomputed
            // against the black card. Plots sit one lift above pure black
            // so panels read as panels, not holes.
            if *theme == iced::Theme::Oxocarbon {
                pal.settings_bg = iced::Color::BLACK;
                pal.card_bg = iced::Color::BLACK;
                let plot_lift = iced::Color::from_rgb8(0x0A, 0x0A, 0x0C);
                pal.temp_plot_bg = plot_lift;
                pal.curve_plot_bg = plot_lift;
                pal.gray =
                    dim_text(iced::theme::palette::Pair::new(iced::Color::BLACK, p.text).text);
            }
            // Gruvbox's signature accent is orange; iced's built-in primary
            // is aqua. Bright orange needs dark text to stay readable.
            // Green takes the bright variant; iced's olive reads muddy.
            if *theme == iced::Theme::GruvboxDark {
                let orange = iced::Color::from_rgb8(0xFE, 0x80, 0x19);
                pal.select_bg = orange;
                pal.select_text = iced::Color::BLACK;
                pal.slider_fill = orange;
                pal.fan_curve = orange;
                pal.green = iced::Color::from_rgb8(0xB8, 0xBB, 0x26);
            }
            pal
        }
        _ => Palette {
            header: COLOR_HEADER,
            gray: COLOR_GRAY,
            green: COLOR_GREEN,
            error: iced::Color::from_rgb(0.9, 0.3, 0.3),
            error_bg: iced::Color::from_rgba(0.9, 0.2, 0.2, 0.1),
            warning: iced::Color::from_rgb(0.9, 0.6, 0.0),
            warning_bg: iced::Color::from_rgba(0.9, 0.6, 0.0, 0.15),
            card_bg: COLOR_CARD_BG,
            card_border: COLOR_CARD_BORDER,
            settings_bg: COLOR_SETTINGS_BG,
            dark: COLOR_DARK,
            not_supported_bg: COLOR_NOT_SUPPORTED_BG,
            not_supported_text: COLOR_NOT_SUPPORTED_TEXT,
            btn_text: iced::Color::from_rgb(0.85, 0.85, 0.85),
            btn_text_dim: iced::Color::from_rgb(0.5, 0.5, 0.5),
            btn_hover_text: iced::Color::WHITE,
            btn_border: iced::Color::from_rgb(0.4, 0.4, 0.4),
            btn_hover_bg: iced::Color::from_rgba(0.3, 0.3, 0.3, 0.5),
            btn_pressed_bg: iced::Color::from_rgba(0.2, 0.2, 0.2, 0.6),
            toggle_off_text: iced::Color::from_rgb(0.7, 0.7, 0.7),
            toggle_off_border: iced::Color::from_rgb(0.4, 0.4, 0.4),
            select_bg: iced::Color::from_rgb(0.23, 0.51, 0.96),
            select_text: iced::Color::WHITE,
            slider_empty: COLOR_DARK,
            slider_fill: COLOR_CURVE,
            fan_curve: COLOR_CURVE,
            temp_plot_bg: iced::Color::from_rgb(0.10, 0.10, 0.13),
            curve_plot_bg: iced::Color::from_rgb(0.12, 0.12, 0.15),
            temp_axis_text: iced::Color::from_rgb(0.5, 0.5, 0.5),
            curve_axis_text: iced::Color::from_rgb(0.6, 0.6, 0.6),
            grid_base: iced::Color::WHITE,
        },
    }
}

/// Resolves the palette for a theme preference without needing a live theme
/// value. Used where only the preference is at hand (App state, snapshots).
pub fn palette_for(id: ThemeId) -> Palette {
    palette(&id.iced())
}

/// Minimum poll interval: 200ms avoids overlapping framework_tool calls.
pub const POLL_RATE_MIN_MS: u32 = 200;
/// Minimum charge limit enforced by EC to preserve battery health.
pub const CHARGE_LIMIT_MIN: u32 = 25;
/// Maximum charge limit per EC spec.
pub const CHARGE_LIMIT_MAX: u32 = 100;

pub const FONT_SECTION: f32 = 14.0;
pub const FONT_BODY: f32 = 12.0;
pub const FONT_SMALL: f32 = 10.0;

/// Idle threshold before background polling slows to save CPU.
pub const IDLE_THRESHOLD_MS: u64 = 10_000;
/// Background poll interval when idle and not in Curve mode.
pub const IDLE_INTERVAL_MS: u64 = 2_000;
/// UI tick interval when idle.
pub const UI_IDLE_INTERVAL_MS: u64 = 1_000;
/// UI tick interval when hidden; no view rebuild, only tray liveness.
pub const UI_HIDDEN_INTERVAL_MS: u64 = 2_000;

/// Expansion card scan interval; fixed wall-clock so hotplug stays detectable.
pub const EXPANSION_SCAN_MS: u64 = 10_000;
/// Versions scan interval.
pub const VERSIONS_REFRESH_MS: u64 = 60_000;

/// Consecutive samples required to classify a port as stable (USB-A card).
pub const STABLE_THRESHOLD: usize = 2;

pub const SENSOR_COLORS: [iced::Color; 10] = [
    iced::Color {
        r: 0.23,
        g: 0.51,
        b: 0.96,
        a: 1.0,
    },
    iced::Color {
        r: 0.94,
        g: 0.27,
        b: 0.27,
        a: 1.0,
    },
    iced::Color {
        r: 0.06,
        g: 0.73,
        b: 0.51,
        a: 1.0,
    },
    iced::Color {
        r: 0.96,
        g: 0.62,
        b: 0.04,
        a: 1.0,
    },
    iced::Color {
        r: 0.55,
        g: 0.36,
        b: 0.96,
        a: 1.0,
    },
    iced::Color {
        r: 0.96,
        g: 0.35,
        b: 0.70,
        a: 1.0,
    },
    iced::Color {
        r: 0.20,
        g: 0.80,
        b: 0.80,
        a: 1.0,
    },
    iced::Color {
        r: 0.80,
        g: 0.80,
        b: 0.20,
        a: 1.0,
    },
    iced::Color {
        r: 1.00,
        g: 0.60,
        b: 0.20,
        a: 1.0,
    },
    iced::Color {
        r: 0.40,
        g: 0.70,
        b: 0.30,
        a: 1.0,
    },
];

/// Sensor data color for UI chrome (chips, dots, toggles) under a theme
/// preference. Verbatim on every theme except Light, where saturated
/// yellows/cyans wash out on white plots and are darkened to hold
/// contrast. Sensor identity (index → hue) never changes, only the Light
/// luminance. History chart lines intentionally stay verbatim via
/// `sensor_color`: sensor curves are frozen by user directive.
pub fn sensor_tint(id: ThemeId, idx: usize) -> iced::Color {
    let c = SENSOR_COLORS[idx % SENSOR_COLORS.len()];
    if id == ThemeId::Light {
        iced::Color {
            r: c.r * 0.55,
            g: c.g * 0.55,
            b: c.b * 0.55,
            a: c.a,
        }
    } else {
        c
    }
}

pub fn card<'a>(content: iced::Element<'a, crate::Message>) -> iced::Element<'a, crate::Message> {
    iced::widget::container(content)
        .padding(12)
        .width(iced::Length::Fill)
        .style(|theme| {
            let pal = palette(theme);
            iced::widget::container::Style {
                background: Some(pal.card_bg.into()),
                border: iced::Border::default()
                    .rounded(8)
                    .color(pal.card_border)
                    .width(1),
                ..Default::default()
            }
        })
        .into()
}

pub fn colored_dot<'a>(color: iced::Color, size: f32) -> iced::Element<'a, crate::Message> {
    iced::widget::container(iced::widget::text("").size(FONT_SMALL))
        .width(size)
        .height(size)
        .center_x(size)
        .center_y(size)
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
        iced::widget::text(format!("{}:", label))
            .size(FONT_BODY)
            .style(|theme| iced::widget::text::Style {
                color: Some(palette(theme).gray)
            }),
        iced::widget::text(value.to_owned()).size(FONT_BODY),
    ]
    .spacing(8)
    .into()
}

pub fn mode_style(
    theme: &iced::Theme,
    status: iced::widget::button::Status,
    selected: bool,
) -> iced::widget::button::Style {
    use iced::widget::button::Status;
    let pal = palette(theme);
    if selected {
        // Selected keeps the accent fill; hover/pressed brighten text
        // border slightly instead of going invisible.
        let hovered = matches!(status, Status::Hovered | Status::Pressed);
        iced::widget::button::Style {
            background: Some(pal.select_bg.into()),
            text_color: if hovered {
                iced::Color::WHITE
            } else {
                pal.select_text
            },
            border: iced::Border::default()
                .rounded(6)
                .width(1)
                .color(if hovered { pal.btn_text } else { pal.select_bg }),
            ..iced::widget::button::Style::default()
        }
    } else {
        let (bg, text) = match status {
            Status::Hovered => (Some(pal.btn_hover_bg.into()), pal.btn_hover_text),
            Status::Pressed => (Some(pal.btn_pressed_bg.into()), pal.btn_hover_text),
            Status::Disabled => (Some(iced::Color::TRANSPARENT.into()), pal.btn_text_dim),
            _ => (Some(iced::Color::TRANSPARENT.into()), pal.toggle_off_text),
        };
        iced::widget::button::Style {
            background: bg,
            text_color: text,
            border: iced::Border::default()
                .rounded(6)
                .width(1)
                .color(pal.toggle_off_border),
            ..iced::widget::button::Style::default()
        }
    }
}

/// Back-compat wrapper for call sites that ignore Status. Prefer
/// `mode_style(theme, status, selected)` so hover/pressed give feedback.
pub fn mode_style_selected(theme: &iced::Theme, selected: bool) -> iced::widget::button::Style {
    mode_style(theme, iced::widget::button::Status::Active, selected)
}

pub fn rule_style(theme: &iced::Theme) -> iced::widget::rule::Style {
    let pal = palette(theme);
    iced::widget::rule::Style {
        color: pal.card_border,
        radius: 0.0.into(),
        fill_mode: iced::widget::rule::FillMode::Full,
        snap: true,
    }
}

pub fn scrollable_style(
    theme: &iced::Theme,
    status: iced::widget::scrollable::Status,
) -> iced::widget::scrollable::Style {
    use iced::widget::scrollable::Status;
    let pal = palette(theme);
    // Start from the theme default (rails, gap, auto_scroll) so iced
    // upgrades don't rot this override, then pin our rail colors so
    // Black-theme scrollbars don't stay invisible on pure BLACK.
    let mut style = iced::widget::scrollable::default(theme, status);
    let rail_bg: iced::Background = pal.card_bg.into();
    let scroller_bg: iced::Background = pal.btn_border.into();
    style.vertical_rail.background = Some(rail_bg);
    style.horizontal_rail.background = Some(rail_bg);
    style.vertical_rail.scroller.background = scroller_bg;
    style.horizontal_rail.scroller.background = scroller_bg;
    match status {
        Status::Hovered {
            is_horizontal_scrollbar_hovered,
            is_vertical_scrollbar_hovered,
            ..
        } => {
            // Highlight the hovered scroller so Black-theme scrollbars
            // don't stay invisible.
            if is_horizontal_scrollbar_hovered || is_vertical_scrollbar_hovered {
                let hl: iced::Background = pal.toggle_off_border.into();
                style.vertical_rail.scroller.background = hl;
                style.horizontal_rail.scroller.background = hl;
            }
            style
        }
        _ => style,
    }
}

pub fn slider_style(
    theme: &iced::Theme,
    status: iced::widget::slider::Status,
) -> iced::widget::slider::Style {
    use iced::widget::slider::{Handle, HandleShape, Rail};
    // Matches curve control points: round handle with white ring.
    let (radius, border_width) = match status {
        iced::widget::slider::Status::Active => (5.0, 2.0),
        iced::widget::slider::Status::Hovered => (6.0, 2.0),
        iced::widget::slider::Status::Dragged => (7.0, 2.0),
    };
    let pal = palette(theme);
    iced::widget::slider::Style {
        rail: Rail {
            // Filled side follows the theme accent; Default keeps curve indigo.
            backgrounds: (pal.slider_fill.into(), pal.slider_empty.into()),
            width: 4.0,
            border: iced::Border::default().rounded(2),
        },
        handle: Handle {
            shape: HandleShape::Circle { radius },
            background: pal.slider_fill.into(),
            border_width,
            // Ring follows the theme: Default's grid_base is white, so the
            // long-standing look is unchanged; Light gets a dark ring.
            border_color: pal.grid_base,
        },
    }
}

/// Disabled fill shared by inputs and checkboxes. Normally the plot panel
/// (weakest) so it differs from both the active card (weak) and the
/// settings container (weaker). Black forces card and settings to pure
/// BLACK, where the plot lift (#0A0A0C) is nearly invisible, so Black
/// gets a one-step brighter lift (#1A1A1E) to keep disabled boxes visible.
fn disabled_bg(theme: &iced::Theme, pal: &Palette) -> iced::Color {
    if *theme == iced::Theme::Oxocarbon {
        iced::Color::from_rgb8(0x1A, 0x1A, 0x1E)
    } else {
        pal.temp_plot_bg
    }
}

/// Text input style from the app palette so inputs match cards instead of
/// iced stock. Default shifts a hair versus stock Dark (card-tinted
/// surfaces instead of theme background).
/// Hover uses the text color so it stays visible on every theme
/// (`card_border` and `toggle_off_border` both resolve to
/// `background.strong` outside Default, so a border-to-border hover would
/// be a no-op). Focus uses the select accent; focus wins over hover.
pub fn input_style(
    theme: &iced::Theme,
    status: iced::widget::text_input::Status,
) -> iced::widget::text_input::Style {
    use iced::widget::text_input::Status;
    let pal = palette(theme);
    let border = |c: iced::Color| iced::Border {
        color: c,
        width: 1.0,
        radius: 6.0.into(),
    };
    let active = iced::widget::text_input::Style {
        background: pal.card_bg.into(),
        border: border(pal.card_border),
        icon: pal.gray,
        placeholder: pal.btn_text_dim,
        value: pal.btn_text,
        selection: pal.select_bg,
    };
    match status {
        Status::Active => active,
        Status::Hovered => iced::widget::text_input::Style {
            border: border(pal.btn_text),
            ..active
        },
        // Focus wins over hover: a focused field keeps the accent border
        // even while the mouse is over it.
        Status::Focused { .. } => iced::widget::text_input::Style {
            border: border(pal.select_bg),
            ..active
        },
        Status::Disabled => iced::widget::text_input::Style {
            // Disabled uses the plot panel (weakest) so it differs from
            // both the active card (weak) and the settings container
            // (weaker), including Black where card and settings are both
            // pure BLACK (lifted to #1A1A1E so the box doesn't vanish).
            // `pal.dark` would melt into the border outside Default.
            // Selection is unreachable when disabled, so it intentionally
            // inherits the active accent via `..active`.
            background: disabled_bg(theme, &pal).into(),
            border: border(pal.card_border),
            icon: pal.toggle_off_text,
            value: pal.btn_text_dim,
            placeholder: pal.toggle_off_text,
            ..active
        },
    }
}

/// Checkbox style from the app palette; the tick box tracks the select
/// accent so it matches mode buttons instead of iced stock.
/// Note: the current call sites use label-less `checkbox(bool)`, so
/// `text_color` has no visible effect yet; it is kept for future
/// `.label()` uses. Likewise the unchecked `icon_color` never renders
/// (the tick only shows when checked) and is kept for exhaustiveness.
pub fn checkbox_style(
    theme: &iced::Theme,
    status: iced::widget::checkbox::Status,
) -> iced::widget::checkbox::Style {
    use iced::widget::checkbox::Status;
    let pal = palette(theme);
    let border = |c: iced::Color| iced::Border {
        color: c,
        width: 1.0,
        radius: 4.0.into(),
    };
    let (box_bg, box_border, icon, text) = match status {
        Status::Active { is_checked } => (
            if is_checked {
                pal.select_bg
            } else {
                pal.card_bg
            },
            if is_checked {
                pal.select_bg
            } else {
                pal.toggle_off_border
            },
            if is_checked {
                pal.select_text
            } else {
                pal.gray
            },
            Some(pal.btn_text),
        ),
        Status::Hovered { is_checked } => (
            if is_checked {
                pal.select_bg
            } else {
                pal.card_bg
            },
            if is_checked {
                pal.select_bg
            } else {
                pal.btn_text
            },
            if is_checked {
                pal.select_text
            } else {
                pal.btn_text
            },
            Some(pal.btn_text),
        ),
        Status::Disabled { is_checked } => (
            // Unchecked disabled uses the lifted plot panel so it differs
            // from both the active card and the settings container,
            // including Black where card and settings are both pure BLACK.
            // `pal.dark` equals the border color outside Default and would
            // hide the outline. Checked disabled keeps the accent fill so
            // state stays readable.
            if is_checked {
                pal.select_bg
            } else {
                disabled_bg(theme, &pal)
            },
            if is_checked {
                pal.select_bg
            } else {
                pal.toggle_off_border
            },
            if is_checked {
                pal.select_text
            } else {
                pal.toggle_off_text
            },
            Some(pal.btn_text_dim),
        ),
    };
    iced::widget::checkbox::Style {
        background: iced::Background::Color(box_bg),
        icon_color: icon,
        border: border(box_border),
        text_color: text,
    }
}

pub fn btn_style(
    theme: &iced::Theme,
    status: iced::widget::button::Status,
) -> iced::widget::button::Style {
    let pal = palette(theme);
    let (bg, text_color) = match status {
        iced::widget::button::Status::Hovered => {
            (Some(pal.btn_hover_bg.into()), pal.btn_hover_text)
        }
        iced::widget::button::Status::Pressed => {
            (Some(pal.btn_pressed_bg.into()), pal.btn_hover_text)
        }
        iced::widget::button::Status::Disabled => {
            (Some(iced::Color::TRANSPARENT.into()), pal.btn_text_dim)
        }
        _ => (Some(iced::Color::TRANSPARENT.into()), pal.btn_text),
    };
    iced::widget::button::Style {
        background: bg,
        text_color,
        border: iced::Border::default()
            .rounded(6)
            .width(1)
            .color(pal.btn_border),
        ..iced::widget::button::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_id_round_trips() {
        for id in ThemeId::ALL {
            assert_eq!(ThemeId::parse(id.label()), Some(id));
        }
        assert_eq!(ThemeId::parse("nope"), None);
        assert_eq!(ThemeId::parse("LIGHT"), Some(ThemeId::Light));
    }

    #[test]
    fn default_palette_matches_current_constants() {
        // The Default look must not drift: every slot equals the constant
        // it replaced.
        let pal = palette_for(ThemeId::Default);
        assert_eq!(pal.header, COLOR_HEADER);
        assert_eq!(pal.gray, COLOR_GRAY);
        assert_eq!(pal.green, COLOR_GREEN);
        assert_eq!(pal.card_bg, COLOR_CARD_BG);
        assert_eq!(pal.card_border, COLOR_CARD_BORDER);
        assert_eq!(pal.settings_bg, COLOR_SETTINGS_BG);
        assert_eq!(pal.dark, COLOR_DARK);
        assert_eq!(pal.not_supported_bg, COLOR_NOT_SUPPORTED_BG);
        assert_eq!(pal.not_supported_text, COLOR_NOT_SUPPORTED_TEXT);
        assert_eq!(pal.slider_empty, COLOR_DARK);
        assert_eq!(pal.select_bg, iced::Color::from_rgb(0.23, 0.51, 0.96));
        assert_eq!(pal.select_text, iced::Color::WHITE);
        assert_eq!(pal.slider_fill, COLOR_CURVE);
        assert_eq!(pal.fan_curve, COLOR_CURVE);
        assert_eq!(pal.error, iced::Color::from_rgb(0.9, 0.3, 0.3));
        assert_eq!(pal.warning, iced::Color::from_rgb(0.9, 0.6, 0.0));
        assert_eq!(pal.warning_bg, iced::Color::from_rgba(0.9, 0.6, 0.0, 0.15));
        assert_eq!(pal.btn_text, iced::Color::from_rgb(0.85, 0.85, 0.85));
        assert_eq!(pal.temp_plot_bg, iced::Color::from_rgb(0.10, 0.10, 0.13));
        assert_eq!(pal.curve_plot_bg, iced::Color::from_rgb(0.12, 0.12, 0.15));
        assert_eq!(pal.temp_axis_text, iced::Color::from_rgb(0.5, 0.5, 0.5));
        assert_eq!(pal.curve_axis_text, iced::Color::from_rgb(0.6, 0.6, 0.6));
        assert_eq!(pal.grid_base, iced::Color::WHITE);
    }

    #[test]
    fn gruvbox_uses_signature_orange_accent() {
        // iced's Gruvbox primary is aqua; the recognizable accent is orange.
        let pal = palette_for(ThemeId::GruvboxDark);
        let orange = iced::Color::from_rgb8(0xFE, 0x80, 0x19);
        assert_eq!(pal.select_bg, orange);
        assert_eq!(pal.select_text, iced::Color::BLACK);
        assert_eq!(pal.slider_fill, orange);
        assert_eq!(pal.fan_curve, orange);
        assert_eq!(pal.green, iced::Color::from_rgb8(0xB8, 0xBB, 0x26));
    }

    #[test]
    fn black_scheme_uses_true_black_surfaces() {
        // Oxocarbon alone is dark gray; Black forces pure black surfaces,
        // with plots one lift above so panels read as panels, not holes.
        let pal = palette_for(ThemeId::Black);
        let plot_lift = iced::Color::from_rgb8(0x0A, 0x0A, 0x0C);
        assert_eq!(pal.settings_bg, iced::Color::BLACK);
        assert_eq!(pal.card_bg, iced::Color::BLACK);
        assert_eq!(pal.temp_plot_bg, plot_lift);
        assert_eq!(pal.curve_plot_bg, plot_lift);
    }

    #[test]
    fn light_palette_uses_dark_text() {
        // Light background needs dark text; iced's own palette provides it.
        let pal = palette_for(ThemeId::Light);
        assert_ne!(pal.header, COLOR_HEADER);
        assert_ne!(pal.card_bg, COLOR_CARD_BG);
    }

    #[test]
    fn gray_contrasts_with_card_in_every_theme() {
        // Regression: borrowing secondary.weak.text rendered near-black on
        // dark cards. Gray must always stand clearly off the surface behind it.
        // Composite the dim alpha first: blended text leans toward the card.
        fn lum(c: iced::Color) -> f32 {
            0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
        }
        for id in ThemeId::ALL {
            if id == ThemeId::Default {
                continue;
            }
            let pal = palette_for(id);
            let eff = iced::Color {
                r: pal.gray.r * pal.gray.a + pal.card_bg.r * (1.0 - pal.gray.a),
                g: pal.gray.g * pal.gray.a + pal.card_bg.g * (1.0 - pal.gray.a),
                b: pal.gray.b * pal.gray.a + pal.card_bg.b * (1.0 - pal.gray.a),
                a: 1.0,
            };
            let gap = (lum(eff) - lum(pal.card_bg)).abs();
            assert!(
                gap > 0.3,
                "{id:?}: gray too close to card bg (gap {gap:.2})"
            );
        }
    }

    #[test]
    fn secondary_text_is_dimmed_outside_default() {
        // Subtitles sit below headers everywhere except Default, which is
        // frozen verbatim.
        for id in ThemeId::ALL {
            let pal = palette_for(id);
            if id == ThemeId::Default {
                assert_eq!(pal.gray.a, 1.0);
            } else {
                assert_eq!(pal.gray.a, 0.7, "{id:?}: gray should be dimmed");
            }
        }
    }

    #[test]
    fn axis_text_is_dimmed_outside_default() {
        // Axis labels sit below plot data like subtitles sit below headers.
        let pal = palette_for(ThemeId::Dracula);
        assert_eq!(pal.temp_axis_text.a, 0.7);
        assert_eq!(pal.curve_axis_text.a, 0.7);
    }

    #[test]
    fn sensor_colors_verbatim_except_light() {
        // Identity (index → hue) never changes; only Light luminance does.
        for id in ThemeId::ALL {
            for (idx, expected) in SENSOR_COLORS.iter().enumerate() {
                let c = sensor_tint(id, idx);
                if id == ThemeId::Light {
                    assert_ne!(c, *expected, "Light must tint sensor {idx}");
                    assert!(c.r <= expected.r);
                } else {
                    assert_eq!(c, *expected, "{id:?} must keep sensor {idx}");
                }
            }
        }
    }

    #[track_caller]
    fn bg_color(bg: &iced::Background) -> iced::Color {
        match *bg {
            iced::Background::Color(c) => c,
            _ => panic!("expected solid color background, got {bg:?}"),
        }
    }

    fn unchecked_of(status: iced::widget::checkbox::Status) -> iced::widget::checkbox::Status {
        use iced::widget::checkbox::Status;
        match status {
            Status::Active { .. } => Status::Active { is_checked: false },
            Status::Hovered { .. } => Status::Hovered { is_checked: false },
            Status::Disabled { .. } => Status::Disabled { is_checked: false },
        }
    }

    #[test]
    fn input_hover_differs_from_active_in_every_theme() {
        // Regression: card_border and toggle_off_border both resolve to
        // background.strong outside Default, so a border-to-border hover
        // would be invisible. Hover must use the text color instead.
        use iced::widget::text_input::Status;
        for id in ThemeId::ALL {
            let theme = id.iced();
            let active = input_style(&theme, Status::Active);
            let hovered = input_style(&theme, Status::Hovered);
            assert_ne!(
                hovered.border.color, active.border.color,
                "{id:?}: hover border must differ from active"
            );
        }
    }

    #[test]
    fn input_focus_uses_accent_in_every_theme() {
        use iced::widget::text_input::Status;
        for id in ThemeId::ALL {
            let theme = id.iced();
            let pal = palette(&theme);
            let active = input_style(&theme, Status::Active);
            for is_hovered in [false, true] {
                let focused = input_style(&theme, Status::Focused { is_hovered });
                assert_eq!(
                    focused.border.color, pal.select_bg,
                    "{id:?} hovered={is_hovered}: focus must use accent"
                );
                assert_ne!(
                    focused.border.color, active.border.color,
                    "{id:?} hovered={is_hovered}: focus must differ from active"
                );
            }
        }
    }

    #[test]
    fn input_disabled_keeps_visible_border() {
        // Regression: `pal.dark` equals the border color outside Default,
        // so a dark-on-dark disabled field would lose its outline.
        // Disabled uses the plot panel instead.
        use iced::widget::text_input::Status;
        for id in ThemeId::ALL {
            let theme = id.iced();
            let disabled = input_style(&theme, Status::Disabled);
            let bg = bg_color(&disabled.background);
            assert_ne!(
                bg, disabled.border.color,
                "{id:?}: disabled bg must differ from border"
            );
        }
    }

    #[test]
    fn input_disabled_differs_from_active_in_every_theme() {
        // Regression: Black forces both card_bg and settings_bg to pure
        // BLACK, so a settings_bg disabled fill would be identical to the
        // active fill. Disabled uses temp_plot_bg (weakest) instead.
        use iced::widget::text_input::Status;
        for id in ThemeId::ALL {
            let theme = id.iced();
            let active = input_style(&theme, Status::Active);
            let disabled = input_style(&theme, Status::Disabled);
            assert_ne!(
                bg_color(&disabled.background),
                bg_color(&active.background),
                "{id:?}: disabled bg must differ from active bg"
            );
        }
    }

    #[test]
    fn checkbox_checked_differs_from_unchecked() {
        use iced::widget::checkbox::Status;
        for id in ThemeId::ALL {
            let theme = id.iced();
            for checked_status in [
                Status::Active { is_checked: true },
                Status::Hovered { is_checked: true },
                Status::Disabled { is_checked: true },
            ] {
                let checked = checkbox_style(&theme, checked_status);
                let unchecked = checkbox_style(&theme, unchecked_of(checked_status));
                assert_ne!(
                    bg_color(&checked.background),
                    bg_color(&unchecked.background),
                    "{id:?} {checked_status:?}: box bg must differ"
                );
            }
        }
    }

    #[test]
    fn checkbox_unchecked_disabled_keeps_visible_border() {
        // Same dark-on-dark trap as inputs: unchecked disabled must not
        // reuse `pal.dark` as the fill.
        use iced::widget::checkbox::Status;
        for id in ThemeId::ALL {
            let theme = id.iced();
            let s = checkbox_style(&theme, Status::Disabled { is_checked: false });
            assert_ne!(
                bg_color(&s.background),
                s.border.color,
                "{id:?}: unchecked disabled bg must differ from border"
            );
        }
    }

    #[test]
    fn checkbox_unchecked_disabled_differs_from_active() {
        // Same Black collision as inputs: card_bg and settings_bg are both
        // BLACK, so disabled must use the plot lift instead.
        use iced::widget::checkbox::Status;
        for id in ThemeId::ALL {
            let theme = id.iced();
            let active = checkbox_style(&theme, Status::Active { is_checked: false });
            let disabled = checkbox_style(&theme, Status::Disabled { is_checked: false });
            assert_ne!(
                bg_color(&disabled.background),
                bg_color(&active.background),
                "{id:?}: unchecked disabled bg must differ from active bg"
            );
        }
    }
}
