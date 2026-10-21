//! Keeps each shared folder connected to the other members.
//!
//! iroh-docs syncs a folder with the members it is told about, and keeps it live over gossip
//! from then on. It does not start a folder again when the node restarts, it does not try
//! again when a sync fails, and it often does not remember a member at all: a sync whose
//! entries were exchanged can still end with an error while the connection closes, and then
//! the member is not recorded. So this task, one for each folder:
//!
//! - Remembers the members in the database: the ones in the ticket the folder was joined with,
//!   and each device it syncs with or meets over gossip.
//! - Starts the live sync with those members when the node starts, or the folder is created
//!   or joined.
//! - Syncs with a member again when a sync with it fails, waiting longer after each failure.
//! - While the folder has no gossip neighbors but knows members, syncs with them again soon,
//!   waiting longer each time. Gossip joins only once, so a join that failed, as when a member
//!   could not be found yet after a restart, would otherwise leave the folder without live
//!   updates until the next resync.
//! - Now and then syncs with every member, which picks up a change that was made while a
//!   sync was already running and so reached neither the sync nor gossip.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use iroh::{EndpointAddr, EndpointId};
use iroh_docs::NamespaceId;
use iroh_docs::api::Doc;
use iroh_docs::engine::LiveEvent;
use iroh_docs::protocol::Docs;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::contacts;
use crate::db::Database;

/// How long to wait before syncing again after the first failure. It doubles with each
/// failure in a row, up to `RETRY_MAX`.
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(300);
/// How often the folder is synced with every member it knows.
const RESYNC_INTERVAL: Duration = Duration::from_secs(60);
/// How long a folder with no neighbors waits before syncing with its members again. It
/// doubles each time, up to `RESYNC_INTERVAL`.
const ALONE_FIRST: Duration = Duration::from_secs(5);

/// Starts keeping a folder connected. The task runs until the node stops, or until it is
/// aborted through the returned handle, as when the folder is left.
pub fn spawn(
    docs: Docs,
    database: Arc<Database>,
    namespace: NamespaceId,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        if let Err(error) = run(docs, database, namespace).await {
            warn!(%error, "a shared folder stopped reconnecting");
        }
    })
    .abort_handle()
}

async fn run(docs: Docs, database: Arc<Database>, namespace: NamespaceId) -> Result<(), String> {
    let doc = docs
        .open(namespace)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("the folder is not on this device")?;
    // Subscribe first, so a sync that fails straight away is seen.
    let mut events = doc.subscribe().await.map_err(|error| error.to_string())?;
    sync_with_members(&doc, &database, namespace).await;

    let (due, mut retries) = mpsc::unbounded_channel::<EndpointId>();
    let mut failures: HashMap<EndpointId, u32> = HashMap::new();
    let mut waiting: HashSet<EndpointId> = HashSet::new();
    let mut resync = tokio::time::interval_at(
        tokio::time::Instant::now() + RESYNC_INTERVAL,
        RESYNC_INTERVAL,
    );
    let mut neighbors: HashSet<EndpointId> = HashSet::new();
    let mut alone_wait = ALONE_FIRST;
    let alone = tokio::time::sleep(alone_wait);
    tokio::pin!(alone);
    loop {
        tokio::select! {
            () = &mut alone => {
                if neighbors.is_empty() && sync_with_members(&doc, &database, namespace).await > 0 {
                    alone_wait = (alone_wait * 2).min(RESYNC_INTERVAL);
                } else {
                    alone_wait = ALONE_FIRST;
                }
                alone.as_mut().reset(tokio::time::Instant::now() + alone_wait);
            }
            event = events.next() => match event {
                Some(Ok(LiveEvent::NeighborUp(peer))) => {
                    neighbors.insert(peer);
                    remember(&database, namespace, peer);
                }
                Some(Ok(LiveEvent::NeighborDown(peer))) => {
                    neighbors.remove(&peer);
                }
                Some(Ok(LiveEvent::SyncFinished(sync))) => match sync.result {
                    Ok(_) => {
                        failures.remove(&sync.peer);
                        remember(&database, namespace, sync.peer);
                    }
                    // The entries were exchanged; only closing the connection failed. iroh-docs
                    // reports that often, when the other side hangs up first, and a retry would
                    // end the same way. The resync below catches whatever it missed.
                    Err(error) if error.starts_with("Failed to close") => {
                        failures.remove(&sync.peer);
                        remember(&database, namespace, sync.peer);
                    }
                    Err(error) => {
                        let count = failures.entry(sync.peer).or_default();
                        *count += 1;
                        if waiting.insert(sync.peer) {
                            let delay = retry_delay(*count);
                            debug!(peer = %sync.peer.fmt_short(), %error, ?delay, "a folder sync failed; trying again");
                            let due = due.clone();
                            let peer = sync.peer;
                            tokio::spawn(async move {
                                tokio::time::sleep(delay).await;
                                let _ = due.send(peer);
                            });
                        }
                    }
                },
                Some(_) => {}
                None => return Err("the folder's events ended".to_owned()),
            },
            Some(peer) = retries.recv() => {
                waiting.remove(&peer);
                if let Err(error) = doc.start_sync(vec![EndpointAddr::new(peer)]).await {
                    warn!(%error, "could not sync a shared folder again");
                }
            }
            _ = resync.tick() => {
                sync_with_members(&doc, &database, namespace).await;
            }
        }
    }
}

/// Starts syncing with every member the folder knows, and rejoins its gossip through them.
/// iroh-docs adds the members it remembers. Returns how many members this device knows.
async fn sync_with_members(doc: &Doc, database: &Database, namespace: NamespaceId) -> usize {
    let members = match database.folder_peers(namespace) {
        Ok(members) => members,
        Err(error) => {
            warn!(%error, "could not read a shared folder's members");
            Vec::new()
        }
    };
    let known = members.len();
    let addrs = members.into_iter().map(EndpointAddr::new).collect();
    if let Err(error) = doc.start_sync(addrs).await {
        warn!(%error, "could not sync a shared folder");
    }
    known
}

fn remember(database: &Database, namespace: NamespaceId, peer: EndpointId) {
    if let Err(error) = database.record_folder_peer(namespace, peer, contacts::now()) {
        warn!(%error, "could not record a shared folder's member");
    }
}

/// How long to wait after `failures` failed syncs in a row.
fn retry_delay(failures: u32) -> Duration {
    RETRY_FIRST
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(RETRY_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_after_each_failure_up_to_five_minutes() {
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(2), Duration::from_secs(4));
        assert_eq!(retry_delay(5), Duration::from_secs(32));
        assert_eq!(retry_delay(9), RETRY_MAX);
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
    }
}
