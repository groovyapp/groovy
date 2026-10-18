//! The node runs Groovy's async services on a dedicated thread, so callers
//! never block on them. It owns an iroh endpoint and a router for Groovy's
//! protocols. Callers talk to it through a [`NodeHandle`] and receive its
//! lifecycle through an [`EventStream`].

use std::io;
use std::net::Ipv4Addr;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use iroh::endpoint::{BindError, Connection, InvalidSocketAddr, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, TransportAddr, Watcher};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{info, warn};

const COMMAND_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 64;
const WORKER_THREADS: usize = 2;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The ALPN for Groovy's protocol. Connections offering any other ALPN are refused.
pub const GROOVY_ALPN: &[u8] = b"groovy/0";

/// Errors from starting the node or sending it a command.
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("failed to build the async runtime")]
    Runtime(#[source] io::Error),
    #[error("failed to start the node thread")]
    Spawn(#[source] io::Error),
    #[error("failed to bind the network endpoint")]
    Bind(#[source] BindError),
    #[error("invalid bind address")]
    InvalidBindAddress(#[source] InvalidSocketAddr),
    #[error("the node has stopped")]
    Stopped,
}

/// Which networks the node uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// n0's public relays and address lookup. Use this in the app.
    Public,
    /// Loopback only, with no relays or address lookup. Peers must be reached
    /// by their full address. Use this in tests.
    LocalOnly,
}

/// What a node needs to start.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// The node's identity. Its public key is the endpoint ID peers dial.
    pub secret_key: SecretKey,
    pub network: Network,
}

/// Lifecycle events published by the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeEvent {
    Started,
    /// The node's reachable addresses changed. Covers the home relay and direct addresses.
    AddressChanged(EndpointAddr),
    Stopped,
}

enum Command {
    Ping(oneshot::Sender<()>),
    Addr(oneshot::Sender<EndpointAddr>),
}

/// The relay URL the node is reachable through, if it has a home relay.
pub fn relay_url(addr: &EndpointAddr) -> Option<String> {
    addr.addrs.iter().find_map(|transport| match transport {
        TransportAddr::Relay(url) => Some(url.to_string()),
        _ => None,
    })
}

/// Entry point for starting a node.
pub struct Node;

impl Node {
    /// Starts the node on its own thread and returns a handle for sending it
    /// commands, plus the stream of its events. Returns an error if the
    /// endpoint cannot be bound.
    pub fn start(config: NodeConfig) -> Result<(NodeHandle, EventStream), NodeError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("groovy-node")
            .enable_all()
            .build()
            .map_err(NodeError::Runtime)?;

        let endpoint_id = config.secret_key.public();
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Subscribe before the thread starts so `Started` is never missed.
        let (event_tx, event_rx) = broadcast::channel(EVENT_CAPACITY);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);

        let thread = thread::Builder::new()
            .name("groovy-node".into())
            .spawn(move || {
                runtime.block_on(run(config, command_rx, shutdown_rx, event_tx, ready_tx))
            })
            .map_err(NodeError::Spawn)?;

        // Wait until the endpoint is bound, so a bind failure is returned here.
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(NodeError::Stopped);
            }
        }

        let handle = NodeHandle {
            endpoint_id,
            commands: command_tx,
            shutdown: Arc::new(Mutex::new(Some(shutdown_tx))),
            thread: Arc::new(Mutex::new(Some(thread))),
        };
        Ok((handle, EventStream { receiver: event_rx }))
    }
}

async fn bind(config: &NodeConfig) -> Result<Endpoint, NodeError> {
    let builder = match config.network {
        Network::Public => Endpoint::builder(presets::N0),
        Network::LocalOnly => Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .map_err(NodeError::InvalidBindAddress)?,
    };
    builder
        .secret_key(config.secret_key.clone())
        .bind()
        .await
        .map_err(NodeError::Bind)
}

async fn run(
    config: NodeConfig,
    mut commands: mpsc::Receiver<Command>,
    shutdown: oneshot::Receiver<()>,
    events: broadcast::Sender<NodeEvent>,
    ready: std_mpsc::SyncSender<Result<(), NodeError>>,
) {
    let endpoint = match bind(&config).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let router = Router::builder(endpoint.clone())
        .accept(GROOVY_ALPN, HoldOpen)
        .spawn();
    let mut addresses = endpoint.watch_addr();

    info!(endpoint_id = %endpoint.id(), "node started");
    let _ = events.send(NodeEvent::Started);
    let _ = ready.send(Ok(()));

    tokio::pin!(shutdown);
    let mut watching = true;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Ping(reply)) => {
                    let _ = reply.send(());
                }
                Some(Command::Addr(reply)) => {
                    let _ = reply.send(endpoint.addr());
                }
                // Every handle has been dropped.
                None => break,
            },
            // Shutdown was requested, or its sender was dropped.
            _ = &mut shutdown => break,
            update = addresses.updated(), if watching => match update {
                Ok(addr) => {
                    let _ = events.send(NodeEvent::AddressChanged(addr));
                }
                Err(_) => {
                    warn!("address watcher closed; no more address events");
                    watching = false;
                }
            },
        }
    }

    if let Err(error) = router.shutdown().await {
        warn!(%error, "router did not shut down cleanly");
    }
    endpoint.close().await;
    info!("node stopped");
    let _ = events.send(NodeEvent::Stopped);
}

/// Placeholder for Groovy's protocol handler, which lands in #17. It keeps a
/// connection open until the peer closes it.
#[derive(Debug)]
struct HoldOpen;

impl ProtocolHandler for HoldOpen {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        connection.closed().await;
        Ok(())
    }
}

/// A cheaply cloneable handle to a running node.
///
/// The node stops when shutdown is requested or when the last handle is dropped.
#[derive(Clone)]
pub struct NodeHandle {
    endpoint_id: EndpointId,
    commands: mpsc::Sender<Command>,
    shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl NodeHandle {
    /// This node's endpoint ID, which is its public key.
    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint_id
    }

    /// The node's current address, including its home relay and direct addresses.
    pub async fn addr(&self) -> Result<EndpointAddr, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Addr(reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)
    }

    /// Round-trips a command through the node, proving it is responsive.
    pub async fn ping(&self) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Ping(reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)
    }

    /// Asks the node to stop and waits up to `timeout` for its thread to finish.
    /// Returns `true` if it stopped in time. Blocks the calling thread, so it
    /// is for use at shutdown, not in a UI event loop.
    ///
    /// Only the first call waits. Later calls return `true` straight away.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let signal = self.shutdown.lock().expect("shutdown lock poisoned").take();
        if let Some(signal) = signal {
            let _ = signal.send(());
        }

        let Some(thread) = self.thread.lock().expect("thread lock poisoned").take() else {
            return true;
        };

        let deadline = Instant::now() + timeout;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                warn!(?timeout, "node did not stop in time; detaching its thread");
                return false;
            }
            thread::sleep(POLL_INTERVAL);
        }
        if thread.join().is_err() {
            warn!("node thread panicked");
        }
        true
    }
}

/// Lifecycle events from a node. Yields `None` once the node has stopped and
/// all of its events have been read.
pub struct EventStream {
    receiver: broadcast::Receiver<NodeEvent>,
}

impl EventStream {
    /// Waits for the next event. Events skipped because this stream fell
    /// behind are logged and passed over.
    pub async fn next(&mut self) -> Option<NodeEvent> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "node event stream lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use iroh::endpoint::presets::Minimal;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(3);

    fn local_config() -> NodeConfig {
        NodeConfig {
            secret_key: SecretKey::generate(),
            network: Network::LocalOnly,
        }
    }

    /// Starts a node and waits for its `Started` event.
    async fn start(config: NodeConfig) -> (NodeHandle, EventStream) {
        let (handle, mut events) = Node::start(config).expect("node starts");
        assert_eq!(events.next().await, Some(NodeEvent::Started));
        (handle, events)
    }

    #[tokio::test]
    async fn starts_responds_and_stops() {
        let (handle, mut events) = start(local_config()).await;

        handle.ping().await.expect("node answers a ping");

        assert!(handle.shutdown(TIMEOUT), "node stops in time");
        assert_eq!(events.next().await, Some(NodeEvent::Stopped));
        assert_eq!(events.next().await, None);
    }

    #[tokio::test]
    async fn endpoint_id_is_the_identity_public_key() {
        let config = local_config();
        let expected = config.secret_key.public();
        let (handle, _events) = start(config).await;

        assert_eq!(handle.endpoint_id(), expected);
        assert!(handle.shutdown(TIMEOUT));
    }

    #[tokio::test]
    async fn address_includes_a_bound_socket() {
        let (handle, _events) = start(local_config()).await;

        let addr = handle.addr().await.expect("node reports its address");

        assert_eq!(addr.id, handle.endpoint_id());
        assert!(!addr.addrs.is_empty(), "{addr:?}");
        assert!(handle.shutdown(TIMEOUT));
    }

    #[tokio::test]
    async fn two_nodes_accept_connections_by_address() {
        let (node_a, _events_a) = start(local_config()).await;
        let (node_b, _events_b) = start(local_config()).await;
        let addr_b = node_b.addr().await.unwrap();

        let client = Endpoint::builder(Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .expect("client binds");
        let connection = client
            .connect(addr_b, GROOVY_ALPN)
            .await
            .expect("node B accepts the Groovy ALPN");
        connection.close(0u32.into(), b"done");

        client.close().await;
        assert!(node_a.shutdown(TIMEOUT));
        assert!(node_b.shutdown(TIMEOUT));
    }

    #[tokio::test]
    async fn ping_fails_after_shutdown() {
        let (handle, _events) = start(local_config()).await;
        assert!(handle.shutdown(TIMEOUT));

        assert!(matches!(handle.ping().await, Err(NodeError::Stopped)));
    }

    #[tokio::test]
    async fn shutdown_can_be_called_twice() {
        let (handle, _events) = start(local_config()).await;
        assert!(handle.shutdown(TIMEOUT));
        assert!(handle.shutdown(TIMEOUT));
    }

    #[tokio::test]
    async fn dropping_every_handle_stops_the_node() {
        let (handle, mut events) = start(local_config()).await;
        drop(handle);

        assert_eq!(events.next().await, Some(NodeEvent::Stopped));
    }
}
