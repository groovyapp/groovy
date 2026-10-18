//! Minimize / maximize / close buttons for platforms without a native caption
//! (Wayland and X11), where the app handles every click itself.

use gpui::{prelude::*, *};

use crate::theme::ActiveTheme;

const BUTTON_WIDTH: Pixels = px(36.);
const ICON_SIZE: Pixels = px(12.);
const CLOSE_HOVER: u32 = 0xe81120;

#[derive(IntoElement)]
pub struct ClientWindowControls;

impl RenderOnce for ClientWindowControls {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let supported = window.window_controls();

        div()
            .id("client-window-controls")
            .flex()
            .flex_row()
            .h_full()
            // Keep clicks from reaching the titlebar's drag handler.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(supported.minimize, |controls| {
                controls.child(ControlButton::Minimize)
            })
            .when(supported.maximize, |controls| {
                controls.child(if window.is_maximized() {
                    ControlButton::Restore
                } else {
                    ControlButton::Maximize
                })
            })
            .child(ControlButton::Close)
    }
}

#[derive(IntoElement)]
enum ControlButton {
    Minimize,
    Restore,
    Maximize,
    Close,
}

impl ControlButton {
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
            Self::Minimize => "–",
            Self::Restore => "❐",
            Self::Maximize => "□",
            Self::Close => "✕",
        }
    }

    fn activate(&self, window: &mut Window) {
        match self {
            Self::Minimize => window.minimize_window(),
            Self::Restore | Self::Maximize => window.zoom_window(),
            Self::Close => window.remove_window(),
        }
    }
}

impl RenderOnce for ControlButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (hover_bg, hover_fg, active_bg) = match self {
            Self::Close => {
                let red: Hsla = rgb(CLOSE_HOVER).into();
                (red, white(), red.opacity(0.8))
            }
            _ => (
                theme.ghost_element_hover,
                theme.text,
                theme.ghost_element_active,
            ),
        };

        let icon = self.icon();

        div()
            .id(self.id())
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .w(BUTTON_WIDTH)
            .h_full()
            .text_size(ICON_SIZE)
            .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
            .active(move |style| style.bg(active_bg).text_color(hover_fg))
            .on_click(move |_, window, _cx| self.activate(window))
            .child(icon)
    }
}
