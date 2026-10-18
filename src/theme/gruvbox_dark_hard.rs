use gpui::rgb;

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
        }
    }
}
