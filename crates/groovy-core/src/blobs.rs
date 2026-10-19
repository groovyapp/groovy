//! Blob storage: content-addressed files kept on this device and served to
//! trusted contacts over iroh-blobs.
//!
//! Blobs are stored by the BLAKE3 hash of their content, so each one can be
//! checked as it arrives. Each blob is kept by a tag, which keeps it from being
//! removed; a blob without a tag is eligible for garbage collection.

use std::fmt;
use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh_blobs::BlobsProtocol;

use crate::node::PeerGate;

/// The application close code sent to a peer that is not trusted.
pub const CLOSE_NOT_TRUSTED: u32 = 3;

/// Serves blobs only to peers the user has paired with. Anyone else is refused
/// before any blob is read.
pub struct TrustedBlobs {
    inner: BlobsProtocol,
    gate: Arc<dyn PeerGate>,
}

impl TrustedBlobs {
    pub fn new(inner: BlobsProtocol, gate: Arc<dyn PeerGate>) -> Self {
        Self { inner, gate }
    }
}

impl fmt::Debug for TrustedBlobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustedBlobs").finish_non_exhaustive()
    }
}

impl ProtocolHandler for TrustedBlobs {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if !self.gate.is_trusted(&connection.remote_id()) {
            connection.close(CLOSE_NOT_TRUSTED.into(), b"not trusted");
            return Ok(());
        }
        ProtocolHandler::accept(&self.inner, connection).await
    }

    async fn shutdown(&self) {
        ProtocolHandler::shutdown(&self.inner).await;
    }
}
