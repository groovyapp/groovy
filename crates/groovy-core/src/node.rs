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
    Endpoint, EndpointAddr, EndpointId, PublicKey, RelayMap, RelayMode, SecretKey, TransportAddr,
    Watcher,
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

use crate::avatar;
use crate::blobs::{self, SharedHashes, Withdrawn};
use crate::config::{Discovery, RelaySetting};
use crate::contacts;
use crate::db::Database;
use crate::files;
use crate::folders::{
    self, ExportReport, Folder, FolderAccess, FolderEntry, FolderStatus, Protected,
};
use crate::gated::TrustedOnly;
use crate::groups::{
    self, GroupDetails, GroupEnvelope, GroupId, GroupMessage, GroupPage, GroupRow, GroupTicket,
};
use crate::link::{self, LINK_ALPN, LinkProtocol, LinkTicket, OpenLinks};
use crate::membership::{self, Change, KeyRecord, Members, Record, Role, record_key};
use crate::messages::{self, Attachment, DeliveryState, MESSAGE_VERSION, Message, MessageId};
use crate::outbox::{Outbox, Trigger};
use crate::pairing::{RateLimiter, verification_code};
use crate::presence::{Route, route_of};
use crate::protocol::{self, FileOffer, PeerInfo, PeerProfile, ProtocolError, Reply, Request};
use crate::replicas::{self, ReplicaHealth, ReplicaInfo};
use crate::shares::Share;
use crate::storage::StorageUsage;
use crate::transfers::{Direction, Transfer, TransferState};
use crate::user::{DeviceCert, Revocation, UserClaim, UserDevice, UserKeyStore};
use crate::{folder_peers, folder_sync};
use iroh::address_lookup::MemoryLookup;
use iroh_docs::DocTicket;
use iroh_docs::store::Query;
use iroh_gossip::api::{Event as GossipEvent, GossipSender};
use iroh_gossip::proto::TopicId;

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
    #[error("message failed: {0}")]
    Message(String),
    #[error("group chat failed: {0}")]
    Group(String),
    #[error("linking devices failed: {0}")]
    User(String),
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
    /// Where the user's key is kept, once this device belongs to a user. `None` turns linking
    /// devices off.
    pub user_keys: Option<Arc<dyn UserKeyStore>>,
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
    /// A peer that offered files closed the connection before the user answered, so the
    /// offer is gone.
    OfferWithdrawn {
        peer: EndpointId,
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
    /// A direct message arrived from a contact and is stored.
    MessageReceived(Message),
    /// A group message arrived from a member and is stored.
    GroupMessageReceived(GroupMessage),
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
        Option<Vec<Hash>>,
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
    LinkDevice(String, oneshot::Sender<Result<String, NodeError>>),
    AcceptLink(
        String,
        String,
        oneshot::Sender<Result<PublicKey, NodeError>>,
    ),
    UserDevices(oneshot::Sender<Result<Vec<UserDevice>, NodeError>>),
    RevokeDevice(EndpointId, oneshot::Sender<Result<(), NodeError>>),
    LeaveFolder(NamespaceId, oneshot::Sender<Result<(), NodeError>>),
    SendMessage(
        EndpointAddr,
        String,
        Option<MessageId>,
        oneshot::Sender<Result<Message, NodeError>>,
    ),
    SendAttachment(
        EndpointAddr,
        String,
        PathBuf,
        oneshot::Sender<Result<Message, NodeError>>,
    ),
    DownloadAttachment(MessageId, usize, oneshot::Sender<Result<i64, NodeError>>),
    Search(
        String,
        usize,
        oneshot::Sender<Result<Vec<messages::SearchHit>, NodeError>>,
    ),
    MessagesBefore(
        EndpointId,
        Option<messages::Cursor>,
        usize,
        oneshot::Sender<Result<Vec<Message>, NodeError>>,
    ),
    GroupMessagesBefore(
        GroupId,
        Option<messages::Cursor>,
        usize,
        oneshot::Sender<Result<Vec<GroupMessage>, NodeError>>,
    ),
    DeleteMessage(MessageId, oneshot::Sender<Result<bool, NodeError>>),
    ClearConversation(EndpointId, oneshot::Sender<Result<usize, NodeError>>),
    ClearGroup(GroupId, oneshot::Sender<Result<usize, NodeError>>),
    Messages(EndpointId, oneshot::Sender<Result<Vec<Message>, NodeError>>),
    CreateGroup(String, oneshot::Sender<Result<GroupId, NodeError>>),
    JoinGroup(String, oneshot::Sender<Result<GroupId, NodeError>>),
    GroupTicketFor(GroupId, oneshot::Sender<Result<String, NodeError>>),
    SendGroupMessage(
        GroupId,
        String,
        Option<MessageId>,
        oneshot::Sender<Result<GroupMessage, NodeError>>,
    ),
    GroupMessages(
        GroupId,
        oneshot::Sender<Result<Vec<GroupMessage>, NodeError>>,
    ),
    Groups(oneshot::Sender<Result<Vec<groups::GroupInfo>, NodeError>>),
    GroupDetailsFor(GroupId, oneshot::Sender<Result<GroupDetails, NodeError>>),
    RemoveGroupMember(GroupId, EndpointId, oneshot::Sender<Result<(), NodeError>>),
    SetGroupRole(
        GroupId,
        EndpointId,
        Option<Role>,
        oneshot::Sender<Result<(), NodeError>>,
    ),
    RenameGroup(GroupId, String, oneshot::Sender<Result<(), NodeError>>),
    GroupRequest(EndpointId, Request, oneshot::Sender<GroupAnswer>),
    FolderStatus(
        NamespaceId,
        oneshot::Sender<Result<FolderStatus, NodeError>>,
    ),
    RenameFolder(NamespaceId, String, oneshot::Sender<Result<(), NodeError>>),
    FolderHealth(
        NamespaceId,
        oneshot::Sender<Result<ReplicaHealth, NodeError>>,
    ),
    VerifyFolder(NamespaceId, oneshot::Sender<Result<ReplicaInfo, NodeError>>),
    SetKeepCopy(NamespaceId, bool, oneshot::Sender<Result<(), NodeError>>),
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
                                cancel: None,
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

async fn bind(config: &NodeConfig) -> Result<(Endpoint, MemoryLookup), NodeError> {
    // Addresses this node learns of other devices, so it can reach them by id.
    let directory = MemoryLookup::new();
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
        .address_lookup(directory.clone())
        .secret_key(config.secret_key.clone())
        .bind()
        .await
        .map(|endpoint| (endpoint, directory))
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
    let (endpoint, directory) = match bind(&config).await {
        Ok(bound) => bound,
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
    let withdrawn: Withdrawn = Arc::new(Mutex::new(HashSet::new()));
    // The content of downloads in progress, so collection leaves it alone.
    let in_flight: Protected = Arc::new(Mutex::new(HashSet::new()));
    let blobs = match &config.blobs {
        Some(dir) => {
            let mut options = iroh_blobs::store::fs::options::Options::new(dir);
            options.gc = config.garbage_interval.map(|interval| GcConfig {
                interval,
                add_protected: Some(protect_folder_content(vec![
                    folder_live.clone(),
                    in_flight.clone(),
                ])),
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
    // Gossip carries group chat and the shared folders' replicas, so it runs wherever there is a
    // database to keep them in.
    let gossip = config
        .database
        .as_ref()
        .map(|_| Gossip::builder().spawn(endpoint.clone()));
    if let Some(database) = &config.database
        && let Ok(addresses) = database.all_addresses()
    {
        for addr in addresses {
            directory.add_endpoint_info(addr);
        }
    }
    // The replicas that shared folders and group membership keep, beside the blobs.
    let docs = match (&blobs, &config.blobs, &gossip) {
        (Some(store), Some(blobs_dir), Some(gossip)) => {
            let docs_dir = blobs_dir.parent().unwrap_or(blobs_dir).join("docs");
            if let Err(error) = std::fs::create_dir_all(&docs_dir) {
                warn!(%error, "shared folders are not available");
            }
            let blob_api: iroh_blobs::api::Store = std::ops::Deref::deref(store).clone();
            match Docs::persistent(docs_dir)
                .spawn(endpoint.clone(), blob_api, gossip.clone())
                .await
            {
                Ok(docs) => Some(docs),
                Err(error) => {
                    warn!(%error, "shared folders are not available");
                    None
                }
            }
        }
        _ => None,
    };
    let receiving = Receiving {
        docs: docs.clone(),
        topics: Arc::new(Mutex::new(HashMap::new())),
        contact_syncs: Arc::new(Mutex::new(HashMap::new())),
        gossip: gossip.clone(),
        groups: Arc::new(Mutex::new(HashMap::new())),
        secret: config.secret_key.clone(),
        directory: directory.clone(),
        endpoint: endpoint.clone(),
        events: events.clone(),
        store: blobs.clone(),
        downloads: config.downloads.clone(),
        book: startup.book.clone(),
        slots: Arc::new(Semaphore::new(MAX_ACTIVE_RECEIVES)),
        next_id: startup.next_id.clone(),
        database: config.database.clone(),
        shared: startup.shared.clone(),
        withdrawn: withdrawn.clone(),
        in_flight: in_flight.clone(),
        user_keys: config.user_keys.clone(),
        open_link: Arc::new(Mutex::new(None)),
        profile: profile.clone(),
        gate: gate.clone(),
        outbox: config
            .database
            .clone()
            .map(|database| Outbox::new(database, gate.clone())),
    };
    if let Some(outbox) = receiving.outbox.clone() {
        spawn_outbox(receiving.clone(), outbox, events.subscribe());
    }
    spawn_groups(&receiving);
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
                database: config.database.clone(),
                directory: directory.clone(),
            },
        },
    );
    if let Some(gossip) = &gossip {
        router = router.accept(GOSSIP_ALPN, TrustedOnly::new(gossip.clone(), gate.clone()));
    }
    if let Some(docs) = &docs {
        router = router.accept(
            iroh_docs::ALPN,
            TrustedOnly::new(docs.clone(), gate.clone()),
        );
    }
    // Shared folders keep their membership documents here, beside the blobs.
    let folders = match (&blobs, &docs, &config.database) {
        (Some(store), Some(docs), Some(database)) => {
            let state = FolderState {
                docs: docs.clone(),
                store: store.clone(),
                database: database.clone(),
                live: folder_live.clone(),
                me: endpoint.id(),
                tasks: Arc::new(Mutex::new(HashMap::new())),
                peers: Arc::new(Mutex::new(HashMap::new())),
            };
            let verifying = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(VERIFY_DELAY).await;
                loop {
                    verifying.verify_all().await;
                    tokio::time::sleep(VERIFY_INTERVAL).await;
                }
            });
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
                Ok(saved) => saved.iter().for_each(|folder| state.start(folder)),
                Err(error) => warn!(%error, "could not read the shared folders"),
            }
            Some(state)
        }
        _ => None,
    };

    if let Some(store) = &blobs {
        let (events, messages) = blobs::event_channel();
        tokio::spawn(blobs::decide(
            messages,
            gate.clone(),
            receiving.shared.clone(),
            withdrawn.clone(),
        ));
        router = router.accept(iroh_blobs::ALPN, BlobsProtocol::new(store, Some(events)));
    }
    if let (Some(database), Some(keys)) = (&config.database, &config.user_keys) {
        router = router.accept(
            LINK_ALPN,
            LinkProtocol {
                links: receiving.open_link.clone(),
                keys: keys.clone(),
                database: database.clone(),
            },
        );
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

    // The user claim this device shows is read from the database, which a handshake or a link can
    // change, so it is refreshed now and every few seconds.
    refresh_claim(&receiving);
    let mut claim_tick = tokio::time::interval(Duration::from_secs(5));
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
                                    state.start(&folder);
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
                                    state.start(&folder);
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
                Some(Command::AddToFolder(namespace, key, source, seen, reply)) => {
                    let folders = folders.clone();
                    let live = folder_live.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => {
                                match folders::add_file_seeing(
                                    &state.docs,
                                    &state.store,
                                    namespace,
                                    &key,
                                    &source,
                                    seen,
                                )
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
                Some(Command::LeaveFolder(namespace, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => {
                                // Stop syncing the directory first, so it does not write to a folder
                                // that is going away.
                                if let Some(task) = state
                                    .tasks
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .remove(&namespace)
                                {
                                    task.abort();
                                }
                                state.disconnect(namespace);
                                folders::leave(&state.docs, &state.database, namespace)
                                    .await
                                    .map_err(folder_error)
                            }
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::SendMessage(addr, body, reply_to, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome =
                            send_message(&receiving, addr, body, reply_to, Vec::new()).await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::SendAttachment(addr, body, path, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = send_attachment(&receiving, addr, body, path).await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::DownloadAttachment(message, index, reply)) => {
                    let outcome = download_attachment(&receiving, message, index);
                    let _ = reply.send(outcome);
                }
                Some(Command::Search(query, limit, reply)) => {
                    let outcome = messages_database(&receiving).and_then(|database| {
                        database
                            .search_messages(&query, limit)
                            .map_err(|error| NodeError::Message(error.to_string()))
                    });
                    let _ = reply.send(outcome);
                }
                Some(Command::MessagesBefore(peer, before, limit, reply)) => {
                    let outcome = messages_database(&receiving).and_then(|database| {
                        database
                            .messages_before(&peer, before, limit)
                            .map_err(|error| NodeError::Message(error.to_string()))
                    });
                    let _ = reply.send(outcome);
                }
                Some(Command::GroupMessagesBefore(group, before, limit, reply)) => {
                    let outcome = group_database(&receiving).and_then(|database| {
                        database
                            .group_messages_before(&group, before, limit)
                            .map_err(group_error)
                    });
                    let _ = reply.send(outcome);
                }
                Some(Command::DeleteMessage(id, reply)) => {
                    let outcome = messages_database(&receiving).and_then(|database| {
                        // A direct message and a group message have different tables; the id is
                        // in one of them.
                        let direct = database
                            .delete_message(id)
                            .map_err(|error| NodeError::Message(error.to_string()))?;
                        let group = database.delete_group_message(id).map_err(group_error)?;
                        Ok(direct || group)
                    });
                    let _ = reply.send(outcome);
                }
                Some(Command::ClearConversation(peer, reply)) => {
                    let outcome = messages_database(&receiving).and_then(|database| {
                        database
                            .clear_conversation(&peer)
                            .map_err(|error| NodeError::Message(error.to_string()))
                    });
                    let _ = reply.send(outcome);
                }
                Some(Command::ClearGroup(group, reply)) => {
                    let outcome = group_database(&receiving).and_then(|database| {
                        database.clear_group_messages(&group).map_err(group_error)
                    });
                    let _ = reply.send(outcome);
                }
                Some(Command::CreateGroup(name, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = create_group(&receiving, &name).await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::GroupDetailsFor(group, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = group_details(&receiving, group).await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::RemoveGroupMember(group, member, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = remove_member(&receiving, group, member).await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::SetGroupRole(group, member, role, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = set_role(&receiving, group, member, role).await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::RenameGroup(group, name, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = rename_group(&receiving, group, &name).await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::GroupRequest(peer, request, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let answer = answer_group(&receiving, peer, request).await;
                        let _ = reply.send(answer);
                    });
                }
                Some(Command::JoinGroup(ticket, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = join_group(&receiving, &ticket).await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::GroupTicketFor(group, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome = group_ticket(&receiving, group).await;
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::SendGroupMessage(group, body, reply_to, reply)) => {
                    let _ = reply.send(send_group_message(&receiving, group, body, reply_to));
                }
                Some(Command::GroupMessages(group, reply)) => {
                    let outcome = group_database(&receiving)
                        .and_then(|database| database.group_messages(&group).map_err(group_error));
                    let _ = reply.send(outcome);
                }
                Some(Command::Groups(reply)) => {
                    let outcome = group_database(&receiving)
                        .and_then(|database| database.groups().map_err(group_error));
                    let _ = reply.send(outcome);
                }
                Some(Command::Messages(peer, reply)) => {
                    let outcome = match &receiving.database {
                        Some(database) => database
                            .messages(&peer)
                            .map_err(|error| NodeError::Message(error.to_string())),
                        None => Err(NodeError::Message("messages need a database".to_owned())),
                    };
                    let _ = reply.send(outcome);
                }
                Some(Command::FolderStatus(namespace, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => folders::status(&state.docs, namespace)
                                .await
                                .map_err(folder_error),
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::FolderHealth(namespace, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => state.health(namespace).await,
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::VerifyFolder(namespace, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => state.verify(namespace, true).await,
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::SetKeepCopy(namespace, keep, reply)) => {
                    let folders = folders.clone();
                    tokio::spawn(async move {
                        let outcome = match folder_state(&folders) {
                            Ok(state) => state.set_keep_copy(namespace, keep).await,
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(outcome);
                    });
                }
                Some(Command::RenameFolder(namespace, name, reply)) => {
                    let outcome = match folder_state(&folders) {
                        Ok(state) => state
                            .database
                            .rename_folder(namespace, &name)
                            .map_err(|error| NodeError::Folder(error.to_string()))
                            .and_then(|found| {
                                if found {
                                    Ok(())
                                } else {
                                    Err(NodeError::Folder("there is no folder with that id".to_owned()))
                                }
                            }),
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(outcome);
                }
                Some(Command::LinkDevice(name, reply)) => {
                    let _ = reply.send(link_device(&receiving, &name));
                }
                Some(Command::AcceptLink(ticket, name, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(accept_link(&receiving, &ticket, &name).await);
                    });
                }
                Some(Command::UserDevices(reply)) => {
                    let _ = reply.send(user_devices(&receiving));
                }
                Some(Command::RevokeDevice(device, reply)) => {
                    let _ = reply.send(revoke_device(&receiving, device));
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
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(pause_transfer(&receiving, id).await);
                    });
                }
                Some(Command::ResumeTransfer(id, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(resume_transfer_any(&receiving, id).await);
                    });
                }
                Some(Command::CancelTransfer(id, reply)) => {
                    let receiving = receiving.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(cancel_transfer(&receiving, id).await);
                    });
                }
                Some(Command::SendFiles(addr, paths, reply)) if receiving.store.is_none() => {
                    let _ = reply.send(Err(NodeError::BlobsDisabled));
                    let _ = (addr, paths);
                }
                Some(Command::SendFiles(addr, paths, reply)) => {
                    let id = receiving.next_id.fetch_add(1, Ordering::SeqCst);
                    let (cancel, cancelled) = oneshot::channel();
                    record_send(&receiving, id, addr.id, cancel);
                    let task_receiving = receiving.clone();
                    tokio::spawn(async move {
                        let outcome =
                            send_files(&task_receiving, id, addr, paths, cancelled).await;
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
            _ = claim_tick.tick() => refresh_claim(&receiving),
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

    // Receives still running stop now, and are queued again, so the next start picks them
    // up from what they have. Stopping them is not a failure.
    let running: Vec<(i64, Option<tokio::task::AbortHandle>)> = lock_book(&receiving.book)
        .iter_mut()
        .filter(|(_, slot)| {
            slot.transfer.direction == Direction::Receive && slot.transfer.state.is_unfinished()
        })
        .map(|(id, slot)| (*id, slot.abort.take()))
        .collect();
    for (id, abort) in running {
        if let Some(abort) = abort {
            abort.abort();
        }
        set_transfer(&receiving, id, Some(TransferState::Queued), None);
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
    /// For a send that is still waiting for its answer: fires to cancel it.
    cancel: Option<oneshot::Sender<()>>,
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
    /// This device, which names its copy in the folder's replica reports.
    me: EndpointId,
    /// The sync task of each folder this device saves, so leaving a folder can stop it.
    tasks: Arc<Mutex<HashMap<NamespaceId, tokio::task::AbortHandle>>>,
    /// The task that keeps each folder connected to its members.
    peers: Arc<Mutex<HashMap<NamespaceId, tokio::task::AbortHandle>>>,
}

impl FolderState {
    /// Keeps a folder connected to the other members, and starts keeping its directory in
    /// step with it, if this device saves it.
    fn start(&self, folder: &Folder) {
        let task = folder_peers::spawn(self.docs.clone(), self.database.clone(), folder.namespace);
        if let Some(old) = self
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(folder.namespace, task)
        {
            old.abort();
        }
        self.sync(folder);
    }

    /// Stops keeping a folder connected, as when it is left.
    fn disconnect(&self, namespace: NamespaceId) {
        if let Some(task) = self
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&namespace)
        {
            task.abort();
        }
    }

    /// Starts keeping a folder's directory in step with it, if this device saves it.
    fn sync(&self, folder: &Folder) {
        if let Some(dir) = folder.path.as_ref().filter(|_| folder.keep_copy) {
            let task = folder_sync::spawn(
                self.docs.clone(),
                std::ops::Deref::deref(&self.store).clone(),
                folder.namespace,
                dir.clone(),
                self.live.clone(),
            );
            if let Some(old) = self
                .tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(folder.namespace, task)
            {
                old.abort();
            }
        }
    }
}

impl FolderState {
    fn keeps(&self, namespace: NamespaceId) -> Result<bool, NodeError> {
        match self.database.folder(namespace) {
            Ok(Some(folder)) => Ok(folder.keep_copy),
            Ok(None) => Err(NodeError::Folder(
                "there is no folder with that id".to_owned(),
            )),
            Err(error) => Err(NodeError::Folder(error.to_string())),
        }
    }

    async fn health(&self, namespace: NamespaceId) -> Result<ReplicaHealth, NodeError> {
        let keeps = self.keeps(namespace)?;
        replicas::health(
            &self.docs,
            std::ops::Deref::deref(&self.store),
            namespace,
            self.me,
            keeps,
            contacts::now(),
        )
        .await
        .map_err(folder_error)
    }

    /// Checks this device's copy of a folder and records the result for the other members.
    async fn verify(&self, namespace: NamespaceId, force: bool) -> Result<ReplicaInfo, NodeError> {
        let keeps = self.keeps(namespace)?;
        replicas::verify(
            &self.docs,
            std::ops::Deref::deref(&self.store),
            namespace,
            self.me,
            keeps,
            contacts::now(),
            force,
        )
        .await
        .map_err(folder_error)
    }

    async fn verify_all(&self) {
        let Ok(folders) = self.database.folders() else {
            return;
        };
        for folder in folders {
            if let Err(error) = self.verify(folder.namespace, false).await {
                warn!(%error, "could not check a shared folder's copy");
            }
        }
    }

    /// Changes whether this device keeps a full copy of a folder, and starts or stops saving
    /// its files to the directory accordingly.
    async fn set_keep_copy(&self, namespace: NamespaceId, keep: bool) -> Result<(), NodeError> {
        replicas::set_keep_copy(&self.docs, &self.database, namespace, keep)
            .await
            .map_err(folder_error)?;
        if keep {
            if let Ok(Some(folder)) = self.database.folder(namespace) {
                self.sync(&folder);
            }
        } else if let Some(task) = self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&namespace)
        {
            task.abort();
        }
        Ok(())
    }
}

/// How long after start the first check of the shared folders waits, and how often they are
/// checked after that.
const VERIFY_DELAY: Duration = Duration::from_secs(5);
const VERIFY_INTERVAL: Duration = Duration::from_secs(60);

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
/// Lets collection also keep the content of downloads in progress, which no tag holds until
/// their file list has arrived.
fn protect_folder_content(sets: Vec<Protected>) -> ProtectCb {
    Arc::new(move |protected: &mut HashSet<Hash>| {
        for live in &sets {
            protected.extend(lock_live(live).iter().copied());
        }
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
        // A device that does not keep a folder lets its files go. The folder's own records
        // are small, and always kept.
        if folder.keep_copy
            && let Ok(entries) = folders::entries(&state.docs, folder.namespace).await
        {
            hashes.extend(entries.iter().map(|entry| entry.hash));
        }
        if let Ok(records) = replicas::record_hashes(&state.docs, folder.namespace).await {
            hashes.extend(records);
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
    withdrawn: Withdrawn,
    /// Content being downloaded, which collection must not remove.
    in_flight: Protected,
    user_keys: Option<Arc<dyn UserKeyStore>>,
    /// The link this device has open for a new device, if any.
    open_link: OpenLinks,
    profile: SharedProfile,
    gate: Arc<dyn PeerGate>,
    /// Messages waiting for their devices to take them. Kept only where there is a database.
    outbox: Option<Outbox>,
    gossip: Option<Gossip>,
    /// The gossip senders of the groups this node is running.
    groups: GroupSenders,
    /// This node's key, which signs the group messages it writes.
    secret: SecretKey,
    /// Addresses of other devices, so they can be dialed by id.
    directory: MemoryLookup,
    /// The document service, which holds each group's membership.
    docs: Option<Docs>,
    /// The task running each group's current topic.
    topics: GroupTasks,
    /// When contacts were last exchanged with each of this user's other devices.
    contact_syncs: Arc<Mutex<HashMap<EndpointId, Instant>>>,
}

/// What a send's offer carries before its files are imported.
fn unsent_offer(receiving: &Receiving) -> FileOffer {
    FileOffer {
        names: Vec::new(),
        root: Hash::EMPTY,
        files: 0,
        bytes: 0,
        from: receiving.endpoint.addr(),
    }
}

/// Sends a message to a trusted contact and keeps it here. The message is queued for the
/// contact's devices, and the first attempt starts in the background. Returns the message as
/// queued, in the state `Sending`. Its state changes as the devices take it.
async fn send_message(
    receiving: &Receiving,
    addr: EndpointAddr,
    body: String,
    reply_to: Option<MessageId>,
    attachments: Vec<Attachment>,
) -> Result<Message, NodeError> {
    let Some(database) = receiving.database.clone() else {
        return Err(NodeError::Message("messages need a database".to_owned()));
    };
    let attachments = messages::clean_attachments(&attachments)
        .map_err(|error| NodeError::Message(error.to_string()))?;
    let body = if body.trim().is_empty() && !attachments.is_empty() {
        String::new()
    } else {
        messages::clean_body(&body).map_err(|error| NodeError::Message(error.to_string()))?
    };
    if !receiving.gate.is_trusted(&addr.id) {
        return Err(NodeError::Message(
            "messages go only to trusted contacts".to_owned(),
        ));
    }
    let own = receiving.endpoint.id();
    database
        .remember_address(addr.id, &addr)
        .map_err(|error| NodeError::Message(error.to_string()))?;
    receiving.directory.add_endpoint_info(addr.clone());
    // Never earlier than this device's previous message to the contact, so its own order holds.
    let timestamp = match database.last_sent_timestamp(&addr.id, &own) {
        Ok(Some(last)) => messages::now_millis().max(last + 1),
        Ok(None) => messages::now_millis(),
        Err(error) => return Err(NodeError::Message(error.to_string())),
    };
    let message = Message {
        id: messages::new_id(),
        conversation: addr.id,
        author: own,
        timestamp,
        body,
        reply_to,
        state: DeliveryState::Sending,
        attachments,
    };
    database
        .insert_message(&message)
        .map_err(|error| NodeError::Message(error.to_string()))?;
    let devices = database
        .contact_devices(&addr.id)
        .map_err(|error| NodeError::Message(error.to_string()))?;
    database
        .enqueue(message.id, &devices)
        .map_err(|error| NodeError::Message(error.to_string()))?;
    for device in devices {
        tokio::spawn(flush_by_dial(receiving.clone(), device, Trigger::Send));
    }
    Ok(message)
}

/// Starts the topic of each group this device is in, with the key each is at.
fn spawn_groups(receiving: &Receiving) {
    let Some(database) = receiving.database.clone() else {
        return;
    };
    let groups = match database.groups() {
        Ok(groups) => groups,
        Err(error) => {
            warn!(%error, "could not read the groups");
            return;
        }
    };
    for group in groups {
        if let Err(error) = start_topic(receiving, group.id) {
            warn!(%error, "a group could not be started");
        }
    }
}

/// Renames a group, as an admin.
async fn rename_group(receiving: &Receiving, group: GroupId, name: &str) -> Result<(), NodeError> {
    let row = row_of(receiving, &group)?;
    let own = receiving.endpoint.id();
    let name = groups::clean_name(name).map_err(group_error)?;
    if !members_of(receiving, &row).await?.is_admin(&own) {
        return Err(NodeError::Group(
            "only an admin can rename the group".to_owned(),
        ));
    }
    write_record(
        receiving,
        row.doc,
        &membership::sign_record(
            &receiving.secret,
            group,
            Change::Name(name.to_owned()),
            messages::now_millis(),
        ),
    )
    .await?;
    group_database(receiving)?
        .set_group_name(&group, &name)
        .map_err(group_error)
}

/// Sends a message with a file attached. The file goes into the blob store and the contact is
/// told where to fetch it: nothing is sent until the contact asks for it. An image also gets a
/// small preview, which travels with the message.
async fn send_attachment(
    receiving: &Receiving,
    addr: EndpointAddr,
    body: String,
    path: PathBuf,
) -> Result<Message, NodeError> {
    let store = receiving.store.clone().ok_or(NodeError::BlobsDisabled)?;
    let imported = files::import(&store, std::slice::from_ref(&path), |_, _| {})
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    // Keeps the file while a message refers to it, as a send does.
    store
        .tags()
        .set(
            format!("attachment-{}", imported.root),
            HashAndFormat::hash_seq(imported.root),
        )
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let preview = attachment_preview(path).await;
    let attachment = Attachment {
        name: imported.names.first().cloned().unwrap_or_default(),
        root: imported.root,
        files: imported.files,
        bytes: imported.bytes,
        preview,
    };
    send_message(receiving, addr, body, None, vec![attachment]).await
}

/// A preview for an image, if the file is one and its preview is small enough to send.
async fn attachment_preview(path: PathBuf) -> Option<Vec<u8>> {
    let is_image = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        });
    if !is_image {
        return None;
    }
    tokio::task::spawn_blocking(move || avatar::thumbnail_from_file(&path).ok())
        .await
        .ok()
        .flatten()
        .filter(|preview| preview.len() <= messages::MAX_PREVIEW_BYTES)
}

/// Fetches an attachment of a message from the contact who sent it, as a transfer. The transfer
/// reports its progress, and can be paused and resumed like any other receive.
fn download_attachment(
    receiving: &Receiving,
    message: MessageId,
    index: usize,
) -> Result<i64, NodeError> {
    let database = group_database_for_messages(receiving)?;
    let found = database
        .message(message)
        .map_err(|error| NodeError::Message(error.to_string()))?
        .ok_or_else(|| NodeError::Message("there is no message with that id".to_owned()))?;
    if found.author == receiving.endpoint.id() {
        return Err(NodeError::Message(
            "this device sent the file, so it has it".to_owned(),
        ));
    }
    let attachment = found
        .attachments
        .get(index)
        .cloned()
        .ok_or_else(|| NodeError::Message("the message has no such attachment".to_owned()))?;
    if receiving.store.is_none() {
        return Err(NodeError::BlobsDisabled);
    }
    let sender = found.author;
    let from = database
        .address_of(&sender)
        .ok()
        .flatten()
        .unwrap_or_else(|| EndpointAddr::from(sender));
    let offer = FileOffer {
        names: vec![attachment.name],
        root: attachment.root,
        files: attachment.files,
        bytes: attachment.bytes,
        from,
    };
    let id = new_transfer(receiving, sender, offer.clone());
    spawn_receive(receiving, id, sender, offer);
    Ok(id)
}

/// The database that messages are kept in.
fn group_database_for_messages(receiving: &Receiving) -> Result<Arc<Database>, NodeError> {
    messages_database(receiving)
}

fn messages_database(receiving: &Receiving) -> Result<Arc<Database>, NodeError> {
    receiving
        .database
        .clone()
        .ok_or_else(|| NodeError::Message("messages need a database".to_owned()))
}

/// The senders of the groups' current topics, so a message can be sent to a group.
type GroupSenders = Arc<Mutex<HashMap<GroupId, GossipSender>>>;

/// The task running each group's current topic, so it can be replaced when the key changes.
type GroupTasks = Arc<Mutex<HashMap<GroupId, tokio::task::AbortHandle>>>;

/// How long a joiner waits for the admin who admitted it to show up as an admin in its own copy
/// of the membership document, before it gives up on that admin's key.
const ADMIN_WAIT: Duration = Duration::from_secs(15);

/// What a member answers a group request with.
enum GroupAnswer {
    Page(GroupPage),
    Key(Option<KeyRecord>),
    Stored,
    Refused,
}

/// The membership document of a group this device is in.
fn docs_of(receiving: &Receiving) -> Result<Docs, NodeError> {
    receiving
        .docs
        .clone()
        .ok_or_else(|| NodeError::Group("group chat needs the document service".to_owned()))
}

/// Reads every membership record in a group's document. Records whose content has not arrived
/// yet are left out until it does.
async fn read_records(
    receiving: &Receiving,
    namespace: NamespaceId,
) -> Result<Vec<Record>, NodeError> {
    let docs = docs_of(receiving)?;
    let store = receiving
        .store
        .clone()
        .ok_or_else(|| NodeError::Group("group chat needs blob storage".to_owned()))?;
    let blobs: &iroh_blobs::api::Store = std::ops::Deref::deref(&store);
    let doc = docs
        .open(namespace)
        .await
        .map_err(group_error)?
        .ok_or_else(|| NodeError::Group("the group's document is missing".to_owned()))?;
    let mut stream = Box::pin(doc.get_many(Query::all()).await.map_err(group_error)?);
    let mut records = Vec::new();
    while let Some(entry) = stream.next().await {
        let entry = entry.map_err(group_error)?;
        if !entry.key().starts_with(b"record/") {
            continue;
        }
        let Ok(bytes) = blobs.blobs().get_bytes(entry.content_hash()).await else {
            continue;
        };
        if let Ok(record) = postcard::from_bytes::<Record>(&bytes) {
            records.push(record);
        }
    }
    Ok(records)
}

/// Writes a membership record into a group's document.
async fn write_record(
    receiving: &Receiving,
    namespace: NamespaceId,
    record: &Record,
) -> Result<(), NodeError> {
    let docs = docs_of(receiving)?;
    let doc = docs
        .open(namespace)
        .await
        .map_err(group_error)?
        .ok_or_else(|| NodeError::Group("the group's document is missing".to_owned()))?;
    let author = docs.author_default().await.map_err(group_error)?;
    let bytes = postcard::to_stdvec(record).map_err(group_error)?;
    doc.set_bytes(author, record_key(record).into_bytes(), bytes)
        .await
        .map_err(group_error)?;
    Ok(())
}

/// Who is in a group, as its document says now.
async fn members_of(receiving: &Receiving, row: &GroupRow) -> Result<Members, NodeError> {
    let records = read_records(receiving, row.doc).await?;
    Ok(Members::replay(row.id, row.founder, &records))
}

fn row_of(receiving: &Receiving, group: &GroupId) -> Result<GroupRow, NodeError> {
    group_database(receiving)?
        .group_row(group)
        .map_err(group_error)?
        .ok_or_else(|| NodeError::Group("there is no group with that id".to_owned()))
}

/// Runs a group's current topic, replacing whatever topic it was on. The key a group is at
/// decides the topic, so this is called whenever the key changes.
fn start_topic(receiving: &Receiving, group: GroupId) -> Result<(), NodeError> {
    let row = row_of(receiving, &group)?;
    let members = topic_bootstrap(receiving, group)?;
    // The old topic's sender goes first, so nothing is sent on it once the key has changed.
    receiving
        .groups
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&group);
    let task = tokio::spawn(run_group(
        receiving.clone(),
        group,
        membership::topic_of(&row.key.key),
        members,
    ));
    let previous = receiving
        .topics
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(group, task.abort_handle());
    if let Some(previous) = previous {
        previous.abort();
    }
    Ok(())
}

/// The other members a group's topic is joined through.
fn topic_bootstrap(receiving: &Receiving, group: GroupId) -> Result<Vec<EndpointId>, NodeError> {
    let database = group_database(receiving)?;
    let own = receiving.endpoint.id();
    Ok(database
        .group_members(&group)
        .map_err(group_error)?
        .into_iter()
        .filter(|member| *member != own)
        .collect())
}

/// Makes a new group, with this device as its founder and an admin. Returns its id.
async fn create_group(receiving: &Receiving, name: &str) -> Result<GroupId, NodeError> {
    let database = group_database(receiving)?;
    let docs = docs_of(receiving)?;
    let name = groups::clean_name(name).map_err(group_error)?;
    let secret = &receiving.secret;
    let own = secret.public();
    let group = groups::new_group_id();
    let namespace = docs.create().await.map_err(group_error)?.id();
    let now = messages::now_millis();
    let key = membership::sign_key(secret, group, 0, membership::new_key(), now);
    database
        .insert_group(&GroupRow {
            id: group,
            name: name.clone(),
            created: now / 1000,
            founder: own,
            doc: namespace,
            key,
        })
        .map_err(group_error)?;
    database
        .add_group_member(group, &own)
        .map_err(group_error)?;
    write_record(
        receiving,
        namespace,
        &membership::sign_record(secret, group, Change::Name(name.to_owned()), now),
    )
    .await?;
    write_record(
        receiving,
        namespace,
        &membership::sign_record(
            secret,
            group,
            Change::Role {
                member: own,
                role: Some(Role::Admin),
            },
            now,
        ),
    )
    .await?;
    start_topic(receiving, group)?;
    Ok(group)
}

/// Joins a group from its ticket. The admin it reaches admits this device as a member, and gives
/// it the group's key. Then it fetches the group's messages from the members it can reach.
async fn join_group(receiving: &Receiving, text: &str) -> Result<GroupId, NodeError> {
    let database = group_database(receiving)?;
    let docs = docs_of(receiving)?;
    let ticket = GroupTicket::parse(text).map_err(group_error)?;
    let group = ticket.group;
    if database.group_row(&group).map_err(group_error)?.is_some() {
        return Ok(group);
    }
    let own = receiving.endpoint.id();
    let doc_ticket: DocTicket = ticket.doc.trim().parse().map_err(group_error)?;
    let namespace = docs.import(doc_ticket).await.map_err(group_error)?.id();
    database
        .insert_group(&GroupRow {
            id: group,
            name: "Group".to_owned(),
            created: messages::now_millis() / 1000,
            founder: ticket.founder,
            doc: namespace,
            key: ticket.key.clone(),
        })
        .map_err(group_error)?;
    database
        .add_group_member(group, &own)
        .map_err(group_error)?;
    let peers: Vec<EndpointAddr> = ticket
        .peers
        .iter()
        .filter(|peer| peer.id != own)
        .cloned()
        .collect();
    for peer in &peers {
        remember_peer(receiving, peer);
        database
            .add_group_member(group, &peer.id)
            .map_err(group_error)?;
    }
    start_topic(receiving, group)?;
    // The first admin that admits us gives the key, which may be newer than the ticket's.
    for peer in &peers {
        if let Ok(Some(record)) = ask_key(receiving, group, peer.id, true).await {
            accept_key(receiving, group, record, true).await.ok();
            break;
        }
    }
    for peer in &peers {
        if fetch_history(receiving, group, peer.id).await.is_ok() {
            break;
        }
    }
    Ok(group)
}

/// The ticket that invites someone to a group. Only a member can give one.
async fn group_ticket(receiving: &Receiving, group: GroupId) -> Result<String, NodeError> {
    let row = row_of(receiving, &group)?;
    let own = receiving.endpoint.id();
    let members = members_of(receiving, &row).await?;
    if !members.is_member(&own) {
        return Err(NodeError::Group(
            "only a member can invite others".to_owned(),
        ));
    }
    let docs = docs_of(receiving)?;
    let doc = folders::ticket(&docs, row.doc, folders::FolderAccess::Write)
        .await
        .map_err(group_error)?;
    let database = group_database(receiving)?;
    let mut peers = vec![receiving.endpoint.addr()];
    for member in members.others(&own) {
        if let Some(addr) = database.address_of(&member).map_err(group_error)? {
            peers.push(addr);
        }
    }
    peers.truncate(MAX_TICKET_PEERS);
    Ok(GroupTicket {
        group,
        founder: row.founder,
        doc,
        key: row.key,
        peers,
    }
    .encode())
}

/// Sends a message to a group. It is stored here, and sent to the members online now. The
/// others fetch it when they come back.
fn send_group_message(
    receiving: &Receiving,
    group: GroupId,
    body: String,
    reply_to: Option<MessageId>,
) -> Result<GroupMessage, NodeError> {
    let database = group_database(receiving)?;
    row_of(receiving, &group)?;
    let body = messages::clean_body(&body).map_err(group_error)?;
    let own = receiving.endpoint.id();
    // Never earlier than this device's previous message to the group, so its own order holds.
    let timestamp = match database.latest_author_timestamp(&group, &own) {
        Ok(Some(last)) => messages::now_millis().max(last + 1),
        Ok(None) => messages::now_millis(),
        Err(error) => return Err(group_error(error)),
    };
    let envelope = groups::sign(
        &receiving.secret,
        group,
        messages::new_id(),
        timestamp,
        body,
        reply_to,
    );
    database
        .insert_group_message(&envelope)
        .map_err(group_error)?;
    let sender = receiving
        .groups
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&group)
        .cloned();
    if let Some(sender) = sender {
        let content = postcard::to_stdvec(&envelope).map_err(group_error)?;
        tokio::spawn(async move {
            let _ = sender.broadcast(bytes::Bytes::from(content)).await;
        });
    }
    Ok(GroupMessage {
        id: envelope.id,
        group,
        author: own,
        timestamp,
        body: envelope.body,
        reply_to,
    })
}

/// The group as its members see it: its name, its key epoch, and each member's role.
async fn group_details(receiving: &Receiving, group: GroupId) -> Result<GroupDetails, NodeError> {
    let row = row_of(receiving, &group)?;
    let own = receiving.endpoint.id();
    let members = members_of(receiving, &row).await?;
    if !members.is_member(&own) {
        return Err(NodeError::Group(
            "only a member can see the group".to_owned(),
        ));
    }
    Ok(GroupDetails {
        id: group,
        name: members.name.clone().unwrap_or(row.name),
        epoch: row.key.epoch,
        you_are_admin: members.is_admin(&own),
        members: members.roles.into_iter().collect(),
    })
}

/// Changes a member's role, or removes them, as an admin. The change is recorded in the
/// document; removing someone also changes the group's key, see [`remove_member`].
async fn set_role(
    receiving: &Receiving,
    group: GroupId,
    member: EndpointId,
    role: Option<Role>,
) -> Result<(), NodeError> {
    let row = row_of(receiving, &group)?;
    let own = receiving.endpoint.id();
    let members = members_of(receiving, &row).await?;
    if !members.is_admin(&own) {
        return Err(NodeError::Group(
            "only an admin can change roles".to_owned(),
        ));
    }
    if member == row.founder && role != Some(Role::Admin) {
        return Err(NodeError::Group("the founder stays an admin".to_owned()));
    }
    let now = messages::now_millis();
    write_record(
        receiving,
        row.doc,
        &membership::sign_record(&receiving.secret, group, Change::Role { member, role }, now),
    )
    .await
}

/// Removes a member as an admin. The group moves to a new key, so to a new topic, and the new
/// key is given to each member who remains. The removed member is not sent it, so they stop
/// receiving the group's new messages.
async fn remove_member(
    receiving: &Receiving,
    group: GroupId,
    member: EndpointId,
) -> Result<(), NodeError> {
    let row = row_of(receiving, &group)?;
    let own = receiving.endpoint.id();
    let members = members_of(receiving, &row).await?;
    if !members.is_admin(&own) {
        return Err(NodeError::Group(
            "only an admin can remove a member".to_owned(),
        ));
    }
    if member == row.founder {
        return Err(NodeError::Group("the founder cannot be removed".to_owned()));
    }
    if !members.is_member(&member) {
        return Err(NodeError::Group("that endpoint is not a member".to_owned()));
    }
    let now = messages::now_millis();
    write_record(
        receiving,
        row.doc,
        &membership::sign_record(
            &receiving.secret,
            group,
            Change::Role { member, role: None },
            now,
        ),
    )
    .await?;
    let remaining = members.others(&own);
    let key = membership::sign_key(
        &receiving.secret,
        group,
        row.key.epoch + 1,
        membership::new_key(),
        now,
    );
    group_database(receiving)?
        .set_group_key(&group, &key)
        .map_err(group_error)?;
    start_topic(receiving, group)?;
    for peer in remaining {
        if peer == member {
            continue;
        }
        let receiving = receiving.clone();
        let key = key.clone();
        tokio::spawn(async move {
            if let Err(error) = push_key(&receiving, peer, key).await {
                warn!(%error, "a member could not be given the group's new key");
            }
        });
    }
    Ok(())
}

/// Gives a member the group's key for a newer epoch. Only the key is sent; the member checks
/// it when it arrives.
async fn push_key(
    receiving: &Receiving,
    peer: EndpointId,
    key: KeyRecord,
) -> Result<(), NodeError> {
    let database = group_database(receiving)?;
    let addr = database
        .address_of(&peer)
        .map_err(group_error)?
        .unwrap_or_else(|| EndpointAddr::from(peer));
    let (connection, _peer) = open_peer(
        &receiving.endpoint,
        addr,
        &receiving.profile,
        &receiving.events,
        &receiving.gate,
    )
    .await?;
    let (mut send, mut recv) = connection.open_bi().await.map_err(connection_error)?;
    protocol::write_message(&mut send, &Request::GroupKeyPush(key))
        .await
        .map_err(NodeError::Connect)?;
    let reply: Reply = protocol::read_message(&mut recv)
        .await
        .map_err(NodeError::Connect)?;
    connection.close(0u32.into(), b"key");
    match reply {
        Reply::Stored => Ok(()),
        _ => Err(NodeError::Group(
            "the member did not take the key".to_owned(),
        )),
    }
}

/// Asks a member for the group's current key. Joining asks to be admitted, which only an admin
/// does; an existing member asks only for the key.
async fn ask_key(
    receiving: &Receiving,
    group: GroupId,
    peer: EndpointId,
    join: bool,
) -> Result<Option<KeyRecord>, NodeError> {
    let database = group_database(receiving)?;
    let addr = database
        .address_of(&peer)
        .map_err(group_error)?
        .unwrap_or_else(|| EndpointAddr::from(peer));
    let (connection, _peer) = open_peer(
        &receiving.endpoint,
        addr,
        &receiving.profile,
        &receiving.events,
        &receiving.gate,
    )
    .await?;
    let request = if join {
        Request::GroupJoin { group }
    } else {
        Request::GroupKey { group }
    };
    let (mut send, mut recv) = connection.open_bi().await.map_err(connection_error)?;
    protocol::write_message(&mut send, &request)
        .await
        .map_err(NodeError::Connect)?;
    let key: Option<KeyRecord> = protocol::read_message(&mut recv)
        .await
        .map_err(NodeError::Connect)?;
    connection.close(0u32.into(), b"key");
    Ok(key)
}

/// Keeps a group key if it is newer than the one held, and signed by a member who is an admin
/// (or by the founder). When `wait` is set, a joiner waits for the signer to show as an admin in
/// its own copy of the document, which can lag behind the admin that admitted it.
async fn accept_key(
    receiving: &Receiving,
    group: GroupId,
    record: KeyRecord,
    wait: bool,
) -> Result<bool, NodeError> {
    let row = row_of(receiving, &group)?;
    if record.group != group || !membership::verify_key(&record) || record.epoch <= row.key.epoch {
        return Ok(false);
    }
    let started = Instant::now();
    loop {
        let members = members_of(receiving, &row).await?;
        if members.is_admin(&record.by) || record.by == row.founder {
            group_database(receiving)?
                .set_group_key(&group, &record)
                .map_err(group_error)?;
            start_topic(receiving, group)?;
            return Ok(true);
        }
        if !wait || started.elapsed() >= ADMIN_WAIT {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Brings this device up to date with a member: the group's current key, then the messages it
/// missed.
async fn sync_with(receiving: &Receiving, group: GroupId, peer: EndpointId) {
    // Only members are asked. A removed member can still be on the group's old topic, and
    // would answer with what it wrote after it was removed.
    let Ok(row) = row_of(receiving, &group) else {
        return;
    };
    match members_of(receiving, &row).await {
        Ok(members) if members.is_member(&peer) => {}
        _ => return,
    }
    if let Ok(Some(record)) = ask_key(receiving, group, peer, false).await {
        let _ = accept_key(receiving, group, record, false).await;
    }
    let _ = fetch_history(receiving, group, peer).await;
}

/// Answers a group request from a trusted peer. Only members get history and the key; the
/// admin the peer asks to join admits it, if it is not a member yet.
async fn answer_group(receiving: &Receiving, peer: EndpointId, request: Request) -> GroupAnswer {
    let own = receiving.endpoint.id();
    let group = match &request {
        Request::GroupHistory { group, .. }
        | Request::GroupKey { group }
        | Request::GroupJoin { group } => *group,
        Request::GroupKeyPush(record) => record.group,
        _ => return GroupAnswer::Refused,
    };
    let Ok(row) = row_of(receiving, &group) else {
        return GroupAnswer::Refused;
    };
    let Ok(mut members) = members_of(receiving, &row).await else {
        return GroupAnswer::Refused;
    };
    match request {
        Request::GroupHistory { group, after } => {
            if !members.is_member(&peer) {
                return GroupAnswer::Refused;
            }
            let Ok(database) = group_database(receiving) else {
                return GroupAnswer::Refused;
            };
            let Ok(mut rows) = database.group_envelopes_after(&group, after, groups::PAGE_SIZE + 1)
            else {
                return GroupAnswer::Refused;
            };
            let more = rows.len() > groups::PAGE_SIZE;
            rows.truncate(groups::PAGE_SIZE);
            // The other members this device has addresses for, so the asker can reach them.
            let addresses = members
                .others(&own)
                .into_iter()
                .filter(|member| *member != peer)
                .filter_map(|member| database.address_of(&member).ok().flatten())
                .take(MAX_PAGE_MEMBERS)
                .collect();
            GroupAnswer::Page(GroupPage {
                messages: rows,
                more,
                members: addresses,
            })
        }
        Request::GroupKey { .. } => {
            if members.is_member(&peer) {
                GroupAnswer::Key(Some(row.key))
            } else {
                GroupAnswer::Refused
            }
        }
        Request::GroupJoin { .. } => {
            if !members.is_admin(&own) {
                return GroupAnswer::Key(None);
            }
            if !members.is_member(&peer) {
                let record = membership::sign_record(
                    &receiving.secret,
                    group,
                    Change::Role {
                        member: peer,
                        role: Some(Role::Member),
                    },
                    messages::now_millis(),
                );
                if write_record(receiving, row.doc, &record).await.is_err() {
                    return GroupAnswer::Refused;
                }
                members = match members_of(receiving, &row).await {
                    Ok(members) => members,
                    Err(_) => return GroupAnswer::Refused,
                };
            }
            if members.is_member(&peer) {
                GroupAnswer::Key(Some(row.key))
            } else {
                GroupAnswer::Refused
            }
        }
        Request::GroupKeyPush(record) => match accept_key(receiving, group, record, false).await {
            Ok(true) => GroupAnswer::Stored,
            _ => GroupAnswer::Refused,
        },
        _ => GroupAnswer::Refused,
    }
}

/// How long a group waits before joining its topic again after the subscription ends. It
/// doubles each time the joining fails, up to `GROUP_REJOIN_MAX`.
const GROUP_REJOIN_FIRST: Duration = Duration::from_secs(1);
const GROUP_REJOIN_MAX: Duration = Duration::from_secs(30);
/// How often a group asks its neighbors for messages it has missed, and how long after a
/// neighbor leaves.
const GROUP_CATCH_UP: Duration = Duration::from_secs(60);
const GROUP_CATCH_UP_AFTER_LEAVE: Duration = Duration::from_secs(1);

/// Runs one group's gossip topic until it is replaced or the node stops. Messages that arrive
/// are checked and stored, and each member that connects is brought up to date.
///
/// Gossip does not promise delivery, so the group also asks its neighbors for what it missed:
/// when a neighbor leaves, since a message on its way may have gone with it, and every so
/// often. And gossip closes a subscription that falls behind, so the topic is joined again
/// whenever its subscription ends.
async fn run_group(
    receiving: Receiving,
    group: GroupId,
    topic: TopicId,
    bootstrap: Vec<EndpointId>,
) {
    let Some(gossip) = receiving.gossip.clone() else {
        return;
    };
    let mut bootstrap = bootstrap;
    let mut wait = GROUP_REJOIN_FIRST;
    loop {
        match gossip.subscribe(topic, bootstrap.clone()).await {
            Ok(subscription) => {
                wait = GROUP_REJOIN_FIRST;
                follow_group(&receiving, group, subscription).await;
                info!("a group's subscription ended; joining its topic again");
            }
            Err(error) => {
                warn!(%error, "could not join a group's topic; trying again");
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(GROUP_REJOIN_MAX);
        // The members may have changed while the subscription was down.
        if let Ok(members) = topic_bootstrap(&receiving, group) {
            bootstrap = members;
        }
    }
}

/// Follows one subscription to a group's topic until it ends.
async fn follow_group(
    receiving: &Receiving,
    group: GroupId,
    subscription: iroh_gossip::api::GossipTopic,
) {
    let (sender, mut receiver) = subscription.split();
    receiving
        .groups
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(group, sender);
    let catch_up = |member: EndpointId, delay: Duration| {
        let syncing = receiving.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            sync_with(&syncing, group, member).await;
        });
    };
    let mut tick =
        tokio::time::interval_at(tokio::time::Instant::now() + GROUP_CATCH_UP, GROUP_CATCH_UP);
    loop {
        tokio::select! {
            event = receiver.next() => match event {
                Some(Ok(GossipEvent::Received(message))) => {
                    if let Ok(envelope) = postcard::from_bytes::<GroupEnvelope>(&message.content)
                        && envelope.group == group
                    {
                        store_group_envelope(receiving, envelope);
                    }
                }
                Some(Ok(GossipEvent::NeighborUp(member))) => {
                    if let Some(database) = &receiving.database {
                        let _ = database.add_group_member(group, &member);
                    }
                    catch_up(member, Duration::ZERO);
                }
                Some(Ok(GossipEvent::NeighborDown(_))) => {
                    for member in receiver.neighbors() {
                        catch_up(member, GROUP_CATCH_UP_AFTER_LEAVE);
                    }
                }
                Some(Ok(GossipEvent::Lagged)) => {
                    warn!("a group's subscription fell behind");
                    return;
                }
                Some(Err(error)) => {
                    warn!(%error, "a group's subscription stopped");
                    return;
                }
                None => return,
            },
            _ = tick.tick() => {
                for member in receiver.neighbors() {
                    catch_up(member, Duration::ZERO);
                }
            }
        }
    }
}

/// Stores a group message if it is genuine and from a trusted contact. Returns whether it was
/// new.
fn store_group_envelope(receiving: &Receiving, envelope: GroupEnvelope) -> bool {
    let Some(database) = receiving.database.clone() else {
        return false;
    };
    let own = receiving.endpoint.id();
    if groups::verify(&envelope).is_err() {
        warn!("a group message's signature does not match its author; dropped");
        return false;
    }
    if envelope.author != own && !receiving.gate.is_trusted(&envelope.author) {
        return false;
    }
    match database.insert_group_message(&envelope) {
        Ok(true) => {
            let _ = receiving
                .events
                .send(NodeEvent::GroupMessageReceived(GroupMessage {
                    id: envelope.id,
                    group: envelope.group,
                    author: envelope.author,
                    timestamp: envelope.timestamp,
                    body: envelope.body,
                    reply_to: envelope.reply_to,
                }));
            true
        }
        Ok(false) => false,
        Err(error) => {
            warn!(%error, "could not store a group message");
            false
        }
    }
}

/// Asks a member for the group's messages this device does not have, a page at a time.
/// Returns how many new messages were stored.
async fn fetch_history(
    receiving: &Receiving,
    group: GroupId,
    peer: EndpointId,
) -> Result<usize, NodeError> {
    let database = group_database(receiving)?;
    let own = receiving.endpoint.id();
    let mut after = database
        .latest_group_position(&group)
        .map_err(group_error)?;
    let mut stored = 0;
    loop {
        let addr = database
            .address_of(&peer)
            .map_err(group_error)?
            .unwrap_or_else(|| EndpointAddr::from(peer));
        let (connection, _peer) = open_peer(
            &receiving.endpoint,
            addr,
            &receiving.profile,
            &receiving.events,
            &receiving.gate,
        )
        .await?;
        let page = request_page(&connection, group, after).await;
        connection.close(0u32.into(), b"synced");
        let page = page?;
        for member in page.members {
            if member.id != own {
                remember_peer(receiving, &member);
                database
                    .add_group_member(group, &member.id)
                    .map_err(group_error)?;
            }
        }
        if page.messages.is_empty() {
            break;
        }
        let mut last = after;
        for envelope in page.messages {
            last = Some((envelope.timestamp, envelope.id));
            if store_group_envelope(receiving, envelope) {
                stored += 1;
            }
        }
        if !page.more {
            break;
        }
        after = last;
    }
    Ok(stored)
}

/// Sends one history request on a connection, and reads the page that answers it.
async fn request_page(
    connection: &Connection,
    group: GroupId,
    after: Option<(i64, MessageId)>,
) -> Result<GroupPage, NodeError> {
    let (mut send, mut recv) = connection.open_bi().await.map_err(connection_error)?;
    protocol::write_message(&mut send, &Request::GroupHistory { group, after })
        .await
        .map_err(NodeError::Connect)?;
    protocol::read_message(&mut recv)
        .await
        .map_err(NodeError::Connect)
}

/// Remembers how to reach a peer, in the database and the endpoint's address book.
fn remember_peer(receiving: &Receiving, addr: &EndpointAddr) {
    if let Some(database) = &receiving.database {
        let _ = database.remember_address(addr.id, addr);
    }
    receiving.directory.add_endpoint_info(addr.clone());
}

/// The database that group chat keeps its data in.
fn group_database(receiving: &Receiving) -> Result<Arc<Database>, NodeError> {
    receiving
        .database
        .clone()
        .ok_or_else(|| NodeError::Group("group chat needs a database".to_owned()))
}

fn group_error(error: impl std::fmt::Display) -> NodeError {
    NodeError::Group(error.to_string())
}

/// Most members a ticket names, so it stays short enough to paste.
const MAX_TICKET_PEERS: usize = 8;
/// Most members a history page names, so a page stays under the message limit.
const MAX_PAGE_MEMBERS: usize = 16;

/// Starts the outbox's background work: a flush to each device that connects, and a periodic
/// check for messages whose retry time has come. The check also runs once at startup.
fn spawn_outbox(receiving: Receiving, outbox: Outbox, mut events: broadcast::Receiver<NodeEvent>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(OUTBOX_TICK);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let now = messages::now_millis();
                    if let Err(error) = outbox.database().expire_outbox(now) {
                        warn!(%error, "could not expire the outbox");
                    }
                    match outbox.database().devices_due(now) {
                        Ok(devices) => {
                            for device in devices {
                                tokio::spawn(flush_by_dial(receiving.clone(), device, Trigger::Tick));
                            }
                        }
                        Err(error) => warn!(%error, "could not read the outbox"),
                    }
                }
                event = events.recv() => match event {
                    Ok(NodeEvent::PeerConnected(peer)) => {
                        tokio::spawn(flush_by_dial(
                            receiving.clone(),
                            peer.endpoint_id,
                            Trigger::Connected,
                        ));
                        tokio::spawn(sync_user_contacts(receiving.clone(), peer.endpoint_id));
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
}

/// How long two devices of a user wait before they exchange contacts again. Each exchange is a
/// connection, which the other side sees as a connection too, so this keeps them from looping.
const CONTACT_SYNC_COOLDOWN: Duration = Duration::from_secs(30);

/// Asks another device of this user for the contacts it trusts, and trusts them here too.
async fn sync_user_contacts(receiving: Receiving, peer: EndpointId) {
    let Some(database) = receiving.database.clone() else {
        return;
    };
    if !database.is_own_device(&peer).unwrap_or(false) {
        return;
    }
    {
        let mut synced = receiving
            .contact_syncs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if synced
            .get(&peer)
            .is_some_and(|at| at.elapsed() < CONTACT_SYNC_COOLDOWN)
        {
            return;
        }
        synced.insert(peer, Instant::now());
    }
    let addr = database
        .address_of(&peer)
        .ok()
        .flatten()
        .unwrap_or_else(|| EndpointAddr::from(peer));
    let Ok((connection, _)) = open_peer(
        &receiving.endpoint,
        addr,
        &receiving.profile,
        &receiving.events,
        &receiving.gate,
    )
    .await
    else {
        return;
    };
    let contacts: Option<Vec<crate::user::SharedContact>> = async {
        let (mut send, mut recv) = connection.open_bi().await.ok()?;
        protocol::write_message(&mut send, &Request::UserContacts)
            .await
            .ok()?;
        protocol::read_message(&mut recv).await.ok()
    }
    .await;
    connection.close(0u32.into(), b"contacts");
    let own = receiving.endpoint.id();
    let now = contacts::now();
    for contact in contacts.unwrap_or_default() {
        if contact.endpoint != own && !database.is_own_device(&contact.endpoint).unwrap_or(false) {
            let _ = database.adopt_contact(&contact, now);
        }
    }
}

/// How often the outbox is checked for messages whose retry time has come.
const OUTBOX_TICK: Duration = Duration::from_secs(60);

/// Writes the messages waiting for a device, dialing it first. Only one flush runs for a device
/// at a time, and a flush with nothing due does not dial.
async fn flush_by_dial(receiving: Receiving, device: EndpointId, trigger: Trigger) {
    let Some(outbox) = receiving.outbox.clone() else {
        return;
    };
    if !outbox.claim(&device, trigger) {
        return;
    }
    if outbox.has_work(&device, trigger) {
        let addr = outbox
            .database()
            .address_of(&device)
            .ok()
            .flatten()
            .unwrap_or_else(|| EndpointAddr::from(device));
        match open_peer(
            &receiving.endpoint,
            addr,
            &receiving.profile,
            &receiving.events,
            &receiving.gate,
        )
        .await
        {
            Ok((connection, _peer)) => {
                outbox.flush_on(&connection, device, trigger).await;
                connection.close(0u32.into(), b"flushed");
            }
            Err(error) => outbox.unreachable(&device, &error.to_string(), trigger),
        }
    }
    outbox.release(&device);
}

/// Offers `paths` to a trusted peer, as transfer `id`, and waits for its answer. An accepted
/// offer is fetched by the peer from this node's store. The send can be cancelled while it
/// waits, which stops it at once.
async fn send_files(
    receiving: &Receiving,
    id: i64,
    addr: EndpointAddr,
    paths: Vec<PathBuf>,
    cancelled: oneshot::Receiver<()>,
) -> Result<(), NodeError> {
    tokio::select! {
        outcome = offer_files(receiving, id, addr, paths) => outcome,
        _ = cancelled => Ok(()),
    }
}

async fn offer_files(
    receiving: &Receiving,
    id: i64,
    addr: EndpointAddr,
    paths: Vec<PathBuf>,
) -> Result<(), NodeError> {
    let store = receiving.store.clone().ok_or(NodeError::BlobsDisabled)?;
    let progress = receiving.events.clone();
    let imported = match files::import(&store, &paths, |done, total| {
        let _ = progress.send(NodeEvent::ImportProgress { done, total });
    })
    .await
    {
        Ok(imported) => imported,
        Err(error) => {
            let error = NodeError::Files(error.to_string());
            set_transfer(
                receiving,
                id,
                Some(TransferState::Failed(error.to_string())),
                None,
            );
            return Err(error);
        }
    };
    store
        .tags()
        .set(
            format!("send-{}", imported.root),
            HashAndFormat::hash_seq(imported.root),
        )
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    // Sending the same content again makes it available again.
    let hashes = hashes_of_collection(&store, imported.root).await?;
    for hash in &hashes {
        lock_withdrawn(&receiving.withdrawn).remove(hash);
    }
    let offer = FileOffer {
        names: imported.names,
        root: imported.root,
        files: imported.files,
        bytes: imported.bytes,
        from: receiving.endpoint.addr(),
    };
    record_offer(receiving, id, offer.clone());

    let outcome = exchange_offer(receiving, addr, offer).await;
    match &outcome {
        Ok(()) => set_transfer(receiving, id, Some(TransferState::Completed), None),
        Err(error) => set_transfer(
            receiving,
            id,
            Some(TransferState::Failed(error.to_string())),
            None,
        ),
    }
    outcome
}

/// Sends the offer, and reads the peer's answer.
async fn exchange_offer(
    receiving: &Receiving,
    addr: EndpointAddr,
    offer: FileOffer,
) -> Result<(), NodeError> {
    let (connection, _peer) = open_peer(
        &receiving.endpoint,
        addr,
        &receiving.profile,
        &receiving.events,
        &receiving.gate,
    )
    .await?;
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
        Reply::Pong | Reply::Stored => Err(NodeError::Connect(ProtocolError::Closed)),
    }
}

/// The root and every file of a collection, which is everything a share or send serves.
async fn hashes_of_collection(store: &FsStore, root: Hash) -> Result<Vec<Hash>, NodeError> {
    let bytes = store
        .blobs()
        .get_bytes(root)
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let links = HashSeq::try_from(bytes).map_err(|error| NodeError::Files(error.to_string()))?;
    let mut hashes = vec![root];
    hashes.extend(links.iter());
    Ok(hashes)
}

fn lock_withdrawn(
    withdrawn: &Withdrawn,
) -> std::sync::MutexGuard<'_, std::collections::HashSet<Hash>> {
    withdrawn.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Records a send as waiting for its answer, before its files are imported.
fn record_send(receiving: &Receiving, id: i64, peer: EndpointId, cancel: oneshot::Sender<()>) {
    let now = contacts::now();
    let transfer = Transfer {
        id,
        direction: Direction::Send,
        peer,
        offer: unsent_offer(receiving),
        state: TransferState::Queued,
        done: 0,
        created: now,
        updated: now,
    };
    lock_book(&receiving.book).insert(
        id,
        Slot {
            transfer: transfer.clone(),
            abort: None,
            cancel: Some(cancel),
        },
    );
    let _ = receiving.events.send(NodeEvent::TransferChanged(transfer));
}

/// Stores the send's offer once its files are imported, and saves the send.
fn record_offer(receiving: &Receiving, id: i64, offer: FileOffer) {
    let saved = {
        let mut book = lock_book(&receiving.book);
        let Some(slot) = book.get_mut(&id) else {
            return;
        };
        slot.transfer.offer = offer;
        slot.transfer.updated = contacts::now();
        slot.transfer.clone()
    };
    if let Some(Err(error)) = receiving
        .database
        .as_ref()
        .map(|database| database.insert_transfer(&saved))
    {
        warn!(%error, "could not save a transfer");
    }
    let _ = receiving.events.send(NodeEvent::TransferChanged(saved));
}

/// Cancels a send. A send still waiting for its answer stops at once. One the peer has
/// accepted stops being served, to everyone, including trusted contacts.
async fn cancel_send(receiving: &Receiving, id: i64) -> Result<(), NodeError> {
    let (state, root, cancel) = {
        let mut book = lock_book(&receiving.book);
        let slot = book.get_mut(&id).ok_or(NodeError::UnknownTransfer)?;
        (
            slot.transfer.state.clone(),
            slot.transfer.offer.root,
            slot.cancel.take(),
        )
    };
    match state {
        TransferState::Queued => {
            if let Some(cancel) = cancel {
                let _ = cancel.send(());
            }
            set_transfer(receiving, id, Some(TransferState::Cancelled), None);
            Ok(())
        }
        TransferState::Completed | TransferState::Paused => {
            if let Some(store) = &receiving.store {
                let hashes = hashes_of_collection(store, root).await?;
                lock_withdrawn(&receiving.withdrawn).extend(hashes);
                if let Err(error) = store.tags().delete(format!("send-{root}")).await {
                    warn!(%error, "could not release a sent file");
                }
            }
            set_transfer(receiving, id, Some(TransferState::Cancelled), None);
            Ok(())
        }
        _ => Err(NodeError::WrongTransferState("cancelled")),
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

/// Pauses a transfer. A receive stops fetching. An accepted send stops being served, to
/// everyone, until it is resumed. A send still waiting for its answer cannot be paused.
async fn pause_transfer(receiving: &Receiving, id: i64) -> Result<(), NodeError> {
    let (direction, state, root) = {
        let book = lock_book(&receiving.book);
        let slot = book.get(&id).ok_or(NodeError::UnknownTransfer)?;
        (
            slot.transfer.direction,
            slot.transfer.state.clone(),
            slot.transfer.offer.root,
        )
    };
    match direction {
        Direction::Receive => stop_transfer(receiving, id, TransferState::Paused),
        Direction::Send => {
            if state != TransferState::Completed {
                return Err(NodeError::WrongTransferState("paused"));
            }
            if let Some(store) = &receiving.store {
                let hashes = hashes_of_collection(store, root).await?;
                lock_withdrawn(&receiving.withdrawn).extend(hashes);
            }
            set_transfer(receiving, id, Some(TransferState::Paused), None);
            Ok(())
        }
    }
}

/// Resumes a transfer, whichever way it goes.
async fn resume_transfer_any(receiving: &Receiving, id: i64) -> Result<(), NodeError> {
    let (direction, state, root) = {
        let book = lock_book(&receiving.book);
        let slot = book.get(&id).ok_or(NodeError::UnknownTransfer)?;
        (
            slot.transfer.direction,
            slot.transfer.state.clone(),
            slot.transfer.offer.root,
        )
    };
    match direction {
        Direction::Receive => resume_transfer(receiving, id),
        Direction::Send => {
            if state != TransferState::Paused {
                return Err(NodeError::WrongTransferState("resumed"));
            }
            if let Some(store) = &receiving.store {
                let hashes = hashes_of_collection(store, root).await?;
                for hash in &hashes {
                    lock_withdrawn(&receiving.withdrawn).remove(hash);
                }
            }
            set_transfer(receiving, id, Some(TransferState::Completed), None);
            Ok(())
        }
    }
}

/// Stops protecting a download's content as in progress. The transfer's tag, if it has one,
/// still keeps it. Removes only what is known: the root, and its files if the list is stored.
async fn forget_in_flight(receiving: &Receiving, root: Hash) {
    let hashes = match &receiving.store {
        Some(store) => hashes_of_collection(store, root)
            .await
            .unwrap_or_else(|_| vec![root]),
        None => vec![root],
    };
    let mut in_flight = lock_live(&receiving.in_flight);
    for hash in hashes {
        in_flight.remove(&hash);
    }
}

/// Sets the user claim this device shows in handshakes from the database: its certificate, and the
/// revocations the user has made. A device with no certificate shows none.
fn refresh_claim(receiving: &Receiving) {
    let Some(database) = &receiving.database else {
        return;
    };
    let device = receiving.endpoint.id();
    let claim = database.own_user().ok().flatten().and_then(|user| {
        let cert = database
            .devices_of(&user)
            .ok()?
            .into_iter()
            .find(|entry| entry.cert.device == device && entry.revoked.is_none())?
            .cert;
        Some(UserClaim {
            certificate: cert,
            revocations: database.revocations_of(&user).unwrap_or_default(),
        })
    });
    if let Ok(mut profile) = receiving.profile.lock() {
        profile.user = claim;
    }
}

fn user_error(error: impl std::fmt::Display) -> NodeError {
    NodeError::User(error.to_string())
}

/// The database and key store linking needs, or why this node has none.
fn link_parts(
    receiving: &Receiving,
) -> Result<(&Arc<Database>, &Arc<dyn UserKeyStore>), NodeError> {
    let database = receiving.database.as_ref().ok_or(NodeError::NoDatabase)?;
    let keys = receiving
        .user_keys
        .as_ref()
        .ok_or_else(|| user_error("this node has no place to keep a user key"))?;
    Ok((database, keys))
}

/// Opens a link for a new device, and returns its ticket. The first time, this device becomes a
/// user: a key is made, and this device gets its certificate, named `device_name`.
fn link_device(receiving: &Receiving, device_name: &str) -> Result<String, NodeError> {
    let (database, keys) = link_parts(receiving)?;
    let device = receiving.endpoint.id();
    let user = match keys.load() {
        Some(user) => user,
        None => {
            let user = SecretKey::generate();
            keys.save(&user).map_err(user_error)?;
            user
        }
    };
    let public = user.public();
    database.set_own_user(&public).map_err(user_error)?;
    let has_certificate = database
        .devices_of(&public)
        .map_err(user_error)?
        .iter()
        .any(|entry| entry.cert.device == device);
    if !has_certificate {
        let cert = DeviceCert::issue(&user, device, device_name, contacts::now());
        database.save_device(&cert).map_err(user_error)?;
    }
    refresh_claim(receiving);
    let secret = link::open_link(&receiving.open_link);
    Ok(LinkTicket {
        user: public,
        secret,
        addr: receiving.endpoint.addr(),
    }
    .encode())
}

/// Links this device to the user in a ticket from one of their devices.
async fn accept_link(
    receiving: &Receiving,
    ticket: &str,
    device_name: &str,
) -> Result<PublicKey, NodeError> {
    let (database, keys) = link_parts(receiving)?;
    if keys.load().is_some() {
        return Err(user_error("this device already belongs to a user"));
    }
    let ticket = LinkTicket::parse(ticket).map_err(user_error)?;
    let user = link::accept_link(
        &receiving.endpoint,
        &ticket,
        device_name,
        keys.as_ref(),
        database,
    )
    .await
    .map_err(user_error)?;
    refresh_claim(receiving);
    Ok(user)
}

/// The devices of the user this device belongs to, including itself.
fn user_devices(receiving: &Receiving) -> Result<Vec<UserDevice>, NodeError> {
    let database = receiving.database.as_ref().ok_or(NodeError::NoDatabase)?;
    match database.own_user().map_err(user_error)? {
        Some(user) => database.devices_of(&user).map_err(user_error),
        None => Ok(Vec::new()),
    }
}

/// Revokes one of the user's devices. Contacts refuse it once they have the revocation, which
/// they get the next time they connect to a device of this user.
fn revoke_device(receiving: &Receiving, device: EndpointId) -> Result<(), NodeError> {
    let (database, keys) = link_parts(receiving)?;
    if device == receiving.endpoint.id() {
        return Err(user_error("a device cannot revoke itself"));
    }
    let user = keys
        .load()
        .ok_or_else(|| user_error("this device does not belong to a user"))?;
    let known = database
        .devices_of(&user.public())
        .map_err(user_error)?
        .iter()
        .any(|entry| entry.cert.device == device);
    if !known {
        return Err(user_error("that is not one of this user's devices"));
    }
    let revocation = Revocation::issue(&user, device, contacts::now());
    database.save_revocation(&revocation).map_err(user_error)?;
    refresh_claim(receiving);
    Ok(())
}

/// Cancels a transfer, whichever way it goes.
async fn cancel_transfer(receiving: &Receiving, id: i64) -> Result<(), NodeError> {
    let direction = lock_book(&receiving.book)
        .get(&id)
        .map(|slot| slot.transfer.direction)
        .ok_or(NodeError::UnknownTransfer)?;
    match direction {
        Direction::Send => cancel_send(receiving, id).await,
        Direction::Receive => stop_transfer(receiving, id, TransferState::Cancelled),
    }
}

/// Records a new transfer as queued, and returns its id.
fn new_transfer(receiving: &Receiving, peer: EndpointId, offer: FileOffer) -> i64 {
    let id = receiving.next_id.fetch_add(1, Ordering::SeqCst);
    let now = contacts::now();
    let transfer = Transfer {
        id,
        direction: Direction::Receive,
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
            cancel: None,
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
        // Tag the content first, so collection keeps what is already fetched for as long as the
        // transfer exists. Without this, a download longer than the collection interval has its
        // partial blobs removed, and fails when it is saved.
        if let Some(store) = &task.store
            && let Err(error) = store
                .tags()
                .set(
                    format!("transfer-{id}"),
                    HashAndFormat::hash_seq(offer.root),
                )
                .await
        {
            warn!(%error, "could not keep a transfer's content");
        }
        let _slot = task.slots.clone().acquire_owned().await;
        set_transfer(&task, id, Some(TransferState::Active), None);
        match receive(&task, id, &offer).await {
            Ok(()) => {
                // The transfer's tag keeps the content from here on. It was marked completed
                // before the event that says so was sent.
                forget_in_flight(&task, offer.root).await;
            }
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

    // Read the list of files first, and protect every file from collection before any of its
    // data arrives. Collection could otherwise remove a file partway through its download.
    let list = iroh_blobs::get::request::get_blob(connection.clone(), offer.root)
        .await
        .map_err(|error| NodeError::Files(error.to_string()))?;
    let links = HashSeq::try_from(list).map_err(|error| NodeError::Files(error.to_string()))?;
    {
        let mut in_flight = lock_live(&receiving.in_flight);
        in_flight.insert(offer.root);
        in_flight.extend(links.iter());
    }

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
    // Completed before the event, so anyone who sees the event also sees the final state.
    set_transfer(
        receiving,
        id,
        Some(TransferState::Completed),
        Some(offer.bytes),
    );
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
        if slot.transfer.direction == Direction::Send {
            return Err(NodeError::WrongTransferState("resumed"));
        }
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
        if slot.transfer.direction == Direction::Send {
            return Err(NodeError::WrongTransferState("paused"));
        }
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
    let (direction, root) = {
        let book = lock_book(&receiving.book);
        let slot = book.get(&id).ok_or(NodeError::UnknownTransfer)?;
        if matches!(
            slot.transfer.state,
            TransferState::Queued | TransferState::Active
        ) {
            return Err(NodeError::WrongTransferState("deleted"));
        }
        (slot.transfer.direction, slot.transfer.offer.root)
    };
    lock_book(&receiving.book).remove(&id);
    if direction == Direction::Receive {
        forget_in_flight(receiving, root).await;
    }
    if let Some(database) = &receiving.database {
        database
            .delete_transfer(id)
            .map_err(|error| NodeError::Files(error.to_string()))?;
    }
    if let Some(store) = &receiving.store {
        let tag = match direction {
            Direction::Receive => format!("transfer-{id}"),
            Direction::Send => format!("send-{root}"),
        };
        if let Err(error) = store.tags().delete(tag).await {
            warn!(%error, "could not release a transfer's content");
        }
    }
    Ok(())
}

/// Deletes the history of every received file, and the tags of files sent, then releases
/// their content. Folders and shares are not touched, and their files stay.
async fn clear_cache(receiving: &Receiving) -> Result<(), NodeError> {
    let finished: Vec<i64> = lock_book(&receiving.book)
        .values()
        .filter(|slot| {
            slot.transfer.direction == Direction::Receive
                && slot.transfer.state == TransferState::Completed
        })
        .map(|slot| slot.transfer.id)
        .collect();
    for id in finished {
        delete_transfer(receiving, id).await?;
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
        .filter(|slot| {
            slot.transfer.direction == Direction::Receive
                && slot.transfer.state == TransferState::Completed
        })
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
        Reply::Pong | Reply::Stored => Err(NodeError::Connect(ProtocolError::Closed)),
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
    database: Option<Arc<Database>>,
    directory: MemoryLookup,
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
            Request::Message(wire) => {
                let id = peer.endpoint_id;
                // Only a trusted peer may send messages. Anyone else gets no answer.
                if !self.gate.is_trusted(&id) {
                    return;
                }
                let Some(database) = self.database.clone() else {
                    return;
                };
                if wire.version != MESSAGE_VERSION {
                    warn!(peer = %id, "a peer sent a message in a version this build does not read");
                    return;
                }
                let Ok(attachments) = messages::clean_attachments(&wire.attachments) else {
                    return;
                };
                let body = if wire.body.trim().is_empty() && !attachments.is_empty() {
                    String::new()
                } else {
                    match messages::clean_body(&wire.body) {
                        Ok(body) => body,
                        Err(_) => return,
                    }
                };
                let message = Message {
                    id: wire.id,
                    conversation: id,
                    author: id,
                    timestamp: wire.timestamp,
                    body,
                    reply_to: wire.reply_to,
                    state: DeliveryState::Delivered,
                    attachments,
                };
                match database.insert_message(&message) {
                    // A message seen before is acknowledged again, without a second event.
                    Ok(true) => {
                        let _ = self.events.send(NodeEvent::MessageReceived(message));
                    }
                    Ok(false) => {}
                    Err(error) => {
                        warn!(%error, "could not store a message");
                        return;
                    }
                }
                reply_and_finish(&mut send, Reply::Stored).await;
            }
            request @ (Request::GroupHistory { .. }
            | Request::GroupKey { .. }
            | Request::GroupJoin { .. }
            | Request::GroupKeyPush(_)) => {
                // Only a trusted peer may take part in a group. Anyone else gets no answer.
                if !self.gate.is_trusted(&peer.endpoint_id) {
                    return;
                }
                let Some(commands) = self.commands.upgrade() else {
                    return;
                };
                let (reply, answer) = oneshot::channel();
                if commands
                    .send(Command::GroupRequest(peer.endpoint_id, request, reply))
                    .await
                    .is_err()
                {
                    return;
                }
                match answer.await {
                    Ok(GroupAnswer::Page(page)) => {
                        if protocol::write_message(&mut send, &page).await.is_ok() {
                            let _ = send.finish();
                        }
                    }
                    Ok(GroupAnswer::Key(key)) => {
                        if protocol::write_message(&mut send, &key).await.is_ok() {
                            let _ = send.finish();
                        }
                    }
                    Ok(GroupAnswer::Stored) => reply_and_finish(&mut send, Reply::Stored).await,
                    Ok(GroupAnswer::Refused) | Err(_) => {}
                }
            }
            Request::UserContacts => {
                // Only another device of this user is told who this device trusts.
                let Some(database) = self.database.clone() else {
                    return;
                };
                if !database.is_own_device(&peer.endpoint_id).unwrap_or(false) {
                    return;
                }
                let contacts = database.shared_contacts().unwrap_or_default();
                if protocol::write_message(&mut send, &contacts).await.is_ok() {
                    let _ = send.finish();
                }
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
                if let Some(database) = &inbox.database {
                    let addr = crate::outbox::address_of_connection(&connection);
                    let _ = database.remember_address(endpoint_id, &addr);
                    inbox.directory.add_endpoint_info(addr);
                }
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
                // An offer the peer has not had answered is withdrawn when it leaves.
                let withdrawn = inbox
                    .offers
                    .lock()
                    .map(|mut offers| offers.remove(&endpoint_id).is_some())
                    .unwrap_or(false);
                if withdrawn {
                    let _ = inbox
                        .events
                        .send(NodeEvent::OfferWithdrawn { peer: endpoint_id });
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
            // The user this device belongs to is not part of the profile the user edits, so it
            // is kept.
            let user = current.user.take();
            *current = PeerProfile { user, ..profile };
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
        self.add_file_to_folder(namespace, key, source, None).await
    }

    /// Adds a file as a write made after the versions in `seen`, rather than the latest one.
    /// This is how a change made offline, from an older copy, is recorded. Versions it did
    /// not see are kept as conflict copies when the folder syncs.
    pub async fn add_to_folder_seeing(
        &self,
        namespace: NamespaceId,
        key: String,
        source: PathBuf,
        seen: Vec<Hash>,
    ) -> Result<(), NodeError> {
        self.add_file_to_folder(namespace, key, source, Some(seen))
            .await
    }

    async fn add_file_to_folder(
        &self,
        namespace: NamespaceId,
        key: String,
        source: PathBuf,
        seen: Option<Vec<Hash>>,
    ) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::AddToFolder(namespace, key, source, seen, reply))
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

    /// Leaves a shared folder. Syncing stops, and the folder is removed from this device. Files
    /// already saved to a directory are kept, and other members keep the folder.
    pub async fn leave_folder(&self, namespace: NamespaceId) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::LeaveFolder(namespace, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Makes a group, and returns its id. Invite others with [`NodeHandle::group_ticket`].
    pub async fn create_group(&self, name: String) -> Result<GroupId, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::CreateGroup(name, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Joins a group from its ticket. Its messages are fetched from the members it names.
    pub async fn join_group(&self, ticket: String) -> Result<GroupId, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::JoinGroup(ticket, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// A ticket that invites someone to a group.
    pub async fn group_ticket(&self, group: GroupId) -> Result<String, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::GroupTicketFor(group, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Sends a message to a group. Members that are offline get it when they come back.
    pub async fn send_group_message(
        &self,
        group: GroupId,
        body: String,
        reply_to: Option<MessageId>,
    ) -> Result<GroupMessage, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SendGroupMessage(group, body, reply_to, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// The messages of a group, in order.
    pub async fn group_messages(&self, group: GroupId) -> Result<Vec<GroupMessage>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::GroupMessages(group, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// A group as its members see it: its name, its key epoch, and each member's role.
    pub async fn group_details(&self, group: GroupId) -> Result<GroupDetails, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::GroupDetailsFor(group, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Removes a member from a group, as an admin. The group moves to a new key, and the
    /// removed member no longer receives its messages.
    pub async fn remove_group_member(
        &self,
        group: GroupId,
        member: EndpointId,
    ) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RemoveGroupMember(group, member, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Makes a member an admin or a plain member, as an admin. `None` removes them instead,
    /// which is [`NodeHandle::remove_group_member`].
    pub async fn set_group_role(
        &self,
        group: GroupId,
        member: EndpointId,
        role: Option<Role>,
    ) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SetGroupRole(group, member, role, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Renames a group, as an admin.
    pub async fn rename_group(&self, group: GroupId, name: String) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RenameGroup(group, name, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// The groups this device is in.
    pub async fn groups(&self) -> Result<Vec<groups::GroupInfo>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Groups(reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Sends a direct message to a trusted contact. Returns the message once the contact has
    /// stored it, or with the state `Failed` if it could not be delivered.
    pub async fn send_message(
        &self,
        to: EndpointAddr,
        body: String,
        reply_to: Option<MessageId>,
    ) -> Result<Message, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SendMessage(to, body, reply_to, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Searches every conversation and group for messages with these words, newest first.
    /// A word matches any word that starts with it.
    pub async fn search_messages(
        &self,
        query: String,
        limit: usize,
    ) -> Result<Vec<messages::SearchHit>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Search(query, limit, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Up to `limit` messages with a contact before `before`, in conversation order. With no
    /// cursor, the latest messages.
    pub async fn messages_before(
        &self,
        peer: EndpointId,
        before: Option<messages::Cursor>,
        limit: usize,
    ) -> Result<Vec<Message>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::MessagesBefore(peer, before, limit, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Up to `limit` messages of a group before `before`, in group order. With no cursor, the
    /// latest messages.
    pub async fn group_messages_before(
        &self,
        group: GroupId,
        before: Option<messages::Cursor>,
        limit: usize,
    ) -> Result<Vec<GroupMessage>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::GroupMessagesBefore(group, before, limit, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Deletes a message from this device. The contact keeps its copy. Returns whether a message
    /// was deleted.
    pub async fn delete_message(&self, id: MessageId) -> Result<bool, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::DeleteMessage(id, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Deletes every message with a contact from this device. Returns how many were deleted.
    pub async fn clear_conversation(&self, peer: EndpointId) -> Result<usize, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ClearConversation(peer, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Deletes every message of a group from this device. The group stays.
    pub async fn clear_group_messages(&self, group: GroupId) -> Result<usize, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ClearGroup(group, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Sends a message with a file attached. The contact fetches the file with
    /// [`NodeHandle::download_attachment`] when it wants it.
    pub async fn send_attachment(
        &self,
        to: EndpointAddr,
        body: String,
        path: PathBuf,
    ) -> Result<Message, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SendAttachment(to, body, path, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Fetches an attachment of a message from the contact who sent it. Returns the transfer,
    /// which reports progress.
    pub async fn download_attachment(
        &self,
        message: MessageId,
        index: usize,
    ) -> Result<i64, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::DownloadAttachment(message, index, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// The messages with a contact, in conversation order.
    pub async fn messages(&self, peer: EndpointId) -> Result<Vec<Message>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Messages(peer, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// How big a folder is, and how many peers it is syncing with.
    pub async fn folder_status(&self, namespace: NamespaceId) -> Result<FolderStatus, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::FolderStatus(namespace, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// How many devices hold a complete copy of a folder, and what each has. This device is
    /// read now; the others are as they last reported.
    pub async fn folder_health(&self, namespace: NamespaceId) -> Result<ReplicaHealth, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::FolderHealth(namespace, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Checks this device's copy of a folder now, and tells the other members the result.
    pub async fn verify_folder(&self, namespace: NamespaceId) -> Result<ReplicaInfo, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::VerifyFolder(namespace, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Sets whether this device keeps a full copy of a folder. A device that does not keep it
    /// follows the list of files and fetches none of them.
    pub async fn set_keep_copy(&self, namespace: NamespaceId, keep: bool) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SetKeepCopy(namespace, keep, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Sets the name this device shows for a folder.
    pub async fn rename_folder(
        &self,
        namespace: NamespaceId,
        name: String,
    ) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RenameFolder(namespace, name, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Opens a link so another device can join this user, and returns the ticket to give it. The
    /// ticket works once, for ten minutes. The first time, this device becomes a user, and its
    /// own certificate is named `device_name`.
    pub async fn link_device(&self, device_name: String) -> Result<String, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::LinkDevice(device_name, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Links this device to the user in a ticket from one of their devices. This device is then
    /// trusted by that user's contacts. Returns the user.
    pub async fn accept_link(
        &self,
        ticket: String,
        device_name: String,
    ) -> Result<PublicKey, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::AcceptLink(ticket, device_name, reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// The devices of the user this device belongs to, including this one.
    pub async fn user_devices(&self) -> Result<Vec<UserDevice>, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::UserDevices(reply))
            .await
            .map_err(|_| NodeError::Stopped)?;
        response.await.map_err(|_| NodeError::Stopped)?
    }

    /// Revokes one of this user's devices. A device cannot revoke itself.
    pub async fn revoke_device(&self, device: EndpointId) -> Result<(), NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RevokeDevice(device, reply))
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
                user: None,
            },
            gate: Arc::new(AllowAll),
            discovery: Discovery::default(),
            relay: RelaySetting::Public,
            blobs: None,
            downloads: None,
            database: None,
            garbage_interval: None,
            user_keys: None,
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
            user: None,
        });
        let seen = node_a.connect(addr_b).await.unwrap();

        assert_eq!(seen.profile.display_name, "Grace Hopper");
        assert_eq!(seen.profile.avatar_hash, Some([9; 32]));
        assert!(node_a.shutdown(TIMEOUT));
        assert!(node_b.shutdown(TIMEOUT));
    }
}
