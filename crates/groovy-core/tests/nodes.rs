//! Integration tests: several nodes in one process, each with its own data
//! directory. Everything runs on loopback, so no network or relay is needed.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use groovy_core::{
    AllowAll, ContactGate, DataDir, Database, EventStream, GROOVY_ALPN, Network, Node, NodeConfig,
    NodeEvent, NodeHandle, PeerProfile, SecretKey, Trust,
};
use iroh::endpoint::presets::Minimal;
use iroh::{Endpoint, EndpointAddr, RelayMode};

const TIMEOUT: Duration = Duration::from_secs(5);

fn config(secret_key: SecretKey) -> NodeConfig {
    NodeConfig {
        secret_key,
        network: Network::LocalOnly,
        profile: PeerProfile::default(),
        gate: Arc::new(AllowAll),
        discovery: groovy_core::Discovery::default(),
    }
}

/// Starts a node and waits for it to report that it has started.
async fn start(secret_key: SecretKey) -> (NodeHandle, EventStream) {
    let (handle, mut events) = Node::start(config(secret_key)).expect("node starts");
    assert_eq!(events.next().await, Some(NodeEvent::Started));
    (handle, events)
}

/// A plain loopback endpoint that dials nodes, standing in for a peer.
async fn dialer() -> Endpoint {
    Endpoint::builder(Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("loopback address is valid")
        .bind()
        .await
        .expect("dialer binds")
}

async fn dial(from: &Endpoint, to: EndpointAddr) -> Result<(), String> {
    let connection = tokio::time::timeout(TIMEOUT, from.connect(to, GROOVY_ALPN))
        .await
        .map_err(|_| "connect timed out".to_owned())?
        .map_err(|error| error.to_string())?;
    connection.close(0u32.into(), b"done");
    Ok(())
}

#[tokio::test]
async fn every_node_accepts_connections_from_every_other_node() {
    let mut nodes = Vec::new();
    for _ in 0..3 {
        nodes.push(start(SecretKey::generate()).await);
    }
    let mut addrs = Vec::new();
    for (handle, _) in &nodes {
        addrs.push(handle.addr().await.expect("node has an address"));
    }
    let dialer = dialer().await;

    for addr in &addrs {
        dial(&dialer, addr.clone())
            .await
            .expect("each node accepts the Groovy ALPN");
    }

    dialer.close().await;
    for (handle, _) in nodes {
        assert!(handle.shutdown(TIMEOUT), "node stops in time");
    }
}

#[tokio::test]
async fn a_node_keeps_its_endpoint_id_across_restarts() {
    let secret = SecretKey::generate();
    let expected = secret.public();

    let (first, _events) = start(secret.clone()).await;
    assert_eq!(first.endpoint_id(), expected);
    assert!(first.shutdown(TIMEOUT));

    let (second, _events) = start(secret).await;
    assert_eq!(second.endpoint_id(), expected);
    assert!(second.shutdown(TIMEOUT));
}

#[test]
fn two_data_directories_do_not_share_state() {
    let tmp = tempfile::tempdir().unwrap();
    let first = DataDir::at(tmp.path().join("first"));
    let second = DataDir::at(tmp.path().join("second"));
    first.create_all().unwrap();
    second.create_all().unwrap();

    Database::open(&first.database_file())
        .unwrap()
        .set_setting("theme", "dark")
        .unwrap();

    let other = Database::open(&second.database_file()).unwrap();
    assert_eq!(other.get_setting("theme").unwrap(), None);
}

#[test]
fn data_written_to_a_directory_survives_reopening_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = DataDir::at(tmp.path());
    dir.create_all().unwrap();

    Database::open(&dir.database_file())
        .unwrap()
        .save_display_name("Ada")
        .unwrap();

    let reopened = Database::open(&dir.database_file()).unwrap();
    assert_eq!(reopened.profile().unwrap().unwrap().display_name, "Ada");
}

#[tokio::test]
async fn a_peer_offering_another_protocol_version_is_refused() {
    let (node, _events) = start(SecretKey::generate()).await;
    let addr = node.addr().await.unwrap();
    let dialer = dialer().await;

    let result = tokio::time::timeout(TIMEOUT, dialer.connect(addr, b"groovy/1"))
        .await
        .expect("the refusal arrives in time");

    let error = result.expect_err("a connection on another ALPN is refused");
    // The node maps this iroh message to ProtocolError::UnsupportedProtocol when
    // it dials a peer. This test dials raw, so it checks the text that mapping relies on.
    assert!(error.to_string().contains("known protocol"), "{error}");
    dialer.close().await;
    assert!(node.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_handshake_with_another_version_gets_our_version_back() {
    let (node, _events) = start(SecretKey::generate()).await;
    let addr = node.addr().await.unwrap();
    let dialer = dialer().await;
    let connection = dialer.connect(addr, GROOVY_ALPN).await.unwrap();

    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    let theirs = groovy_core::protocol::Hello {
        protocol_version: groovy_core::protocol::PROTOCOL_VERSION + 1,
        profile: PeerProfile::default(),
    };
    groovy_core::protocol::write_message(&mut send, &theirs)
        .await
        .unwrap();
    let ours: groovy_core::protocol::Hello = groovy_core::protocol::read_message(&mut recv)
        .await
        .unwrap();

    assert_eq!(
        ours.protocol_version,
        groovy_core::protocol::PROTOCOL_VERSION
    );
    dialer.close().await;
    assert!(node.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_message_over_the_size_limit_closes_the_connection() {
    let (node, _events) = start(SecretKey::generate()).await;
    let addr = node.addr().await.unwrap();
    let dialer = dialer().await;
    let connection = dialer.connect(addr, GROOVY_ALPN).await.unwrap();

    let (mut send, _recv) = connection.open_bi().await.unwrap();
    // Claim a body far larger than the limit, and never send it.
    send.write_all(&u32::MAX.to_be_bytes()).await.unwrap();

    tokio::time::timeout(TIMEOUT, connection.closed())
        .await
        .expect("the node closes the connection");
    dialer.close().await;
    assert!(node.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_ticket_copied_from_one_node_lets_another_connect() {
    let (node_a, _events_a) = start(SecretKey::generate()).await;
    let (node_b, _events_b) = start(SecretKey::generate()).await;

    // A shares its ticket, and B pastes it.
    let shared = groovy_core::ticket::encode(&node_a.addr().await.unwrap());
    let dialled = groovy_core::ticket::parse(&shared, node_b.endpoint_id())
        .expect("the pasted ticket is accepted");
    let peer = node_b.connect(dialled).await.expect("B connects to A");

    assert_eq!(peer.endpoint_id, node_a.endpoint_id());
    assert!(node_a.shutdown(TIMEOUT));
    assert!(node_b.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_blocked_peer_is_refused_and_an_ordinary_peer_is_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = DataDir::at(tmp.path());
    dir.create_all().unwrap();
    let db = Arc::new(Database::open(&dir.database_file()).unwrap());

    let blocked = SecretKey::generate();
    let ordinary = SecretKey::generate();
    // This node knows the blocked peer and has blocked it.
    db.record_handshake(
        &groovy_core::PeerInfo {
            endpoint_id: blocked.public(),
            profile: PeerProfile::default(),
        },
        0,
    )
    .unwrap();
    db.set_trust(blocked.public(), Trust::Blocked).unwrap();

    let node_config = NodeConfig {
        gate: Arc::new(ContactGate::new(db.clone())),
        ..config(SecretKey::generate())
    };
    let (server, mut server_events) = Node::start(node_config).expect("server starts");
    assert_eq!(server_events.next().await, Some(NodeEvent::Started));
    let server_addr = server.addr().await.unwrap();

    // The blocked peer is refused.
    let (blocked_node, _events) = start(blocked).await;
    let refused = blocked_node.connect(server_addr.clone()).await;
    assert!(refused.is_err(), "a blocked peer connects: {refused:?}");

    // An ordinary peer gets in, and the server records it.
    let (ordinary_node, _events) = start(ordinary.clone()).await;
    ordinary_node
        .connect(server_addr)
        .await
        .expect("an ordinary peer connects");
    loop {
        if let NodeEvent::PeerConnected(_) = server_events.next().await.unwrap() {
            break;
        }
    }
    let recorded = db.contact(ordinary.public()).unwrap();
    assert!(recorded.is_some(), "the ordinary peer is recorded");

    // This node will not dial a peer it has blocked.
    let blocked_addr = blocked_node.addr().await.unwrap();
    let outgoing = server.connect(blocked_addr).await;
    assert!(
        matches!(outgoing, Err(groovy_core::NodeError::Blocked)),
        "{outgoing:?}"
    );

    ordinary_node.shutdown(TIMEOUT);
    blocked_node.shutdown(TIMEOUT);
    assert!(server.shutdown(TIMEOUT));
}
