use iced::{Color, Element, Length, Point, Size};
use std::cell::{Cell, OnceCell};
use std::collections::BTreeMap;
use std::sync::Arc;

const TEMP_MAX: f32 = 110.0;
const TEMP_MIN: f32 = 0.0;
/// Default history window in seconds.
pub const HISTORY_SECONDS: i64 = 30;
/// Selectable history window lengths.
pub const HISTORY_WINDOW_OPTIONS: [i64; 3] = [15, 30, 60];
/// Buffer retention; keeps longest window to avoid gaps on switch.
pub const HISTORY_MAX_MS: i64 = 300_000;
const Y_LABELS: [&str; 7] = ["0", "20", "40", "60", "80", "100", "110°C"];

#[derive(Clone)]
pub struct TempSample {
    pub ts_ms: i64,
    pub temps: std::sync::Arc<BTreeMap<String, i32>>,
}

/// Double-buffer publish interval; 1s lag is invisible on a 30s window.
pub const HISTORY_PUBLISH_MS: i64 = 1_000;

/// Double-buffered history: writer mutates `draft` in place, readers get throttled `Arc` snapshot.
#[derive(Clone)]
pub struct ThermalHistory {
    draft: std::collections::VecDeque<TempSample>,
    published: Arc<std::collections::VecDeque<TempSample>>,
    last_publish_ms: i64,
    window_ms: i64,
    /// Track whether draft changed since last publish to avoid unnecessary clone.
    draft_dirty: bool,
}

impl Default for ThermalHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl ThermalHistory {
    pub fn new() -> Self {
        let mut draft = std::collections::VecDeque::new();
        // Pre-reserve for max window (~300 samples) to avoid reallocation.
        draft.reserve(350);
        Self {
            draft,
            published: Arc::new(std::collections::VecDeque::new()),
            last_publish_ms: 0,
            window_ms: HISTORY_SECONDS * 1_000,
            draft_dirty: false,
        }
    }

    /// Update retention window and prune old samples.
    pub fn set_window(&mut self, window_seconds: i64) {
        self.window_ms = (window_seconds * 1_000).clamp(5_000, HISTORY_MAX_MS);
        // Retain up to max window so switching from 30s to 60s shows history.
        let now = crate::util::monotonic_ms() as i64;
        let cutoff = now - HISTORY_MAX_MS;
        while let Some(front) = self.draft.front() {
            if front.ts_ms < cutoff {
                self.draft.pop_front();
                self.draft_dirty = true;
            } else {
                break;
            }
        }
        // Force next snapshot to republish so the new window is reflected
        // immediately without waiting for the next push_sample.
        self.last_publish_ms = 0;
    }

    /// Push sample and prune entries outside retention (max window) to allow window switches.
    pub fn push_sample(&mut self, sample: TempSample, now_ms: i64) {
        self.draft.push_back(sample);
        self.draft_dirty = true;
        let cutoff = now_ms - HISTORY_MAX_MS;
        while let Some(front) = self.draft.front() {
            if front.ts_ms < cutoff {
                self.draft.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn last_timestamp(&self) -> Option<i64> {
        self.draft.back().map(|s| s.ts_ms)
    }

    /// Return `Arc` snapshot, re-publishing at most once per interval and only when draft changed.
    pub fn snapshot(&mut self, now_ms: i64) -> Arc<std::collections::VecDeque<TempSample>> {
        if self.draft_dirty
            && (self.last_publish_ms == 0 || now_ms - self.last_publish_ms >= HISTORY_PUBLISH_MS)
        {
            self.published = Arc::new(self.draft.clone());
            self.last_publish_ms = now_ms;
            self.draft_dirty = false;
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

/// State in widget Tree; survives `view()` rebuilds.
struct TempChartState {
    cache: OnceCell<iced::widget::canvas::Cache<iced::Renderer>>,
    /// SAFETY: `Arc` pointer is stable; key changes only when new data arrives.
    cached_key: Cell<(*const (), usize, *const (), *const (), i64, i64)>,
    /// Reused line-point buffer to avoid per-frame allocation.
    points_buf: std::cell::RefCell<Vec<(f32, f32)>>,
    last_theme: std::cell::RefCell<Option<String>>,
}

impl Default for TempChartState {
    fn default() -> Self {
        Self {
            cache: OnceCell::new(),
            cached_key: Cell::new((
                std::ptr::null::<()>(),
                0,
                std::ptr::null::<()>(),
                std::ptr::null::<()>(),
                0,
                0,
            )),
            last_theme: std::cell::RefCell::new(None),
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
        // include last timestamp to avoid ptr-reuse false hit; additionally hash sensor_names length
        let last_ts = self.samples.back().map(|s| s.ts_ms).unwrap_or(0);
        let key = (
            Arc::as_ptr(&self.samples) as *const (),
            self.samples.len(),
            Arc::as_ptr(&self.sensor_names) as *const (),
            Arc::as_ptr(&self.colors) as *const (),
            self.window_seconds,
            last_ts,
        );
        // theme change should invalidate cache (previously ignored)
        let theme_str = format!("{:?}", _theme);
        let theme_changed = state.last_theme.borrow().as_deref() != Some(&theme_str);
        if state.cached_key.get() != key || theme_changed {
            state.cached_key.set(key);
            *state.last_theme.borrow_mut() = Some(theme_str);
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
    // Top margin keeps "110" label visible.
    let margin_top = 10.0f32;
    let margin_bottom = 18.0f32;
    let plot_w = (size.width - margin_left - margin_right).max(1.0);
    let plot_h = (size.height - margin_top - margin_bottom).max(1.0);
    let origin = Point::new(margin_left, margin_top);

    frame.fill_rectangle(
        origin,
        Size::new(plot_w, plot_h),
        Color::from_rgb(0.10, 0.10, 0.13),
    );

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

    frame.stroke_rectangle(
        origin,
        Size::new(plot_w, plot_h),
        iced::widget::canvas::Stroke::default()
            .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.2))
            .with_width(1.0),
    );

    // Y labels left-aligned so they share a common edge.
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

    // X-axis: newest at right ("now"); labels run from window length inward.
    let step = (window_seconds / 3).max(5);
    // Skip boundary label to avoid clipping at left edge.
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
    // Right-aligned "now" at right edge.
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

    for (sensor_idx, sensor_name) in sensor_names.iter().enumerate() {
        let color = if colors.is_empty() {
            Color::WHITE
        } else {
            colors[sensor_idx % colors.len()]
        };

        points_buf.clear();
        points_buf.extend(
            samples
                .iter()
                .filter(|s| s.ts_ms >= start_ms)
                .filter_map(|s| {
                    let temp = *s.temps.get(sensor_name)? as f32;
                    let t_ratio = (s.ts_ms - start_ms) as f32 / (now_ms - start_ms).max(1) as f32;
                    let clamped = temp.clamp(TEMP_MIN, TEMP_MAX);
                    Some((t_ratio, clamped))
                }),
        );

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
            // Single sample: draw dot so first reading is visible.
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
        let now = 500_000i64;
        h.push_sample(sample(now - 400_000), now);
        h.push_sample(sample(now - 20_000), now);
        h.push_sample(sample(now - 10_000), now);
        h.push_sample(sample(now), now);
        assert_eq!(
            h.draft.len(),
            3,
            "expired entry beyond 300s retention should be pruned"
        );
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
        assert!(
            !Arc::ptr_eq(&snap1, &snap2),
            "should republish after interval"
        );
        assert_eq!(snap2.len(), 2);
    }

    #[test]
    fn set_window_retains_up_to_300s() {
        let mut h = ThermalHistory::new();
        let now = 500_000i64;
        // Fill with samples spanning 200s (<300s retention)
        h.push_sample(sample(now - 200_000), now - 200_000);
        h.push_sample(sample(now - 100_000), now - 100_000);
        h.push_sample(sample(now), now);
        // Switch window from 30s to 60s should not prune 200s-old entry (within 300s)
        h.set_window(60);
        assert_eq!(h.draft.len(), 3);
        // Push with far future prunes beyond 300s
        h.push_sample(sample(now + 150_000), now + 150_000);
        // Oldest (now-200k) is now 350k old relative to new now, should be pruned
        assert_eq!(h.draft.len(), 3);
    }

    #[test]
    fn set_window_clamps_and_prunes() {
        let mut h = ThermalHistory::new();
        let now = 500_000i64;
        h.push_sample(sample(now), now);
        h.set_window(15);
        assert_eq!(h.window_ms, 15_000);
        h.set_window(60);
        assert_eq!(h.window_ms, 60_000);
        // Very large window clamps to 300s
        h.set_window(1000);
        assert_eq!(h.window_ms, HISTORY_MAX_MS);
    }
}
