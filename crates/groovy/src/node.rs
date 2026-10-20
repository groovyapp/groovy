//! Runs the Groovy node for the life of the app and forwards its events to the UI.

use std::time::Duration;

use gpui::{App, AppContext, Global};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use groovy_core::protocol::FileOffer;
use groovy_core::transfers::Transfer;
use groovy_core::{
    ContactGate, Database, EndpointAddr, EndpointId, Network, Node, NodeConfig, NodeEvent,
    NodeHandle, PeerInfo, PeerProfile, Route, Tracker, Trust, relay_url,
};
use tracing::{error, info, warn};

use crate::data::AppData;

/// How often content nothing references is removed from the blob store.
const GARBAGE_INTERVAL: Duration = Duration::from_secs(60);

/// How long quitting waits for the node to stop before exiting anyway.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Shortens an endpoint ID for display: its first and last few characters.
pub fn abbreviate_id(id: &str) -> String {
    const KEEP: usize = 8;
    if id.len() <= KEEP * 2 + 1 {
        return id.to_owned();
    }
    format!("{}…{}", &id[..KEEP], &id[id.len() - KEEP..])
}

/// Where the node is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeStatus {
    Starting,
    Online,
    Offline,
}

/// The node's lifecycle and reachability, as seen from the UI thread.
pub struct NodeState {
    status: NodeStatus,
    endpoint_id: EndpointId,
    addr: Option<EndpointAddr>,
    /// Kept so the UI can publish profile changes to peers.
    handle: Option<NodeHandle>,
    /// Pairing requests waiting for the user, oldest first.
    pairings: Vec<PairingRequest>,
    /// Trusted contacts that answered their last check, and how they are reached.
    routes: HashMap<EndpointId, Route>,
    /// Accepted transfers, by id, as the node last reported them.
    transfers: BTreeMap<i64, Transfer>,
    /// How fast each transfer has been moving, from its last few reports.
    speeds: HashMap<i64, Speed>,
    /// The files each received transfer saved, where they are on disk.
    saved: HashMap<i64, Vec<PathBuf>>,
    /// Offers from trusted peers that the user has not answered, oldest first.
    offers: Vec<FileOfferRequest>,
}

/// A trusted peer offering files, waiting for the user to accept or decline.
#[derive(Debug, Clone)]
pub struct FileOfferRequest {
    pub peer: PeerInfo,
    pub offer: FileOffer,
}

/// The bytes a transfer had when it was last sampled, and its speed since then.
struct Speed {
    done: u64,
    at: Instant,
    bytes_per_second: f64,
}

/// How often a transfer's speed is worked out, at most.
const SPEED_INTERVAL: Duration = Duration::from_secs(1);

/// A peer asking to pair, and the code the user compares with theirs.
#[derive(Debug, Clone)]
pub struct PairingRequest {
    pub peer: PeerInfo,
    pub code: String,
}

impl Global for NodeState {}

impl NodeState {
    /// The node's latest reachable address, once it has been reported.
    pub fn addr(&self) -> Option<&EndpointAddr> {
        self.addr.as_ref()
    }

    /// Pairing requests waiting for the user, oldest first.
    pub fn pairings(&self) -> &[PairingRequest] {
        &self.pairings
    }

    /// Every accepted transfer, oldest first.
    pub fn transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers.values()
    }

    /// How fast a transfer is moving, in bytes per second, once it has been sampled.
    pub fn speed(&self, id: i64) -> Option<f64> {
        self.speeds.get(&id).map(|speed| speed.bytes_per_second)
    }

    /// The files a received transfer saved, where they are on disk.
    pub fn saved_files(&self, id: i64) -> &[PathBuf] {
        self.saved.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// File offers waiting for the user, oldest first.
    pub fn offers(&self) -> &[FileOfferRequest] {
        &self.offers
    }

    /// Stores a transfer's latest state, and works out its speed when it has moved on.
    fn record_transfer(&mut self, transfer: Transfer) {
        let now = Instant::now();
        let speed = self.speeds.entry(transfer.id).or_insert(Speed {
            done: transfer.done,
            at: now,
            bytes_per_second: 0.,
        });
        let elapsed = now.duration_since(speed.at);
        if elapsed >= SPEED_INTERVAL && transfer.done >= speed.done {
            speed.bytes_per_second = (transfer.done - speed.done) as f64 / elapsed.as_secs_f64();
            speed.done = transfer.done;
            speed.at = now;
        }
        self.transfers.insert(transfer.id, transfer);
    }

    /// How a trusted contact is reached, if it answered its last check. `None` means
    /// it is not reachable now.
    pub fn route(&self, peer: &EndpointId) -> Option<Route> {
        self.routes.get(peer).copied()
    }

    fn set_route(&mut self, peer: EndpointId, route: Option<Route>) {
        match route {
            Some(route) => self.routes.insert(peer, route),
            None => self.routes.remove(&peer),
        };
    }

    /// The handle for talking to the node, once it has started.
    pub fn handle(&self) -> Option<NodeHandle> {
        self.handle.clone()
    }

    /// Tells peers about a change to this node's profile.
    fn publish(&self, profile: PeerProfile) {
        if let Some(handle) = &self.handle {
            handle.set_profile(profile);
        }
    }

    pub fn status(&self) -> NodeStatus {
        self.status
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint_id
    }

    /// The relay the node is reachable through, once its address is known.
    pub fn relay(&self) -> Option<String> {
        self.addr.as_ref().and_then(relay_url)
    }
}

/// How often the presence loop wakes to check whether any contact is due.
const PRESENCE_TICK: Duration = Duration::from_secs(15);

/// Checks trusted contacts for presence on a schedule, and records how each is reached.
fn spawn_presence(
    cx: &mut App,
    handle: NodeHandle,
    database: Arc<Database>,
    network_changes: Arc<AtomicU64>,
) {
    cx.spawn(async move |cx| {
        let mut tracker = Tracker::default();
        let mut seen_changes = 0;
        loop {
            cx.background_executor().timer(PRESENCE_TICK).await;

            // A change of address means the network changed, so recheck every contact now.
            let changes = network_changes.load(Ordering::SeqCst);
            if changes != seen_changes {
                seen_changes = changes;
                tracker.reset_all(Instant::now());
            }

            let db = database.clone();
            let contacts = cx
                .background_executor()
                .spawn(async move { db.contacts() })
                .await;
            let Ok(contacts) = contacts else {
                continue;
            };
            let trusted: Vec<EndpointId> = contacts
                .iter()
                .filter(|contact| contact.trust == Trust::Trusted)
                .map(|contact| contact.endpoint_id)
                .collect();
            tracker.retain(&trusted);
            tracker.track(trusted, Instant::now());

            for peer in tracker.due(Instant::now()) {
                // On the public network, the ID is enough to find the peer.
                let result = handle.check_presence(EndpointAddr::from(peer)).await;
                tracker.record(peer, result.is_ok(), Instant::now());
                let route = result.ok();
                cx.update(|cx| {
                    if cx.has_global::<NodeState>() {
                        cx.global_mut::<NodeState>().set_route(peer, route);
                        cx.refresh_windows();
                    }
                });
            }
        }
    })
    .detach();
}

/// Answers a pairing request. The request is removed at once, and the answer is
/// sent in the background.
pub fn answer_pairing(cx: &mut App, peer: EndpointId, accept: bool) {
    if !cx.has_global::<NodeState>() {
        return;
    }
    let state = cx.global_mut::<NodeState>();
    state
        .pairings
        .retain(|request| request.peer.endpoint_id != peer);
    let Some(handle) = state.handle.clone() else {
        return;
    };
    cx.refresh_windows();
    cx.spawn(async move |_| {
        if let Err(error) = handle.answer_pairing(peer, accept).await {
            warn!(%error, "could not answer a pairing request");
        }
    })
    .detach();
}

/// Accepts or declines a file offer. The offer is removed at once, and the answer is sent
/// in the background.
pub fn answer_offer(cx: &mut App, peer: EndpointId, accept: bool) {
    if !cx.has_global::<NodeState>() {
        return;
    }
    let state = cx.global_mut::<NodeState>();
    state
        .offers
        .retain(|request| request.peer.endpoint_id != peer);
    let Some(handle) = state.handle.clone() else {
        return;
    };
    cx.refresh_windows();
    cx.spawn(async move |_| {
        if let Err(error) = handle.answer_offer(peer, accept).await {
            warn!(%error, "could not answer a file offer");
        }
    })
    .detach();
}

/// Tells peers about a change to this node's profile. Does nothing if the node is not running.
pub fn publish_profile(cx: &App, profile: PeerProfile) {
    if let Some(state) = cx.try_global::<NodeState>() {
        state.publish(profile);
    }
}

/// Starts the node with the identity in [`AppData`], once. Called at launch
/// when onboarding is done, and when onboarding finishes. A node that cannot
/// start is recorded as offline, so the status bar still has something to show.
pub fn start(cx: &mut App) {
    if cx.has_global::<NodeState>() {
        return;
    }
    let secret_key = cx.global::<AppData>().identity.secret_key().clone();
    let endpoint_id = secret_key.public();
    let database = cx.global::<AppData>().database.clone();

    let (handle, events) = match Node::start(NodeConfig {
        secret_key,
        network: Network::Public,
        // Replaced below with the stored profile, which is read in the background.
        profile: PeerProfile::default(),
        gate: Arc::new(ContactGate::new(database.clone())),
        discovery: cx.global::<AppData>().config.discovery,
        relay: cx.global::<AppData>().config.relay.clone(),
        blobs: Some(cx.global::<AppData>().dir.blobs_dir()),
        downloads: groovy_core::files::default_downloads(),
        database: Some(cx.global::<AppData>().database.clone()),
        garbage_interval: Some(GARBAGE_INTERVAL),
        user_keys: Some(Arc::new(groovy_core::user::StoredUserKey::new(
            cx.global::<AppData>().dir.clone(),
            crate::APP_NAME,
        ))),
    }) {
        Ok(started) => started,
        Err(error) => {
            error!(%error, "the Groovy node could not start");
            cx.set_global(NodeState {
                status: NodeStatus::Offline,
                endpoint_id,
                addr: None,
                handle: None,
                pairings: Vec::new(),
                routes: HashMap::new(),
                transfers: BTreeMap::new(),
                speeds: HashMap::new(),
                saved: HashMap::new(),
                offers: Vec::new(),
            });
            return;
        }
    };
    cx.set_global(NodeState {
        status: NodeStatus::Starting,
        endpoint_id,
        addr: None,
        handle: Some(handle.clone()),
        pairings: Vec::new(),
        routes: HashMap::new(),
        transfers: handle
            .transfers()
            .into_iter()
            .map(|transfer| (transfer.id, transfer))
            .collect(),
        speeds: HashMap::new(),
        saved: HashMap::new(),
        offers: Vec::new(),
    });

    let network_changes = Arc::new(AtomicU64::new(0));
    spawn_presence(
        cx,
        handle.clone(),
        database.clone(),
        network_changes.clone(),
    );

    // Publish the stored profile once it has been read.
    let database = cx.global::<AppData>().database.clone();
    let loaded = cx.background_spawn(async move { database.profile() });
    cx.spawn(async move |cx| {
        if let Ok(Some(profile)) = loaded.await {
            cx.update(|cx| publish_profile(cx, PeerProfile::from_profile(&profile)));
        }
    })
    .detach();

    cx.spawn(async move |cx| {
        let mut events = events;
        while let Some(event) = events.next().await {
            info!(?event, "node event");
            if matches!(event, NodeEvent::AddressChanged(_)) {
                network_changes.fetch_add(1, Ordering::SeqCst);
            }
            cx.update(|cx| {
                let state = cx.global_mut::<NodeState>();
                match event {
                    NodeEvent::Started => state.status = NodeStatus::Online,
                    NodeEvent::AddressChanged(addr) => state.addr = Some(addr),
                    NodeEvent::Stopped => state.status = NodeStatus::Offline,
                    NodeEvent::PairingRequested { peer, code } => {
                        state.pairings.push(PairingRequest { peer, code });
                    }
                    NodeEvent::FileOffered { peer, offer } => {
                        state.offers.push(FileOfferRequest { peer, offer });
                    }
                    NodeEvent::OfferWithdrawn { peer } => {
                        state
                            .offers
                            .retain(|request| request.peer.endpoint_id != peer);
                    }
                    NodeEvent::TransferChanged(transfer) => state.record_transfer(transfer),
                    NodeEvent::FileReceived {
                        transfer, saved, ..
                    } => {
                        state.saved.insert(transfer, saved);
                    }
                    // A failed receive shows as a failed transfer, which the transfer list
                    // already reports.
                    NodeEvent::ReceiveFailed { .. } | NodeEvent::ImportProgress { .. } => {}
                    // Peers are shown once the Peers screen exists (#25).
                    NodeEvent::PeerConnected(_) | NodeEvent::PeerDisconnected(_) => {}
                }
                cx.refresh_windows();
            });
        }
    })
    .detach();

    cx.on_app_quit(move |_cx| {
        if !handle.shutdown(SHUTDOWN_TIMEOUT) {
            warn!("Groovy node did not stop before the app exited");
        }
        async {}
    })
    .detach();
}
