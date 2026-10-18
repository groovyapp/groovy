use gpui::{rgb, rgba};

use super::Theme;

impl Theme {
    /// Gruvbox Dark Hard, with the titlebar sharing the window's bg0_h so the
    /// whole window reads as one surface.
    pub fn gruvbox_dark_hard() -> Self {
        Self {
            title_bar_background: rgb(0x1d2021).into(),
            title_bar_inactive_background: rgb(0x1d2021).into(),
            editor_background: rgb(0x1d2021).into(),
            border: rgb(0x3c3836).into(),
            text: rgb(0xfbf1c7).into(),
            ghost_element_hover: rgb(0x3c3836).into(),
            ghost_element_active: rgb(0x504945).into(),
            text_muted: rgb(0xa89984).into(),
            text_disabled: rgb(0x665c54).into(),
            surface: rgb(0x282828).into(),
            input_background: rgb(0x282828).into(),
            accent: rgb(0x83a598).into(),
            accent_hover: rgb(0x458588).into(),
            on_accent: rgb(0x1d2021).into(),
            danger: rgb(0xfb4934).into(),
            danger_hover: rgb(0xcc241d).into(),
            warning: rgb(0xfabd2f).into(),
            success: rgb(0xb8bb26).into(),
            focus_ring: rgb(0x83a598).into(),
            backdrop: rgba(0x00000099).into(),
        }
    }
}
