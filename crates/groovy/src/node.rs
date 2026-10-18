//! Runs the Groovy node for the life of the app and forwards its events to the UI.

use std::time::Duration;

use gpui::{App, Global};
use groovy_core::{EndpointAddr, EndpointId, EventStream, NodeEvent, NodeHandle, relay_url};
use tracing::{info, warn};

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
}

impl Global for NodeState {}

impl NodeState {
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

/// Registers the started node as a global, forwards its events to the UI, and
/// stops it when the app quits.
pub fn init(cx: &mut App, handle: NodeHandle, mut events: EventStream) {
    cx.set_global(NodeState {
        status: NodeStatus::Starting,
        endpoint_id: handle.endpoint_id(),
        addr: None,
    });

    cx.spawn(async move |cx| {
        while let Some(event) = events.next().await {
            info!(?event, "node event");
            cx.update(|cx| {
                let state = cx.global_mut::<NodeState>();
                match event {
                    NodeEvent::Started => state.status = NodeStatus::Online,
                    NodeEvent::AddressChanged(addr) => state.addr = Some(addr),
                    NodeEvent::Stopped => state.status = NodeStatus::Offline,
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
