//! A horizontal bar showing a fraction from 0 to 1.

use gpui::{prelude::*, *};

use crate::theme::ActiveTheme;

#[derive(IntoElement)]
pub struct ProgressBar {
    fraction: f32,
}

impl ProgressBar {
    /// `fraction` is clamped to 0..=1.
    pub fn new(fraction: f32) -> Self {
        Self {
            fraction: fraction.clamp(0., 1.),
        }
    }
}

impl RenderOnce for ProgressBar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .w_full()
            .h(px(6.))
            .rounded_full()
            .bg(theme.border)
            .overflow_hidden()
            .child(
                div()
                    .h_full()
                    .w(relative(self.fraction))
                    .rounded_full()
                    .bg(theme.accent),
            )
    }
}
