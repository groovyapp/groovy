//! Integration tests: several nodes in one process, each with its own data
//! directory. Everything runs on loopback, so no network or relay is needed.

use std::net::Ipv4Addr;
use std::time::Duration;

use groovy_core::{
    DataDir, Database, EventStream, GROOVY_ALPN, Network, Node, NodeConfig, NodeEvent, NodeHandle,
    PeerProfile, SecretKey,
};
use iroh::endpoint::presets::Minimal;
use iroh::{Endpoint, EndpointAddr, RelayMode};

const TIMEOUT: Duration = Duration::from_secs(5);

fn config(secret_key: SecretKey) -> NodeConfig {
    NodeConfig {
        secret_key,
        network: Network::LocalOnly,
        profile: PeerProfile::default(),
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
