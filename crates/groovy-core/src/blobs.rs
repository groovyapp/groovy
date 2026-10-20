//! Serving blobs. Each request is decided as it arrives: a trusted contact may fetch
//! anything this node stores, and anyone may fetch the content of an active share.
//! Every other request is refused. A blocked peer is refused before any request.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use iroh_blobs::Hash;
use iroh_blobs::provider::events::{
    AbortReason, ConnectMode, EventMask, EventSender, ProviderMessage, RequestMode,
};
use tokio::sync::mpsc;

use crate::node::PeerGate;

/// The hashes anyone may fetch: the roots and files of active shares.
pub type SharedHashes = Arc<Mutex<HashSet<Hash>>>;

/// Hashes withdrawn by their sender. They are refused to everyone, trusted peers included.
pub type Withdrawn = Arc<Mutex<HashSet<Hash>>>;

/// The event sender blob serving reports to, and the receiver the decisions are made on.
pub fn event_channel() -> (EventSender, mpsc::Receiver<ProviderMessage>) {
    let (tx, rx) = mpsc::channel(64);
    let mask = EventMask {
        connected: ConnectMode::Intercept,
        get: RequestMode::Intercept,
        get_many: RequestMode::Intercept,
        ..EventMask::DEFAULT
    };
    (EventSender::new(tx, mask), rx)
}

/// Decides every connection and request until serving stops.
pub async fn decide(
    mut messages: mpsc::Receiver<ProviderMessage>,
    gate: Arc<dyn PeerGate>,
    shared: SharedHashes,
    withdrawn: Withdrawn,
) {
    // Which peer each connection belongs to, so a request can be checked against it.
    let mut peers = HashMap::new();
    while let Some(message) = messages.recv().await {
        match message {
            ProviderMessage::ClientConnected(msg) => {
                let allowed = match msg.inner.endpoint_id {
                    Some(peer) if gate.may_connect(&peer) => {
                        peers.insert(msg.inner.connection_id, peer);
                        true
                    }
                    _ => false,
                };
                let _ = msg.tx.send(verdict(allowed)).await;
            }
            ProviderMessage::ConnectionClosed(msg) => {
                peers.remove(&msg.inner.connection_id);
            }
            ProviderMessage::GetRequestReceived(msg) => {
                let hash = msg.inner.request.hash;
                let withdrawn_hash = withdrawn
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains(&hash);
                let trusted = trusted(&peers, &gate, msg.inner.connection_id);
                let shared = shared
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains(&hash);
                let _ = msg
                    .tx
                    .send(verdict(!withdrawn_hash && (trusted || shared)))
                    .await;
            }
            // Asking for several blobs at once is for contacts only.
            ProviderMessage::GetManyRequestReceived(msg) => {
                let trusted = trusted(&peers, &gate, msg.inner.connection_id);
                let _ = msg.tx.send(verdict(trusted)).await;
            }
            _ => {}
        }
    }
}

fn trusted(
    peers: &HashMap<u64, iroh::EndpointId>,
    gate: &Arc<dyn PeerGate>,
    connection: u64,
) -> bool {
    peers
        .get(&connection)
        .is_some_and(|peer| gate.is_trusted(peer))
}

fn verdict(allowed: bool) -> Result<(), AbortReason> {
    if allowed {
        Ok(())
    } else {
        Err(AbortReason::Permission)
    }
}
