//! Opens the data directory and config before the app starts.

use gpui::Global;
use groovy_core::{Config, DataDir};

use crate::APP_NAME;

/// The data directory and config, shared with the rest of the app.
#[expect(dead_code, reason = "read by settings (#14)")]
pub struct AppData {
    pub dir: DataDir,
    pub config: Config,
}

impl Global for AppData {}

/// Creates the data directory and config on first run, then loads them.
pub fn open() -> AppData {
    let dir = DataDir::resolve(APP_NAME).expect("failed to locate the Groovy data directory");
    dir.create_all()
        .expect("failed to create the Groovy data directory");
    let config = Config::load_or_create(&dir).expect("failed to load the Groovy config");
    AppData { dir, config }
}

/// Makes the opened data available to the rest of the app.
pub fn init(cx: &mut gpui::App, data: AppData) {
    cx.set_global(data);
}
