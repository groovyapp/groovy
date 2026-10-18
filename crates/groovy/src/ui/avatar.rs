//! A circle with someone's initials, colored from their name so it stays the same between launches.

use std::sync::Arc;

use gpui::{App, Hsla, Image, Pixels, SharedString, Window, div, img, prelude::*, px};

use crate::theme::ActiveTheme;

#[derive(IntoElement)]
pub struct Avatar {
    name: SharedString,
    size: Pixels,
    image: Option<Arc<Image>>,
}

impl Avatar {
    pub fn new(name: impl Into<SharedString>) -> Self {
        Self {
            name: name.into(),
            size: px(32.),
            image: None,
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    /// Shows this picture instead of the initials, when there is one.
    pub fn image(mut self, image: Option<Arc<Image>>) -> Self {
        self.image = image;
        self
    }
}

/// Up to two initials: the first letters of the first two words, or of the first word alone.
pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().and_then(|word| word.chars().next());
    let second = words.next().and_then(|word| word.chars().next());
    first
        .into_iter()
        .chain(second)
        .flat_map(char::to_uppercase)
        .collect()
}

/// A stable hue from the name: the same name always gets the same color.
fn background(name: &str, cx: &gpui::App) -> Hsla {
    let theme = cx.theme();
    let palette = [theme.accent, theme.success, theme.warning, theme.danger];
    let hash = name.bytes().fold(0usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as usize)
    });
    palette[hash % palette.len()]
}

impl RenderOnce for Avatar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        if let Some(image) = self.image {
            return img(image)
                .size(self.size)
                .flex_none()
                .rounded_full()
                .into_any_element();
        }
        let theme = cx.theme();
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(self.size)
            .rounded_full()
            .bg(background(&self.name, cx))
            .text_color(theme.on_accent)
            .text_size(self.size * 0.4)
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .child(initials(&self.name))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn initials_come_from_the_first_two_words() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("grace brewster hopper"), "GB");
        assert_eq!(initials("linus"), "L");
        assert_eq!(initials("  "), "");
    }
}
