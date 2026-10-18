//! Groovy's core services: networking, storage and messaging.
//!
//! This crate must not depend on `gpui`, so that the desktop app, the CLI and
//! tests can all share it.

mod config;
pub mod db;
pub mod logging;
mod node;
mod paths;

pub use config::{CURRENT_VERSION, Config, ConfigError};
pub use db::{Database, DbError, SCHEMA_VERSION};
pub use node::{EventStream, Node, NodeError, NodeEvent, NodeHandle};
pub use paths::{DataDir, PathsError, override_env_var};
