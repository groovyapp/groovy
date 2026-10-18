use gpui::rgb;

use super::Theme;

impl Theme {
    /// Gruvbox Light Hard, the light counterpart of [`Theme::gruvbox_dark_hard`].
    pub fn gruvbox_light_hard() -> Self {
        Self {
            title_bar_background: rgb(0xf9f5d7).into(),
            title_bar_inactive_background: rgb(0xf9f5d7).into(),
            editor_background: rgb(0xf9f5d7).into(),
            border: rgb(0xebdbb2).into(),
            text: rgb(0x282828).into(),
            ghost_element_hover: rgb(0xebdbb2).into(),
            ghost_element_active: rgb(0xd5c4a1).into(),
        }
    }
}
