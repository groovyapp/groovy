// Release builds are GUI-only; debug builds keep a console for logs (same as Zed).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod theme;
mod title_bar;
mod window_frame;
mod workspace;

use gpui::{App, Application};

use workspace::Workspace;

pub const APP_NAME: &str = "gpui-template";

fn main() {
    Application::new().run(|cx: &mut App| {
        theme::init(cx);
        quit_when_last_window_closes(cx);

        Workspace::open_window(cx).expect("failed to open main window");
        cx.activate(true);
    });
}

/// Windows already exits when the last window closes; macOS and Linux don't.
fn quit_when_last_window_closes(cx: &mut App) {
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
}
