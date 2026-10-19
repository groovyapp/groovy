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

use std::collections::HashMap;
use std::path::PathBuf;

use iroh::endpoint::{BindError, Connection, InvalidSocketAddr, RecvStream, SendStream, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, SecretKey, TransportAddr, Watcher,
};
use iroh_blobs::{BlobsProtocol, Hash, HashAndFormat, store::fs::FsStore};
use iroh_mainline_address_lookup::DhtAddressLookup;
use iroh_mdns_address_lookup::MdnsAddressLookup;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{info, warn};

use crate::blobs::TrustedBlobs;
use crate::config::{Discovery, RelaySetting};
use crate::files;
use crate::pairing::{RateLimiter, verification_code};
use crate::presence::{Route, route_of};
use crate::protocol::{self, FileOffer, PeerInfo, PeerProfile, ProtocolError, Reply, Request};

const COMMAND_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 64;
const WORKER_THREADS: usize = 2;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The ALPN for Groovy's protocol. Connections offering any other ALPN are refused.
pub const GROOVY_ALPN: &[u8] = protocol::ALPN;

/// The profile shared with peers, shared between the node's tasks and its handle.
type SharedProfile = Arc<Mutex<PeerProfile>>;

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
    #[error("blob storage failed: {0}")]
    Blobs(String),
    #[error("blob storage is turned off for this node")]
    BlobsDisabled,
    #[error("the relay setting is invalid: {0}")]
    InvalidRelay(String),
    #[error("the peer could not be reached")]
    Connect(#[source] ProtocolError),
    #[error("this peer is blocked")]
    Blocked,
    #[error("the other person declined the pairing")]
    PairingDeclined,
    #[error("there is no pairing request from that peer to answer")]
    NoPendingPairing,
    #[error("the other person declined the files")]
    OfferDeclined,
    #[error("there are no offered files from that peer to answer")]
    NoPendingOffer,
    #[error("sending or receiving the files failed: {0}")]
    Files(String),
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

/// Decides which peers may connect, and hears about each handshake.
///
/// `may_connect` is asked before a connection is accepted or dialed, so a refused
/// peer never reaches a protocol. `handshook` is told about each peer that
/// completes the handshake, in either direction.
pub trait PeerGate: Send + Sync + 'static {
    fn may_connect(&self, peer: &EndpointId) -> bool;
    fn handshook(&self, peer: &PeerInfo);
    /// Whether the user has paired with `peer`. A trusted peer may send any request.
    fn is_trusted(&self, peer: &EndpointId) -> bool;
    /// Records that the user has paired with `peer`.
    fn pair(&self, peer: &EndpointId);
}

/// A gate that lets every peer in and ignores handshakes.
pub struct AllowAll;

impl PeerGate for AllowAll {
    fn may_connect(&self, _peer: &EndpointId) -> bool {
        true
    }

    fn handshook(&self, _peer: &PeerInfo) {}

    fn is_trusted(&self, _peer: &EndpointId) -> bool {
        false
    }

    fn pair(&self, _peer: &EndpointId) {}
}

/// What a node needs to start.
#[derive(Clone)]
pub struct NodeConfig {
    /// The node's identity. Its public key is the endpoint ID peers dial.
    pub secret_key: SecretKey,
    pub network: Network,
    /// What peers see about this node. Change it later with [`NodeHandle::set_profile`].
    pub profile: PeerProfile,
    /// Decides which peers may connect. Use [`AllowAll`] where nothing is blocked.
    pub gate: Arc<dyn PeerGate>,
    /// Extra ways to find peers, beyond their endpoint ID. Used on the public network.
    pub discovery: Discovery,
    /// Which relays to use. Used on the public network.
    pub relay: RelaySetting,
    /// Where blobs are stored. `None` turns blob storage off.
    pub blobs: Option<std::path::PathBuf>,
}

/// Lifecycle events published by the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeEvent {
    Started,
    /// The node's reachable addresses changed. Covers the home relay and direct addresses.
    AddressChanged(EndpointAddr),
    /// A peer finished the handshake, either because it dialed us or because we dialed it.
    PeerConnected(PeerInfo),
    /// A connection to a peer closed.
    PeerDisconnected(EndpointId),
    /// A peer asks to pair. The user compares `code` with the peer's and answers.
    PairingRequested {
        peer: PeerInfo,
        code: String,
    },
    /// A trusted peer offers files. The user accepts or declines them.
    FileOffered {
        peer: PeerInfo,
        offer: FileOffer,
    },
    /// Files are being imported to send: `done` of `total` are in the store.
    ImportProgress {
        done: u64,
        total: u64,
    },
    /// The files from an accepted offer are now in this node's store.
    FileReceived {
        peer: EndpointId,
        names: Vec<String>,
        root: Hash,
    },
    Stopped,
}

enum Command {
    Ping(oneshot::Sender<()>),
    Addr(oneshot::Sender<EndpointAddr>),
    Connect(EndpointAddr, oneshot::Sender<Result<PeerInfo, NodeError>>),
    RequestPairing(EndpointAddr, oneshot::Sender<Result<PeerInfo, NodeError>>),
    AnswerPairing(EndpointId, bool, oneshot::Sender<Result<(), NodeError>>),
    SendFiles(
        EndpointAddr,
        Vec<PathBuf>,
        oneshot::Sender<Result<(), NodeError>>,
    ),
    AnswerOffer(EndpointId, bool, oneshot::Sender<Result<(), NodeError>>),
    CheckPresence(EndpointAddr, oneshot::Sender<Result<Route, NodeError>>),
    AddBlob(Vec<u8>, oneshot::Sender<Result<Hash, NodeError>>),
    ReadBlob(Hash, oneshot::Sender<Result<Option<Vec<u8>>, NodeError>>),
    ListCollection(
        Hash,
        oneshot::Sender<Result<Vec<(String, Hash)>, NodeError>>,
    ),
}

/// Pairing requests waiting for this node's user, keyed by the peer that sent them.
type Pending = Arc<std::sync::Mutex<HashMap<EndpointId, SendStream>>>;

/// File offers waiting for this node's user, keyed by the peer that sent them.
type PendingOffers = Arc<std::sync::Mutex<HashMap<EndpointId, (SendStream, FileOffer)>>>;

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
        let profile: SharedProfile = Arc::new(Mutex::new(config.profile.clone()));
        let profile_for_handle = profile.clone();
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Subscribe before the thread starts so `Started` is never missed.
        let (event_tx, event_rx) = broadcast::channel(EVENT_CAPACITY);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);

        let thread = thread::Builder::new()
            .name("groovy-node".into())
            .spawn(move || {
                runtime.block_on(run(
                    config,
                    profile,
                    command_rx,
                    shutdown_rx,
                    event_tx,
                    ready_tx,
                ))
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
            profile: profile_for_handle,
            commands: command_tx,
            shutdown: Arc::new(Mutex::new(Some(shutdown_tx))),
            thread: Arc::new(Mutex::new(Some(thread))),
        };
        Ok((handle, EventStream { receiver: event_rx }))
    }
}

async fn bind(config: &NodeConfig) -> Result<Endpoint, NodeError> {
    let builder = match config.network {
        Network::Public => {
            let mut builder = Endpoint::builder(presets::N0);
            builder = match &config.relay {
                RelaySetting::Public => builder,
                RelaySetting::Disabled => builder.relay_mode(RelayMode::Disabled),
                RelaySetting::Custom { urls, auth_token } => {
                    if urls.is_empty() {
                        return Err(NodeError::InvalidRelay(
                            "add at least one relay URL under [relay]".to_owned(),
                        ));
                    }
                    let mut map = RelayMap::try_from_iter(urls.iter().map(String::as_str))
                        .map_err(|error| NodeError::InvalidRelay(error.to_string()))?;
                    if let Some(token) = auth_token {
                        map = map.with_auth_token(token.clone());
                    }
                    builder.relay_mode(RelayMode::Custom(map))
                }
            };
            if config.discovery.local_network {
                builder = builder.address_lookup(MdnsAddressLookup::builder());
            }
            if config.discovery.dht {
                builder = builder.address_lookup(DhtAddressLookup::builder());
            }
            builder
        }
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
    profile: SharedProfile,
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
    let gate = config.gate.clone();
    let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let offers: PendingOffers = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let blobs = match &config.blobs {
        Some(dir) => match FsStore::load(dir).await {
            Ok(store) => Some(store),
            Err(error) => {
                let _ = ready.send(Err(NodeError::Blobs(error.to_string())));
                return;
            }
        },
        None => None,
    };
    let mut router = Router::builder(endpoint.clone()).accept(
        protocol::ALPN,
        GroovyProtocol {
            profile: profile.clone(),
            inbox: Inbox {
                own_id: endpoint.id(),
                gate: gate.clone(),
                events: events.clone(),
                pending: pending.clone(),
                offers: offers.clone(),
                limiter: Arc::new(std::sync::Mutex::new(RateLimiter::default())),
            },
        },
    );
    if let Some(store) = &blobs {
        let served = TrustedBlobs::new(BlobsProtocol::new(store, None), gate.clone());
        router = router.accept(iroh_blobs::ALPN, served);
    }
    let router = router.spawn();
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
                Some(Command::RequestPairing(addr, reply)) => {
                    let endpoint = endpoint.clone();
                    let profile = profile.clone();
                    let events = events.clone();
                    let gate = gate.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(
                            dial_to_pair(&endpoint, addr, &profile, &events, &gate).await,
                        );
                    });
                }
                Some(Command::AddBlob(data, reply)) => {
                    let result = match &blobs {
                        Some(store) => match store.blobs().add_bytes(data).await {
                            Ok(temp) => {
                                // A persistent tag keeps the blob across restarts and garbage collection.
                                let hash = temp.hash;
                                store
                                    .tags()
                                    .create(temp.hash_and_format())
                                    .await
                                    .map(|_| hash)
                                    .map_err(|error| NodeError::Blobs(error.to_string()))
                            }
                            Err(error) => Err(NodeError::Blobs(error.to_string())),
                        },
                        None => Err(NodeError::BlobsDisabled),
                    };
                    let _ = reply.send(result);
                }
                Some(Command::ReadBlob(hash, reply)) => {
                    let result = match &blobs {
                        Some(store) => match store.blobs().get_bytes(hash).await {
                            Ok(bytes) => Ok(Some(bytes.to_vec())),
                            Err(_) => Ok(None),
                        },
                        None => Err(NodeError::BlobsDisabled),
                    };
                    let _ = reply.send(result);
                }
                Some(Command::ListCollection(root, reply)) => {
                    let result = match &blobs {
                        Some(store) => files::list(store, root)
                            .await
                            .map_err(|error| NodeError::Files(error.to_string())),
                        None => Err(NodeError::BlobsDisabled),
                    };
                    let _ = reply.send(result);
                }
                Some(Command::CheckPresence(addr, reply)) => {
                    let endpoint = endpoint.clone();
                    let profile = profile.clone();
                    let gate = gate.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(check_presence(&endpoint, addr, &profile, &gate).await);
                    });
                }
                Some(Command::AnswerPairing(peer, accept, reply)) => {
                    let pending = pending.clone();
                    let gate = gate.clone();
                    let taken = pending.lock().ok().and_then(|mut map| map.remove(&peer));
                    tokio::spawn(async move {
                        let outcome = match taken {
                            None => Err(NodeError::NoPendingPairing),
                            Some(mut send) => {
                                let answer = if accept { Reply::Accepted } else { Reply::Declined };
                                reply_and_finish(&mut send, answer).await;
                                if accept {
                                    gate.pair(&peer);
                                }
                                Ok(())
                            }
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::SendFiles(addr, paths, reply)) => {
                    let endpoint = endpoint.clone();
                    let profile = profile.clone();
                    let events = events.clone();
                    let gate = gate.clone();
                    let store = blobs.clone();
                    tokio::spawn(async move {
                        let outcome = send_files(
                            &endpoint, addr, &profile, &events, &gate, store, paths,
                        )
                        .await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::AnswerOffer(peer, accept, reply)) => {
                    let taken = offers.lock().ok().and_then(|mut map| map.remove(&peer));
                    let endpoint = endpoint.clone();
                    let events = events.clone();
                    let store = blobs.clone();
                    tokio::spawn(async move {
                        let outcome = match taken {
                            None => Err(NodeError::NoPendingOffer),
                            Some((mut send, offer)) => {
                                answer_offer(&endpoint, &events, store, peer, &mut send, offer, accept)
                                    .await
                            }
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::Connect(addr, reply)) => {
                    // Dialing takes a network round trip, so it runs on its own task.
                    let endpoint = endpoint.clone();
                    let profile = profile.clone();
                    let events = events.clone();
                    let gate = gate.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(dial(&endpoint, addr, &profile, &events, &gate).await);
                    });
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

fn current_profile(profile: &SharedProfile) -> PeerProfile {
    profile
        .lock()
        .map(|profile| profile.clone())
        .unwrap_or_default()
}

fn connection_error(error: impl std::error::Error + Send + Sync + 'static) -> NodeError {
    NodeError::Connect(ProtocolError::Connection(Box::new(error)))
}

/// Dials `addr` and runs the handshake. The connection is returned open.
async fn open_peer(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    profile: &SharedProfile,
    events: &broadcast::Sender<NodeEvent>,
    gate: &Arc<dyn PeerGate>,
) -> Result<(Connection, PeerInfo), NodeError> {
    if !gate.may_connect(&addr.id) {
        return Err(NodeError::Blocked);
    }
    let connection = endpoint
        .connect(addr, protocol::ALPN)
        .await
        .map_err(|error| {
            // A peer on another protocol version does not offer our ALPN, so the TLS
            // handshake fails with a message that names the protocol.
            if error.to_string().contains("known protocol") {
                NodeError::Connect(ProtocolError::UnsupportedProtocol)
            } else {
                connection_error(error)
            }
        })?;
    let peer = protocol::handshake_outgoing(&connection, current_profile(profile))
        .await
        .map_err(NodeError::Connect)?;
    gate.handshook(&peer);
    let _ = events.send(NodeEvent::PeerConnected(peer.clone()));
    Ok((connection, peer))
}

/// Dials a peer by its endpoint ID, pings it, and reports how it is reached.
/// The connection is closed afterwards, so nothing stays open between checks.
async fn check_presence(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    profile: &SharedProfile,
    gate: &Arc<dyn PeerGate>,
) -> Result<Route, NodeError> {
    if !gate.may_connect(&addr.id) {
        return Err(NodeError::Blocked);
    }
    let connection = endpoint
        .connect(addr, protocol::ALPN)
        .await
        .map_err(connection_error)?;
    protocol::handshake_outgoing(&connection, current_profile(profile))
        .await
        .map_err(NodeError::Connect)?;
    let (mut send, mut recv) = connection.open_bi().await.map_err(connection_error)?;
    protocol::write_message(&mut send, &Request::Ping)
        .await
        .map_err(NodeError::Connect)?;
    let reply: Reply = protocol::read_message(&mut recv)
        .await
        .map_err(NodeError::Connect)?;
    if reply != Reply::Pong {
        return Err(NodeError::Connect(ProtocolError::Closed));
    }
    let route = route_of(&connection);
    connection.close(0u32.into(), b"checked");
    Ok(route)
}

/// Imports `paths` into the blob store, offers them to a trusted peer, and waits for
/// its answer. An accepted offer is fetched by the peer from this node's store.
async fn send_files(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    profile: &SharedProfile,
    events: &broadcast::Sender<NodeEvent>,
    gate: &Arc<dyn PeerGate>,
    store: Option<FsStore>,
    paths: Vec<PathBuf>,
) -> Result<(), NodeError> {
    let store = store.ok_or(NodeError::BlobsDisabled)?;
    let progress = events.clone();
    let imported = files::import(&store, &paths, |done, total| {
        let _ = progress.send(NodeEvent::ImportProgress { done, total });
    })
    .await
    .map_err(|error| NodeError::Files(error.to_string()))?;
    let offer = FileOffer {
        names: imported.names,
        root: imported.root,
        files: imported.files,
        bytes: imported.bytes,
        from: endpoint.addr(),
    };

    let (connection, _peer) = open_peer(endpoint, addr, profile, events, gate).await?;
    let (mut send, mut recv) = connection.open_bi().await.map_err(connection_error)?;
    protocol::write_message(&mut send, &Request::Offer(offer))
        .await
        .map_err(NodeError::Connect)?;
    let reply: Reply = protocol::read_message(&mut recv)
        .await
        .map_err(NodeError::Connect)?;
    connection.close(0u32.into(), b"offered");
    match reply {
        Reply::Accepted => Ok(()),
        Reply::Declined => Err(NodeError::OfferDeclined),
        Reply::Pong => Err(NodeError::Connect(ProtocolError::Closed)),
    }
}

/// Answers an offer. On accept, the files are fetched from the sender into this
/// node's store, verified as they arrive, and then reported as received.
async fn answer_offer(
    endpoint: &Endpoint,
    events: &broadcast::Sender<NodeEvent>,
    store: Option<FsStore>,
    peer: EndpointId,
    send: &mut SendStream,
    offer: FileOffer,
    accept: bool,
) -> Result<(), NodeError> {
    let store = match (accept, store) {
        (false, _) => {
            reply_and_finish(send, Reply::Declined).await;
            return Ok(());
        }
        (true, None) => {
            reply_and_finish(send, Reply::Declined).await;
            return Err(NodeError::BlobsDisabled);
        }
        (true, Some(store)) => store,
    };
    reply_and_finish(send, Reply::Accepted).await;

    let content = HashAndFormat::hash_seq(offer.root);
    let connection = endpoint
        .connect(offer.from.clone(), iroh_blobs::ALPN)
        .await
        .map_err(connection_error)?;
    store
        .remote()
        .fetch(connection, content)
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    store
        .tags()
        .create(content)
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let _ = events.send(NodeEvent::FileReceived {
        peer,
        names: offer.names,
        root: offer.root,
    });
    Ok(())
}

/// Reports a peer's disconnection once its connection closes.
fn keep_open(connection: Connection, peer: EndpointId, events: broadcast::Sender<NodeEvent>) {
    tokio::spawn(async move {
        connection.closed().await;
        let _ = events.send(NodeEvent::PeerDisconnected(peer));
    });
}

async fn dial(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    profile: &SharedProfile,
    events: &broadcast::Sender<NodeEvent>,
    gate: &Arc<dyn PeerGate>,
) -> Result<PeerInfo, NodeError> {
    let (connection, peer) = open_peer(endpoint, addr, profile, events, gate).await?;
    keep_open(connection, peer.endpoint_id, events.clone());
    Ok(peer)
}

/// Dials a peer, asks it to pair, and waits for its user's answer.
async fn dial_to_pair(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    profile: &SharedProfile,
    events: &broadcast::Sender<NodeEvent>,
    gate: &Arc<dyn PeerGate>,
) -> Result<PeerInfo, NodeError> {
    let (connection, peer) = open_peer(endpoint, addr, profile, events, gate).await?;
    match ask_to_pair(&connection, &peer.endpoint_id, gate).await {
        Ok(()) => {
            keep_open(connection, peer.endpoint_id, events.clone());
            Ok(peer)
        }
        Err(error) => {
            connection.close(0u32.into(), b"pairing");
            Err(error)
        }
    }
}

/// Sends a pairing request on a new stream and records the answer.
async fn ask_to_pair(
    connection: &Connection,
    peer: &EndpointId,
    gate: &Arc<dyn PeerGate>,
) -> Result<(), NodeError> {
    let (mut send, mut recv) = connection.open_bi().await.map_err(connection_error)?;
    protocol::write_message(&mut send, &Request::Pair)
        .await
        .map_err(NodeError::Connect)?;
    let reply: Reply = protocol::read_message(&mut recv)
        .await
        .map_err(NodeError::Connect)?;
    match reply {
        Reply::Accepted => {
            gate.pair(peer);
            Ok(())
        }
        Reply::Declined => Err(NodeError::PairingDeclined),
        // A pairing request is never answered with a pong, so this is a protocol error.
        Reply::Pong => Err(NodeError::Connect(ProtocolError::Closed)),
    }
}

/// Everything a connection needs to answer the requests it receives.
#[derive(Clone)]
struct Inbox {
    own_id: EndpointId,
    gate: Arc<dyn PeerGate>,
    events: broadcast::Sender<NodeEvent>,
    pending: Pending,
    offers: PendingOffers,
    limiter: Arc<std::sync::Mutex<RateLimiter>>,
}

impl Inbox {
    /// Answers one request from `peer`. An untrusted peer may send only `Pair`.
    async fn answer(self, peer: PeerInfo, mut send: SendStream, mut recv: RecvStream) {
        let request: Request = match protocol::read_message(&mut recv).await {
            Ok(request) => request,
            Err(error) => {
                warn!(%error, "a peer sent a request this node does not understand");
                return;
            }
        };
        match request {
            Request::Ping => {
                // Only a trusted peer may ask whether this node is reachable. Anyone
                // else gets no answer, which the requester sees as an unreachable peer.
                if self.gate.is_trusted(&peer.endpoint_id) {
                    reply_and_finish(&mut send, Reply::Pong).await;
                }
            }
            Request::Pair => {
                let id = peer.endpoint_id;
                if self.gate.is_trusted(&id) {
                    // Already paired, so there is nothing to ask.
                    reply_and_finish(&mut send, Reply::Accepted).await;
                    return;
                }
                let allowed = self
                    .limiter
                    .lock()
                    .map(|mut limiter| limiter.allow(id, Instant::now()))
                    .unwrap_or(false);
                if !allowed {
                    warn!(peer = %id, "a peer asked to pair too often; declining");
                    reply_and_finish(&mut send, Reply::Declined).await;
                    return;
                }
                let code = verification_code(&self.own_id, &id);
                if let Ok(mut pending) = self.pending.lock() {
                    pending.insert(id, send);
                }
                let _ = self.events.send(NodeEvent::PairingRequested { peer, code });
            }
            Request::Offer(offer) => {
                let id = peer.endpoint_id;
                // Only a trusted peer may offer files. Anyone else gets no answer.
                if !self.gate.is_trusted(&id) {
                    return;
                }
                if let Ok(mut offers) = self.offers.lock() {
                    offers.insert(id, (send, offer.clone()));
                }
                let _ = self.events.send(NodeEvent::FileOffered { peer, offer });
            }
        }
    }
}

/// Writes a reply and closes the stream's send side.
async fn reply_and_finish(send: &mut SendStream, answer: Reply) {
    if protocol::write_message(send, &answer).await.is_ok() {
        let _ = send.finish();
    }
}

/// Groovy's protocol handler: answers handshakes from peers that dial us, and
/// the requests they send after it.
///
/// A blocked endpoint is refused before the handshake. The other peers may send
/// requests on new streams, which are answered by [`Inbox`].
#[derive(Debug)]
struct GroovyProtocol {
    profile: SharedProfile,
    inbox: Inbox,
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox").finish_non_exhaustive()
    }
}

/// The application close code sent to a peer that is refused by the gate.
const CLOSE_BLOCKED: u32 = 2;

impl ProtocolHandler for GroovyProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let inbox = &self.inbox;
        if !inbox.gate.may_connect(&connection.remote_id()) {
            connection.close(CLOSE_BLOCKED.into(), b"blocked");
            return Ok(());
        }
        match protocol::handshake_incoming(&connection, current_profile(&self.profile)).await {
            Ok(peer) => {
                inbox.gate.handshook(&peer);
                let endpoint_id = peer.endpoint_id;
                let _ = inbox.events.send(NodeEvent::PeerConnected(peer.clone()));
                loop {
                    tokio::select! {
                        stream = connection.accept_bi() => match stream {
                            Ok((send, recv)) => {
                                tokio::spawn(inbox.clone().answer(peer.clone(), send, recv));
                            }
                            Err(_) => break,
                        },
                        _ = connection.closed() => break,
                    }
                }
                let _ = inbox.events.send(NodeEvent::PeerDisconnected(endpoint_id));
            }
            Err(error) => warn!(%error, "a peer's handshake failed"),
        }
        Ok(())
    }
}

/// A cheaply cloneable handle to a running node.
///
/// The node stops when shutdown is requested or when the last handle is dropped.
#[derive(Clone)]
pub struct NodeHandle {
    endpoint_id: EndpointId,
    profile: SharedProfile,
    commands: mpsc::Sender<Command>,
    shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl NodeHandle {
    /// This node's endpoint ID, which is its public key.
    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint_id
    }

    /// Replaces the profile peers see. Takes effect for handshakes from now on.
    pub fn set_profile(&self, profile: PeerProfile) {
        if let Ok(mut current) = self.profile.lock() {
            *current = profile;
        }
    }

    /// Dials a peer by its address and runs the handshake. Returns the peer's profile.
    pub async fn connect(&self, addr: EndpointAddr) -> Result<PeerInfo, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Connect(addr, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Asks the peer at `addr` to pair with this node, and waits for its user to answer.
    /// Returns the peer once it accepts. The verification code both sides show is
    /// [`verification_code`](crate::verification_code) of the two endpoint IDs.
    pub async fn request_pairing(&self, addr: EndpointAddr) -> Result<PeerInfo, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RequestPairing(addr, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Answers a pairing request from `peer`. Accepting makes it trusted on this node.
    pub async fn answer_pairing(&self, peer: EndpointId, accept: bool) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::AnswerPairing(peer, accept, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Adds `data` to this node's blob store, kept by a tag so it survives restarts.
    /// Returns its hash, which identifies its content.
    pub async fn add_blob(&self, data: Vec<u8>) -> Result<Hash, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::AddBlob(data, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Lists the files in a collection held by this node, with their paths inside it.
    pub async fn list_collection(&self, root: Hash) -> Result<Vec<(String, Hash)>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ListCollection(root, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Imports `paths` and offers them to a trusted peer. Returns once the peer has
    /// answered; an accepted offer is then fetched by the peer.
    pub async fn send_files(
        &self,
        addr: EndpointAddr,
        paths: Vec<PathBuf>,
    ) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SendFiles(addr, paths, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Accepts or declines files a trusted peer has offered. On accept, returns once
    /// the files are in this node's store.
    pub async fn answer_offer(&self, peer: EndpointId, accept: bool) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::AnswerOffer(peer, accept, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Reads a blob from this node's store, or `None` if it is not there.
    pub async fn read_blob(&self, hash: Hash) -> Result<Option<Vec<u8>>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ReadBlob(hash, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Checks whether a trusted peer is reachable, and whether directly or through a relay.
    /// Pass the peer's address, or just its ID on the public network, where
    /// address lookup finds it. Only trusted peers answer; anyone else is an error.
    pub async fn check_presence(&self, addr: EndpointAddr) -> Result<Route, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::CheckPresence(addr, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
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
        named_config("")
    }

    fn named_config(name: &str) -> NodeConfig {
        NodeConfig {
            secret_key: SecretKey::generate(),
            network: Network::LocalOnly,
            profile: PeerProfile {
                display_name: name.to_owned(),
                avatar_hash: None,
            },
            gate: Arc::new(AllowAll),
            discovery: Discovery::default(),
            relay: RelaySetting::Public,
            blobs: None,
        }
    }

    /// Starts a node and waits for its `Started` event.
    async fn start(config: NodeConfig) -> (NodeHandle, EventStream) {
        let (handle, mut events) = Node::start(config).expect("node starts");
        assert_eq!(events.next().await, Some(NodeEvent::Started));
        (handle, events)
    }

    /// A public-network config with the given relay setting. Refused before any
    /// network traffic, so these tests need no connection.
    fn public_config_with_relay(relay: RelaySetting) -> NodeConfig {
        NodeConfig {
            network: Network::Public,
            relay,
            ..named_config("")
        }
    }

    #[test]
    fn a_relay_url_that_does_not_parse_is_refused() {
        let config = public_config_with_relay(RelaySetting::Custom {
            urls: vec!["not a url".to_owned()],
            auth_token: None,
        });

        let result = Node::start(config);

        assert!(
            matches!(result, Err(NodeError::InvalidRelay(_))),
            "{:?}",
            result.err()
        );
    }

    #[test]
    fn a_custom_relay_list_with_no_relays_is_refused() {
        let config = public_config_with_relay(RelaySetting::Custom {
            urls: Vec::new(),
            auth_token: None,
        });

        let result = Node::start(config);

        assert!(
            matches!(result, Err(NodeError::InvalidRelay(_))),
            "{:?}",
            result.err()
        );
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

    /// Waits for the next event that matches `wanted`, skipping the rest.
    async fn next_matching(
        events: &mut EventStream,
        wanted: impl Fn(&NodeEvent) -> bool,
    ) -> NodeEvent {
        loop {
            let event = events
                .next()
                .await
                .expect("the node keeps publishing events");
            if wanted(&event) {
                return event;
            }
        }
    }

    #[tokio::test]
    async fn two_nodes_exchange_profiles_when_one_dials_the_other() {
        let (node_a, mut events_a) = start(named_config("Ada")).await;
        let (node_b, mut events_b) = start(named_config("Grace")).await;
        let addr_b = node_b.addr().await.unwrap();

        let seen_by_a = node_a.connect(addr_b).await.expect("handshake completes");

        assert_eq!(seen_by_a.endpoint_id, node_b.endpoint_id());
        assert_eq!(seen_by_a.profile.display_name, "Grace");
        let seen_by_b = next_matching(&mut events_b, |event| {
            matches!(event, NodeEvent::PeerConnected(_))
        })
        .await;
        assert!(matches!(
            seen_by_b,
            NodeEvent::PeerConnected(PeerInfo { ref profile, .. }) if profile.display_name == "Ada"
        ));
        let _ = &mut events_a;
        assert!(node_a.shutdown(TIMEOUT));
        assert!(node_b.shutdown(TIMEOUT));
    }

    #[tokio::test]
    async fn a_changed_profile_is_what_later_handshakes_show() {
        let (node_a, _events_a) = start(named_config("Ada")).await;
        let (node_b, _events_b) = start(named_config("Grace")).await;
        let addr_b = node_b.addr().await.unwrap();

        node_b.set_profile(PeerProfile {
            display_name: "Grace Hopper".to_owned(),
            avatar_hash: Some([9; 32]),
        });
        let seen = node_a.connect(addr_b).await.unwrap();

        assert_eq!(seen.profile.display_name, "Grace Hopper");
        assert_eq!(seen.profile.avatar_hash, Some([9; 32]));
        assert!(node_a.shutdown(TIMEOUT));
        assert!(node_b.shutdown(TIMEOUT));
    }
}
