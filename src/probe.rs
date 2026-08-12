use std::sync::Arc;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{mouse, overlay, Clipboard, Shell, Widget};
use iced::{Element, Event, Length, Rectangle, Size, Vector};
use parking_lot::Mutex;

/// Wraps an element and records its laid-out height (logical pixels) into the
/// shared report after every layout pass. The app reads the report in
/// `update()` and resizes the window to match, so the window height always
/// fits the content (no dead space at the bottom, no clipping in the common
/// case). Width is left untouched.
///
/// The widget is transparent: layout, events, drawing and overlays are all
/// delegated to the wrapped element.
pub struct HeightProbe<'a, Message> {
    content: Element<'a, Message>,
    report: Arc<Mutex<Option<f32>>>,
}

impl<'a, Message> std::fmt::Debug for HeightProbe<'a, Message> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeightProbe")
            .field("report", &self.report.lock())
            .finish()
    }
}

impl<'a, Message: Clone + 'a> HeightProbe<'a, Message> {
    /// Wraps `content` into an element that records the content height into
    /// `report` on every layout pass.
    pub fn wrap(
        content: Element<'a, Message>,
        report: Arc<Mutex<Option<f32>>>,
    ) -> Element<'a, Message> {
        Element::new(Self { content, report })
    }
}

impl<'a, Message: Clone> Widget<Message, iced::Theme, iced::Renderer>
    for HeightProbe<'a, Message>
{
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.content.as_widget())]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        *self.report.lock() = Some(node.size().height);
        node
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}
