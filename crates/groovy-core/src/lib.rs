//! Groovy's core services: networking, storage and messaging.
//!
//! This crate must not depend on `gpui`, so that the desktop app, the CLI and
//! tests can all share it.

mod avatar;
pub mod backup;
mod config;
pub mod db;
pub mod identity;
pub mod logging;
mod node;
mod paths;
pub mod profile;

pub use avatar::{AvatarError, thumbnail, thumbnail_from_file};
pub use config::{CURRENT_VERSION, Config, ConfigError};
pub use db::{Database, DbError, SCHEMA_VERSION};
pub use identity::{Identity, IdentityError, Location};
pub use iroh::{EndpointAddr, EndpointId, SecretKey};
pub use node::{
    EventStream, GROOVY_ALPN, Network, Node, NodeConfig, NodeError, NodeEvent, NodeHandle,
    relay_url,
};
pub use paths::{DataDir, PathsError, override_env_var};
pub use profile::{Profile, validate_display_name};
