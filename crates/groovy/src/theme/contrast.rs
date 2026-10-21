//! Checks that the text and focus colours of both themes are readable: WCAG 2.1 AA, 4.5 to 1 for
//! text and 3 to 1 for the focus ring. Disabled text is exempt, as the standard allows.

use gpui::{Hsla, Rgba};

use super::Theme;

/// The WCAG contrast ratio between two colours, from 1 to 21.
pub fn ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (luminance(a.to_rgb()), luminance(b.to_rgb()));
    let (light, dark) = if la > lb { (la, lb) } else { (lb, la) };
    (light + 0.05) / (dark + 0.05)
}

fn luminance(color: Rgba) -> f32 {
    let linear = |channel: f32| {
        if channel <= 0.03928 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// The text colours each theme draws on each background, with what they are used for.
fn text_pairs(theme: &Theme) -> Vec<(&'static str, Hsla, Hsla, f32)> {
    vec![
        (
            "text on the window",
            theme.text,
            theme.editor_background,
            4.5,
        ),
        ("text on a surface", theme.text, theme.surface, 4.5),
        (
            "muted text on the window",
            theme.text_muted,
            theme.editor_background,
            4.5,
        ),
        (
            "muted text on a surface",
            theme.text_muted,
            theme.surface,
            4.5,
        ),
        (
            "text on an accent button",
            theme.on_accent,
            theme.accent,
            4.5,
        ),
        (
            "danger text on the window",
            theme.danger,
            theme.editor_background,
            4.5,
        ),
        (
            "focus ring on the window",
            theme.focus_ring,
            theme.editor_background,
            3.0,
        ),
        (
            "focus ring on a surface",
            theme.focus_ring,
            theme.surface,
            3.0,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_light_and_dark_themes_meet_the_contrast_standard() {
        let mut failures = Vec::new();
        for (name, theme) in [
            ("light", Theme::gruvbox_light_hard()),
            ("dark", Theme::gruvbox_dark_hard()),
        ] {
            for (what, fore, back, needed) in text_pairs(&theme) {
                let found = ratio(fore, back);
                if found < needed {
                    failures.push(format!("{name}: {what} is {found:.2} to 1, needs {needed}"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn black_on_white_is_the_full_ratio() {
        let black: Hsla = gpui::rgb(0x000000).into();
        let white: Hsla = gpui::rgb(0xffffff).into();
        assert!((ratio(black, white) - 21.0).abs() < 0.01);
    }
}
