//! Groovy's core services: networking, storage and messaging.
//!
//! This crate must not depend on `gpui`, so that the desktop app, the CLI and
//! tests can all share it.

mod node;

pub use node::{EventStream, Node, NodeError, NodeEvent, NodeHandle};
