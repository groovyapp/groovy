//! The node runs Groovy's async services on a dedicated thread, so callers
//! never block on them. It owns an iroh endpoint and a router for Groovy's
//! protocols. Callers talk to it through a [`NodeHandle`] and receive its
//! lifecycle through an [`EventStream`].

use std::io;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use futures_util::StreamExt;
use iroh::endpoint::{BindError, Connection, InvalidSocketAddr, RecvStream, SendStream, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, SecretKey, TransportAddr, Watcher,
};
use iroh_blobs::api::blobs::AddBytesOptions;
use iroh_blobs::api::remote::GetProgressItem;
use iroh_blobs::format::collection::Collection;
use iroh_blobs::hashseq::HashSeq;
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::store::{GcConfig, ProtectCb, ProtectOutcome};
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, BlobsProtocol, Hash, HashAndFormat, store::fs::FsStore};
use iroh_docs::NamespaceId;
use iroh_docs::protocol::Docs;
use iroh_gossip::net::{GOSSIP_ALPN, Gossip};
use iroh_mainline_address_lookup::DhtAddressLookup;
use iroh_mdns_address_lookup::MdnsAddressLookup;
use thiserror::Error;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};
use tracing::{info, warn};

use crate::blobs::{self, SharedHashes};
use crate::config::{Discovery, RelaySetting};
use crate::contacts;
use crate::db::Database;
use crate::files;
use crate::folder_sync;
use crate::folders::{self, ExportReport, Folder, FolderAccess, FolderEntry, Protected};
use crate::gated::TrustedOnly;
use crate::pairing::{RateLimiter, verification_code};
use crate::presence::{Route, route_of};
use crate::protocol::{self, FileOffer, PeerInfo, PeerProfile, ProtocolError, Reply, Request};
use crate::shares::Share;
use crate::storage::StorageUsage;
use crate::transfers::{Transfer, TransferState};

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
    #[error("shared folder failed: {0}")]
    Folder(String),
    #[error("there is no transfer with that id")]
    UnknownTransfer,
    #[error("there is no share with that id, or it was already revoked")]
    UnknownShare,
    #[error("shares are kept in the database, which this node was started without")]
    NoDatabase,
    #[error("that is not a share ticket: {0}")]
    InvalidTicket(String),
    #[error("that transfer cannot be {0} in its current state")]
    WrongTransferState(&'static str),
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
    /// Whether files from `peer` are accepted without asking the user.
    fn accepts_files_automatically(&self, _peer: &EndpointId) -> bool {
        false
    }
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
    /// The folder accepted files are saved to. `None` keeps them in the blob store only.
    pub downloads: Option<std::path::PathBuf>,
    /// Where transfers are recorded, so the list survives a restart. `None` keeps
    /// them in memory only.
    pub database: Option<Arc<Database>>,
    /// How often content nothing references is removed from the blob store. `None` never
    /// removes it.
    pub garbage_interval: Option<Duration>,
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
    /// A transfer was added, or its state or progress changed. Progress updates come
    /// at most every 100 ms for each transfer.
    TransferChanged(Transfer),
    /// The files from an accepted offer are in this node's store, and saved to the
    /// downloads folder if there is one.
    FileReceived {
        transfer: i64,
        peer: EndpointId,
        names: Vec<String>,
        root: Hash,
        saved: Vec<std::path::PathBuf>,
        /// Payload bytes this receive transferred. Zero if everything was already held.
        fetched: u64,
    },
    /// Receiving an accepted offer failed. Resume it with [`NodeHandle::resume_transfer`];
    /// the bytes already received are kept.
    ReceiveFailed {
        transfer: i64,
        peer: EndpointId,
        offer: FileOffer,
        error: String,
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
    CreateShare(
        Vec<PathBuf>,
        oneshot::Sender<Result<ShareTicket, NodeError>>,
    ),
    RevokeShare(i64, oneshot::Sender<Result<(), NodeError>>),
    ListShares(oneshot::Sender<Result<Vec<Share>, NodeError>>),
    PreviewShare(String, oneshot::Sender<Result<SharePreview, NodeError>>),
    ReceiveShare(String, oneshot::Sender<Result<i64, NodeError>>),
    CreateFolder(
        String,
        Option<PathBuf>,
        oneshot::Sender<Result<Folder, NodeError>>,
    ),
    JoinFolder(
        String,
        Option<PathBuf>,
        oneshot::Sender<Result<Folder, NodeError>>,
    ),
    FolderTicket(
        NamespaceId,
        FolderAccess,
        oneshot::Sender<Result<String, NodeError>>,
    ),
    AddToFolder(
        NamespaceId,
        String,
        PathBuf,
        oneshot::Sender<Result<(), NodeError>>,
    ),
    RemoveFromFolder(NamespaceId, String, oneshot::Sender<Result<(), NodeError>>),
    FolderEntries(
        NamespaceId,
        oneshot::Sender<Result<Vec<FolderEntry>, NodeError>>,
    ),
    ExportFolder(
        NamespaceId,
        oneshot::Sender<Result<ExportReport, NodeError>>,
    ),
    ListFolders(oneshot::Sender<Result<Vec<Folder>, NodeError>>),
    DeleteTransfer(i64, oneshot::Sender<Result<(), NodeError>>),
    ClearCache(oneshot::Sender<Result<(), NodeError>>),
    StorageUsage(oneshot::Sender<Result<StorageUsage, NodeError>>),
    SetStorageCap(Option<u64>, oneshot::Sender<Result<(), NodeError>>),
    PauseTransfer(i64, oneshot::Sender<Result<(), NodeError>>),
    ResumeTransfer(i64, oneshot::Sender<Result<(), NodeError>>),
    CancelTransfer(i64, oneshot::Sender<Result<(), NodeError>>),
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

        let weak_commands = command_tx.downgrade();
        let book: Book = Arc::new(Mutex::new(BTreeMap::new()));
        let mut resume = Vec::new();
        let mut next_id: i64 = 1;
        if let Some(db) = &config.database {
            match db.transfers() {
                Ok(transfers) => {
                    for transfer in transfers {
                        next_id = next_id.max(transfer.id + 1);
                        if transfer.state.is_unfinished() {
                            resume.push(transfer.id);
                        }
                        lock_book(&book).insert(
                            transfer.id,
                            Slot {
                                transfer,
                                abort: None,
                            },
                        );
                    }
                }
                Err(error) => warn!(%error, "could not load the transfers"),
            }
        }
        let mut shared_hashes = Vec::new();
        if let Some(db) = &config.database {
            match db.active_share_hashes() {
                Ok(hashes) => shared_hashes = hashes,
                Err(error) => warn!(%error, "could not load the shares"),
            }
        }
        let startup = Startup {
            book: book.clone(),
            next_id: Arc::new(AtomicI64::new(next_id)),
            resume,
            shared: Arc::new(Mutex::new(shared_hashes.into_iter().collect())),
            commands: weak_commands,
        };
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
                    startup,
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
            book,
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
    // Weak, so the node still stops once every handle is dropped.
    startup: Startup,
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
    // Hashes that shared folders reference. Collection keeps them, so it is refreshed from
    // the folders, and grows as files are added.
    let folder_live: Protected = Arc::new(Mutex::new(HashSet::new()));
    let blobs = match &config.blobs {
        Some(dir) => {
            let mut options = iroh_blobs::store::fs::options::Options::new(dir);
            options.gc = config.garbage_interval.map(|interval| GcConfig {
                interval,
                add_protected: Some(protect_folder_content(folder_live.clone())),
            });
            match FsStore::load_with_opts(dir.join("blobs.db"), options).await {
                Ok(store) => Some(store),
                Err(error) => {
                    let _ = ready.send(Err(NodeError::Blobs(error.to_string())));
                    return;
                }
            }
        }
        None => None,
    };
    let receiving = Receiving {
        endpoint: endpoint.clone(),
        events: events.clone(),
        store: blobs.clone(),
        downloads: config.downloads.clone(),
        book: startup.book.clone(),
        slots: Arc::new(Semaphore::new(MAX_ACTIVE_RECEIVES)),
        next_id: startup.next_id.clone(),
        database: config.database.clone(),
        shared: startup.shared.clone(),
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
                commands: startup.commands.clone(),
                limiter: Arc::new(std::sync::Mutex::new(RateLimiter::default())),
            },
        },
    );
    // Shared folders keep their replicas beside the blobs, and need both to work.
    let folders = match (&blobs, &config.blobs, &config.database) {
        (Some(store), Some(blobs_dir), Some(database)) => {
            let gossip = Gossip::builder().spawn(endpoint.clone());
            let docs_dir = blobs_dir.parent().unwrap_or(blobs_dir).join("docs");
            if let Err(error) = std::fs::create_dir_all(&docs_dir) {
                warn!(%error, "shared folders are not available");
            }
            let blob_api: iroh_blobs::api::Store = std::ops::Deref::deref(store).clone();
            match Docs::persistent(docs_dir)
                .spawn(endpoint.clone(), blob_api, gossip.clone())
                .await
            {
                Ok(docs) => {
                    let state = FolderState {
                        docs: docs.clone(),
                        store: store.clone(),
                        database: database.clone(),
                        live: folder_live.clone(),
                    };
                    if config.garbage_interval.is_some() {
                        let refreshing = state.clone();
                        let live = folder_live.clone();
                        tokio::spawn(async move {
                            loop {
                                refresh_folder_live(&refreshing, &live).await;
                                tokio::time::sleep(FOLDER_REFRESH).await;
                            }
                        });
                    }
                    match database.folders() {
                        Ok(saved) => saved.iter().for_each(|folder| state.sync(folder)),
                        Err(error) => warn!(%error, "could not read the shared folders"),
                    }
                    router = router
                        .accept(GOSSIP_ALPN, TrustedOnly::new(gossip.clone(), gate.clone()))
                        .accept(
                            iroh_docs::ALPN,
                            TrustedOnly::new(docs.clone(), gate.clone()),
                        );
                    Some(state)
                }
                Err(error) => {
                    warn!(%error, "shared folders are not available");
                    None
                }
            }
        }
        _ => None,
    };

    if let Some(store) = &blobs {
        let (events, messages) = blobs::event_channel();
        tokio::spawn(blobs::decide(
            messages,
            gate.clone(),
            receiving.shared.clone(),
        ));
        router = router.accept(iroh_blobs::ALPN, BlobsProtocol::new(store, Some(events)));
    }
    let router = router.spawn();
    let mut addresses = endpoint.watch_addr();

    info!(endpoint_id = %endpoint.id(), "node started");
    let _ = events.send(NodeEvent::Started);
    let _ = ready.send(Ok(()));
    // Transfers that were queued or receiving when the node last stopped start again.
    if receiving.store.is_some() {
        for id in startup.resume.clone() {
            set_transfer(&receiving, id, Some(TransferState::Queued), None);
            let offer = lock_book(&receiving.book)
                .get(&id)
                .map(|slot| (slot.transfer.peer, slot.transfer.offer.clone()));
            if let Some((peer, offer)) = offer {
                spawn_receive(&receiving, id, peer, offer);
            }
        }
    }

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
                Some(Command::CreateShare(paths, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(create_share(&receiving, paths).await);
                    });
                }
                Some(Command::RevokeShare(id, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(revoke_share_and_release(&receiving, id).await);
                    });
                }
                Some(Command::ListShares(reply)) => {
                    let _ = reply.send(list_shares(&receiving));
                }
                Some(Command::PreviewShare(ticket, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = match parse_ticket(&ticket) {
                            Ok(ticket) => preview_share(&receiving, &ticket).await,
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::ReceiveShare(ticket, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(receive_share(&receiving, &ticket).await);
                    });
                }
                Some(Command::CreateFolder(name, path, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => match folders::create(
                                &state.docs,
                                &state.database,
                                name,
                                path,
                                contacts::now(),
                            )
                            .await
                            {
                                Ok(folder) => {
                                    state.sync(&folder);
                                    Ok(folder)
                                }
                                Err(error) => Err(folder_error(error)),
                            },
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::JoinFolder(ticket, path, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => match folders::join(
                                &state.docs,
                                &state.database,
                                &ticket,
                                path,
                                contacts::now(),
                            )
                            .await
                            {
                                Ok(folder) => {
                                    state.sync(&folder);
                                    Ok(folder)
                                }
                                Err(error) => Err(folder_error(error)),
                            },
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::FolderTicket(namespace, access, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => folders::ticket(&state.docs, namespace, access)
                                .await
                                .map_err(folder_error),
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::AddToFolder(namespace, key, source, reply)) => {
                    let folders = folders.clone();
                    let live = folder_live.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => {
                                match folders::add_file(&state.docs, &state.store, namespace, &key, &source)
                                    .await
                                {
                                    Ok(hash) => {
                                        lock_live(&live).insert(hash);
                                        Ok(())
                                    }
                                    Err(error) => Err(folder_error(error)),
                                }
                            }
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::RemoveFromFolder(namespace, key, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => folders::remove(&state.docs, namespace, &key)
                                .await
                                .map_err(folder_error),
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::FolderEntries(namespace, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => folders::entries(&state.docs, namespace)
                                .await
                                .map_err(folder_error),
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::ExportFolder(namespace, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => match state.database.folder(namespace) {
                                Ok(Some(Folder {
                                    path: Some(dir), ..
                                })) => folders::export(&state.docs, &state.store, namespace, &dir)
                                    .await
                                    .map_err(folder_error),
                                Ok(_) => Err(NodeError::Folder(
                                    "this device does not save that folder".to_owned(),
                                )),
                                Err(error) => Err(NodeError::Folder(error.to_string())),
                            },
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::DeleteTransfer(id, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(delete_transfer(&receiving, id).await);
                    });
                }
                Some(Command::ClearCache(reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(clear_cache(&receiving).await);
                    });
                }
                Some(Command::StorageUsage(reply)) => {
                    let receiving = receiving.clone();
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(storage_usage(&receiving, &folders).await);
                    });
                }
                Some(Command::SetStorageCap(cap, reply)) => {
                    let outcome = match &receiving.database {
                        Some(database) => database
                            .set_storage_cap(cap)
                            .map_err(|error| NodeError::Files(error.to_string())),
                        None => Err(NodeError::NoDatabase),
                    };
                    let _ = reply.send(outcome);
                }
                Some(Command::ListFolders(reply)) => {
                    let outcome = match folder_state(&folders) {
                        Ok(state) => state
                            .database
                            .folders()
                            .map_err(|error| NodeError::Folder(error.to_string())),
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(outcome);
                }
                Some(Command::PauseTransfer(id, reply)) => {
                    let _ = reply.send(stop_transfer(&receiving, id, TransferState::Paused));
                }
                Some(Command::ResumeTransfer(id, reply)) => {
                    let _ = reply.send(resume_transfer(&receiving, id));
                }
                Some(Command::CancelTransfer(id, reply)) => {
                    let _ = reply.send(stop_transfer(&receiving, id, TransferState::Cancelled));
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
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = match taken {
                            None => Err(NodeError::NoPendingOffer),
                            Some((mut send, offer)) => {
                                answer_offer(&receiving, peer, &mut send, offer, accept).await
                            }
                        };
                        if let Err(error) = &outcome {
                            warn!(%error, "could not answer a file offer");
                        }
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

/// Every transfer this node knows, by id, with the task receiving it while one runs.
type Book = Arc<Mutex<BTreeMap<i64, Slot>>>;

struct Slot {
    transfer: Transfer,
    abort: Option<tokio::task::AbortHandle>,
}

/// Receives at most this many offers at once. The rest wait in the queue.
const MAX_ACTIVE_RECEIVES: usize = 2;
/// How often a transfer's progress is reported, at most.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// A share as made, with the ticket to give out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareTicket {
    pub share: Share,
    /// Anyone with this can download the share while this node is online.
    pub ticket: String,
}

/// What a share ticket offers, read before anything is downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharePreview {
    pub root: Hash,
    /// Every file's path inside the share.
    pub names: Vec<String>,
    pub files: u64,
    pub bytes: u64,
    /// The node that offers the share.
    pub from: EndpointId,
}

/// What shared folders need: the replicas, the blob store the files live in, and the
/// database that records which folders this device is in.
#[derive(Clone)]
struct FolderState {
    docs: Docs,
    store: FsStore,
    database: Arc<Database>,
    live: Protected,
}

impl FolderState {
    /// Starts keeping a folder's directory in step with it, if this device saves it.
    fn sync(&self, folder: &Folder) {
        if let Some(dir) = &folder.path {
            folder_sync::spawn(
                self.docs.clone(),
                std::ops::Deref::deref(&self.store).clone(),
                folder.namespace,
                dir.clone(),
                self.live.clone(),
            );
        }
    }
}

/// The folder state, or the error to report when shared folders are not available.
fn folder_state(folders: &Option<FolderState>) -> Result<FolderState, NodeError> {
    folders.clone().ok_or_else(|| {
        NodeError::Folder("shared folders need blob storage and a database".to_owned())
    })
}

fn folder_error(error: folders::FolderError) -> NodeError {
    NodeError::Folder(error.to_string())
}

/// How often the hashes shared folders reference are read again.
const FOLDER_REFRESH: Duration = Duration::from_secs(2);

/// Lets collection keep what shared folders reference. The protection is read at the
/// start of each collection, so a file is protected from the moment it is in a folder.
fn protect_folder_content(live: Protected) -> ProtectCb {
    Arc::new(move |protected: &mut HashSet<Hash>| {
        protected.extend(lock_live(&live).iter().copied());
        Box::pin(std::future::ready(ProtectOutcome::Continue))
    })
}

fn lock_live(live: &Protected) -> std::sync::MutexGuard<'_, HashSet<Hash>> {
    live.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads every hash the folders reference, and makes that the protected set.
async fn refresh_folder_live(state: &FolderState, live: &Protected) {
    let Ok(folders) = state.database.folders() else {
        return;
    };
    let mut hashes = HashSet::new();
    for folder in folders {
        if let Ok(entries) = folders::entries(&state.docs, folder.namespace).await {
            hashes.extend(entries.iter().map(|entry| entry.hash));
        }
    }
    *lock_live(live) = hashes;
}

/// What `start` hands the node thread: the transfer book, the next free transfer id,
/// the transfers to pick up again, and a way to send the node its commands.
struct Startup {
    book: Book,
    next_id: Arc<AtomicI64>,
    resume: Vec<i64>,
    shared: SharedHashes,
    commands: mpsc::WeakSender<Command>,
}

/// What an accepted offer needs to be received: the endpoint to fetch with, the events
/// to report on, the blob store, the folder to save into, and the transfer book.
#[derive(Clone)]
struct Receiving {
    endpoint: Endpoint,
    events: broadcast::Sender<NodeEvent>,
    store: Option<FsStore>,
    downloads: Option<std::path::PathBuf>,
    book: Book,
    slots: Arc<Semaphore>,
    next_id: Arc<AtomicI64>,
    database: Option<Arc<Database>>,
    shared: SharedHashes,
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
    store
        .tags()
        .set(
            format!("send-{}", imported.root),
            HashAndFormat::hash_seq(imported.root),
        )
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

/// Answers an offer. On accept, the files are queued to be received.
async fn answer_offer(
    receiving: &Receiving,
    peer: EndpointId,
    send: &mut SendStream,
    offer: FileOffer,
    accept: bool,
) -> Result<(), NodeError> {
    if !accept || receiving.store.is_none() {
        reply_and_finish(send, Reply::Declined).await;
        return if accept {
            Err(NodeError::BlobsDisabled)
        } else {
            Ok(())
        };
    }
    reply_and_finish(send, Reply::Accepted).await;
    let id = new_transfer(receiving, peer, offer.clone());
    spawn_receive(receiving, id, peer, offer);
    Ok(())
}

/// Records a new transfer as queued, and returns its id.
fn new_transfer(receiving: &Receiving, peer: EndpointId, offer: FileOffer) -> i64 {
    let id = receiving.next_id.fetch_add(1, Ordering::SeqCst);
    let now = contacts::now();
    let transfer = Transfer {
        id,
        peer,
        offer,
        state: TransferState::Queued,
        done: 0,
        created: now,
        updated: now,
    };
    if let Some(Err(error)) = receiving
        .database
        .as_ref()
        .map(|db| db.insert_transfer(&transfer))
    {
        warn!(%error, "could not save a transfer");
    }
    lock_book(&receiving.book).insert(
        id,
        Slot {
            transfer: transfer.clone(),
            abort: None,
        },
    );
    let _ = receiving.events.send(NodeEvent::TransferChanged(transfer));
    id
}

/// Changes a transfer's state, and its progress if given. Saves it, and tells listeners.
fn set_transfer(receiving: &Receiving, id: i64, state: Option<TransferState>, done: Option<u64>) {
    let transfer = {
        let mut book = lock_book(&receiving.book);
        let Some(slot) = book.get_mut(&id) else {
            return;
        };
        if let Some(state) = state {
            slot.transfer.state = state;
        }
        if let Some(done) = done {
            slot.transfer.done = done;
        }
        slot.transfer.updated = contacts::now();
        slot.transfer.clone()
    };
    let saved = receiving
        .database
        .as_ref()
        .map(|db| db.update_transfer(id, &transfer.state, transfer.done, transfer.updated));
    if let Some(Err(error)) = saved {
        warn!(%error, "could not save a transfer");
    }
    let _ = receiving.events.send(NodeEvent::TransferChanged(transfer));
}

/// Starts a transfer's receive in the background. It waits for a free slot, then fetches.
/// The transfer's task is kept in the book, so it can be paused or cancelled.
fn spawn_receive(receiving: &Receiving, id: i64, peer: EndpointId, offer: FileOffer) {
    let task = receiving.clone();
    let handle = tokio::spawn(async move {
        let _slot = task.slots.clone().acquire_owned().await;
        set_transfer(&task, id, Some(TransferState::Active), None);
        match receive(&task, id, &offer).await {
            Ok(()) => set_transfer(&task, id, Some(TransferState::Completed), Some(offer.bytes)),
            Err(error) => {
                let message = error.to_string();
                set_transfer(
                    &task,
                    id,
                    Some(TransferState::Failed(message.clone())),
                    None,
                );
                let _ = task.events.send(NodeEvent::ReceiveFailed {
                    transfer: id,
                    peer,
                    offer,
                    error: message,
                });
            }
        }
    });
    if let Some(slot) = lock_book(&receiving.book).get_mut(&id) {
        slot.abort = Some(handle.abort_handle());
    }
}

/// Fetches a transfer's files into the blob store, checking each block as it arrives.
/// Bytes already in the store are not fetched again, so a retry picks up where it stopped.
/// Then saves the files to the downloads folder, if there is one, and reports them.
async fn receive(receiving: &Receiving, id: i64, offer: &FileOffer) -> Result<(), NodeError> {
    let store = receiving.store.as_ref().ok_or(NodeError::BlobsDisabled)?;
    let content = HashAndFormat::hash_seq(offer.root);
    let connection = receiving
        .endpoint
        .connect(offer.from.clone(), iroh_blobs::ALPN)
        .await
        .map_err(connection_error)?;

    let mut fetched = 0;
    let mut last_report: Option<Instant> = None;
    let mut progress = store.remote().fetch(connection, content).stream();
    while let Some(item) = progress.next().await {
        match item {
            GetProgressItem::Progress(done) => {
                if last_report.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL) {
                    last_report = Some(Instant::now());
                    set_transfer(receiving, id, None, Some(done));
                }
            }
            GetProgressItem::Done(stats) => {
                fetched = stats.payload_bytes_read;
                break;
            }
            GetProgressItem::Error(error) => return Err(NodeError::Files(error.to_string())),
        }
    }

    // Named for the transfer, so deleting the transfer releases the content.
    store
        .tags()
        .set(format!("transfer-{}", id), content)
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let saved = match &receiving.downloads {
        Some(dir) => files::export(store, offer.root, dir)
            .await
            .map_err(|error| NodeError::Files(error.to_string()))?,
        None => Vec::new(),
    };
    let peer = offer_peer(receiving, id);
    let _ = receiving.events.send(NodeEvent::FileReceived {
        transfer: id,
        peer,
        names: offer.names.clone(),
        root: offer.root,
        saved,
        fetched,
    });
    Ok(())
}

/// The peer a transfer is from. A transfer that is in the book always has one.
fn offer_peer(receiving: &Receiving, id: i64) -> EndpointId {
    lock_book(&receiving.book)
        .get(&id)
        .map(|slot| slot.transfer.peer)
        .expect("a transfer is in the book while it is received")
}

/// Locks the transfer book. A panic while it was locked leaves it usable.
fn lock_book(book: &Book) -> std::sync::MutexGuard<'_, BTreeMap<i64, Slot>> {
    book.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Resumes a paused or failed transfer. Only what is missing is fetched.
fn resume_transfer(receiving: &Receiving, id: i64) -> Result<(), NodeError> {
    let (peer, offer) = {
        let book = lock_book(&receiving.book);
        let slot = book.get(&id).ok_or(NodeError::UnknownTransfer)?;
        match slot.transfer.state {
            TransferState::Paused | TransferState::Failed(_) => {}
            _ => return Err(NodeError::WrongTransferState("resumed")),
        }
        (slot.transfer.peer, slot.transfer.offer.clone())
    };
    if receiving.store.is_none() {
        return Err(NodeError::BlobsDisabled);
    }
    set_transfer(receiving, id, Some(TransferState::Queued), None);
    spawn_receive(receiving, id, peer, offer);
    Ok(())
}

/// Stops a transfer that is queued or receiving. Its bytes stay in the store.
fn stop_transfer(receiving: &Receiving, id: i64, state: TransferState) -> Result<(), NodeError> {
    let abort = {
        let mut book = lock_book(&receiving.book);
        let slot = book.get_mut(&id).ok_or(NodeError::UnknownTransfer)?;
        let stoppable = matches!(
            slot.transfer.state,
            TransferState::Queued | TransferState::Active | TransferState::Paused
        );
        if !stoppable {
            let verb = if state == TransferState::Paused {
                "paused"
            } else {
                "cancelled"
            };
            return Err(NodeError::WrongTransferState(verb));
        }
        slot.abort.take()
    };
    if let Some(abort) = abort {
        abort.abort();
    }
    set_transfer(receiving, id, Some(state), None);
    Ok(())
}

/// Imports `paths` and makes a share of them. Its blobs are served to anyone with the
/// ticket until the share is revoked.
async fn create_share(
    receiving: &Receiving,
    paths: Vec<PathBuf>,
) -> Result<ShareTicket, NodeError> {
    let store = receiving.store.as_ref().ok_or(NodeError::BlobsDisabled)?;
    let database = receiving.database.as_ref().ok_or(NodeError::NoDatabase)?;
    let imported = files::import(store, &paths, |_, _| {})
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    store
        .tags()
        .set(
            format!("share-{}", imported.root),
            HashAndFormat::hash_seq(imported.root),
        )
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    // The root's links are the metadata and every file, all of which the share serves.
    let root_bytes = store
        .blobs()
        .get_bytes(imported.root)
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let links =
        HashSeq::try_from(root_bytes).map_err(|error| NodeError::Files(error.to_string()))?;
    let mut hashes = vec![imported.root];
    hashes.extend(links.iter());
    let mut share = Share {
        id: 0,
        root: imported.root,
        names: imported.names,
        files: imported.files,
        bytes: imported.bytes,
        hashes: hashes.clone(),
        created: contacts::now(),
        revoked: None,
    };
    share.id = database
        .insert_share(&share)
        .map_err(|error| NodeError::Files(error.to_string()))?;
    lock_shared(&receiving.shared).extend(hashes);

    let ticket = BlobTicket::new(
        receiving.endpoint.addr(),
        imported.root,
        BlobFormat::HashSeq,
    )
    .to_string();
    Ok(ShareTicket { share, ticket })
}

/// Stops a share at once. Its blobs are refused to anyone who is not a contact from now on.
fn revoke_share(receiving: &Receiving, id: i64, now: i64) -> Result<(), NodeError> {
    let database = receiving.database.as_ref().ok_or(NodeError::NoDatabase)?;
    let revoked = database
        .revoke_share(id, now)
        .map_err(|error| NodeError::Files(error.to_string()))?;
    if !revoked {
        return Err(NodeError::UnknownShare);
    }
    let active = database
        .active_share_hashes()
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let mut shared = lock_shared(&receiving.shared);
    shared.clear();
    shared.extend(active);
    Ok(())
}

/// Revokes a share, and releases its tag so its content can be collected.
async fn revoke_share_and_release(receiving: &Receiving, id: i64) -> Result<(), NodeError> {
    let root = receiving
        .database
        .as_ref()
        .and_then(|database| database.shares().ok())
        .and_then(|shares| shares.into_iter().find(|share| share.id == id))
        .map(|share| share.root);
    revoke_share(receiving, id, contacts::now())?;
    if let (Some(root), Some(store)) = (root, &receiving.store)
        && let Err(error) = store.tags().delete(format!("share-{root}")).await
    {
        warn!(%error, "could not release a share's content");
    }
    Ok(())
}

/// Deletes a transfer from the history, and releases its content. A transfer that is
/// still moving cannot be deleted; pause or cancel it first.
async fn delete_transfer(receiving: &Receiving, id: i64) -> Result<(), NodeError> {
    {
        let book = lock_book(&receiving.book);
        let slot = book.get(&id).ok_or(NodeError::UnknownTransfer)?;
        if matches!(
            slot.transfer.state,
            TransferState::Queued | TransferState::Active
        ) {
            return Err(NodeError::WrongTransferState("deleted"));
        }
    }
    lock_book(&receiving.book).remove(&id);
    if let Some(database) = &receiving.database {
        database
            .delete_transfer(id)
            .map_err(|error| NodeError::Files(error.to_string()))?;
    }
    if let Some(store) = &receiving.store
        && let Err(error) = store.tags().delete(format!("transfer-{id}")).await
    {
        warn!(%error, "could not release a transfer's content");
    }
    Ok(())
}

/// Deletes the history of every received file, and the tags of files sent, then releases
/// their content. Folders and shares are not touched, and their files stay.
async fn clear_cache(receiving: &Receiving) -> Result<(), NodeError> {
    let finished: Vec<i64> = lock_book(&receiving.book)
        .values()
        .filter(|slot| slot.transfer.state == TransferState::Completed)
        .map(|slot| slot.transfer.id)
        .collect();
    for id in finished {
        delete_transfer(receiving, id).await?;
    }
    if let Some(store) = &receiving.store {
        let mut sent = store
            .tags()
            .list_prefix("send-")
            .await
            .map_err(|error| NodeError::Files(error.to_string()))?;
        while let Some(tag) = sent.next().await {
            let tag = tag.map_err(|error| NodeError::Files(error.to_string()))?;
            if let Err(error) = store.tags().delete(tag.name.0.clone()).await {
                warn!(%error, "could not release a sent file");
            }
        }
    }
    Ok(())
}

/// How much the node holds: what was received, what the folders hold, and the cap.
async fn storage_usage(
    receiving: &Receiving,
    folders: &Option<FolderState>,
) -> Result<StorageUsage, NodeError> {
    let received_bytes = lock_book(&receiving.book)
        .values()
        .filter(|slot| slot.transfer.state == TransferState::Completed)
        .map(|slot| slot.transfer.offer.bytes)
        .sum();
    let mut folder_bytes = 0;
    let mut cap_bytes = None;
    if let (Some(state), Some(database)) = (folders, &receiving.database) {
        for folder in database.folders().unwrap_or_default() {
            let entries = folders::entries(&state.docs, folder.namespace)
                .await
                .map_err(folder_error)?;
            folder_bytes += entries.iter().map(|entry| entry.size).sum::<u64>();
        }
        cap_bytes = database
            .storage_cap()
            .map_err(|error| NodeError::Files(error.to_string()))?;
    }
    Ok(StorageUsage {
        received_bytes,
        folder_bytes,
        cap_bytes,
    })
}

fn list_shares(receiving: &Receiving) -> Result<Vec<Share>, NodeError> {
    let database = receiving.database.as_ref().ok_or(NodeError::NoDatabase)?;
    database
        .shares()
        .map_err(|error| NodeError::Files(error.to_string()))
}

fn lock_shared(
    shared: &SharedHashes,
) -> std::sync::MutexGuard<'_, std::collections::HashSet<Hash>> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn parse_ticket(ticket: &str) -> Result<BlobTicket, NodeError> {
    BlobTicket::from_str(ticket.trim()).map_err(|error| NodeError::InvalidTicket(error.to_string()))
}

/// Reads what a share offers, without downloading its files. Only the list of names and
/// the sizes are fetched, which is what the user needs to decide.
async fn preview_share(
    receiving: &Receiving,
    ticket: &BlobTicket,
) -> Result<SharePreview, NodeError> {
    let connection = receiving
        .endpoint
        .connect(ticket.addr().clone(), iroh_blobs::ALPN)
        .await
        .map_err(connection_error)?;
    let root = ticket.hash();
    let root_bytes = iroh_blobs::get::request::get_blob(connection.clone(), root)
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let links = HashSeq::try_from(root_bytes.clone())
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let meta = links
        .iter()
        .next()
        .ok_or_else(|| NodeError::Files("the share is empty".to_owned()))?;
    let meta_bytes = iroh_blobs::get::request::get_blob(connection.clone(), meta)
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;

    // The names come from a scratch store that holds only the root and its metadata.
    let scratch = MemStore::new();
    scratch
        .add_bytes_with_opts(AddBytesOptions {
            data: root_bytes,
            format: BlobFormat::HashSeq,
        })
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    scratch
        .add_bytes(meta_bytes)
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let collection = Collection::load(root, scratch.as_ref())
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;

    let mut bytes = 0;
    for file in links.iter().skip(1) {
        let (size, _) = iroh_blobs::get::request::get_unverified_size(&connection, &file)
            .await
            .map_err(|error| NodeError::Files(error.to_string()))?;
        bytes += size;
    }
    Ok(SharePreview {
        root,
        names: collection.iter().map(|(name, _)| name.clone()).collect(),
        files: collection.len() as u64,
        bytes,
        from: ticket.addr().id,
    })
}

/// Receives a share as a transfer, from the node that offers it.
async fn receive_share(receiving: &Receiving, ticket: &str) -> Result<i64, NodeError> {
    let ticket = parse_ticket(ticket)?;
    if receiving.store.is_none() {
        return Err(NodeError::BlobsDisabled);
    }
    let preview = preview_share(receiving, &ticket).await?;
    let offer = FileOffer {
        names: top_level_names(&preview.names),
        root: preview.root,
        files: preview.files,
        bytes: preview.bytes,
        from: ticket.addr().clone(),
    };
    let id = new_transfer(receiving, preview.from, offer.clone());
    spawn_receive(receiving, id, preview.from, offer);
    Ok(id)
}

/// The names at the top of a share: `photos/a.txt` and `photos/b.txt` give `photos`.
fn top_level_names(paths: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for path in paths {
        let top = path.split('/').next().unwrap_or(path).to_owned();
        if !names.contains(&top) {
            names.push(top);
        }
    }
    names
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
    commands: mpsc::WeakSender<Command>,
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
                if self.gate.accepts_files_automatically(&id) {
                    // The user chose to accept this contact's files, so the offer is
                    // answered now, and nothing is shown to the user.
                    if let Ok(mut offers) = self.offers.lock() {
                        offers.insert(id, (send, offer));
                    }
                    if let Some(commands) = self.commands.upgrade() {
                        let (reply, _) = oneshot::channel();
                        let _ = commands.send(Command::AnswerOffer(id, true, reply)).await;
                    }
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
    book: Book,
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

    /// Makes a share of files or folders. Anyone with the ticket can download them while
    /// this node is online, until the share is revoked.
    pub async fn create_share(&self, paths: Vec<PathBuf>) -> Result<ShareTicket, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::CreateShare(paths, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Stops a share. Its content is refused to anyone who is not a contact from now on.
    pub async fn revoke_share(&self, id: i64) -> Result<(), NodeError> {
        self.transfer_command(|reply| Command::RevokeShare(id, reply))
            .await
    }

    /// Every share this node has made, including revoked ones.
    pub async fn shares(&self) -> Result<Vec<Share>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ListShares(reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Reads what a share ticket offers, names and sizes only, before anything is downloaded.
    pub async fn preview_share(&self, ticket: String) -> Result<SharePreview, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::PreviewShare(ticket, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Downloads a share as a transfer, and returns the transfer's id.
    pub async fn receive_share(&self, ticket: String) -> Result<i64, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ReceiveShare(ticket, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Makes a shared folder. `path`, if given, is a local folder whose files are shared and
    /// kept in step with it; the files already in it are shared at once.
    pub async fn create_folder(
        &self,
        name: String,
        path: Option<PathBuf>,
    ) -> Result<Folder, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::CreateFolder(name, path, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Joins a shared folder from its ticket. `path`, if given, is where its files are saved.
    pub async fn join_folder(
        &self,
        ticket: String,
        path: Option<PathBuf>,
    ) -> Result<Folder, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::JoinFolder(ticket, path, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// A ticket another member can use to join this folder, with the access it gives.
    pub async fn folder_ticket(
        &self,
        namespace: NamespaceId,
        access: FolderAccess,
    ) -> Result<String, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::FolderTicket(namespace, access, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Copies a file into a folder at `key`, a relative path such as `photos/a.txt`.
    pub async fn add_to_folder(
        &self,
        namespace: NamespaceId,
        key: String,
        source: PathBuf,
    ) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::AddToFolder(namespace, key, source, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Removes the file at `key` from a folder. Members see it removed when they sync.
    pub async fn remove_from_folder(
        &self,
        namespace: NamespaceId,
        key: String,
    ) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RemoveFromFolder(namespace, key, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// The files a folder has now, as the newest entry for each path.
    pub async fn folder_entries(
        &self,
        namespace: NamespaceId,
    ) -> Result<Vec<FolderEntry>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::FolderEntries(namespace, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Writes the folder's downloaded files to the folder's save path on this device.
    pub async fn export_folder(&self, namespace: NamespaceId) -> Result<ExportReport, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ExportFolder(namespace, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Every shared folder this device is in.
    pub async fn folders(&self) -> Result<Vec<Folder>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ListFolders(reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Deletes a transfer from the history. Its content is freed by the next collection,
    /// unless something else keeps it. A transfer still moving cannot be deleted.
    pub async fn delete_transfer(&self, id: i64) -> Result<(), NodeError> {
        self.transfer_command(|reply| Command::DeleteTransfer(id, reply))
            .await
    }

    /// Clears the cache: the history of received files and the sent files' tags. Their
    /// content is freed by the next collection. Folders and shares keep their files.
    pub async fn clear_cache(&self) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ClearCache(reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// How much the node holds, and the cap the user set.
    pub async fn storage_usage(&self) -> Result<StorageUsage, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::StorageUsage(reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Sets the cap on how much the node should hold, in bytes. `None` removes it.
    pub async fn set_storage_cap(&self, cap: Option<u64>) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SetStorageCap(cap, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Every transfer this node knows, oldest first.
    pub fn transfers(&self) -> Vec<Transfer> {
        lock_book(&self.book)
            .values()
            .map(|slot| slot.transfer.clone())
            .collect()
    }

    /// Pauses a queued or receiving transfer. Its bytes stay in the store, so resuming
    /// it fetches only what is missing.
    pub async fn pause_transfer(&self, id: i64) -> Result<(), NodeError> {
        self.transfer_command(|reply| Command::PauseTransfer(id, reply))
            .await
    }

    /// Resumes a paused or failed transfer.
    pub async fn resume_transfer(&self, id: i64) -> Result<(), NodeError> {
        self.transfer_command(|reply| Command::ResumeTransfer(id, reply))
            .await
    }

    /// Cancels a transfer that is not finished.
    pub async fn cancel_transfer(&self, id: i64) -> Result<(), NodeError> {
        self.transfer_command(|reply| Command::CancelTransfer(id, reply))
            .await
    }

    async fn transfer_command(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<(), NodeError>>) -> Command,
    ) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(command(reply))
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
            downloads: None,
            database: None,
            garbage_interval: None,
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
