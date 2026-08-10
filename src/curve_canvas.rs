use std::sync::Arc;
use iced::{Color, Element, Length, Point, Size};

const AXIS_LABELS: [&str; 6] = ["0", "20", "40", "60", "80", "100"];

pub fn view_curve(points: &[[u32; 2]], all_pts: &Arc<Vec<[u32; 2]>>) -> Element<'static, crate::Message> {
    let points_arc: Arc<[[u32; 2]]> = Arc::from(points);
    iced::widget::canvas(CurveRenderer { all_pts: Arc::clone(all_pts), points: points_arc })
        .width(Length::Fill)
        .height(180)
        .into()
}

struct CurveRenderer {
    all_pts: Arc<Vec<[u32; 2]>>,
    points: Arc<[[u32; 2]]>,
}

impl iced::widget::canvas::Program<crate::Message> for CurveRenderer {
    type State = ();

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
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: iced::Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry> {
        let size = bounds.size();
        let mut frame = iced::widget::canvas::Frame::new(renderer, size);

        if self.all_pts.is_empty() {
            return vec![frame.into_geometry()];
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
            for p in self.all_pts.iter() { b.line_to(to_screen(p[0] as f32, p[1] as f32)); }
            b.line_to(to_screen(100.0, 0.0));
            b.close();
        });
        frame.fill(&fill_path, Color::from_rgba(0.23, 0.51, 0.96, 0.15));

        let curve_path = iced::widget::canvas::Path::new(|b| {
            b.move_to(to_screen(self.all_pts[0][0] as f32, self.all_pts[0][1] as f32));
            for p in self.all_pts.iter().skip(1) { b.line_to(to_screen(p[0] as f32, p[1] as f32)); }
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

        for p in self.points.iter() {
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

        vec![frame.into_geometry()]
    }
}
