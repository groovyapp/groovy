//! Application-wide colors, stored as a gpui global and read via [`ActiveTheme`].
//!
//! To add a theme, create a file next to `one_dark.rs` with its own constructor.

#[cfg(test)]
mod contrast;
mod gruvbox_dark_hard;
mod gruvbox_light_hard;
mod one_dark;

use gpui::{App, Global, Hsla, WindowAppearance};
use groovy_core::Appearance;

use crate::data::AppData;

pub struct Theme {
    pub title_bar_background: Hsla,
    pub title_bar_inactive_background: Hsla,
    pub editor_background: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_disabled: Hsla,
    pub ghost_element_hover: Hsla,
    pub ghost_element_active: Hsla,
    /// Raised surfaces: dialogs, menus, toasts.
    pub surface: Hsla,
    pub input_background: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    /// Text drawn on `accent` and `danger`.
    pub on_accent: Hsla,
    pub danger: Hsla,
    pub danger_hover: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
    pub focus_ring: Hsla,
    /// Dims the window behind modal dialogs.
    pub backdrop: Hsla,
}

impl Global for Theme {}

/// The light or dark theme in use, or `None` to follow the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Light,
    Dark,
}

/// A manual choice of light or dark that overrides the system appearance.
#[derive(Default)]
pub struct ModeOverride(pub Option<Mode>);

impl Global for ModeOverride {}

/// The mode a saved appearance choice means: `None` follows the system.
pub fn mode_for(appearance: Appearance) -> Option<Mode> {
    match appearance {
        Appearance::System => None,
        Appearance::Light => Some(Mode::Light),
        Appearance::Dark => Some(Mode::Dark),
    }
}

/// Applies the appearance saved in the config. Needs [`crate::data::AppData`].
pub fn init(cx: &mut App) {
    let appearance = cx.global::<AppData>().config.appearance;
    cx.set_global(ModeOverride(mode_for(appearance)));
    sync_with_appearance(cx.window_appearance(), cx);
}

/// Picks the light or dark theme: the manual choice if there is one, otherwise the system's.
pub fn sync_with_appearance(appearance: WindowAppearance, cx: &mut App) {
    let mode = cx.global::<ModeOverride>().0.unwrap_or(match appearance {
        WindowAppearance::Light | WindowAppearance::VibrantLight => Mode::Light,
        WindowAppearance::Dark | WindowAppearance::VibrantDark => Mode::Dark,
    });
    cx.set_global(match mode {
        Mode::Light => Theme::gruvbox_light_hard(),
        Mode::Dark => Theme::gruvbox_dark_hard(),
    });
}

/// Sets a manual light or dark choice, or `None` to follow the system again.
#[allow(dead_code)] // Used by the debug gallery; settings (#14) will use it too.
pub fn set_mode_override(mode: Option<Mode>, cx: &mut App) {
    cx.global_mut::<ModeOverride>().0 = mode;
    let appearance = cx.window_appearance();
    sync_with_appearance(appearance, cx);
}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}
