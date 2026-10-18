//! Custom titlebar, mirroring Zed's Windows titlebar
//! (crates/platform_title_bar/src/platform_title_bar.rs).

mod client_controls;
mod window_controls;

use std::sync::{Arc, LazyLock};

use gpui::{prelude::*, *};

use crate::theme::ActiveTheme;
use client_controls::ClientWindowControls;
use window_controls::WindowsWindowControls;

/// Zed hard-codes 32px on Windows to match the native caption height.
const TITLE_BAR_HEIGHT: Pixels = px(32.);
const APP_ICON_SIZE: Pixels = px(16.);

/// 32px so the 16px icon stays sharp up to 200% display scaling.
static APP_ICON: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(
        ImageFormat::Png,
        include_bytes!("../../../../assets/icons/app-icon-32.png").to_vec(),
    ))
});

#[derive(IntoElement)]
pub struct TitleBar {
    title: SharedString,
}

impl TitleBar {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
        }
    }

    /// On Linux the compositor may draw its own titlebar (e.g. KDE Plasma),
    /// in which case ours would be a duplicate.
    pub fn is_visible(window: &Window) -> bool {
        !cfg!(any(target_os = "linux", target_os = "freebsd"))
            || matches!(window.window_decorations(), Decorations::Client { .. })
    }
}

impl RenderOnce for TitleBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let background = if window.is_window_active() {
            theme.title_bar_background
        } else {
            theme.title_bar_inactive_background
        };

        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .h(TITLE_BAR_HEIGHT)
            .pl_2()
            .bg(background)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(img(APP_ICON.clone()).size(APP_ICON_SIZE).flex_none())
                    .child(div().text_sm().child(self.title)),
            );

        if cfg!(target_os = "windows") {
            // Windows handles dragging, snapping, and double-click to
            // maximize for this region.
            bar.window_control_area(WindowControlArea::Drag)
                .child(WindowsWindowControls)
        } else {
            // Wayland and X11 draw no caption for us, so drive the window
            // through the compositor ourselves.
            bar.on_mouse_down(MouseButton::Left, |event, window, _cx| {
                if event.click_count == 2 {
                    window.zoom_window();
                } else {
                    window.start_window_move();
                }
            })
            .on_mouse_down(MouseButton::Right, |event, window, _cx| {
                window.show_window_menu(event.position);
            })
            .child(ClientWindowControls)
        }
    }
}
