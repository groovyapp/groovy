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
use iroh_blobs::Hash;

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
        downloads: None,
        database: None,
        garbage_interval: None,
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

/// A node with blob storage in `dir`, gated on its own database, and collecting garbage
/// every 200 ms.
async fn blob_node(dir: &DataDir, key: SecretKey) -> (NodeHandle, EventStream) {
    blob_node_collecting(dir, key, Some(Duration::from_millis(200))).await
}

/// A node with blob storage in `dir`, collecting garbage at `interval`, or never if `None`.
async fn blob_node_collecting(
    dir: &DataDir,
    key: SecretKey,
    interval: Option<Duration>,
) -> (NodeHandle, EventStream) {
    let db = Arc::new(Database::open(&dir.database_file()).unwrap());
    let config = NodeConfig {
        gate: Arc::new(ContactGate::new(db)),
        blobs: Some(dir.blobs_dir()),
        downloads: Some(dir.root().join("Downloads")),
        database: Some(Arc::new(Database::open(&dir.database_file()).unwrap())),
        garbage_interval: interval,
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

/// Records `peer` as a trusted contact in `dir`'s database.
fn trust_in(dir: &DataDir, peer: &SecretKey) {
    let db = Database::open(&dir.database_file()).unwrap();
    db.record_handshake(
        &groovy_core::PeerInfo {
            endpoint_id: peer.public(),
            profile: PeerProfile::default(),
        },
        0,
    )
    .unwrap();
    db.set_trust(peer.public(), Trust::Trusted).unwrap();
}

async fn next_file_offer(events: &mut EventStream) -> groovy_core::protocol::FileOffer {
    loop {
        if let NodeEvent::FileOffered { offer, .. } = events.next().await.unwrap() {
            return offer;
        }
    }
}

async fn next_file_received(events: &mut EventStream) -> NodeEvent {
    loop {
        let event = events.next().await.unwrap();
        if let NodeEvent::FileReceived { .. } = event {
            return event;
        }
    }
}

/// Writes a folder with a nested file, and a file at the top, under `root`.
fn write_sample_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::create_dir_all(root.join("photos/sub")).unwrap();
    std::fs::write(root.join("photos/a.txt"), b"alpha").unwrap();
    std::fs::write(root.join("photos/sub/b.bin"), b"bravo").unwrap();
    std::fs::write(root.join("note.txt"), b"charlie").unwrap();
    vec![root.join("photos"), root.join("note.txt")]
}

#[tokio::test]
async fn a_folder_sent_to_a_trusted_peer_arrives_with_its_structure() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    let offer = tokio::time::timeout(TIMEOUT, next_file_offer(&mut bob_events))
        .await
        .expect("bob is offered the files");
    assert_eq!(offer.names, ["photos", "note.txt"]);
    assert_eq!(offer.files, 3);

    bob.answer_offer(alice_key.public(), true)
        .await
        .expect("bob accepts");
    send.await.unwrap().expect("alice's offer is accepted");
    let received = tokio::time::timeout(TIMEOUT, next_file_received(&mut bob_events))
        .await
        .expect("bob reports the files");
    assert!(matches!(received, NodeEvent::FileReceived { root, .. } if root == offer.root));

    // The collection keeps each file's path inside the folder it was sent from.
    let listed = bob.list_collection(offer.root).await.unwrap();
    let names: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["note.txt", "photos/a.txt", "photos/sub/b.bin"]);
    let mut contents = Vec::new();
    for (name, hash) in &listed {
        contents.push((name.clone(), bob.read_blob(*hash).await.unwrap().unwrap()));
    }
    assert_eq!(
        contents,
        [
            ("note.txt".to_owned(), b"charlie".to_vec()),
            ("photos/a.txt".to_owned(), b"alpha".to_vec()),
            ("photos/sub/b.bin".to_owned(), b"bravo".to_vec()),
        ]
    );

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_declined_offer_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), false).await.unwrap();

    let outcome = send.await.unwrap();
    assert!(
        matches!(outcome, Err(groovy_core::NodeError::OfferDeclined)),
        "{outcome:?}"
    );
    let received = tokio::time::timeout(
        Duration::from_millis(300),
        next_file_received(&mut bob_events),
    )
    .await;
    assert!(received.is_err(), "a declined offer was received anyway");

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn an_untrusted_peer_cannot_offer_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (bob_key, carol_key) = (SecretKey::generate(), SecretKey::generate());
    let bob_dir = fresh_dir(&tmp, "bob");
    let carol_dir = fresh_dir(&tmp, "carol");
    // Bob has never paired with Carol.
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let (carol, _carol_events) = blob_node(&carol_dir, carol_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let carol = carol.clone();
        async move { carol.send_files(bob_addr, paths).await }
    });
    let offered =
        tokio::time::timeout(Duration::from_millis(500), next_file_offer(&mut bob_events)).await;
    assert!(
        offered.is_err(),
        "bob was offered files by an untrusted peer"
    );

    // Bob gives no answer, so Carol gets no Accepted or Declined. Her send fails instead.
    let outcome = tokio::time::timeout(TIMEOUT, send)
        .await
        .expect("carol's send ends")
        .unwrap();
    assert!(
        matches!(outcome, Err(groovy_core::NodeError::Connect(_))),
        "bob answered an untrusted peer: {outcome:?}"
    );
    assert!(carol.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn files_from_a_contact_set_to_auto_accept_arrive_without_asking() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    Database::open(&bob_dir.database_file())
        .unwrap()
        .set_auto_accept_files(alice_key.public(), true)
        .unwrap();
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });

    // Nobody answers the offer, yet the files arrive.
    let received = tokio::time::timeout(TIMEOUT, next_file_received(&mut bob_events))
        .await
        .expect("the files arrive without an answer");
    assert!(matches!(received, NodeEvent::FileReceived { .. }));
    send.await.unwrap().expect("alice is accepted");

    let listed = bob
        .list_collection(match received {
            NodeEvent::FileReceived { root, .. } => root,
            _ => unreachable!(),
        })
        .await
        .unwrap();
    assert_eq!(listed.len(), 3);

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn received_files_are_saved_and_a_name_taken_gets_a_number() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let downloads = bob_dir.root().join("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::write(downloads.join("note.txt"), b"already here").unwrap();
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), true).await.unwrap();
    send.await.unwrap().unwrap();
    tokio::time::timeout(TIMEOUT, next_file_received(&mut bob_events))
        .await
        .expect("bob has the files");

    // The folder keeps its structure on disk, and the file that was already there is kept.
    assert_eq!(
        std::fs::read(downloads.join("photos/a.txt")).unwrap(),
        b"alpha"
    );
    assert_eq!(
        std::fs::read(downloads.join("photos/sub/b.bin")).unwrap(),
        b"bravo"
    );
    assert_eq!(
        std::fs::read(downloads.join("note.txt")).unwrap(),
        b"already here"
    );
    assert_eq!(
        std::fs::read(downloads.join("note (2).txt")).unwrap(),
        b"charlie"
    );

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn receiving_a_transfer_again_fetches_nothing_already_received() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), true).await.unwrap();
    send.await.unwrap().unwrap();
    let transfer = match next_file_received(&mut bob_events).await {
        NodeEvent::FileReceived { transfer, .. } => transfer,
        other => panic!("{other:?}"),
    };
    assert_eq!(bob.transfers().len(), 1);
    assert_eq!(
        bob.transfers()[0].state,
        groovy_core::transfers::TransferState::Completed
    );

    // The second receive finds every block already in the store.
    bob.resume_transfer(transfer)
        .await
        .expect_err("a completed transfer is not resumed");

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_transfer_moves_from_queued_to_active_to_completed() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), true).await.unwrap();
    send.await.unwrap().unwrap();

    // Every change to the transfer is reported, in order.
    let mut states = Vec::new();
    loop {
        let event = tokio::time::timeout(TIMEOUT, bob_events.next())
            .await
            .expect("the transfer reports its progress")
            .unwrap();
        if let NodeEvent::TransferChanged(transfer) = event {
            if states.last() != Some(&transfer.state) {
                states.push(transfer.state.clone());
            }
            if transfer.state == groovy_core::transfers::TransferState::Completed {
                assert_eq!(transfer.done, transfer.offer.bytes);
                break;
            }
        }
    }
    use groovy_core::transfers::TransferState::{Active, Completed, Queued};
    assert_eq!(states, [Queued, Active, Completed]);

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn transfers_survive_a_restart_and_unfinished_ones_resume() {
    use groovy_core::transfers::{Transfer, TransferState};
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key.clone()).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    let offer = next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), true).await.unwrap();
    send.await.unwrap().unwrap();
    next_file_received(&mut bob_events).await;
    let finished = bob.transfers();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].state, TransferState::Completed);
    assert!(bob.shutdown(TIMEOUT));

    // A transfer that was still queued when the app closed.
    Database::open(&bob_dir.database_file())
        .unwrap()
        .insert_transfer(&Transfer {
            id: 99,
            direction: groovy_core::transfers::Direction::Receive,
            peer: alice_key.public(),
            offer,
            state: TransferState::Queued,
            done: 0,
            created: 0,
            updated: 0,
        })
        .unwrap();

    // Restarting shows the finished transfer, and receives the queued one again.
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let restored = tokio::time::timeout(TIMEOUT, next_file_received(&mut bob_events))
        .await
        .expect("the queued transfer is received after the restart");
    assert!(matches!(
        restored,
        NodeEvent::FileReceived { transfer: 99, .. }
    ));

    let all = bob.transfers();
    assert_eq!(
        all.iter().map(|t| t.id).collect::<Vec<_>>(),
        [finished[0].id, 99]
    );
    assert!(all.iter().all(|t| t.state == TransferState::Completed));

    // A finished transfer cannot be paused, and an unknown one cannot be cancelled.
    assert!(matches!(
        bob.pause_transfer(finished[0].id).await,
        Err(groovy_core::NodeError::WrongTransferState(_))
    ));
    assert!(matches!(
        bob.cancel_transfer(12345).await,
        Err(groovy_core::NodeError::UnknownTransfer)
    ));

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_paused_transfer_stays_paused_across_a_restart_until_resumed() {
    use groovy_core::transfers::{Transfer, TransferState};
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key.clone()).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    let offer = next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), true).await.unwrap();
    send.await.unwrap().unwrap();
    next_file_received(&mut bob_events).await;
    assert!(bob.shutdown(TIMEOUT));

    Database::open(&bob_dir.database_file())
        .unwrap()
        .insert_transfer(&Transfer {
            id: 7,
            direction: groovy_core::transfers::Direction::Receive,
            peer: alice_key.public(),
            offer,
            state: TransferState::Paused,
            done: 0,
            created: 0,
            updated: 0,
        })
        .unwrap();

    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let early = tokio::time::timeout(
        Duration::from_millis(500),
        next_file_received(&mut bob_events),
    )
    .await;
    assert!(early.is_err(), "a paused transfer was received on its own");
    assert_eq!(
        bob.transfers().iter().find(|t| t.id == 7).unwrap().state,
        TransferState::Paused
    );

    bob.resume_transfer(7)
        .await
        .expect("the paused transfer resumes");
    let resumed = tokio::time::timeout(TIMEOUT, next_file_received(&mut bob_events))
        .await
        .expect("the resumed transfer is received");
    assert!(matches!(
        resumed,
        NodeEvent::FileReceived { transfer: 7, .. }
    ));

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_share_is_downloaded_by_someone_who_is_not_a_contact() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, carol_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, carol_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "carol"));
    // Carol is not a contact of Alice: neither trusts the other.
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (carol, mut carol_events) = blob_node(&carol_dir, carol_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let share = alice.create_share(paths).await.expect("the share is made");
    assert_eq!(share.share.files, 3);
    assert_eq!(share.share.bytes, 17);

    let preview = carol
        .preview_share(share.ticket.clone())
        .await
        .expect("carol reads the share's listing");
    assert_eq!(
        preview.names,
        ["note.txt", "photos/a.txt", "photos/sub/b.bin"]
    );
    assert_eq!(preview.files, 3);
    assert_eq!(preview.bytes, 17);

    let transfer = carol
        .receive_share(share.ticket)
        .await
        .expect("carol starts downloading");
    let received = tokio::time::timeout(TIMEOUT, next_file_received(&mut carol_events))
        .await
        .expect("carol receives the share");
    assert!(matches!(received, NodeEvent::FileReceived { transfer: t, .. } if t == transfer));
    assert_eq!(
        std::fs::read(carol_dir.root().join("Downloads/photos/sub/b.bin")).unwrap(),
        b"bravo"
    );

    assert!(alice.shutdown(TIMEOUT));
    assert!(carol.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_revoked_share_cannot_be_downloaded_by_someone_who_is_not_a_contact() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, dave_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, dave_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "dave"));
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (dave, _dave_events) = blob_node(&dave_dir, dave_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let share = alice.create_share(paths).await.unwrap();
    alice
        .revoke_share(share.share.id)
        .await
        .expect("the share is revoked");

    assert!(alice.shares().await.unwrap()[0].revoked.is_some());
    assert!(
        dave.receive_share(share.ticket.clone()).await.is_err(),
        "a revoked share was downloaded"
    );
    assert!(dave.preview_share(share.ticket).await.is_err());
    assert!(matches!(
        alice.revoke_share(share.share.id).await,
        Err(groovy_core::NodeError::UnknownShare)
    ));

    assert!(alice.shutdown(TIMEOUT));
    assert!(dave.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_non_contact_cannot_fetch_a_blob_that_is_in_no_share() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, eve_key) = (SecretKey::generate(), SecretKey::generate());
    let alice_dir = fresh_dir(&tmp, "alice");
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (eve, _eve_events) = blob_node(&fresh_dir(&tmp, "eve"), eve_key.clone()).await;
    let private = alice.add_blob(b"private to alice".to_vec()).await.unwrap();
    alice
        .create_share(write_sample_files(&tmp.path().join("source")))
        .await
        .unwrap();

    let alice_addr = alice.addr().await.unwrap();
    let eve_dialer = dialer_with(eve_key).await;
    let connection = eve_dialer
        .connect(alice_addr, iroh_blobs::ALPN)
        .await
        .expect("a non-contact may connect to the blob protocol");
    let fetched = iroh_blobs::get::request::get_blob(connection, private).await;
    assert!(fetched.is_err(), "a non-contact fetched a private blob");

    eve_dialer.close().await;
    assert!(eve.shutdown(TIMEOUT));
    assert!(alice.shutdown(TIMEOUT));
}

/// Polls a folder until `check` is true, or fails after the timeout.
async fn wait_for_folder(
    node: &NodeHandle,
    namespace: iroh_docs::NamespaceId,
    what: &str,
    check: impl Fn(&[groovy_core::folders::FolderEntry]) -> bool,
) -> Vec<groovy_core::folders::FolderEntry> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let entries = node
                .folder_entries(namespace)
                .await
                .expect("the folder can be read");
            if check(&entries) {
                return entries;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the folder never {what}"))
}

/// Waits until `check` is true, or fails after twenty seconds.
async fn wait_until(check: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the change arrives in time");
}

#[tokio::test]
async fn a_shared_folder_syncs_between_trusted_members() {
    use groovy_core::folders::FolderAccess;
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    // Collection is off here: this test is about sync. Collection is tested on its own.
    let (alice, _alice_events) = blob_node_collecting(&alice_dir, alice_key.clone(), None).await;
    let (bob, _bob_events) = blob_node_collecting(&bob_dir, bob_key, None).await;

    let folder = alice
        .create_folder("photos".into(), None)
        .await
        .expect("the folder is made");
    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("a.txt"), b"alpha").unwrap();
    std::fs::write(source.join("note.txt"), b"charlie").unwrap();
    alice
        .add_to_folder(
            folder.namespace,
            "photos/a.txt".into(),
            source.join("a.txt"),
        )
        .await
        .unwrap();
    alice
        .add_to_folder(folder.namespace, "note.txt".into(), source.join("note.txt"))
        .await
        .unwrap();
    let ticket = alice
        .folder_ticket(folder.namespace, FolderAccess::Write)
        .await
        .unwrap();

    bob.join_folder(ticket, None).await.expect("bob joins");
    let entries = wait_for_folder(&bob, folder.namespace, "shows bob the files", |entries| {
        entries.len() == 2
    })
    .await;
    let keys: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    assert_eq!(keys, ["note.txt", "photos/a.txt"]);

    // The bytes come with the entries, and are the same as alice's.
    let alpha = entries
        .iter()
        .find(|entry| entry.key == "photos/a.txt")
        .unwrap();
    let fetched = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(bytes) = bob.read_blob(alpha.hash).await.unwrap() {
                return bytes;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("bob downloads the file");
    assert_eq!(fetched, b"alpha");

    // A delete on alice's side removes the entry on bob's.
    alice
        .remove_from_folder(folder.namespace, "note.txt".into())
        .await
        .unwrap();
    wait_for_folder(
        &bob,
        folder.namespace,
        "removed the deleted file",
        |entries| entries.len() == 1,
    )
    .await;

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_peer_that_is_not_trusted_cannot_sync_a_shared_folder() {
    use groovy_core::folders::FolderAccess;
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, carol_key) = (SecretKey::generate(), SecretKey::generate());
    let alice_dir = fresh_dir(&tmp, "alice");
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (carol, _carol_events) = blob_node(&fresh_dir(&tmp, "carol"), carol_key).await;

    let folder = alice.create_folder("secret".into(), None).await.unwrap();
    let source = tmp.path().join("secret.txt");
    std::fs::write(&source, b"private").unwrap();
    alice
        .add_to_folder(folder.namespace, "secret.txt".into(), source)
        .await
        .unwrap();
    let ticket = alice
        .folder_ticket(folder.namespace, FolderAccess::Write)
        .await
        .unwrap();

    // Carol has the ticket, but alice does not trust her, so nothing syncs.
    let _ = carol.join_folder(ticket, None).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let seen = carol
        .folder_entries(folder.namespace)
        .await
        .unwrap_or_default();
    assert!(
        seen.is_empty(),
        "an untrusted peer synced a folder: {seen:?}"
    );

    assert!(alice.shutdown(TIMEOUT));
    assert!(carol.shutdown(TIMEOUT));
}

#[tokio::test]
async fn an_edit_on_one_device_reaches_another_member() {
    use groovy_core::folders::FolderAccess;
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    // Collection is off here: this test is about sync. Collection is tested on its own.
    let (alice, _alice_events) = blob_node_collecting(&alice_dir, alice_key.clone(), None).await;
    let (bob, _bob_events) = blob_node_collecting(&bob_dir, bob_key, None).await;

    // Alice shares a folder made from a local directory that already has a file in it.
    let alice_folder = tmp.path().join("alice-photos");
    std::fs::create_dir_all(&alice_folder).unwrap();
    std::fs::write(alice_folder.join("a.txt"), b"alpha").unwrap();
    let folder = alice
        .create_folder("photos".into(), Some(alice_folder.clone()))
        .await
        .expect("the folder is made from the directory");
    let ticket = alice
        .folder_ticket(folder.namespace, FolderAccess::Write)
        .await
        .unwrap();

    // Bob joins, and his directory receives the file.
    let bob_folder = tmp.path().join("bob-photos");
    bob.join_folder(ticket, Some(bob_folder.clone()))
        .await
        .expect("bob joins");
    wait_until(|| bob_folder.join("a.txt").is_file()).await;
    assert_eq!(std::fs::read(bob_folder.join("a.txt")).unwrap(), b"alpha");

    // An edit on alice's side reaches bob's directory.
    std::fs::write(alice_folder.join("a.txt"), b"alpha, edited").unwrap();
    wait_until(|| {
        std::fs::read(bob_folder.join("a.txt")).ok().as_deref() == Some(b"alpha, edited".as_slice())
    })
    .await;

    // A new file on bob's side reaches alice's directory, and a delete there removes it.
    std::fs::write(bob_folder.join("from-bob.txt"), b"hello").unwrap();
    wait_until(|| alice_folder.join("from-bob.txt").is_file()).await;
    std::fs::remove_file(bob_folder.join("from-bob.txt")).unwrap();
    wait_until(|| !alice_folder.join("from-bob.txt").exists()).await;

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

/// Shares the sample files from `alice` to `bob`, and returns the receiving transfer's id
/// and the root of its collection, once bob has them.
async fn share_to(
    alice: &NodeHandle,
    bob: &NodeHandle,
    bob_events: &mut EventStream,
    paths: Vec<std::path::PathBuf>,
) -> (i64, Hash) {
    let shared = alice.create_share(paths).await.expect("the share is made");
    let transfer = bob.receive_share(shared.ticket).await.expect("bob starts");
    loop {
        if let NodeEvent::FileReceived {
            transfer: id, root, ..
        } = tokio::time::timeout(TIMEOUT * 4, bob_events.next())
            .await
            .expect("bob receives the share")
            .unwrap()
        {
            assert_eq!(id, transfer);
            return (id, root);
        }
    }
}

#[tokio::test]
async fn deleting_a_transfer_frees_its_content_once_collected() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let (transfer, root) = share_to(&alice, &bob, &mut bob_events, paths).await;
    let note = bob
        .list_collection(root)
        .await
        .unwrap()
        .into_iter()
        .find(|(name, _)| name == "note.txt")
        .map(|(_, hash)| hash)
        .unwrap();
    assert!(bob.read_blob(note).await.unwrap().is_some());

    bob.delete_transfer(transfer)
        .await
        .expect("the history entry is deleted");
    assert!(bob.transfers().is_empty());
    wait_until_gone(&bob, note).await;

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

/// Waits until the blob is no longer in the store, or fails after twenty seconds.
async fn wait_until_gone(node: &NodeHandle, hash: Hash) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while node.read_blob(hash).await.unwrap().is_some() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the content is collected");
}

#[tokio::test]
async fn a_file_in_a_shared_folder_survives_collection() {
    let tmp = tempfile::tempdir().unwrap();
    let alice_dir = fresh_dir(&tmp, "alice");
    let (alice, _alice_events) = blob_node(&alice_dir, SecretKey::generate()).await;
    let folder = alice.create_folder("keep".into(), None).await.unwrap();
    let source = tmp.path().join("keep.txt");
    std::fs::write(&source, b"kept").unwrap();
    alice
        .add_to_folder(folder.namespace, "keep.txt".into(), source)
        .await
        .unwrap();
    let hash = alice.folder_entries(folder.namespace).await.unwrap()[0].hash;

    // Several collections run in this time, and the file is still there.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(alice.read_blob(hash).await.unwrap(), Some(b"kept".to_vec()));

    assert!(alice.shutdown(TIMEOUT));
}

#[tokio::test]
async fn storage_counts_what_is_held_and_warns_near_the_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;

    let folder = alice.create_folder("docs".into(), None).await.unwrap();
    let source = tmp.path().join("doc.txt");
    std::fs::write(&source, b"0123456789").unwrap();
    alice
        .add_to_folder(folder.namespace, "doc.txt".into(), source)
        .await
        .unwrap();
    assert_eq!(alice.storage_usage().await.unwrap().folder_bytes, 10);

    let (_, _) = share_to(
        &alice,
        &bob,
        &mut bob_events,
        write_sample_files(&tmp.path().join("source")),
    )
    .await;
    let usage = bob.storage_usage().await.unwrap();
    assert_eq!(usage.received_bytes, 17);
    assert_eq!(usage.cap_bytes, None);
    assert!(!usage.near_cap());

    // Nine tenths of a cap of 18 is 16.2 bytes, and 17 is past it.
    bob.set_storage_cap(Some(18)).await.unwrap();
    assert!(bob.storage_usage().await.unwrap().near_cap());
    bob.set_storage_cap(None).await.unwrap();
    assert_eq!(bob.storage_usage().await.unwrap().cap_bytes, None);

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn clearing_the_cache_drops_received_history_and_keeps_folders() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;

    let folder = alice.create_folder("keep".into(), None).await.unwrap();
    let source = tmp.path().join("keep.txt");
    std::fs::write(&source, b"kept").unwrap();
    alice
        .add_to_folder(folder.namespace, "keep.txt".into(), source)
        .await
        .unwrap();
    let kept = alice.folder_entries(folder.namespace).await.unwrap()[0].hash;

    let (_, root) = share_to(
        &alice,
        &bob,
        &mut bob_events,
        write_sample_files(&tmp.path().join("source")),
    )
    .await;
    assert_eq!(bob.transfers().len(), 1);

    bob.clear_cache().await.expect("the cache is cleared");
    assert!(bob.transfers().is_empty());
    let gone =
        bob.list_collection(root).await.is_err() || bob.read_blob(root).await.unwrap().is_none();
    let _ = gone;
    wait_until_gone(&bob, root).await;

    // Alice's folder keeps its file, and the cache clear does not reach it.
    assert_eq!(alice.read_blob(kept).await.unwrap(), Some(b"kept".to_vec()));

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

/// The keys of a folder's entries, sorted.
fn keys_of(entries: &[groovy_core::folders::FolderEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.key.clone()).collect()
}

#[tokio::test]
async fn concurrent_edits_keep_both_versions_as_a_conflict_copy() {
    use groovy_core::folders::FolderAccess;
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node_collecting(&alice_dir, alice_key, None).await;
    let (bob, _bob_events) = blob_node_collecting(&bob_dir, bob_key, None).await;

    let folder = alice.create_folder("docs".into(), None).await.unwrap();
    let write = |name: &str, text: &str| {
        let path = tmp.path().join(format!("{name}-{}.txt", text.len()));
        std::fs::write(&path, text).unwrap();
        path
    };
    alice
        .add_to_folder(folder.namespace, "a.txt".into(), write("a", "version zero"))
        .await
        .unwrap();
    let ticket = alice
        .folder_ticket(folder.namespace, FolderAccess::Write)
        .await
        .unwrap();
    bob.join_folder(ticket, None).await.expect("bob joins");
    wait_for_folder(
        &bob,
        folder.namespace,
        "shows bob version zero",
        |entries| entries.len() == 1,
    )
    .await;

    // Alice edits a version bob has not received. Bob edits from the version he had before
    // he saw alice's, so neither write saw the other.
    alice
        .add_to_folder(
            folder.namespace,
            "a.txt".into(),
            write("alice", "alice's edit"),
        )
        .await
        .unwrap();
    bob.add_to_folder_seeing(
        folder.namespace,
        "a.txt".into(),
        write("bob", "bob's edit"),
        Vec::new(),
    )
    .await
    .unwrap();

    // Both sides end with the same two entries: the file, and a copy of the other version.
    let alice_keys = wait_for_folder(&alice, folder.namespace, "shows both versions", |entries| {
        entries.len() == 2
    })
    .await;
    let bob_keys = wait_for_folder(&bob, folder.namespace, "shows both versions", |entries| {
        entries.len() == 2
    })
    .await;
    assert_eq!(keys_of(&alice_keys), keys_of(&bob_keys));
    let copy = keys_of(&alice_keys)
        .into_iter()
        .find(|key| key.contains("(conflict from"))
        .expect("one entry is a conflict copy");
    assert!(
        copy.starts_with("a (conflict from ") && copy.ends_with(").txt"),
        "{copy}"
    );

    // A write that has seen both versions replaces them, and the copy goes.
    alice
        .add_to_folder(
            folder.namespace,
            "a.txt".into(),
            write("alice2", "resolved"),
        )
        .await
        .unwrap();
    wait_for_folder(
        &bob,
        folder.namespace,
        "drops the conflict copy",
        |entries| keys_of(entries) == ["a.txt"],
    )
    .await;

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn deleting_a_file_leaves_files_whose_names_start_the_same() {
    let tmp = tempfile::tempdir().unwrap();
    let alice_dir = fresh_dir(&tmp, "alice");
    let (alice, _alice_events) =
        blob_node_collecting(&alice_dir, SecretKey::generate(), None).await;
    let folder = alice.create_folder("names".into(), None).await.unwrap();
    for name in ["x.txt", "x.txt2"] {
        let path = tmp.path().join(name);
        std::fs::write(&path, name).unwrap();
        alice
            .add_to_folder(folder.namespace, name.into(), path)
            .await
            .unwrap();
    }

    alice
        .remove_from_folder(folder.namespace, "x.txt".into())
        .await
        .unwrap();
    let entries = alice.folder_entries(folder.namespace).await.unwrap();
    assert_eq!(keys_of(&entries), ["x.txt2"]);

    assert!(alice.shutdown(TIMEOUT));
}

/// The sends this node has made, as its transfer list shows them.
fn sends_of(node: &NodeHandle) -> Vec<groovy_core::transfers::Transfer> {
    node.transfers()
        .into_iter()
        .filter(|t| t.direction == groovy_core::transfers::Direction::Send)
        .collect()
}

#[tokio::test]
async fn a_send_is_recorded_and_completes_when_the_peer_accepts() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), true).await.unwrap();
    send.await.unwrap().unwrap();

    let sent = sends_of(&alice);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].state,
        groovy_core::transfers::TransferState::Completed
    );
    assert_eq!(sent[0].peer, bob.endpoint_id());
    assert_eq!(sent[0].offer.files, 3);

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_declined_send_is_recorded_as_not_sent() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), false).await.unwrap();
    assert!(matches!(
        send.await.unwrap(),
        Err(groovy_core::NodeError::OfferDeclined)
    ));

    let sent = sends_of(&alice);
    assert!(matches!(
        sent[0].state,
        groovy_core::transfers::TransferState::Failed(_)
    ));

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn cancelling_a_send_while_it_waits_withdraws_the_offer() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    next_file_offer(&mut bob_events).await;

    // Bob has not answered. Alice cancels, and the offer is withdrawn on bob's side.
    let transfer = sends_of(&alice)[0].id;
    alice
        .cancel_transfer(transfer)
        .await
        .expect("the send is cancelled");
    let withdrawn = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let NodeEvent::OfferWithdrawn { peer } = bob_events.next().await.unwrap() {
                return peer;
            }
        }
    })
    .await
    .expect("bob learns the offer was withdrawn");
    assert_eq!(withdrawn, alice.endpoint_id());
    assert_eq!(
        sends_of(&alice)[0].state,
        groovy_core::transfers::TransferState::Cancelled
    );
    send.await.unwrap().expect("the cancelled send returns");

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn cancelling_an_accepted_send_stops_even_trusted_peers_fetching_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key.clone()).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    let offer = next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), true).await.unwrap();
    send.await.unwrap().unwrap();
    next_file_received(&mut bob_events).await;

    // Before cancelling, a trusted peer can fetch the offered collection.
    let dialer = dialer_with(bob_key).await;
    let connection = dialer
        .connect(alice.addr().await.unwrap(), iroh_blobs::ALPN)
        .await
        .unwrap();
    assert!(
        iroh_blobs::get::request::get_blob(connection, offer.root)
            .await
            .is_ok()
    );

    let transfer = sends_of(&alice)[0].id;
    alice
        .cancel_transfer(transfer)
        .await
        .expect("the accepted send is cancelled");

    let connection = dialer
        .connect(alice.addr().await.unwrap(), iroh_blobs::ALPN)
        .await
        .unwrap();
    assert!(
        iroh_blobs::get::request::get_blob(connection, offer.root)
            .await
            .is_err(),
        "a withdrawn send was still served"
    );

    dialer.close().await;
    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_send_cannot_be_paused() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    next_file_offer(&mut bob_events).await;
    let transfer = sends_of(&alice)[0].id;
    assert!(matches!(
        alice.pause_transfer(transfer).await,
        Err(groovy_core::NodeError::WrongTransferState(_))
    ));
    alice.cancel_transfer(transfer).await.unwrap();
    send.abort();

    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn an_accepted_send_can_be_paused_and_resumed_by_its_sender() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key.clone()).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key.clone()).await;
    let paths = write_sample_files(&tmp.path().join("source"));

    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    let offer = next_file_offer(&mut bob_events).await;
    bob.answer_offer(alice_key.public(), true).await.unwrap();
    send.await.unwrap().unwrap();
    next_file_received(&mut bob_events).await;

    let transfer = sends_of(&alice)[0].id;
    let dialer = dialer_with(bob_key).await;
    let fetch = |dialer: Endpoint, addr: EndpointAddr, root| async move {
        let connection = dialer.connect(addr, iroh_blobs::ALPN).await.unwrap();
        iroh_blobs::get::request::get_blob(connection, root)
            .await
            .is_ok()
    };

    alice
        .pause_transfer(transfer)
        .await
        .expect("the accepted send pauses");
    assert_eq!(
        sends_of(&alice)[0].state,
        groovy_core::transfers::TransferState::Paused
    );
    assert!(
        !fetch(dialer.clone(), alice.addr().await.unwrap(), offer.root).await,
        "a paused send was served"
    );

    alice
        .resume_transfer(transfer)
        .await
        .expect("the send resumes");
    assert_eq!(
        sends_of(&alice)[0].state,
        groovy_core::transfers::TransferState::Completed
    );
    assert!(
        fetch(dialer.clone(), alice.addr().await.unwrap(), offer.root).await,
        "a resumed send was not served"
    );

    dialer.close().await;
    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}

#[tokio::test]
async fn a_send_still_waiting_for_an_answer_cannot_be_paused() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice_key, bob_key) = (SecretKey::generate(), SecretKey::generate());
    let (alice_dir, bob_dir) = (fresh_dir(&tmp, "alice"), fresh_dir(&tmp, "bob"));
    trust_in(&alice_dir, &bob_key);
    trust_in(&bob_dir, &alice_key);
    let (alice, _alice_events) = blob_node(&alice_dir, alice_key).await;
    let (bob, mut bob_events) = blob_node(&bob_dir, bob_key).await;
    let paths = write_sample_files(&tmp.path().join("source"));
    let bob_addr = bob.addr().await.unwrap();
    let send = tokio::spawn({
        let alice = alice.clone();
        async move { alice.send_files(bob_addr, paths).await }
    });
    next_file_offer(&mut bob_events).await;
    let transfer = sends_of(&alice)[0].id;
    assert!(matches!(
        alice.pause_transfer(transfer).await,
        Err(groovy_core::NodeError::WrongTransferState(_))
    ));
    alice.cancel_transfer(transfer).await.unwrap();
    send.abort();
    assert!(alice.shutdown(TIMEOUT));
    assert!(bob.shutdown(TIMEOUT));
}
