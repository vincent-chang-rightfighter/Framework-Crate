use std::cell::{Cell, OnceCell};
use std::sync::Arc;
use iced::{Color, Element, Length, Point, Size};
use iced::widget::canvas::Cache;

const AXIS_LABELS: [&str; 6] = ["0", "20", "40", "60", "80", "100"];

pub fn view_curve(points: &[[u32; 2]], all_pts: &Arc<Vec<[u32; 2]>>) -> Element<'static, crate::Message> {
    let points_arc: Arc<[[u32; 2]]> = Arc::from(points);
    iced::widget::canvas(CurveRenderer {
        all_pts: Arc::clone(all_pts),
        points: points_arc,
    })
    .width(Length::Fill)
    .height(180)
    .into()
}

struct CurveRenderer {
    all_pts: Arc<Vec<[u32; 2]>>,
    points: Arc<[[u32; 2]]>,
}

/// Persistent state living in the widget `Tree` — survives `view()` rebuilds.
struct CurveState {
    cache: OnceCell<Cache<iced::Renderer>>,
    cached_key: Cell<(*const (), usize)>,
    last_points: std::cell::RefCell<Option<Arc<[[u32; 2]]>>>,
}

impl Default for CurveState {
    fn default() -> Self {
        Self {
            cache: OnceCell::new(),
            cached_key: Cell::new((std::ptr::null::<()>(), 0)),
            last_points: std::cell::RefCell::new(None),
        }
    }
}

impl iced::widget::canvas::Program<crate::Message> for CurveRenderer {
    type State = CurveState;

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
            Arc::as_ptr(&self.all_pts) as *const (),
            self.all_pts.len(),
        );
        // Always compare points CONTENT, not just pointers: a rebuilt
        // snapshot's Vec can be allocated at the same address as the
        // previous one (same-size allocation, just freed), so an address
        // key alone would miss the change and draw a stale curve. The
        // vectors hold at most a handful of points, so the per-frame
        // comparison is negligible.
        let points_changed = state.last_points.borrow().as_deref() != Some(self.points.as_ref());
        if state.cached_key.get() != key || points_changed {
            state.cached_key.set(key);
            if points_changed {
                *state.last_points.borrow_mut() = Some(Arc::clone(&self.points));
            }
            if let Some(cache) = state.cache.get() {
                cache.clear();
            }
        }

        let cache = state.cache.get_or_init(Cache::new);
        let geo = cache.draw(renderer, size, |frame| {
            draw_curve_contents(
                frame,
                &self.all_pts,
                &self.points,
                size,
            );
        });
        vec![geo]
    }
}

fn draw_curve_contents(
    frame: &mut iced::widget::canvas::Frame<iced::Renderer>,
    all_pts: &Arc<Vec<[u32; 2]>>,
    points: &Arc<[[u32; 2]]>,
    size: Size,
) {
    if all_pts.is_empty() {
        return;
    }

    let margin = 5.0f32;
    let plot_w = size.width - margin * 2.0;
    let plot_h = size.height - margin * 2.0 - 14.0;
    let origin = Point::new(margin, margin);

    let to_screen = |x: f32, y: f32| -> Point {
        Point::new(
            origin.x + (x / 100.0) * plot_w,
            origin.y + plot_h - (y / 100.0) * plot_h,
        )
    };

    frame.fill_rectangle(origin, Size::new(plot_w, plot_h), Color::from_rgb(0.12, 0.12, 0.15));

    let grid_stroke = iced::widget::canvas::Stroke::default()
        .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.1))
        .with_width(0.5);
    for v in [20.0, 40.0, 60.0, 80.0] {
        frame.stroke(&iced::widget::canvas::Path::line(to_screen(v, 0.0), to_screen(v, 100.0)), grid_stroke);
        frame.stroke(&iced::widget::canvas::Path::line(to_screen(0.0, v), to_screen(100.0, v)), grid_stroke);
    }

    frame.stroke_rectangle(origin, Size::new(plot_w, plot_h),
        iced::widget::canvas::Stroke::default().with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.3)).with_width(1.0));

    let fill_path = iced::widget::canvas::Path::new(|b| {
        b.move_to(to_screen(0.0, 0.0));
        for p in all_pts.iter() { b.line_to(to_screen(p[0] as f32, p[1] as f32)); }
        b.line_to(to_screen(100.0, 0.0));
        b.close();
    });
    frame.fill(&fill_path, Color::from_rgba(0.23, 0.51, 0.96, 0.15));

    let curve_path = iced::widget::canvas::Path::new(|b| {
        b.move_to(to_screen(all_pts[0][0] as f32, all_pts[0][1] as f32));
        for p in all_pts.iter().skip(1) { b.line_to(to_screen(p[0] as f32, p[1] as f32)); }
    });
    frame.stroke(&curve_path, iced::widget::canvas::Stroke::default()
        .with_color(Color::from_rgb(0.23, 0.51, 0.96)).with_width(2.0));

    let font = iced::Font::with_name("Consolas");
    for (i, v) in [0u32, 20, 40, 60, 80, 100].iter().enumerate() {
        let x = origin.x + (*v as f32 / 100.0) * plot_w;
        frame.fill_text(iced::widget::canvas::Text {
            content: AXIS_LABELS[i].to_owned(),
            position: Point::new(x, origin.y + plot_h + 4.0),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: iced::Pixels(9.0), font,
            align_x: iced::alignment::Horizontal::Center.into(),
            align_y: iced::alignment::Vertical::Top,
            line_height: iced::widget::text::LineHeight::default(),
            shaping: iced::widget::text::Shaping::Basic,
            max_width: f32::INFINITY,
        });
        let y = origin.y + plot_h - (*v as f32 / 100.0) * plot_h;
        frame.fill_text(iced::widget::canvas::Text {
            content: AXIS_LABELS[i].to_owned(),
            position: Point::new(origin.x - 4.0, y),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: iced::Pixels(9.0), font,
            align_x: iced::alignment::Horizontal::Right.into(),
            align_y: iced::alignment::Vertical::Center,
            line_height: iced::widget::text::LineHeight::default(),
            shaping: iced::widget::text::Shaping::Basic,
            max_width: f32::INFINITY,
        });
    }

    // Draw the editor squares in temperature order, matching the sorted
    // curve — the config keeps the user's entry order, and drawing squares
    // in that order would make P0/P1 appear to swap after the user drags
    // one past the other.
    let mut sorted_points: Vec<[u32; 2]> = points.to_vec();
    sorted_points.sort_by_key(|p| p[0]);
    for p in sorted_points.iter() {
        let center = to_screen(p[0] as f32, p[1] as f32);
        let r = 6.0f32;
        let sq = iced::widget::canvas::Path::new(|b| {
            b.move_to(Point::new(center.x - r, center.y - r));
            b.line_to(Point::new(center.x + r, center.y - r));
            b.line_to(Point::new(center.x + r, center.y + r));
            b.line_to(Point::new(center.x - r, center.y + r));
            b.close();
        });
        frame.fill(&sq, Color::from_rgb(1.0, 0.4, 0.2));
        frame.stroke(&sq, iced::widget::canvas::Stroke::default().with_color(Color::WHITE).with_width(2.0));
    }
}
