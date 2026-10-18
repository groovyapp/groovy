//! Application-wide colors, stored as a gpui global and read via [`ActiveTheme`].
//!
//! To add a theme, create a file next to `one_dark.rs` with its own constructor.

mod gruvbox_dark_hard;
mod gruvbox_light_hard;
mod one_dark;

use gpui::{App, Global, Hsla, WindowAppearance};

pub struct Theme {
    pub title_bar_background: Hsla,
    pub title_bar_inactive_background: Hsla,
    pub editor_background: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub ghost_element_hover: Hsla,
    pub ghost_element_active: Hsla,
}

impl Global for Theme {}

pub fn init(cx: &mut App) {
    sync_with_appearance(cx.window_appearance(), cx);
}

/// Picks the light or dark theme to match the system appearance.
pub fn sync_with_appearance(appearance: WindowAppearance, cx: &mut App) {
    cx.set_global(match appearance {
        WindowAppearance::Light | WindowAppearance::VibrantLight => Theme::gruvbox_light_hard(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark => Theme::gruvbox_dark_hard(),
    });
}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}
