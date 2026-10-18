//! Runs the Groovy node for the life of the app and forwards its events to the UI.

use std::time::Duration;

use gpui::{App, Global};
use groovy_core::{EndpointAddr, EventStream, NodeEvent, NodeHandle};
use tracing::{info, warn};

/// How long quitting waits for the node to stop before exiting anyway.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// The node's lifecycle and reachable address, as seen from the UI thread.
pub struct NodeState {
    running: bool,
    addr: Option<EndpointAddr>,
}

impl Global for NodeState {}

impl NodeState {
    /// Whether the node has started and not yet stopped.
    #[expect(dead_code, reason = "read by the status bar (#10)")]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// The node's latest address, once it has been reported.
    #[expect(dead_code, reason = "read by the status bar (#10)")]
    pub fn addr(&self) -> Option<&EndpointAddr> {
        self.addr.as_ref()
    }
}

/// Registers the started node as a global, forwards its events to the UI, and
/// stops it when the app quits.
pub fn init(cx: &mut App, handle: NodeHandle, mut events: EventStream) {
    cx.set_global(NodeState {
        running: false,
        addr: None,
    });

    cx.spawn(async move |cx| {
        while let Some(event) = events.next().await {
            info!(?event, "node event");
            cx.update(|cx| {
                let state = cx.global_mut::<NodeState>();
                match event {
                    NodeEvent::Started => state.running = true,
                    NodeEvent::AddressChanged(addr) => state.addr = Some(addr),
                    NodeEvent::Stopped => state.running = false,
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
