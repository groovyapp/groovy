//! The root view of the main window: the sidebar, the selected screen, and the status bar.

mod screen;
mod sidebar;
mod status_bar;

use std::sync::Arc;

use gpui::{prelude::*, *};
use groovy_core::Database;

use crate::{
    APP_NAME,
    data::AppData,
    theme::{self, ActiveTheme},
    title_bar::TitleBar,
    window_frame::{client_side_decorations, set_native_border_color},
};
pub use screen::{Screen, bind_keys};
use screen::{ShowFiles, ShowMessages, ShowPeers, ShowSettings, ShowStorage};
use sidebar::Sidebar;
use status_bar::StatusBar;

const DEFAULT_WINDOW_SIZE: Size<Pixels> = size(px(1280.), px(800.));
/// Same minimum as Zed, so the titlebar controls never overlap.
const MIN_WINDOW_SIZE: Size<Pixels> = size(px(360.), px(240.));

pub struct Workspace {
    current: Screen,
    database: Arc<Database>,
    focus_handle: FocusHandle,
    _appearance_subscription: Subscription,
}

impl Workspace {
    /// Opens the main window on `screen`. Needs the data opened at startup.
    pub fn open_window(cx: &mut App, screen: Screen) -> Result<WindowHandle<Self>> {
        let database = cx.global::<AppData>().database.clone();
        cx.open_window(window_options(cx), |window, cx| {
            set_native_border_color(window, cx);
            cx.new(|cx| Self::new(window, cx, screen, database))
        })
    }

    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        current: Screen,
        database: Arc<Database>,
    ) -> Self {
        // Follow the system's light/dark setting while the app is running.
        let _appearance_subscription = cx.observe_window_appearance(window, |_, window, cx| {
            theme::sync_with_appearance(window.appearance(), cx);
            set_native_border_color(window, cx);
            cx.notify();
        });

        // Shortcuts are dispatched from the focused element, so the window
        // needs one to start with.
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        Self {
            current,
            database,
            focus_handle,
            _appearance_subscription,
        }
    }

    fn select(&mut self, screen: Screen, cx: &mut Context<Self>) {
        if screen != self.current {
            self.current = screen;
            screen.remember(self.database.clone(), cx);
            cx.notify();
        }
    }
}

fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            DEFAULT_WINDOW_SIZE,
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(APP_NAME.into()),
            // Hides the native caption; `TitleBar` draws our own.
            appears_transparent: true,
            traffic_light_position: None,
        }),
        // Linux only; Windows and macOS ignore this.
        window_decorations: Some(window_decorations()),
        app_id: Some(APP_NAME.to_lowercase()),
        window_min_size: Some(MIN_WINDOW_SIZE),
        ..Default::default()
    }
}

/// Prefer the desktop's own frame (KDE, wlroots, X11 window managers).
/// Compositors without xdg-decoration support, like GNOME, fall back to
/// client-side, which `client_side_decorations` and `TitleBar` handle.
/// Set `GROOVY_WINDOW_DECORATIONS=client` to force our own frame, the
/// counterpart of Zed's `ZED_WINDOW_DECORATIONS`.
fn window_decorations() -> WindowDecorations {
    match std::env::var("GROOVY_WINDOW_DECORATIONS").as_deref() {
        Ok("client") => WindowDecorations::Client,
        _ => WindowDecorations::Server,
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let workspace = cx.entity().clone();

        let sidebar = Sidebar::new(self.current, move |screen, _, cx| {
            workspace.update(cx, |workspace, cx| workspace.select(screen, cx));
        });

        let body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .child(sidebar)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .p_6()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.current.label()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child(self.current.description()),
                    )
                    .child(
                        div()
                            .mt_4()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child("This screen is not built yet."),
                    ),
            );

        let content = div()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.editor_background)
            .text_color(theme.text)
            .on_action(cx.listener(|this, _: &ShowFiles, _, cx| this.select(Screen::Files, cx)))
            .on_action(
                cx.listener(|this, _: &ShowMessages, _, cx| this.select(Screen::Messages, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowPeers, _, cx| this.select(Screen::Peers, cx)))
            .on_action(cx.listener(|this, _: &ShowStorage, _, cx| this.select(Screen::Storage, cx)))
            .on_action(
                cx.listener(|this, _: &ShowSettings, _, cx| this.select(Screen::Settings, cx)),
            )
            .when(TitleBar::is_visible(window), |d| {
                d.child(TitleBar::new(APP_NAME))
            })
            .child(body)
            .child(StatusBar);

        client_side_decorations(content, window, cx)
    }
}
