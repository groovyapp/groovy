//! Opens the data directory and config when the app starts.

use gpui::{App, Global};
use groovy_core::{Config, DataDir};

use crate::APP_NAME;

/// The data directory and config, shared with the rest of the app.
#[expect(dead_code, reason = "read by logging (#4) and settings (#14)")]
pub struct AppData {
    pub dir: DataDir,
    pub config: Config,
}

impl Global for AppData {}

/// Creates the data directory and config on first run, then loads them.
pub fn init(cx: &mut App) {
    let dir = DataDir::resolve(APP_NAME).expect("failed to locate the Groovy data directory");
    dir.create_all()
        .expect("failed to create the Groovy data directory");
    let config = Config::load_or_create(&dir).expect("failed to load the Groovy config");
    cx.set_global(AppData { dir, config });
}
