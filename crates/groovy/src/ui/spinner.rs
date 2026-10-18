//! A rotating loader for work whose length is unknown.

use std::time::Duration;

use gpui::{prelude::*, *};

use super::icon::Icon;
use crate::theme::ActiveTheme;

#[derive(IntoElement)]
pub struct Spinner {
    id: ElementId,
    size: Pixels,
}

impl Spinner {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            size: px(16.),
        }
    }
}

impl RenderOnce for Spinner {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = cx.theme().text_muted;
        svg()
            .path(Icon::Loader.path())
            .size(self.size)
            .flex_none()
            .text_color(color)
            .with_animation(
                self.id,
                Animation::new(Duration::from_millis(900)).repeat(),
                |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
            )
    }
}
