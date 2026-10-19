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
        relay: groovy_core::RelaySetting::Public,
        blobs: None,
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

/// A node that gates connections on its own database, which the test can read.
async fn gated_node(dir: &DataDir, key: SecretKey) -> (NodeHandle, EventStream, Arc<Database>) {
    let db = Arc::new(Database::open(&dir.database_file()).unwrap());
    let config = NodeConfig {
        gate: Arc::new(ContactGate::new(db.clone())),
        ..config(key)
    };
    let (handle, mut events) = Node::start(config).expect("node starts");
    assert_eq!(events.next().await, Some(NodeEvent::Started));
    (handle, events, db)
}

fn fresh_dir(tmp: &tempfile::TempDir, name: &str) -> DataDir {
    let dir = DataDir::at(tmp.path().join(name));
    dir.create_all().unwrap();
    dir
}

async fn next_pairing_request(events: &mut EventStream) -> (groovy_core::PeerInfo, String) {
    loop {
        if let NodeEvent::PairingRequested { peer, code } = events.next().await.unwrap() {
            return (peer, code);
        }
    }
}

#[tokio::test]
async fn an_accepted_pairing_makes_both_sides_trusted() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice, _alice_events, alice_db) =
        gated_node(&fresh_dir(&tmp, "alice"), alice_key.clone()).await;
    let (bob, mut bob_events, bob_db) = gated_node(&fresh_dir(&tmp, "bob"), bob_key.clone()).await;

    let request = tokio::spawn({
        let alice = alice.clone();
        let bob_addr = bob.addr().await.unwrap();
        async move { alice.request_pairing(bob_addr).await }
    });
    let (from, code) = next_pairing_request(&mut bob_events).await;
    assert_eq!(from.endpoint_id, alice_key.public());
    assert_eq!(
        code,
        groovy_core::verification_code(&alice_key.public(), &bob_key.public())
    );

    bob.answer_pairing(alice_key.public(), true).await.unwrap();
    let paired = request.await.unwrap().expect("alice is paired");

    assert_eq!(paired.endpoint_id, bob_key.public());
    assert_eq!(
        bob_db.contact(alice_key.public()).unwrap().unwrap().trust,
        Trust::Trusted
    );
    assert_eq!(
        alice_db.contact(bob_key.public()).unwrap().unwrap().trust,
        Trust::Trusted
    );
    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_declined_pairing_leaves_both_sides_untrusted() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice, _alice_events, alice_db) =
        gated_node(&fresh_dir(&tmp, "alice"), alice_key.clone()).await;
    let (bob, mut bob_events, bob_db) = gated_node(&fresh_dir(&tmp, "bob"), bob_key.clone()).await;

    let request = tokio::spawn({
        let alice = alice.clone();
        let bob_addr = bob.addr().await.unwrap();
        async move { alice.request_pairing(bob_addr).await }
    });
    next_pairing_request(&mut bob_events).await;
    bob.answer_pairing(alice_key.public(), false).await.unwrap();

    let outcome = request.await.unwrap();
    assert!(
        matches!(outcome, Err(groovy_core::NodeError::PairingDeclined)),
        "{outcome:?}"
    );
    assert_ne!(
        bob_db.contact(alice_key.public()).unwrap().map(|c| c.trust),
        Some(Trust::Trusted)
    );
    assert_ne!(
        alice_db.contact(bob_key.public()).unwrap().map(|c| c.trust),
        Some(Trust::Trusted)
    );
    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_peer_that_asks_too_often_is_declined_without_a_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice, _alice_events, _alice_db) =
        gated_node(&fresh_dir(&tmp, "alice"), alice_key.clone()).await;
    let (bob, mut bob_events, _bob_db) = gated_node(&fresh_dir(&tmp, "bob"), bob_key.clone()).await;
    let bob_addr = bob.addr().await.unwrap();

    // Three requests are allowed; they wait for Bob's answer.
    let mut waiting = Vec::new();
    for _ in 0..3 {
        let alice = alice.clone();
        let addr = bob_addr.clone();
        waiting.push(tokio::spawn(
            async move { alice.request_pairing(addr).await },
        ));
        next_pairing_request(&mut bob_events).await;
    }

    // The fourth is refused at once, and Bob is not asked.
    let fourth = tokio::time::timeout(TIMEOUT, alice.request_pairing(bob_addr))
        .await
        .expect("the refusal is immediate");
    assert!(
        matches!(fourth, Err(groovy_core::NodeError::PairingDeclined)),
        "{fourth:?}"
    );
    let asked = tokio::time::timeout(
        Duration::from_millis(300),
        next_pairing_request(&mut bob_events),
    )
    .await;
    assert!(asked.is_err(), "a fourth request reached the user");

    for handle in &waiting {
        handle.abort();
    }
    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_trusted_contact_is_reachable_and_an_untrusted_one_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key, carol_key) = (
        SecretKey::generate(),
        SecretKey::generate(),
        SecretKey::generate(),
    );
    let (alice, _alice_events, _alice_db) =
        gated_node(&fresh_dir(&tmp, "alice"), alice_key.clone()).await;
    let (bob, _bob_events, bob_db) = gated_node(&fresh_dir(&tmp, "bob"), bob_key.clone()).await;
    let (carol, _carol_events, _carol_db) =
        gated_node(&fresh_dir(&tmp, "carol"), carol_key.clone()).await;

    // Bob trusts Alice, and knows Carol but has not paired with her.
    bob_db
        .record_handshake(
            &groovy_core::PeerInfo {
                endpoint_id: alice_key.public(),
                profile: PeerProfile::default(),
            },
            0,
        )
        .unwrap();
    bob_db
        .set_trust(alice_key.public(), Trust::Trusted)
        .unwrap();
    bob_db
        .record_handshake(
            &groovy_core::PeerInfo {
                endpoint_id: carol_key.public(),
                profile: PeerProfile::default(),
            },
            0,
        )
        .unwrap();

    let bob_addr = bob.addr().await.unwrap();
    let reached = alice.check_presence(bob_addr.clone()).await;
    assert_eq!(reached.unwrap(), groovy_core::Route::Direct);

    // Bob has not paired with Carol, so Carol gets no answer either.
    let refused = carol.check_presence(bob_addr.clone()).await;
    assert!(
        refused.is_err(),
        "Bob answered an untrusted peer: {refused:?}"
    );
    let carol_addr = carol.addr().await.unwrap();
    let untrusted = tokio::time::timeout(TIMEOUT, bob.check_presence(carol_addr)).await;
    let untrusted = untrusted.expect("the check ends");
    assert!(
        untrusted.is_err(),
        "an untrusted peer got a reply: {untrusted:?}"
    );

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
    assert!(carol.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_contact_that_drops_off_is_unreachable_and_reachable_again_when_it_returns() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice, _alice_events, _alice_db) =
        gated_node(&fresh_dir(&tmp, "alice"), alice_key.clone()).await;
    let bob_dir = fresh_dir(&tmp, "bob");
    {
        let bob_db = Database::open(&bob_dir.database_file()).unwrap();
        bob_db
            .record_handshake(
                &groovy_core::PeerInfo {
                    endpoint_id: alice_key.public(),
                    profile: PeerProfile::default(),
                },
                0,
            )
            .unwrap();
        bob_db
            .set_trust(alice_key.public(), Trust::Trusted)
            .unwrap();
    }

    let (bob, _bob_events, _) = gated_node(&bob_dir, bob_key.clone()).await;
    let first_address = bob.addr().await.unwrap();
    assert!(alice.check_presence(first_address.clone()).await.is_ok());

    // Bob goes away: the check fails, and does not hang.
    assert!(bob.shutdown(TIMEOUT));
    let down = tokio::time::timeout(TIMEOUT, alice.check_presence(first_address)).await;
    assert!(
        !matches!(down, Ok(Ok(_))),
        "an offline contact answered: {down:?}"
    );

    // Bob comes back with the same identity and database, at a new address.
    let (bob_again, _events, _) = gated_node(&bob_dir, bob_key.clone()).await;
    let new_address = bob_again.addr().await.unwrap();
    let back = alice.check_presence(new_address).await;
    assert!(
        back.is_ok(),
        "the returning contact is not reachable: {back:?}"
    );

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob_again.shutdown(TIMEOUT));
}

/// A node with blob storage in `dir`, gated on its own database.
async fn blob_node(dir: &DataDir, key: SecretKey) -> (NodeHandle, EventStream) {
    let db = Arc::new(Database::open(&dir.database_file()).unwrap());
    let config = NodeConfig {
        gate: Arc::new(ContactGate::new(db)),
        blobs: Some(dir.blobs_dir()),
        ..config(key)
    };
    let (handle, mut events) = Node::start(config).expect("node starts");
    assert_eq!(events.next().await, Some(NodeEvent::Started));
    (handle, events)
}

/// A loopback endpoint with a chosen identity, which asks blobs from other nodes.
async fn dialer_with(key: SecretKey) -> Endpoint {
    Endpoint::builder(Minimal)
        .secret_key(key)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("loopback address is valid")
        .bind()
        .await
        .expect("dialer binds")
}

#[tokio::test]
async fn a_blob_survives_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = fresh_dir(&tmp, "node");
    let data = b"hello, durable world".to_vec();

    let (first, _events) = blob_node(&dir, SecretKey::generate()).await;
    let hash = first.add_blob(data.clone()).await.expect("blob is added");
    assert!(first.shutdown(TIMEOUT));

    let (second, _events) = blob_node(&dir, SecretKey::generate()).await;
    let read = second.read_blob(hash).await.expect("the store reopens");

    assert_eq!(read, Some(data));
    assert!(second.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_trusted_peer_fetches_a_blob_and_an_untrusted_one_cannot() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key, carol_key) = (
        SecretKey::generate(),
        SecretKey::generate(),
        SecretKey::generate(),
    );
    let alice_dir = fresh_dir(&tmp, "alice");
    {
        let db = Database::open(&alice_dir.database_file()).unwrap();
        db.record_handshake(
            &groovy_core::PeerInfo {
                endpoint_id: bob_key.public(),
                profile: PeerProfile::default(),
            },
            0,
        )
        .unwrap();
        db.set_trust(bob_key.public(), Trust::Trusted).unwrap();
    }
    let (alice, _events) = blob_node(&alice_dir, alice_key).await;
    let data = b"shared with a contact".to_vec();
    let hash = alice.add_blob(data.clone()).await.unwrap();
    let alice_addr = alice.addr().await.unwrap();

    let bob = dialer_with(bob_key).await;
    let connection = bob
        .connect(alice_addr.clone(), iroh_blobs::ALPN)
        .await
        .expect("bob connects");
    let fetched = iroh_blobs::get::request::get_blob(connection, hash).await;
    assert_eq!(
        fetched.expect("bob is trusted and gets the blob").to_vec(),
        data
    );

    let carol = dialer_with(carol_key).await;
    let refused = carol.connect(alice_addr, iroh_blobs::ALPN).await;
    let refused_fetch = match refused {
        Err(_) => true,
        Ok(connection) => iroh_blobs::get::request::get_blob(connection, hash)
            .await
            .is_err(),
    };
    assert!(refused_fetch, "an untrusted peer got a blob");

    bob.close().await;
    carol.close().await;
    assert!(alice.shutdown(TIMEOUT));
}
