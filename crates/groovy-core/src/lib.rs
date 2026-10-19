//! Groovy's core services: networking, storage and messaging.
//!
//! This crate must not depend on `gpui`, so that the desktop app, the CLI and
//! tests can all share it.

mod avatar;
pub mod backup;
pub mod blobs;
mod config;
pub mod contacts;
pub mod db;
pub mod files;
pub mod identity;
pub mod logging;
mod node;
pub mod pairing;
mod paths;
pub mod presence;
pub mod profile;
pub mod protocol;
pub mod shares;
pub mod ticket;
pub mod transfers;

pub use avatar::{AvatarError, thumbnail, thumbnail_from_file};
pub use config::{
    Appearance, CURRENT_VERSION, Config, ConfigError, Discovery, RelaySetting, parse_relay_urls,
};
pub use contacts::{Contact, ContactGate, Trust};
pub use db::{Database, DbError, SCHEMA_VERSION};
pub use identity::{Identity, IdentityError, Location};
pub use iroh::{EndpointAddr, EndpointId, SecretKey};
pub use node::{
    AllowAll, EventStream, GROOVY_ALPN, Network, Node, NodeConfig, NodeError, NodeEvent,
    NodeHandle, PeerGate, SharePreview, ShareTicket, relay_url,
};
pub use pairing::verification_code;
pub use paths::{DataDir, DataLock, PathsError, override_env_var};
pub use presence::{Route, Tracker};
pub use profile::{Profile, validate_display_name};
pub use protocol::{PeerInfo, PeerProfile, ProtocolError, avatar_hash};
pub use ticket::TicketError;
