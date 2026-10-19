//! Wraps a protocol so that only trusted contacts may connect to it.

use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};

use crate::node::PeerGate;

/// The application close code for a connection refused because the peer is not trusted.
pub const CLOSE_NOT_TRUSTED: u32 = 3;

/// Serves `inner` only to trusted contacts. Anyone else's connection is closed before the
/// protocol sees it.
#[derive(Clone)]
pub struct TrustedOnly<P> {
    inner: P,
    gate: Arc<dyn PeerGate>,
}

impl<P> TrustedOnly<P> {
    pub fn new(inner: P, gate: Arc<dyn PeerGate>) -> Self {
        Self { inner, gate }
    }
}

impl<P> std::fmt::Debug for TrustedOnly<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustedOnly").finish_non_exhaustive()
    }
}

impl<P: ProtocolHandler> ProtocolHandler for TrustedOnly<P> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if !self.gate.is_trusted(&connection.remote_id()) {
            connection.close(CLOSE_NOT_TRUSTED.into(), b"not trusted");
            return Ok(());
        }
        self.inner.accept(connection).await
    }

    async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}
