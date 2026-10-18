//! Runs the Groovy node for the life of the app and forwards its events to the UI.

use std::time::Duration;

use gpui::{App, Global};
use groovy_core::{Node, NodeEvent};
use tracing::{info, warn};

/// How long quitting waits for the node to stop before exiting anyway.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// The node's lifecycle as seen from the UI thread.
pub struct NodeState {
    running: bool,
}

impl Global for NodeState {}

impl NodeState {
    /// Whether the node has started and not yet stopped.
    #[expect(dead_code, reason = "read by the status bar (#10)")]
    pub fn is_running(&self) -> bool {
        self.running
    }
}

/// Starts the node, registers it as a global and stops it when the app quits.
pub fn init(cx: &mut App) {
    let (handle, mut events) = Node::start().expect("failed to start the Groovy node");
    cx.set_global(NodeState { running: false });

    cx.spawn(async move |cx| {
        while let Some(event) = events.next().await {
            info!(?event, "node event");
            let running = event == NodeEvent::Started;
            cx.update(|cx| {
                cx.global_mut::<NodeState>().running = running;
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
