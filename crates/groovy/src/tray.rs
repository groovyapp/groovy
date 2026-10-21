//! The tray icon, with Groovy's status and a few quick actions: open the window, pause or resume
//! transfers, and quit.
//!
//! Only Linux has a tray here, through the freedesktop StatusNotifierItem protocol. Elsewhere
//! [`start`] reports that there is no tray, and the window closes as it always did.
//!
//! The tray's clicks run on its own thread, so they are queued and the app takes them on its
//! next tick, the same way notifications are handled.

#[cfg(target_os = "linux")]
use std::sync::{Mutex, PoisonError};
#[cfg(target_os = "linux")]
use std::time::Duration;

use gpui::{App, AppContext, Global};
#[cfg(target_os = "linux")]
use groovy_core::transfers::{Direction, TransferState};

/// What a tray click asks the app to do.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Open,
    PauseTransfers,
    ResumeTransfers,
    Quit,
}

/// Clicks the tray has made since the last call, oldest first.
#[cfg(target_os = "linux")]
static COMMANDS: Mutex<Vec<Command>> = Mutex::new(Vec::new());

/// The commands queued since the last call.
#[cfg(target_os = "linux")]
pub fn take_commands() -> Vec<Command> {
    std::mem::take(&mut *COMMANDS.lock().unwrap_or_else(PoisonError::into_inner))
}

#[cfg(target_os = "linux")]
fn push(command: Command) {
    COMMANDS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(command);
}

/// The tray, when one is running.
#[cfg(target_os = "linux")]
pub struct TrayHandle(ksni::blocking::Handle<imp::GroovyTray>);

#[cfg(not(target_os = "linux"))]
pub struct TrayHandle(());

impl Global for TrayHandle {}

/// Whether a tray icon is running for this app.
pub fn running(cx: &App) -> bool {
    cx.has_global::<TrayHandle>()
}

/// Starts the tray icon, and begins taking its clicks. Returns whether it started. Without a
/// tray the app behaves as it did before one existed.
pub fn start(cx: &mut App) -> bool {
    #[cfg(target_os = "linux")]
    {
        use ksni::blocking::TrayMethods;
        let icon = imp::icon();
        match imp::GroovyTray::new(icon).spawn() {
            Ok(handle) => {
                cx.set_global(TrayHandle(handle));
                poll(cx);
                true
            }
            Err(error) => {
                tracing::warn!(%error, "the tray icon could not start; closing the window will quit");
                false
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cx;
        false
    }
}

/// Acts on the tray's clicks, and keeps its status current, every so often.
#[cfg(target_os = "linux")]
fn poll(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            cx.update(|cx| {
                for command in take_commands() {
                    act(cx, command);
                }
                refresh_status(cx);
            });
        }
    })
    .detach();
}

#[cfg(target_os = "linux")]
fn act(cx: &mut App, command: Command) {
    match command {
        Command::Open => crate::show_main_window(cx),
        Command::Quit => cx.quit(),
        Command::PauseTransfers | Command::ResumeTransfers => {
            let Some(handle) = cx
                .try_global::<crate::node::NodeState>()
                .and_then(|node| node.handle())
            else {
                return;
            };
            let pause = command == Command::PauseTransfers;
            cx.spawn(async move |_| {
                for transfer in handle.transfers() {
                    let receiving = transfer.direction == Direction::Receive;
                    let state = &transfer.state;
                    if pause
                        && matches!(state, TransferState::Queued | TransferState::Active)
                        && receiving
                    {
                        let _ = handle.pause_transfer(transfer.id).await;
                    }
                    if !pause && *state == TransferState::Paused && receiving {
                        let _ = handle.resume_transfer(transfer.id).await;
                    }
                }
            })
            .detach();
        }
    }
}

/// How many transfers are waiting or moving, and whether the node is online.
#[cfg(target_os = "linux")]
fn counts(cx: &App) -> (bool, usize) {
    let online = cx
        .try_global::<crate::node::NodeState>()
        .is_some_and(|node| node.status() == crate::node::NodeStatus::Online);
    let active = cx
        .try_global::<crate::node::NodeState>()
        .and_then(|node| node.handle())
        .map(|handle| {
            handle
                .transfers()
                .iter()
                .filter(|t| matches!(t.state, TransferState::Queued | TransferState::Active))
                .count()
        })
        .unwrap_or(0);
    (online, active)
}

#[cfg(target_os = "linux")]
fn refresh_status(cx: &mut App) {
    let (online, active) = counts(cx);
    let shown = imp::Shown { online, active };
    if let Some(tray) = cx.try_global::<TrayHandle>() {
        tray.0.update(|state| {
            if state.shown != shown {
                state.shown = shown;
            }
        });
    }
}

/// Changes the background settings, and saves the config. Starting on login also writes or
/// removes the login entry.
pub fn update_background(cx: &mut App, change: impl FnOnce(&mut groovy_core::Background)) {
    let config = {
        let data = cx.global_mut::<crate::data::AppData>();
        change(&mut data.config.background);
        data.config.clone()
    };
    let dir = cx.global::<crate::data::AppData>().dir.clone();
    let start_on_login = config.background.start_on_login;
    cx.background_spawn(async move {
        if let Err(error) = config.save(&dir) {
            tracing::warn!(%error, "could not save the background settings");
        }
        if let Err(error) = crate::autostart::set_enabled(start_on_login) {
            tracing::warn!(%error, "could not change starting on login");
        }
    })
    .detach();
}

#[cfg(target_os = "linux")]
mod imp {
    use ksni::menu::StandardItem;
    use ksni::{Icon, MenuItem, ToolTip, Tray};

    use super::{Command, push};

    /// What the tray shows: whether the node is online, and how many transfers are moving.
    #[derive(Clone, PartialEq, Eq)]
    pub struct Shown {
        pub online: bool,
        pub active: usize,
    }

    pub struct GroovyTray {
        pub shown: Shown,
        icon: Vec<Icon>,
    }

    impl GroovyTray {
        pub fn new(icon: Vec<Icon>) -> Self {
            Self {
                shown: Shown {
                    online: false,
                    active: 0,
                },
                icon,
            }
        }

        fn description(&self) -> String {
            let status = if self.shown.online {
                "Online"
            } else {
                "Starting or offline"
            };
            match self.shown.active {
                0 => status.to_owned(),
                1 => format!("{status}, 1 transfer"),
                n => format!("{status}, {n} transfers"),
            }
        }
    }

    impl Tray for GroovyTray {
        fn id(&self) -> String {
            "groovy".to_owned()
        }

        fn title(&self) -> String {
            "Groovy".to_owned()
        }

        fn icon_pixmap(&self) -> Vec<Icon> {
            self.icon.clone()
        }

        fn tool_tip(&self) -> ToolTip {
            ToolTip {
                icon_name: String::new(),
                icon_pixmap: self.icon.clone(),
                title: "Groovy".to_owned(),
                description: self.description(),
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            push(Command::Open);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem {
                    label: format!("Open Groovy ({})", self.description()),
                    activate: Box::new(|_| push(Command::Open)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Pause transfers".to_owned(),
                    activate: Box::new(|_| push(Command::PauseTransfers)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Resume transfers".to_owned(),
                    activate: Box::new(|_| push(Command::ResumeTransfers)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit Groovy".to_owned(),
                    activate: Box::new(|_| push(Command::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// The app icon as the tray expects it: ARGB32 in network byte order.
    pub fn icon() -> Vec<Icon> {
        let bytes = include_bytes!("../../../assets/icons/app-icon-32.png");
        let Ok(decoded) = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        else {
            return Vec::new();
        };
        let rgba = decoded.to_rgba8();
        let (width, height) = rgba.dimensions();
        let mut data = Vec::with_capacity(rgba.len());
        for pixel in rgba.pixels() {
            let [r, g, b, a] = pixel.0;
            data.extend_from_slice(&[a, r, g, b]);
        }
        vec![Icon {
            width: width as i32,
            height: height as i32,
            data,
        }]
    }
}
