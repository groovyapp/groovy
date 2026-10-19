//! Runs the Groovy node for the life of the app and forwards its events to the UI.

use std::time::Duration;

use gpui::{App, AppContext, Global};
use groovy_core::{
    EndpointAddr, EndpointId, Network, Node, NodeConfig, NodeEvent, NodeHandle, PeerProfile,
    relay_url,
};
use tracing::{error, info, warn};

use crate::data::AppData;

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
}

impl Global for NodeState {}

impl NodeState {
    /// The node's latest reachable address, once it has been reported.
    pub fn addr(&self) -> Option<&EndpointAddr> {
        self.addr.as_ref()
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

    let (handle, events) = match Node::start(NodeConfig {
        secret_key,
        network: Network::Public,
        // Replaced below with the stored profile, which is read in the background.
        profile: PeerProfile::default(),
    }) {
        Ok(started) => started,
        Err(error) => {
            error!(%error, "the Groovy node could not start");
            cx.set_global(NodeState {
                status: NodeStatus::Offline,
                endpoint_id,
                addr: None,
                handle: None,
            });
            return;
        }
    };
    cx.set_global(NodeState {
        status: NodeStatus::Starting,
        endpoint_id,
        addr: None,
        handle: Some(handle.clone()),
    });

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
            cx.update(|cx| {
                let state = cx.global_mut::<NodeState>();
                match event {
                    NodeEvent::Started => state.status = NodeStatus::Online,
                    NodeEvent::AddressChanged(addr) => state.addr = Some(addr),
                    NodeEvent::Stopped => state.status = NodeStatus::Offline,
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
