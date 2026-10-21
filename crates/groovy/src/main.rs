// Release builds are GUI-only; debug builds keep a console for logs (same as Zed).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod backup;
mod data;
mod deeplink;
mod devices;
mod files;
mod folders;
mod instance;
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
mod url_scheme;
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
    // A running copy takes this launch's arguments, and this launch ends.
    let args: Vec<String> = std::env::args().collect();
    let socket = dir.root().join("instance.sock");
    if matches!(instance::claim(&socket, &args)?, instance::Claim::Forwarded) {
        return Ok(());
    }
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

            spawn_instance_poll(cx);
            sync_link_handler(cx);
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

/// Quits when the last window closes, unless the tray is up and close-to-tray is on: then
/// Groovy keeps running, and the node with it.
fn quit_when_last_window_closes(cx: &mut App) {
    // GPUI quits on its own when the last window closes, except on macOS. That runs after this
    // handler and would undo close-to-tray, so the decision is left to this handler alone.
    cx.set_quit_mode(gpui::QuitMode::Explicit);
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

/// Takes the requests other launches made, and acts on them: bringing the window forward, or
/// showing a link's place in the app.
fn spawn_instance_poll(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            cx.update(|cx| {
                for item in instance::take_incoming() {
                    match item {
                        instance::Incoming::Show => show_main_window(cx),
                        instance::Incoming::Link(text) => {
                            if let Some(link) = deeplink::parse(&text) {
                                route_link(cx, link);
                            }
                        }
                    }
                }
            });
        }
    })
    .detach();
}

/// Shows a link's place in the app. A link only fills in an input; the user presses the button.
fn route_link(cx: &mut App, link: deeplink::Link) {
    show_main_window(cx);
    let workspace = cx
        .windows()
        .iter()
        .find_map(|window| window.downcast::<Workspace>());
    if let Some(workspace) = workspace {
        workspace
            .update(cx, |workspace, _, cx| workspace.open_link(link, cx))
            .ok();
    }
}

/// Keeps the `groovy://` registration in step with the setting. A run with the data directory
/// overridden is a test or second copy, so it leaves the registration to the real one.
fn sync_link_handler(cx: &mut App) {
    if std::env::var_os(groovy_core::override_env_var(APP_NAME)).is_some() {
        return;
    }
    let on = cx.global::<data::AppData>().config.background.open_links;
    std::thread::spawn(move || {
        if let Err(error) = url_scheme::sync(on) {
            tracing::warn!(%error, "could not register for groovy:// links");
        }
    });
}
