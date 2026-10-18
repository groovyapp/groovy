// Release builds are GUI-only; debug builds keep a console for logs (same as Zed).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod data;
mod node;
mod theme;
mod title_bar;
mod window_frame;
mod workspace;

use gpui::App;
use gpui_platform::application;

use workspace::Workspace;

/// The package name from `Cargo.toml`, used for the window title, app ID and
/// desktop entry matching.
pub const APP_NAME: &str = env!("CARGO_PKG_NAME");

fn main() {
    let data = data::open();
    let _log_guard =
        groovy_core::logging::init(&data.dir.logs_dir()).expect("failed to start logging");
    groovy_core::logging::install_panic_hook();

    application().run(move |cx: &mut App| {
        theme::init(cx);
        data::init(cx, data);
        node::init(cx);
        quit_when_last_window_closes(cx);

        Workspace::open_window(cx).expect("failed to open main window");
        cx.activate(true);
    });
}

/// Windows already exits when the last window closes; macOS and Linux don't.
fn quit_when_last_window_closes(cx: &mut App) {
    cx.on_window_closed(|cx, _window_id| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
}
