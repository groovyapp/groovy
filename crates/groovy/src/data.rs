//! Opens the data directory and config before the app starts.

use anyhow::Context;
use gpui::Global;
use groovy_core::{Config, DataDir, Database};

use crate::APP_NAME;

/// The data directory, config and database, shared with the rest of the app.
#[expect(dead_code, reason = "read by settings (#14) and the node (#7)")]
pub struct AppData {
    pub dir: DataDir,
    pub config: Config,
    pub database: Database,
}

impl Global for AppData {}

/// Locates the data directory and creates it on first run.
pub fn prepare() -> anyhow::Result<DataDir> {
    let dir = DataDir::resolve(APP_NAME).context("failed to locate the data directory")?;
    dir.create_all()
        .context("failed to create the data directory")?;
    Ok(dir)
}

/// Loads the config and opens the database, migrating it if needed.
pub fn load(dir: DataDir) -> anyhow::Result<AppData> {
    let config = Config::load_or_create(&dir).context("failed to load the config")?;
    let database = Database::open(&dir.database_file()).context("failed to open the database")?;
    Ok(AppData {
        dir,
        config,
        database,
    })
}

/// Makes the opened data available to the rest of the app.
pub fn init(cx: &mut gpui::App, data: AppData) {
    cx.set_global(data);
}
