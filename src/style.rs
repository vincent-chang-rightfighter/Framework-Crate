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
            // near-black on dark cards. Recompute against the card color.
            let card_bg = e.background.weak.color;
            let gray = iced::theme::palette::Pair::new(card_bg, p.text).text;
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
                temp_plot_bg: e.background.weakest.color,
                curve_plot_bg: e.background.weakest.color,
                temp_axis_text: p.text,
                curve_axis_text: p.text,
                grid_base: p.text,
            };
            // Black goes true-black on the large surfaces; controls keep
            // Oxocarbon grays so they stay visible, and gray is recomputed
            // against the black card.
            if *theme == iced::Theme::Oxocarbon {
                pal.settings_bg = iced::Color::BLACK;
                pal.card_bg = iced::Color::BLACK;
                pal.temp_plot_bg = iced::Color::BLACK;
                pal.curve_plot_bg = iced::Color::BLACK;
                pal.gray = iced::theme::palette::Pair::new(iced::Color::BLACK, p.text).text;
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

pub fn mode_style(theme: &iced::Theme, selected: bool) -> iced::widget::button::Style {
    let pal = palette(theme);
    if selected {
        iced::widget::button::Style {
            background: Some(pal.select_bg.into()),
            text_color: pal.select_text,
            border: iced::Border::default()
                .rounded(6)
                .width(1)
                .color(pal.select_bg),
            ..iced::widget::button::Style::default()
        }
    } else {
        iced::widget::button::Style {
            background: Some(iced::Color::TRANSPARENT.into()),
            text_color: pal.toggle_off_text,
            border: iced::Border::default()
                .rounded(6)
                .width(1)
                .color(pal.toggle_off_border),
            ..iced::widget::button::Style::default()
        }
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
            border_color: iced::Color::WHITE,
        },
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
    fn black_scheme_uses_true_black_surfaces() {
        // Oxocarbon alone is dark gray; Black forces pure black surfaces.
        let pal = palette_for(ThemeId::Black);
        assert_eq!(pal.settings_bg, iced::Color::BLACK);
        assert_eq!(pal.card_bg, iced::Color::BLACK);
        assert_eq!(pal.temp_plot_bg, iced::Color::BLACK);
        assert_eq!(pal.curve_plot_bg, iced::Color::BLACK);
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
        fn lum(c: iced::Color) -> f32 {
            0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
        }
        for id in ThemeId::ALL {
            if id == ThemeId::Default {
                continue;
            }
            let pal = palette_for(id);
            let gap = (lum(pal.gray) - lum(pal.card_bg)).abs();
            assert!(
                gap > 0.3,
                "{id:?}: gray too close to card bg (gap {gap:.2})"
            );
        }
    }
}
