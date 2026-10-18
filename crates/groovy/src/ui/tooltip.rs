//! Tooltips for any element with an id.

use gpui::{AnyView, App, Context, SharedString, Window, div, prelude::*};

use crate::theme::ActiveTheme;

struct Tooltip {
    text: SharedString,
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .text_color(theme.text)
            .text_xs()
            .child(self.text.clone())
    }
}

/// A builder for `.tooltip(...)` that shows `text`.
pub fn text(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_window, cx| {
        let text = text.clone();
        cx.new(|_| Tooltip { text }).into()
    }
}
