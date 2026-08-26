use std::sync::Arc;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, overlay};
use iced::{Element, Event, Length, Rectangle, Size, Vector};
use parking_lot::Mutex;

/// Records laid-out height into shared report to auto-size window.
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
    pub fn wrap(
        content: Element<'a, Message>,
        report: Arc<Mutex<Option<f32>>>,
    ) -> Element<'a, Message> {
        Element::new(Self { content, report })
    }
}

impl<'a, Message: Clone> Widget<Message, iced::Theme, iced::Renderer> for HeightProbe<'a, Message> {
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
        let Some(child) = tree.children.first_mut() else {
            return layout::Node::new(limits.loose().max());
        };
        let node = self.content.as_widget_mut().layout(child, renderer, limits);
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
        if let Some(child) = tree.children.first() {
            self.content
                .as_widget()
                .draw(child, renderer, theme, style, layout, cursor, viewport);
        }
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
        if let Some(child) = tree.children.first_mut() {
            self.content.as_widget_mut().update(
                child, event, layout, cursor, renderer, clipboard, shell, viewport,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if let Some(child) = tree.children.first() {
            return self
                .content
                .as_widget()
                .mouse_interaction(child, layout, cursor, viewport, renderer);
        }
        mouse::Interaction::default()
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        if let Some(child) = tree.children.first_mut() {
            self.content
                .as_widget_mut()
                .operate(child, layout, renderer, operation);
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        let child = tree.children.first_mut()?;
        self.content
            .as_widget_mut()
            .overlay(child, layout, renderer, viewport, translation)
    }
}
