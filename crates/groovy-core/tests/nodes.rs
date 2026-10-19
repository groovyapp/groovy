//! Integration tests: several nodes in one process, each with its own data
//! directory. Everything runs on loopback, so no network or relay is needed.

use std::net::Ipv4Addr;
use std::time::Duration;

use groovy_core::{
    DataDir, Database, EventStream, GROOVY_ALPN, Network, Node, NodeConfig, NodeEvent, NodeHandle,
    SecretKey,
};
use iroh::endpoint::presets::Minimal;
use iroh::{Endpoint, EndpointAddr, RelayMode};

const TIMEOUT: Duration = Duration::from_secs(5);

fn config(secret_key: SecretKey) -> NodeConfig {
    NodeConfig {
        secret_key,
        network: Network::LocalOnly,
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
