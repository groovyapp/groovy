//! Opens the data directory and config before the app starts.

use std::sync::Arc;

use anyhow::Context;
use gpui::Global;
use groovy_core::{Config, DataDir, Database, Identity, Location};
use tracing::{info, warn};

use crate::APP_NAME;

/// The data directory, config, database and identity, shared with the rest of the app.
pub struct AppData {
    pub dir: DataDir,
    pub config: Config,
    pub database: Arc<Database>,
    pub identity: Identity,
}

impl Global for AppData {}

/// Locates the data directory and creates it on first run.
pub fn prepare() -> anyhow::Result<DataDir> {
    let dir = DataDir::resolve(APP_NAME).context("failed to locate the data directory")?;
    dir.create_all()
        .context("failed to create the data directory")?;
    Ok(dir)
}

/// Loads the config, opens the database (migrating it if needed) and loads the identity.
pub fn load(dir: DataDir) -> anyhow::Result<AppData> {
    let config = Config::load_or_create(&dir).context("failed to load the config")?;
    let database = Database::open(&dir.database_file()).context("failed to open the database")?;
    let identity =
        Identity::load_or_create(&dir, APP_NAME).context("failed to load the identity")?;
    match identity.location() {
        Location::Keyring => info!(public_key = %identity.public_key(), "identity loaded"),
        Location::File => warn!(
            public_key = %identity.public_key(),
            "identity is kept in a file because the OS credential store is unavailable"
        ),
    }
    Ok(AppData {
        dir,
        config,
        database: Arc::new(database),
        identity,
    })
}

/// Makes the opened data available to the rest of the app.
pub fn init(cx: &mut gpui::App, data: AppData) {
    cx.set_global(data);
}
