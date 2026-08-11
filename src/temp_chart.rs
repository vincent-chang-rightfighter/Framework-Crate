use iced::{Color, Element, Length, Point, Size};
use std::cell::{Cell, OnceCell};
use std::collections::BTreeMap;
use std::sync::Arc;

const TEMP_MAX: f32 = 100.0;
const TEMP_MIN: f32 = 0.0;
pub const HISTORY_SECONDS: f32 = 30.0;
pub const HISTORY_MS: i64 = 30_000;
const Y_LABELS: [&str; 6] = ["0", "20", "40", "60", "80", "100"];
const X_LABELS: [&str; 4] = ["0s", "10s", "20s", "30s"];

#[derive(Clone)]
pub struct TempSample {
    pub ts_ms: i64,
    pub temps: std::sync::Arc<BTreeMap<String, i32>>,
}

pub struct TempHistory {
    pub samples: Arc<std::collections::VecDeque<TempSample>>,
    pub colors: Arc<Vec<Color>>,
    pub sensor_names: Arc<Vec<String>>,
}

pub fn view_temp_chart(history: TempHistory) -> Element<'static, crate::Message> {
    iced::widget::canvas(TempChartRenderer {
        samples: history.samples,
        colors: history.colors,
        sensor_names: history.sensor_names,
    })
    .width(Length::Fill)
    .height(140)
    .into()
}

struct TempChartRenderer {
    samples: Arc<std::collections::VecDeque<TempSample>>,
    colors: Arc<Vec<Color>>,
    sensor_names: Arc<Vec<String>>,
}

/// Persistent state living in the widget `Tree` — survives `view()` rebuilds.
struct TempChartState {
    cache: OnceCell<iced::widget::canvas::Cache<iced::Renderer>>,
    cached_key: Cell<(*const (), usize, *const ())>,
    /// Reused line-point buffer, kept in the tree so it is not re-allocated
    /// on every `view()` rebuild.
    points_buf: std::cell::RefCell<Vec<(f32, f32)>>,
}

impl Default for TempChartState {
    fn default() -> Self {
        Self {
            cache: OnceCell::new(),
            cached_key: Cell::new((std::ptr::null::<()>(), 0, std::ptr::null::<()>())),
            points_buf: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl iced::widget::canvas::Program<crate::Message> for TempChartRenderer {
    type State = TempChartState;

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &iced::Event,
        _bounds: iced::Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Option<iced::widget::canvas::Action<crate::Message>> {
        None
    }

    fn draw(
        &self,
        state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: iced::Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry> {
        let size = bounds.size();
        let key = (
            Arc::as_ptr(&self.samples) as *const (),
            self.samples.len(),
            Arc::as_ptr(&self.sensor_names) as *const (),
        );
        if state.cached_key.get() != key {
            state.cached_key.set(key);
            if let Some(cache) = state.cache.get() {
                cache.clear();
            }
        }

        let cache = state.cache.get_or_init(iced::widget::canvas::Cache::new);
        let geo = cache.draw(renderer, size, |frame| {
            let mut points_buf = state.points_buf.borrow_mut();
            draw_temp_chart_contents(
                frame,
                &self.samples,
                &self.sensor_names,
                &self.colors,
                &mut points_buf,
                size,
            );
        });
        vec![geo]
    }
}

fn draw_temp_chart_contents(
    frame: &mut iced::widget::canvas::Frame<iced::Renderer>,
    samples: &Arc<std::collections::VecDeque<TempSample>>,
    sensor_names: &Arc<Vec<String>>,
    colors: &Arc<Vec<Color>>,
    points_buf: &mut Vec<(f32, f32)>,
    size: Size,
) {
        let margin_left = 30.0f32;
        let margin_right = 8.0f32;
        let margin_top = 4.0f32;
        let margin_bottom = 18.0f32;
        let plot_w = size.width - margin_left - margin_right;
        let plot_h = size.height - margin_top - margin_bottom;
        let origin = Point::new(margin_left, margin_top);

        // Background
        frame.fill_rectangle(
            origin,
            Size::new(plot_w, plot_h),
            Color::from_rgb(0.10, 0.10, 0.13),
        );

        // Grid lines (horizontal for temp levels)
        let grid_stroke = iced::widget::canvas::Stroke::default()
            .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.08))
            .with_width(0.5);
        for temp in [20.0, 40.0, 60.0, 80.0] {
            let y = origin.y + plot_h - ((temp - TEMP_MIN) / (TEMP_MAX - TEMP_MIN)) * plot_h;
            frame.stroke(
                &iced::widget::canvas::Path::line(
                    Point::new(origin.x, y),
                    Point::new(origin.x + plot_w, y),
                ),
                grid_stroke,
            );
        }

        // Border
        frame.stroke_rectangle(
            origin,
            Size::new(plot_w, plot_h),
            iced::widget::canvas::Stroke::default()
                .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.2))
                .with_width(1.0),
        );

        // Y-axis labels
        let font = iced::Font::with_name("Consolas");
        for (i, temp) in [0, 20, 40, 60, 80, 100].iter().enumerate() {
            let y = origin.y + plot_h - ((*temp as f32 - TEMP_MIN) / (TEMP_MAX - TEMP_MIN)) * plot_h;
            frame.fill_text(iced::widget::canvas::Text {
                content: Y_LABELS[i].to_owned(),
                position: Point::new(origin.x - 4.0, y),
                color: Color::from_rgb(0.5, 0.5, 0.5),
                size: iced::Pixels(8.0),
                font,
                align_x: iced::alignment::Horizontal::Right.into(),
                align_y: iced::alignment::Vertical::Center,
                line_height: iced::widget::text::LineHeight::default(),
                shaping: iced::widget::text::Shaping::Basic,
                max_width: f32::INFINITY,
            });
        }

        // X-axis time labels (0s, 10s, 20s, 30s)
        for (i, sec) in [0, 10, 20, 30].iter().enumerate() {
            let x = origin.x + (*sec as f32 / HISTORY_SECONDS) * plot_w;
            frame.fill_text(iced::widget::canvas::Text {
                content: X_LABELS[i].to_owned(),
                position: Point::new(x, origin.y + plot_h + 4.0),
                color: Color::from_rgb(0.5, 0.5, 0.5),
                size: iced::Pixels(8.0),
                font,
                align_x: iced::alignment::Horizontal::Center.into(),
                align_y: iced::alignment::Vertical::Top,
                line_height: iced::widget::text::LineHeight::default(),
                shaping: iced::widget::text::Shaping::Basic,
                max_width: f32::INFINITY,
            });
        }

        if samples.is_empty() || sensor_names.is_empty() {
            frame.fill_text(iced::widget::canvas::Text {
                content: "Waiting for data...".to_string(),
                position: Point::new(origin.x + plot_w / 2.0, origin.y + plot_h / 2.0),
                color: Color::from_rgb(0.4, 0.4, 0.4),
                size: iced::Pixels(11.0),
                font,
                align_x: iced::alignment::Horizontal::Center.into(),
                align_y: iced::alignment::Vertical::Center,
                line_height: iced::widget::text::LineHeight::default(),
                shaping: iced::widget::text::Shaping::Basic,
                max_width: f32::INFINITY,
            });
            return;
        }

        let now_ms = samples.back().map(|s| s.ts_ms).unwrap_or(0);
        let start_ms = now_ms - HISTORY_MS;

        // Draw lines per sensor
        for (sensor_idx, sensor_name) in sensor_names.iter().enumerate() {
            let color = if colors.is_empty() {
                Color::WHITE
            } else {
                colors[sensor_idx % colors.len()]
            };

            points_buf.clear();
            points_buf.extend(samples.iter()
                .filter(|s| s.ts_ms >= start_ms)
                .filter_map(|s| {
                    let temp = *s.temps.get(sensor_name)? as f32;
                    let t_ratio = (s.ts_ms - start_ms) as f32 / (now_ms - start_ms).max(1) as f32;
                    let clamped = temp.clamp(TEMP_MIN, TEMP_MAX);
                    Some((t_ratio, clamped))
                }));

            if points_buf.len() >= 2 {
                let path = iced::widget::canvas::Path::new(|b| {
                    let first = &points_buf[0];
                    b.move_to(Point::new(
                        origin.x + first.0 * plot_w,
                        origin.y + plot_h - ((first.1 - TEMP_MIN) / (TEMP_MAX - TEMP_MIN)) * plot_h,
                    ));
                    for pt in points_buf.iter().skip(1) {
                        b.line_to(Point::new(
                            origin.x + pt.0 * plot_w,
                            origin.y + plot_h - ((pt.1 - TEMP_MIN) / (TEMP_MAX - TEMP_MIN)) * plot_h,
                        ));
                    }
                });
                frame.stroke(
                    &path,
                    iced::widget::canvas::Stroke::default()
                        .with_color(color)
                        .with_width(1.5),
                );
            }
        }
}
