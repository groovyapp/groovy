//! A small count, such as unread messages. Counts above 99 show as "99+".

use gpui::{prelude::*, *};

use crate::theme::ActiveTheme;

#[derive(IntoElement)]
pub struct Badge {
    count: usize,
}

impl Badge {
    pub fn new(count: usize) -> Self {
        Self { count }
    }
}

/// The label shown for `count`: the number, or "99+" beyond that.
pub fn label(count: usize) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}

impl RenderOnce for Badge {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .min_w(px(18.))
            .h(px(18.))
            .px_1()
            .rounded_full()
            .bg(theme.accent)
            .text_color(theme.on_accent)
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .child(label(self.count))
    }
}

#[cfg(test)]
mod tests {
    use super::label;

    #[test]
    fn counts_cap_at_ninety_nine_plus() {
        assert_eq!(label(0), "0");
        assert_eq!(label(99), "99");
        assert_eq!(label(100), "99+");
    }
}
