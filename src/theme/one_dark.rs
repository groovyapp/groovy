use gpui::rgb;

use super::Theme;

impl Theme {
    /// Zed's default "One Dark" theme (assets/themes/one/one.json).
    #[allow(dead_code)] // Alternative to the default; swap it in via `theme::init`.
    pub fn one_dark() -> Self {
        Self {
            title_bar_background: rgb(0x3b414d).into(),
            title_bar_inactive_background: rgb(0x2e343e).into(),
            editor_background: rgb(0x282c33).into(),
            border: rgb(0x464b57).into(),
            text: rgb(0xdce0e5).into(),
            ghost_element_hover: rgb(0x363c46).into(),
            ghost_element_active: rgb(0x454a56).into(),
        }
    }
}
