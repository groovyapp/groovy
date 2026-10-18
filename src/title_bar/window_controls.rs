//! Minimize / maximize / close buttons, mirroring Zed's
//! (crates/platform_title_bar/src/platforms/platform_windows.rs).
//!
//! Clicks are handled natively: each button reports a [`WindowControlArea`]
//! to the Windows hit-test, so the OS performs the action.

use gpui::{prelude::*, *};

use crate::theme::ActiveTheme;

const CAPTION_BUTTON_WIDTH: Pixels = px(36.);
const CAPTION_ICON_SIZE: Pixels = px(10.);
/// Windows' standard close-button red: rgb(232, 17, 32).
const CLOSE_HOVER: u32 = 0xe81120;

#[derive(IntoElement)]
pub struct WindowsWindowControls;

impl RenderOnce for WindowsWindowControls {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .id("windows-window-controls")
            .font_family(caption_font())
            .flex()
            .flex_row()
            .justify_center()
            .content_stretch()
            .h_full()
            .child(CaptionButton::Minimize)
            .child(if window.is_maximized() {
                CaptionButton::Restore
            } else {
                CaptionButton::Maximize
            })
            .child(CaptionButton::Close)
    }
}

#[cfg(not(target_os = "windows"))]
fn caption_font() -> &'static str {
    "Segoe Fluent Icons"
}

/// Windows 11 ships "Segoe Fluent Icons"; Windows 10 only has "Segoe MDL2 Assets".
#[cfg(target_os = "windows")]
fn caption_font() -> &'static str {
    use windows::Wdk::System::SystemServices::RtlGetVersion;

    let mut version = unsafe { std::mem::zeroed() };
    let status = unsafe { RtlGetVersion(&mut version) };

    if status.is_ok() && version.dwBuildNumber >= 22000 {
        "Segoe Fluent Icons"
    } else {
        "Segoe MDL2 Assets"
    }
}

#[derive(IntoElement)]
enum CaptionButton {
    Minimize,
    Restore,
    Maximize,
    Close,
}

impl CaptionButton {
    fn id(&self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Restore => "restore",
            Self::Maximize => "maximize",
            Self::Close => "close",
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            Self::Minimize => "\u{e921}",
            Self::Restore => "\u{e923}",
            Self::Maximize => "\u{e922}",
            Self::Close => "\u{e8bb}",
        }
    }

    fn control_area(&self) -> WindowControlArea {
        match self {
            Self::Close => WindowControlArea::Close,
            Self::Maximize | Self::Restore => WindowControlArea::Max,
            Self::Minimize => WindowControlArea::Min,
        }
    }
}

impl RenderOnce for CaptionButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (hover_bg, hover_fg, active_bg, active_fg) = match self {
            Self::Close => {
                let red: Hsla = rgb(CLOSE_HOVER).into();
                (red, white(), red.opacity(0.8), white().opacity(0.8))
            }
            _ => (
                theme.ghost_element_hover,
                theme.text,
                theme.ghost_element_active,
                theme.text,
            ),
        };

        div()
            .id(self.id())
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .content_center()
            .occlude()
            .w(CAPTION_BUTTON_WIDTH)
            .h_full()
            .text_size(CAPTION_ICON_SIZE)
            .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
            .active(move |style| style.bg(active_bg).text_color(active_fg))
            .window_control_area(self.control_area())
            .child(self.icon())
    }
}
