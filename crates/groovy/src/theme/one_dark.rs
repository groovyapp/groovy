use gpui::{rgb, rgba};

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
            text_muted: rgb(0x878a98).into(),
            text_disabled: rgb(0x5c6370).into(),
            surface: rgb(0x2f343e).into(),
            input_background: rgb(0x282c33).into(),
            accent: rgb(0x74ade8).into(),
            accent_hover: rgb(0x8fbdf0).into(),
            on_accent: rgb(0x282c33).into(),
            danger: rgb(0xd07277).into(),
            danger_hover: rgb(0xe08a8e).into(),
            warning: rgb(0xdec184).into(),
            success: rgb(0xa1c181).into(),
            focus_ring: rgb(0x74ade8).into(),
            backdrop: rgba(0x00000099).into(),
        }
    }
}
