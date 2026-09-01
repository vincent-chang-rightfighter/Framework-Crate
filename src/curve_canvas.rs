use iced::widget::canvas::Cache;
use iced::{Color, Element, Length, Point, Size};
use std::cell::Cell;
use std::sync::Arc;

// Unit suffix only on last label; "100" is right-aligned to avoid crowding "110°C".
const AXIS_LABELS_X: [&str; 7] = ["0", "20", "40", "60", "80", "100", "110°C"];
const AXIS_LABELS_Y: [&str; 6] = ["0", "20", "40", "60", "80", "100%"];
/// Temperature axis range; extends past 100°C so high readings stay visible.
const TEMP_RANGE: f32 = crate::types::CURVE_TEMP_MAX as f32;
const POINT_RADIUS: f32 = 3.0;
const HIT_RADIUS: f32 = 12.0;

/// Live sensor reading projected onto the curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorMark {
    pub temp: i32,
    pub color: iced::Color,
}

pub fn view_curve(
    points: Arc<[[u32; 2]]>,
    all_pts: Arc<Vec<[u32; 2]>>,
    marks: Arc<Vec<SensorMark>>,
) -> Element<'static, crate::Message> {
    // Map sorted position to original index so drag emits correct index.
    let mut sorted_indices: Vec<usize> = (0..points.len()).collect();
    sorted_indices.sort_by_key(|&i| points[i][0]);
    iced::widget::canvas(CurveRenderer {
        all_pts,
        points,
        sorted_indices,
        marks,
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

/// State in widget Tree; survives `view()` rebuilds.
struct CurveState {
    cache: Cache<iced::Renderer>,
    cached_key: Cell<(*const (), usize, u64)>,
    last_points: std::cell::RefCell<Option<Arc<[[u32; 2]]>>>,
    /// Previous marks to detect temperature changes needing cache clear.
    last_marks: std::cell::RefCell<Option<Arc<Vec<SensorMark>>>>,
    /// Config index being dragged; stable identity when points cross.
    dragging: Cell<Option<usize>>,
    /// Config index under cursor.
    hover: Cell<Option<usize>>,
    /// Previous hover/drag to detect highlight needing redraw.
    last_hover: Cell<Option<usize>>,
    last_drag: Cell<Option<usize>>,
}

impl Default for CurveState {
    fn default() -> Self {
        Self {
            cache: Cache::new(),
            cached_key: Cell::new((std::ptr::null::<()>(), 0, 0)),
            last_points: std::cell::RefCell::new(None),
            last_marks: std::cell::RefCell::new(None),
            dragging: Cell::new(None),
            hover: Cell::new(None),
            last_hover: Cell::new(None),
            last_drag: Cell::new(None),
        }
    }
}

/// Layout matching `draw_curve_contents`.
struct Layout {
    origin: Point,
    plot_w: f32,
    plot_h: f32,
}

impl Layout {
    /// Left gutter for Y-axis labels.
    const LEFT_GUTTER: f32 = 34.0;
    /// Top margin to avoid clipping top label.
    const TOP_MARGIN: f32 = 10.0;
    const RIGHT_MARGIN: f32 = 5.0;
    /// Space below plot for X-axis labels.
    const AXIS_LABEL_SPACE: f32 = 14.0;

    fn new(size: Size) -> Self {
        // Clamp to avoid inf/NaN on degenerate sizes.
        let plot_w = (size.width - Self::LEFT_GUTTER - Self::RIGHT_MARGIN).max(1.0);
        let plot_h =
            (size.height - Self::TOP_MARGIN - Self::RIGHT_MARGIN - Self::AXIS_LABEL_SPACE).max(1.0);
        Self {
            origin: Point::new(Self::LEFT_GUTTER, Self::TOP_MARGIN),
            plot_w,
            plot_h,
        }
    }

    /// Canvas (temp 0-110, duty 0-100) to screen.
    fn to_screen(&self, x: f32, y: f32) -> Point {
        Point::new(
            self.origin.x + (x / TEMP_RANGE) * self.plot_w,
            self.origin.y + self.plot_h - (y / 100.0) * self.plot_h,
        )
    }

    /// Screen back to canvas space.
    fn screen_to_canvas(&self, p: Point) -> (f32, f32) {
        let temp = (p.x - self.origin.x) / self.plot_w * TEMP_RANGE;
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

        // Keep drag alive outside canvas; clamps handle out-of-plot values.
        if let Some(config_idx) = state.dragging.get() {
            let cursor_pos = match cursor.position_in(bounds) {
                Some(p) => p,
                None => match cursor.position() {
                    Some(abs) => Point::new(abs.x - bounds.x, abs.y - bounds.y),
                    None => {
                        // Cursor left window; end drag.
                        state.dragging.set(None);
                        return Some(iced::widget::canvas::Action::request_redraw());
                    }
                },
            };
            match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { .. }) => {
                    // Guard against config reload shrinking points mid-drag.
                    if config_idx >= self.points.len() {
                        state.dragging.set(None);
                        return Some(iced::widget::canvas::Action::request_redraw());
                    }
                    // Locked points (100–110) are not draggable.
                    if self.points[config_idx][0] >= crate::types::CURVE_TEMP_LOCK_START {
                        return None;
                    }
                    let (raw_temp, raw_duty) = layout.screen_to_canvas(cursor_pos);
                    // Editable range is 0..99; 100–110 is locked 100%
                    let temp = (raw_temp.round() as i32)
                        .clamp(0, crate::types::CURVE_TEMP_EDIT_MAX as i32)
                        as u32;
                    let duty = (raw_duty.round() as i32).clamp(0, 100) as u32;
                    // Only publish when rounded value changes to avoid redundant rebuilds.
                    if self.points[config_idx] != [temp, duty] {
                        return Some(iced::widget::canvas::Action::publish(
                            crate::Message::FanCurvePointMoved(config_idx, temp, duty),
                        ));
                    }
                    return None;
                }
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )) => {
                    state.dragging.set(None);
                    return Some(iced::widget::canvas::Action::request_redraw().and_capture());
                }
                _ => {}
            }
            return None;
        }

        // Hover tracking only while cursor is over canvas.
        let Some(cursor_pos) = cursor.position_in(bounds) else {
            if state.hover.get().is_some() {
                state.hover.set(None);
                return Some(iced::widget::canvas::Action::request_redraw());
            }
            return None;
        };
        // Use pre-sorted indices to avoid per-move allocation, skip locked points.
        let mut nearest: Option<usize> = None;
        let mut best_dist = f32::INFINITY;
        for &config_idx in &self.sorted_indices {
            let pt = &self.points[config_idx];
            if pt[0] >= crate::types::CURVE_TEMP_LOCK_START {
                continue;
            }
            let dist = cursor_pos.distance(layout.to_screen(pt[0] as f32, pt[1] as f32));
            if dist < best_dist {
                best_dist = dist;
                nearest = Some(config_idx);
            }
        }
        let nearest = nearest.filter(|_| best_dist <= HIT_RADIUS);

        match event {
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)) => {
                if let Some(idx) = nearest {
                    state.dragging.set(Some(idx));
                    state.hover.set(Some(idx));
                    return Some(iced::widget::canvas::Action::request_redraw().and_capture());
                }
            }
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { .. }) => {
                let old_hover = state.hover.get();
                if old_hover != nearest {
                    state.hover.set(nearest);
                    return Some(iced::widget::canvas::Action::request_redraw());
                }
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
        // order-dependent hash to avoid collision like [30,0]+[45,20] vs [30,20]+[45,0]
        let content_hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            for p in self.all_pts.iter() {
                p[0].hash(&mut hasher);
                p[1].hash(&mut hasher);
            }
            hasher.finish()
        };
        let key = (
            Arc::as_ptr(&self.all_pts) as *const (),
            self.all_pts.len(),
            content_hash,
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
            state.cache.clear();
        }

        let geo = state.cache.draw(renderer, size, |frame| {
            draw_curve_contents(
                frame,
                &self.all_pts,
                &self.points,
                &self.sorted_indices,
                &self.marks,
                size,
                (state.hover.get(), state.dragging.get()),
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
            let near = self.sorted_indices.iter().any(|idx| {
                let pt = &self.points[*idx];
                if pt[0] >= crate::types::CURVE_TEMP_LOCK_START {
                    return false;
                }
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
    sorted_indices: &[usize],
    marks: &Arc<Vec<SensorMark>>,
    size: Size,
    highlight: (Option<usize>, Option<usize>),
) {
    if all_pts.is_empty() {
        return;
    }
    let (hover_idx, drag_idx) = highlight;

    let layout = Layout::new(size);
    let to_screen = |x: f32, y: f32| layout.to_screen(x, y);

    // Clip plot contents so thick strokes never bleed past border.
    let plot_rect = iced::Rectangle::new(layout.origin, Size::new(layout.plot_w, layout.plot_h));
    frame.with_clip(plot_rect, |f| {
        f.fill_rectangle(
            layout.origin,
            Size::new(layout.plot_w, layout.plot_h),
            Color::from_rgb(0.12, 0.12, 0.15),
        );
        // Locked zone 100–110°C: subtle highlight to indicate forced 100%
        let lock_x0 = to_screen(crate::types::CURVE_TEMP_LOCK_START as f32, 0.0).x;
        let lock_x1 = to_screen(TEMP_RANGE, 0.0).x;
        let lock_w = (lock_x1 - lock_x0).max(0.0);
        f.fill_rectangle(
            Point::new(lock_x0, layout.origin.y),
            Size::new(lock_w, layout.plot_h),
            Color::from_rgba(0.9, 0.3, 0.1, 0.08),
        );
        f.fill_rectangle(
            Point::new(lock_x0, layout.origin.y),
            Size::new(lock_w, layout.plot_h),
            Color::from_rgba(0.9, 0.3, 0.1, 0.03),
        );

        let grid_stroke = iced::widget::canvas::Stroke::default()
            .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.1))
            .with_width(0.5);
        for v in [20.0, 40.0, 60.0, 80.0, 100.0] {
            f.stroke(
                &iced::widget::canvas::Path::line(to_screen(v, 0.0), to_screen(v, 100.0)),
                grid_stroke,
            );
        }
        for v in [20.0, 40.0, 60.0, 80.0] {
            f.stroke(
                &iced::widget::canvas::Path::line(to_screen(0.0, v), to_screen(TEMP_RANGE, v)),
                grid_stroke,
            );
        }

        f.stroke_rectangle(
            layout.origin,
            Size::new(layout.plot_w, layout.plot_h),
            iced::widget::canvas::Stroke::default()
                .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.3))
                .with_width(1.0),
        );

        let curve_path = iced::widget::canvas::Path::new(|b| {
            b.move_to(to_screen(all_pts[0][0] as f32, all_pts[0][1] as f32));
            for p in all_pts.iter().skip(1) {
                b.line_to(to_screen(p[0] as f32, p[1] as f32));
            }
        });
        f.stroke(
            &curve_path,
            iced::widget::canvas::Stroke::default()
                .with_color(crate::style::COLOR_CURVE)
                .with_width(2.0),
        );

        for &config_idx in sorted_indices.iter() {
            let p = &points[config_idx];
            let center = to_screen(p[0] as f32, p[1] as f32);
            let is_locked = p[0] >= crate::types::CURVE_TEMP_LOCK_START;
            let (fill_color, stroke_color, r) = if is_locked {
                (
                    Color::from_rgb(0.6, 0.6, 0.6),
                    Color::from_rgba(1.0, 1.0, 1.0, 0.5),
                    POINT_RADIUS,
                )
            } else if drag_idx == Some(config_idx) {
                (crate::style::COLOR_CURVE, Color::WHITE, POINT_RADIUS + 2.0)
            } else if hover_idx == Some(config_idx) {
                (crate::style::COLOR_CURVE, Color::WHITE, POINT_RADIUS + 1.0)
            } else {
                (crate::style::COLOR_CURVE, Color::WHITE, POINT_RADIUS)
            };
            let circle = iced::widget::canvas::Path::circle(center, r);
            f.fill(&circle, fill_color);
            f.stroke(
                &circle,
                iced::widget::canvas::Stroke::default()
                    .with_color(stroke_color)
                    .with_width(2.0),
            );
        }
        // Locked zone label
        {
            let lock_x0 = to_screen(crate::types::CURVE_TEMP_LOCK_START as f32, 0.0).x;
            let lock_x1 = to_screen(TEMP_RANGE, 0.0).x;
            let cx = (lock_x0 + lock_x1) * 0.5;
            f.fill_text(iced::widget::canvas::Text {
                content: "LOCK 100%".to_string(),
                position: Point::new(cx, layout.origin.y + 2.0),
                color: Color::from_rgba(0.9, 0.4, 0.2, 0.45),
                size: iced::Pixels(7.0),
                font: iced::Font::with_name("Consolas"),
                align_x: iced::alignment::Horizontal::Center.into(),
                align_y: iced::alignment::Vertical::Top,
                line_height: iced::widget::text::LineHeight::default(),
                shaping: iced::widget::text::Shaping::Basic,
                max_width: f32::INFINITY,
            });
        }

        // Live sensor markers: dashed crosshair plus colored dot.
        let plot_top = layout.origin.y;
        let plot_bottom = layout.origin.y + layout.plot_h;
        let plot_left = layout.origin.x;
        let plot_right = layout.origin.x + layout.plot_w;
        for mark in marks.iter() {
            let temp = (mark.temp as f32).clamp(0.0, TEMP_RANGE);
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
            f.stroke(
                &iced::widget::canvas::Path::line(
                    Point::new(pos.x, plot_top),
                    Point::new(pos.x, plot_bottom),
                ),
                dash,
            );
            f.stroke(
                &iced::widget::canvas::Path::line(
                    Point::new(plot_left, pos.y),
                    Point::new(plot_right, pos.y),
                ),
                dash,
            );
            let dot = iced::widget::canvas::Path::circle(pos, 4.0);
            f.fill(&dot, mark.color);
            f.stroke(
                &dot,
                iced::widget::canvas::Stroke::default()
                    .with_color(Color::WHITE)
                    .with_width(1.5),
            );
        }
    });

    // Axis labels outside clipped region.
    let font = iced::Font::with_name("Consolas");
    // X-axis: right-align 100/110°C to keep inside canvas.
    for (i, v) in [0u32, 20, 40, 60, 80, 100, 110].iter().enumerate() {
        // "0" left-aligns with gutter; high values right-align to avoid crowding.
        let (align_x, x) = if *v == 0 {
            (iced::alignment::Horizontal::Left, 2.0)
        } else if *v >= 100 {
            (
                iced::alignment::Horizontal::Right,
                layout.origin.x + (*v as f32 / TEMP_RANGE) * layout.plot_w,
            )
        } else {
            (
                iced::alignment::Horizontal::Center,
                layout.origin.x + (*v as f32 / TEMP_RANGE) * layout.plot_w,
            )
        };
        frame.fill_text(iced::widget::canvas::Text {
            content: AXIS_LABELS_X[i].to_owned(),
            position: Point::new(x, layout.origin.y + layout.plot_h + 4.0),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: iced::Pixels(9.0),
            font,
            align_x: align_x.into(),
            align_y: iced::alignment::Vertical::Top,
            line_height: iced::widget::text::LineHeight::default(),
            shaping: iced::widget::text::Shaping::Basic,
            max_width: f32::INFINITY,
        });
    }
    // Y-axis labels left-aligned; skip "0" to avoid overlap with X-axis origin.
    for (i, v) in [0u32, 20, 40, 60, 80, 100].iter().enumerate() {
        let y = layout.origin.y + layout.plot_h - (*v as f32 / 100.0) * layout.plot_h;
        if *v != 0 {
            frame.fill_text(iced::widget::canvas::Text {
                content: AXIS_LABELS_Y[i].to_owned(),
                position: Point::new(2.0, y),
                color: Color::from_rgb(0.6, 0.6, 0.6),
                size: iced::Pixels(9.0),
                font,
                align_x: iced::alignment::Horizontal::Left.into(),
                align_y: iced::alignment::Vertical::Center,
                line_height: iced::widget::text::LineHeight::default(),
                shaping: iced::widget::text::Shaping::Basic,
                max_width: f32::INFINITY,
            });
        }
    }
}
