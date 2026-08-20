use iced::{Color, Element, Length, Point, Size};
use std::cell::{Cell, OnceCell};
use std::collections::BTreeMap;
use std::sync::Arc;

const TEMP_MAX: f32 = 110.0;
const TEMP_MIN: f32 = 0.0;
/// Default history window in seconds (used for the x-axis scale).
pub const HISTORY_SECONDS: i64 = 30;
/// Selectable history window lengths in seconds.
pub const HISTORY_WINDOW_OPTIONS: [i64; 3] = [15, 30, 60];
/// Buffer retention in milliseconds: keep the longest selectable window so
/// switching windows never shows a gap.
pub const HISTORY_MAX_MS: i64 = 60_000;
const Y_LABELS: [&str; 7] = ["0", "20", "40", "60", "80", "100", "110°C"];

#[derive(Clone)]
pub struct TempSample {
    pub ts_ms: i64,
    pub temps: std::sync::Arc<BTreeMap<String, i32>>,
}

/// How often the double-buffer `published` snapshot is refreshed (ms). The
/// chart shows a 30s window, so a 1s publishing lag is invisible while it
/// cuts full-deque clones from "every changed poll" to 1/s.
pub const HISTORY_PUBLISH_MS: i64 = 1_000;

/// Double-buffered temperature history.
///
/// The background writer mutates `draft` in place (push + prune, no
/// allocation beyond VecDeque growth). Readers receive an `Arc` snapshot
/// that is re-published at most once per `HISTORY_PUBLISH_MS`, so the
/// per-sample full-history clone is eliminated.
#[derive(Clone)]
pub struct ThermalHistory {
    draft: std::collections::VecDeque<TempSample>,
    published: Arc<std::collections::VecDeque<TempSample>>,
    last_publish_ms: i64,
}

impl Default for ThermalHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl ThermalHistory {
    pub fn new() -> Self {
        Self {
            draft: std::collections::VecDeque::new(),
            published: Arc::new(std::collections::VecDeque::new()),
            last_publish_ms: 0,
        }
    }

    /// Writer side: push a sample and drop entries older than the window.
    /// Caller must hold the write lock.
    pub fn push_sample(&mut self, sample: TempSample, now_ms: i64) {
        self.draft.push_back(sample);
        let cutoff = now_ms - HISTORY_MAX_MS;
        while let Some(front) = self.draft.front() {
            if front.ts_ms <= cutoff {
                self.draft.pop_front();
            } else {
                break;
            }
        }
    }

    /// Reader side: return an `Arc` snapshot of the history, re-publishing
    /// the draft at most once per `HISTORY_PUBLISH_MS`. Cheap when the
    /// interval has not elapsed (just an Arc refcount increment).
    pub fn snapshot(&mut self, now_ms: i64) -> Arc<std::collections::VecDeque<TempSample>> {
        if self.last_publish_ms == 0 || now_ms - self.last_publish_ms >= HISTORY_PUBLISH_MS {
            self.published = Arc::new(self.draft.clone());
            self.last_publish_ms = now_ms;
        }
        Arc::clone(&self.published)
    }
}

pub struct TempHistory {
    pub samples: Arc<std::collections::VecDeque<TempSample>>,
    pub colors: Arc<Vec<Color>>,
    pub sensor_names: Arc<Vec<String>>,
    /// Length of the displayed window in seconds.
    pub window_seconds: i64,
}

pub fn view_temp_chart(history: TempHistory) -> Element<'static, crate::Message> {
    iced::widget::canvas(TempChartRenderer {
        samples: history.samples,
        colors: history.colors,
        sensor_names: history.sensor_names,
        window_seconds: history.window_seconds,
    })
    .width(Length::Fill)
    .height(140)
    .into()
}

struct TempChartRenderer {
    samples: Arc<std::collections::VecDeque<TempSample>>,
    colors: Arc<Vec<Color>>,
    sensor_names: Arc<Vec<String>>,
    window_seconds: i64,
}

/// Persistent state living in the widget `Tree` — survives `view()` rebuilds.
struct TempChartState {
    cache: OnceCell<iced::widget::canvas::Cache<iced::Renderer>>,
    /// Cache invalidation key: (samples Arc ptr, samples len, sensor_names Arc ptr).
    ///
    /// SAFETY: `Arc` never reallocates its backing allocation, so the base
    /// pointer is stable for the lifetime of the allocation. Two `Arc`s
    /// wrapping the same data share the same pointer. The key changes only
    /// when `ViewSnapshot` clones a new `Arc` (i.e. new data arrived), which
    /// is exactly when the canvas needs re-drawing. This avoids hashing or
    /// deep-comparing the entire sample deque on every frame.
    cached_key: Cell<(*const (), usize, *const (), i64)>,
    /// Reused line-point buffer, kept in the tree so it is not re-allocated
    /// on every `view()` rebuild.
    points_buf: std::cell::RefCell<Vec<(f32, f32)>>,
}

impl Default for TempChartState {
    fn default() -> Self {
        Self {
            cache: OnceCell::new(),
            cached_key: Cell::new((std::ptr::null::<()>(), 0, std::ptr::null::<()>(), 0)),
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
            self.window_seconds,
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
                self.window_seconds,
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
    window_seconds: i64,
    points_buf: &mut Vec<(f32, f32)>,
    size: Size,
) {
    let margin_left = 36.0f32;
    let margin_right = 8.0f32;
    // Top margin tall enough for the "110" label at the top edge.
    let margin_top = 10.0f32;
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
    for temp in [20.0, 40.0, 60.0, 80.0, 100.0] {
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

    // Y-axis labels, left-aligned at the gutter edge so every label starts
    // on the same line.
    let font = iced::Font::with_name("Consolas");
    for (i, temp) in [0, 20, 40, 60, 80, 100, 110].iter().enumerate() {
        let y = origin.y + plot_h - ((*temp as f32 - TEMP_MIN) / (TEMP_MAX - TEMP_MIN)) * plot_h;
        frame.fill_text(iced::widget::canvas::Text {
            content: Y_LABELS[i].to_owned(),
            position: Point::new(2.0, y),
            color: Color::from_rgb(0.5, 0.5, 0.5),
            size: iced::Pixels(8.0),
            font,
            align_x: iced::alignment::Horizontal::Left.into(),
            align_y: iced::alignment::Vertical::Center,
            line_height: iced::widget::text::LineHeight::default(),
            shaping: iced::widget::text::Shaping::Basic,
            max_width: f32::INFINITY,
        });
    }

    // X-axis time labels. Samples are plotted newest-first: t_ratio = 1.0
    // (the right edge) is "now", so the labels must run from the window
    // length at the left down to "now" at the right. The "0" coordinate is
    // not labeled — "now" takes its place.
    let step = (window_seconds / 3).max(5);
    // Skip the window-boundary label (e.g. "30s" on a 30s window): it would
    // sit on the plot's left edge. Only interior ticks + "now" are shown.
    let mut secs = window_seconds - step;
    while secs > 0 {
        let x = origin.x + (1.0 - secs as f32 / window_seconds as f32) * plot_w;
        frame.fill_text(iced::widget::canvas::Text {
            content: format!("{}s", secs),
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
        secs -= step;
    }
    // "now" at the right edge, right-aligned so it is never clipped.
    frame.fill_text(iced::widget::canvas::Text {
        content: "now".to_string(),
        position: Point::new(origin.x + plot_w, origin.y + plot_h + 4.0),
        color: Color::from_rgb(0.5, 0.5, 0.5),
        size: iced::Pixels(8.0),
        font,
        align_x: iced::alignment::Horizontal::Right.into(),
        align_y: iced::alignment::Vertical::Top,
        line_height: iced::widget::text::LineHeight::default(),
        shaping: iced::widget::text::Shaping::Basic,
        max_width: f32::INFINITY,
    });

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
    let start_ms = now_ms - window_seconds * 1_000;

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
        } else if points_buf.len() == 1 {
            // A single sample cannot form a line; draw a dot so the first
            // reading is visible immediately.
            let pt = &points_buf[0];
            let center = Point::new(
                origin.x + pt.0 * plot_w,
                origin.y + plot_h - ((pt.1 - TEMP_MIN) / (TEMP_MAX - TEMP_MIN)) * plot_h,
            );
            frame.fill(&iced::widget::canvas::Path::circle(center, 2.0), color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64) -> TempSample {
        TempSample {
            ts_ms: ts,
            temps: Arc::new(BTreeMap::new()),
        }
    }

    #[test]
    fn history_new_is_empty() {
        let h = ThermalHistory::new();
        assert!(h.draft.is_empty());
        assert!(h.published.is_empty());
        assert_eq!(h.last_publish_ms, 0);
    }

    #[test]
    fn push_sample_prunes_expired_entries() {
        let mut h = ThermalHistory::new();
        let now = 100_000i64;
        // Old sample far outside the 30s window
        h.push_sample(sample(now - 100_000), now);
        // Recent samples
        h.push_sample(sample(now - 20_000), now);
        h.push_sample(sample(now - 10_000), now);
        h.push_sample(sample(now), now);
        assert_eq!(h.draft.len(), 3, "expired entry should be pruned");
        assert_eq!(h.draft.front().unwrap().ts_ms, now - 20_000);
    }

    #[test]
    fn snapshot_publishes_immediately_on_first_call() {
        let mut h = ThermalHistory::new();
        let now = 100_000i64;
        h.push_sample(sample(now), now);
        let snap = h.snapshot(now);
        assert_eq!(snap.len(), 1);
        assert!(Arc::ptr_eq(&snap, &h.published));
    }

    #[test]
    fn snapshot_reuses_arc_within_publish_interval() {
        let mut h = ThermalHistory::new();
        let now = 100_000i64;
        h.push_sample(sample(now), now);
        let snap1 = h.snapshot(now);
        h.push_sample(sample(now + 200), now + 200);
        // Still within the 1s publish interval
        let snap2 = h.snapshot(now + 900);
        assert!(Arc::ptr_eq(&snap1, &snap2), "no republish within interval");
        assert_eq!(snap2.len(), 1);
    }

    #[test]
    fn snapshot_republishes_after_interval() {
        let mut h = ThermalHistory::new();
        let now = 100_000i64;
        h.push_sample(sample(now), now);
        let snap1 = h.snapshot(now);
        h.push_sample(sample(now + 1_500), now + 1_500);
        let snap2 = h.snapshot(now + 1_500);
        assert!(!Arc::ptr_eq(&snap1, &snap2), "should republish after interval");
        assert_eq!(snap2.len(), 2);
    }
}
