// Release builds are GUI-only; debug builds keep a console for logs (same as Zed).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod backup;
mod data;
mod devices;
mod files;
mod folders;
mod messages;
mod node;
mod notifications;
mod onboarding;
mod peers;
mod settings;
mod settings_profile;
mod theme;
mod tickets;
mod title_bar;
mod tray;
mod ui;
mod window_frame;
mod workspace;

use std::process::ExitCode;

use anyhow::Context;
use gpui::App;
use gpui_platform::application;
use groovy_core::DataDir;

use workspace::Workspace;

/// The package name from `Cargo.toml`, used for the window title, app ID and
/// desktop entry matching.
pub const APP_NAME: &str = env!("CARGO_PKG_NAME");

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Logging may not have started, so stderr is the fallback. Release
            // builds on Windows have no console, so there the log file is the
            // only record.
            eprintln!("Groovy could not start: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let dir = data::prepare()?;
    let _log_guard =
        groovy_core::logging::init(&dir.logs_dir()).context("failed to start logging")?;
    groovy_core::logging::install_panic_hook();

    // Errors from here on also go to the log file.
    let result = start_app(dir);
    if let Err(error) = &result {
        tracing::error!("Groovy could not start: {error:#}");
    }
    result
}

fn start_app(dir: DataDir) -> anyhow::Result<()> {
    let data = data::load(dir)?;
    let last_screen = workspace::Screen::load(&data.database);
    let onboarded = onboarding::is_completed(&data.database);

    application()
        .with_assets(ui::icon::Assets)
        .run(move |cx: &mut App| {
            data::init(cx, data);
            theme::init(cx);
            ui::input::bind_keys(cx);
            workspace::bind_keys(cx);
            #[cfg(debug_assertions)]
            ui::gallery::init(cx);
            // Until onboarding is done, the identity may still change, so the node waits.
            if onboarded {
                node::start(cx);
            }
            quit_when_last_window_closes(cx);

            // With a tray, `--background` (the login entry's flag) starts without a window.
            let tray_running = tray::start(cx);
            let background = std::env::args().any(|arg| arg == "--background");
            if !(background && tray_running) {
                // Failing to open the first window leaves nothing to show, so it stays a hard failure.
                Workspace::open_window(cx, last_screen, onboarded)
                    .expect("failed to open main window");
                cx.activate(true);
            }
        });
    Ok(())
}

/// Windows already exits when the last window closes; macOS and Linux don't. With a tray and
/// close-to-tray on, closing the last window keeps Groovy running, and the node with it.
fn quit_when_last_window_closes(cx: &mut App) {
    cx.on_window_closed(|cx, _window_id| {
        let keeps_running =
            tray::running(cx) && cx.global::<data::AppData>().config.background.close_to_tray;
        if cx.windows().is_empty() && !keeps_running {
            cx.quit();
        }
    })
    .detach();
}

/// Brings the main window forward, or opens it if it was closed.
pub fn show_main_window(cx: &mut App) {
    match cx.windows().first().copied() {
        Some(window) => {
            window
                .update(cx, |_, window, _| window.activate_window())
                .ok();
        }
        None => {
            let database = cx.global::<data::AppData>().database.clone();
            let screen = workspace::Screen::load(&database);
            let onboarded = onboarding::is_completed(&database);
            if let Err(error) = Workspace::open_window(cx, screen, onboarded) {
                tracing::error!(%error, "could not open the main window");
            }
        }
    }
    cx.activate(true);
}
