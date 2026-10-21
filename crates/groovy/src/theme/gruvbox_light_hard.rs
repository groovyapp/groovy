use gpui::{rgb, rgba};

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
            text_muted: rgb(0x665a51).into(),
            text_disabled: rgb(0xa89984).into(),
            surface: rgb(0xfbf1c7).into(),
            input_background: rgb(0xfbf1c7).into(),
            accent: rgb(0x076678).into(),
            accent_hover: rgb(0x458588).into(),
            on_accent: rgb(0xfbf1c7).into(),
            danger: rgb(0x9d0006).into(),
            danger_hover: rgb(0xcc241d).into(),
            warning: rgb(0xb57614).into(),
            success: rgb(0x79740e).into(),
            focus_ring: rgb(0x076678).into(),
            backdrop: rgba(0x00000066).into(),
        }
    }
}
