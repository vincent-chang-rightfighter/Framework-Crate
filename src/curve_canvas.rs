use std::cell::{Cell, OnceCell};
use std::sync::Arc;
use iced::{Color, Element, Length, Point, Size};
use iced::widget::canvas::Cache;

const AXIS_LABELS: [&str; 6] = ["0", "20", "40", "60", "80", "100"];
const POINT_RADIUS: f32 = 3.0;
const HIT_RADIUS: f32 = 12.0;
/// Curve line and control point color (#6b75ff).
const CURVE_COLOR: Color = Color::from_rgb(0x6B as f32 / 255.0, 0x75 as f32 / 255.0, 0xFF as f32 / 255.0);

/// A live sensor reading drawn on the curve: the sensor's current
/// temperature projected onto the curve, using its sensor color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorMark {
    pub temp: i32,
    pub color: iced::Color,
}

pub fn view_curve(
    points: &[[u32; 2]],
    all_pts: &Arc<Vec<[u32; 2]>>,
    marks: &Arc<Vec<SensorMark>>,
) -> Element<'static, crate::Message> {
    let points_arc: Arc<[[u32; 2]]> = Arc::from(points);
    // Build a mapping from sorted position back to the original config
    // index so that dragging a rendered point emits the correct index.
    let mut sorted_indices: Vec<usize> = (0..points.len()).collect();
    sorted_indices.sort_by_key(|&i| points[i][0]);
    iced::widget::canvas(CurveRenderer {
        all_pts: Arc::clone(all_pts),
        points: points_arc,
        sorted_indices,
        marks: Arc::clone(marks),
    })
    .width(Length::Fill)
    .height(180)
    .into()
}

struct CurveRenderer {
    all_pts: Arc<Vec<[u32; 2]>>,
    points: Arc<[[u32; 2]]>,
    sorted_indices: Vec<usize>,
    marks: Arc<Vec<SensorMark>>,
}

/// Persistent state living in the widget `Tree` — survives `view()` rebuilds.
struct CurveState {
    cache: OnceCell<Cache<iced::Renderer>>,
    cached_key: Cell<(*const (), usize)>,
    last_points: std::cell::RefCell<Option<Arc<[[u32; 2]]>>>,
    /// Snapshot of the sensor marks from the previous draw — used to detect
    /// live temperature changes that need a cache clear.
    last_marks: std::cell::RefCell<Option<Arc<Vec<SensorMark>>>>,
    /// Index (into sorted order) of the point currently being dragged.
    dragging: Cell<Option<usize>>,
    /// Index (into sorted order) of the point under the cursor (hover).
    hover: Cell<Option<usize>>,
    /// Previous hover/drag values — used to detect when the highlight
    /// needs a cache clear so the circles redraw with updated colours.
    last_hover: Cell<Option<usize>>,
    last_drag: Cell<Option<usize>>,
}

impl Default for CurveState {
    fn default() -> Self {
        Self {
            cache: OnceCell::new(),
            cached_key: Cell::new((std::ptr::null::<()>(), 0)),
            last_points: std::cell::RefCell::new(None),
            last_marks: std::cell::RefCell::new(None),
            dragging: Cell::new(None),
            hover: Cell::new(None),
            last_hover: Cell::new(None),
            last_drag: Cell::new(None),
        }
    }
}

/// Layout constants matching `draw_curve_contents`.
struct Layout {
    origin: Point,
    plot_w: f32,
    plot_h: f32,
}

impl Layout {
    fn new(size: Size) -> Self {
        let margin = 5.0f32;
        let plot_w = size.width - margin * 2.0;
        let plot_h = size.height - margin * 2.0 - 14.0;
        Self { origin: Point::new(margin, margin), plot_w, plot_h }
    }

    /// Convert canvas-space (temp 0–100, duty 0–100) to screen coordinates.
    fn to_screen(&self, x: f32, y: f32) -> Point {
        Point::new(
            self.origin.x + (x / 100.0) * self.plot_w,
            self.origin.y + self.plot_h - (y / 100.0) * self.plot_h,
        )
    }

    /// Convert screen coordinates back to canvas-space (temp, duty).
    fn screen_to_canvas(&self, p: Point) -> (f32, f32) {
        let temp = (p.x - self.origin.x) / self.plot_w * 100.0;
        let duty = (self.origin.y + self.plot_h - p.y) / self.plot_h * 100.0;
        (temp, duty)
    }
}

impl iced::widget::canvas::Program<crate::Message> for CurveRenderer {
    type State = CurveState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &iced::Event,
        bounds: iced::Rectangle,
        cursor: iced::mouse::Cursor,
    ) -> Option<iced::widget::canvas::Action<crate::Message>> {
        let layout = Layout::new(bounds.size());
        // During a drag the cursor may leave the canvas bounds; keep the
        // drag alive using the absolute position minus the canvas origin,
        // and let the temp/duty clamps handle out-of-plot values.
        let cursor_pos = match cursor.position_in(bounds) {
            Some(p) => p,
            None => match (state.dragging.get(), cursor.position()) {
                (Some(_), Some(abs)) => Point::new(abs.x - bounds.x, abs.y - bounds.y),
                _ => {
                    // Cursor left the canvas — clear hover / drag.
                    if state.hover.get().is_some() {
                        state.hover.set(None);
                        return Some(iced::widget::canvas::Action::request_redraw());
                    }
                    if state.dragging.get().is_some() {
                        state.dragging.set(None);
                    }
                    return None;
                }
            },
        };
        let sorted: Vec<[u32; 2]> = {
            let mut v = self.points.to_vec();
            v.sort_by_key(|p| p[0]);
            v
        };

        // Find nearest point (in sorted order) within HIT_RADIUS.
        let nearest = sorted.iter().enumerate().min_by(|(_, a), (_, b)| {
            let da = cursor_pos.distance(layout.to_screen(a[0] as f32, a[1] as f32));
            let db = cursor_pos.distance(layout.to_screen(b[0] as f32, b[1] as f32));
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        }).filter(|(_, pt)| {
            cursor_pos.distance(layout.to_screen(pt[0] as f32, pt[1] as f32)) <= HIT_RADIUS
        }).map(|(i, _)| i);

        match event {
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)) => {
                if let Some(idx) = nearest {
                    state.dragging.set(Some(idx));
                    state.hover.set(Some(idx));
                    return Some(iced::widget::canvas::Action::capture());
                }
            }
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { .. }) => {
                if let Some(drag_idx) = state.dragging.get() {
                    // Convert cursor to temp/duty, clamped to valid ranges.
                    let (raw_temp, raw_duty) = layout.screen_to_canvas(cursor_pos);
                    let temp = (raw_temp.round() as i32).clamp(1, 99) as u32;
                    let duty = (raw_duty.round() as i32).clamp(0, 100) as u32;
                    let config_idx = self.sorted_indices[drag_idx];
                    return Some(iced::widget::canvas::Action::publish(
                        crate::Message::FanCurvePointMoved(config_idx, temp, duty),
                    ));
                }
                // Hover tracking.
                let old_hover = state.hover.get();
                if old_hover != nearest {
                    state.hover.set(nearest);
                    return Some(iced::widget::canvas::Action::request_redraw());
                }
            }
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left))
                if state.dragging.get().is_some() =>
            {
                state.dragging.set(None);
                return Some(iced::widget::canvas::Action::capture());
            }
            _ => {}
        }
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
        let points_changed = state.last_points.borrow().as_deref() != Some(self.points.as_ref());
        let marks_changed = state.last_marks.borrow().as_deref() != Some(self.marks.as_ref());
        let highlight_changed = state.last_hover.get() != state.hover.get()
            || state.last_drag.get() != state.dragging.get();
        if state.cached_key.get() != key || points_changed || marks_changed || highlight_changed {
            state.cached_key.set(key);
            if points_changed {
                *state.last_points.borrow_mut() = Some(Arc::clone(&self.points));
            }
            if marks_changed {
                *state.last_marks.borrow_mut() = Some(Arc::clone(&self.marks));
            }
            state.last_hover.set(state.hover.get());
            state.last_drag.set(state.dragging.get());
            if let Some(cache) = state.cache.get() {
                cache.clear();
            }
        }

        let cache = state.cache.get_or_init(Cache::new);
        let hover_idx = state.hover.get();
        let drag_idx = state.dragging.get();
        let geo = cache.draw(renderer, size, |frame| {
            draw_curve_contents(
                frame,
                &self.all_pts,
                &self.points,
                &self.marks,
                size,
                hover_idx,
                drag_idx,
            );
        });
        vec![geo]
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: iced::Rectangle,
        cursor: iced::mouse::Cursor,
    ) -> iced::mouse::Interaction {
        if state.dragging.get().is_some() {
            return iced::mouse::Interaction::Grabbing;
        }
        if let Some(pos) = cursor.position_in(bounds) {
            let layout = Layout::new(bounds.size());
            let sorted: Vec<[u32; 2]> = {
                let mut v = self.points.to_vec();
                v.sort_by_key(|p| p[0]);
                v
            };
            let near = sorted.iter().any(|pt| {
                pos.distance(layout.to_screen(pt[0] as f32, pt[1] as f32)) <= HIT_RADIUS
            });
            if near {
                return iced::mouse::Interaction::Pointer;
            }
        }
        iced::mouse::Interaction::default()
    }
}

fn draw_curve_contents(
    frame: &mut iced::widget::canvas::Frame<iced::Renderer>,
    all_pts: &Arc<Vec<[u32; 2]>>,
    points: &Arc<[[u32; 2]]>,
    marks: &Arc<Vec<SensorMark>>,
    size: Size,
    hover_idx: Option<usize>,
    drag_idx: Option<usize>,
) {
    if all_pts.is_empty() {
        return;
    }

    let layout = Layout::new(size);
    let to_screen = |x: f32, y: f32| layout.to_screen(x, y);

    frame.fill_rectangle(layout.origin, Size::new(layout.plot_w, layout.plot_h), Color::from_rgb(0.12, 0.12, 0.15));

    let grid_stroke = iced::widget::canvas::Stroke::default()
        .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.1))
        .with_width(0.5);
    for v in [20.0, 40.0, 60.0, 80.0] {
        frame.stroke(&iced::widget::canvas::Path::line(to_screen(v, 0.0), to_screen(v, 100.0)), grid_stroke);
        frame.stroke(&iced::widget::canvas::Path::line(to_screen(0.0, v), to_screen(100.0, v)), grid_stroke);
    }

    frame.stroke_rectangle(layout.origin, Size::new(layout.plot_w, layout.plot_h),
        iced::widget::canvas::Stroke::default().with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.3)).with_width(1.0));

    let curve_path = iced::widget::canvas::Path::new(|b| {
        b.move_to(to_screen(all_pts[0][0] as f32, all_pts[0][1] as f32));
        for p in all_pts.iter().skip(1) { b.line_to(to_screen(p[0] as f32, p[1] as f32)); }
    });
    frame.stroke(&curve_path, iced::widget::canvas::Stroke::default()
        .with_color(CURVE_COLOR).with_width(2.0));

    let font = iced::Font::with_name("Consolas");
    for (i, v) in [0u32, 20, 40, 60, 80, 100].iter().enumerate() {
        let x = layout.origin.x + (*v as f32 / 100.0) * layout.plot_w;
        frame.fill_text(iced::widget::canvas::Text {
            content: AXIS_LABELS[i].to_owned(),
            position: Point::new(x, layout.origin.y + layout.plot_h + 4.0),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: iced::Pixels(9.0), font,
            align_x: iced::alignment::Horizontal::Center.into(),
            align_y: iced::alignment::Vertical::Top,
            line_height: iced::widget::text::LineHeight::default(),
            shaping: iced::widget::text::Shaping::Basic,
            max_width: f32::INFINITY,
        });
        let y = layout.origin.y + layout.plot_h - (*v as f32 / 100.0) * layout.plot_h;
        frame.fill_text(iced::widget::canvas::Text {
            content: AXIS_LABELS[i].to_owned(),
            position: Point::new(layout.origin.x - 4.0, y),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: iced::Pixels(9.0), font,
            align_x: iced::alignment::Horizontal::Right.into(),
            align_y: iced::alignment::Vertical::Center,
            line_height: iced::widget::text::LineHeight::default(),
            shaping: iced::widget::text::Shaping::Basic,
            max_width: f32::INFINITY,
        });
    }

    // Draw control points as circles, in sorted temperature order.
    let mut sorted_points: Vec<[u32; 2]> = points.to_vec();
    sorted_points.sort_by_key(|p| p[0]);
    for (sorted_pos, p) in sorted_points.iter().enumerate() {
        let center = to_screen(p[0] as f32, p[1] as f32);
        let (fill_color, stroke_color, r) = if drag_idx == Some(sorted_pos) {
            (CURVE_COLOR, Color::WHITE, POINT_RADIUS + 2.0)
        } else if hover_idx == Some(sorted_pos) {
            (CURVE_COLOR, Color::WHITE, POINT_RADIUS + 1.0)
        } else {
            (CURVE_COLOR, Color::WHITE, POINT_RADIUS)
        };
        let circle = iced::widget::canvas::Path::circle(center, r);
        frame.fill(&circle, fill_color);
        frame.stroke(&circle, iced::widget::canvas::Stroke::default()
            .with_color(stroke_color).with_width(2.0));
    }

    // Live sensor markers drawn on top: dashed crosshair through the curve
    // position at the sensor's current temperature, plus a dot in the
    // sensor color.
    let plot_top = layout.origin.y;
    let plot_bottom = layout.origin.y + layout.plot_h;
    let plot_left = layout.origin.x;
    let plot_right = layout.origin.x + layout.plot_w;
    for mark in marks.iter() {
        let temp = (mark.temp as f32).clamp(0.0, 100.0);
        let duty = crate::fan_control::calculate_duty_from_curve(mark.temp, all_pts) as f32;
        let pos = to_screen(temp, duty);
        let dash = iced::widget::canvas::Stroke {
            style: iced::widget::canvas::Style::Solid(mark.color),
            width: 1.0,
            line_cap: iced::widget::canvas::LineCap::Round,
            line_join: iced::widget::canvas::LineJoin::Round,
            line_dash: iced::widget::canvas::LineDash {
                segments: &[4.0, 4.0],
                offset: 0,
            },
        };
        frame.stroke(&iced::widget::canvas::Path::line(
            Point::new(pos.x, plot_top),
            Point::new(pos.x, plot_bottom),
        ), dash);
        frame.stroke(&iced::widget::canvas::Path::line(
            Point::new(plot_left, pos.y),
            Point::new(plot_right, pos.y),
        ), dash);
        let dot = iced::widget::canvas::Path::circle(pos, 4.0);
        frame.fill(&dot, mark.color);
        frame.stroke(&dot, iced::widget::canvas::Stroke::default()
            .with_color(Color::WHITE).with_width(1.5));
    }
}
