//! Checks that an interrupted receive resumes without fetching finished bytes again.
//!
//! Run it from the repository root, which takes several minutes for the default size:
//!
//! ```text
//! cargo run --release -p groovy-core --example resume_check
//! cargo run --release -p groovy-core --example resume_check -- --mb 512
//! ```
//!
//! A large file is shared from one node to another over loopback. The receive is paused
//! once part of the file has arrived, then resumed. The check passes if the resume fetches
//! only what was missing, and the file arrives intact.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use groovy_core::protocol::PeerProfile;
use groovy_core::{
    ContactGate, DataDir, Database, Network, Node, NodeConfig, NodeEvent, RelaySetting, SecretKey,
};

const CHUNK: usize = 1 << 20;

/// How the receive is interrupted: paused, or by stopping and starting the node again.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Interrupt {
    Pause,
    Restart,
}

fn parse_args() -> (u64, Interrupt) {
    let mut args = std::env::args().skip(1);
    let mut mb = 5 * 1024;
    let mut interrupt = Interrupt::Pause;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--mb" => mb = value.parse().expect("--mb is a number"),
            "--interrupt" => {
                interrupt = match value.as_str() {
                    "pause" => Interrupt::Pause,
                    "restart" => Interrupt::Restart,
                    other => panic!("--interrupt is pause or restart, not {other}"),
                }
            }
            other => panic!("unknown flag {other}"),
        }
    }
    (mb, interrupt)
}

fn node_config(dir: &DataDir, database: &Arc<Database>) -> NodeConfig {
    NodeConfig {
        secret_key: SecretKey::generate(),
        network: Network::LocalOnly,
        profile: PeerProfile::default(),
        gate: Arc::new(ContactGate::new(database.clone())),
        discovery: groovy_core::Discovery::default(),
        relay: RelaySetting::Disabled,
        blobs: Some(dir.blobs_dir()),
        downloads: Some(dir.root().join("Downloads")),
        database: Some(database.clone()),
        garbage_interval: None,
    }
}

#[tokio::main]
async fn main() {
    let (mb, interrupt) = parse_args();
    let total = mb * CHUNK as u64;
    let root = std::env::current_dir()
        .expect("the current folder is known")
        .join("target/resume-check");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("the check folder can be created");

    let source = root.join("big.bin");
    {
        let mut file = fs::File::create(&source).expect("the source file can be created");
        let chunk: Vec<u8> = (0..CHUNK).map(|i| (i % 253) as u8).collect();
        for _ in 0..mb {
            file.write_all(&chunk)
                .expect("the source file can be written");
        }
        file.sync_all().expect("the source file can be flushed");
    }

    let alice_dir = DataDir::at(root.join("alice"));
    let bob_dir = DataDir::at(root.join("bob"));
    for dir in [&alice_dir, &bob_dir] {
        dir.create_all().expect("the node folder can be created");
    }
    let alice_db = Arc::new(Database::open(&alice_dir.database_file()).expect("database opens"));
    let bob_db = Arc::new(Database::open(&bob_dir.database_file()).expect("database opens"));
    let (alice, mut alice_events) =
        Node::start(node_config(&alice_dir, &alice_db)).expect("alice starts");
    let (mut bob, mut bob_events) =
        Node::start(node_config(&bob_dir, &bob_db)).expect("bob starts");
    wait_started(&mut alice_events).await;
    wait_started(&mut bob_events).await;

    let shared = alice
        .create_share(vec![source.clone()])
        .await
        .expect("the share is made");
    let transfer = bob
        .receive_share(shared.ticket)
        .await
        .expect("bob starts receiving");

    // Pause once a quarter of the file has arrived.
    let pause_at = total / 4;
    let mut progress = 0;
    while progress < pause_at {
        if let NodeEvent::TransferChanged(t) = next(&mut bob_events).await
            && t.id == transfer
        {
            progress = t.done;
        }
    }
    let paused_at = bob
        .transfers()
        .into_iter()
        .find(|t| t.id == transfer)
        .map(|t| t.done)
        .unwrap_or(0);
    let started = Instant::now();
    match interrupt {
        Interrupt::Pause => {
            bob.pause_transfer(transfer)
                .await
                .expect("the receive pauses");
            println!(
                "paused with {:.0} MiB of {mb} MiB received",
                paused_at as f64 / CHUNK as f64
            );
            // Resume, and count what the resume fetches.
            bob.resume_transfer(transfer)
                .await
                .expect("the receive resumes");
        }
        Interrupt::Restart => {
            // Stop the node, as if the app were closed, then start it again. The receive
            // is picked up at startup without anything asking for it.
            let stopped = bob.shutdown(Duration::from_secs(5));
            println!("old node stopped in time: {stopped}");
            println!(
                "stopped with {:.0} MiB of {mb} MiB received",
                paused_at as f64 / CHUNK as f64
            );
            let started_node = Node::start(node_config(&bob_dir, &bob_db)).expect("bob restarts");
            bob_events = started_node.1;
            wait_started(&mut bob_events).await;
            println!(
                "new node started; restored transfers: {:?}",
                bob.transfers()
                    .iter()
                    .map(|t| (t.id, t.state.clone(), t.done))
                    .collect::<Vec<_>>()
            );
            bob = started_node.0;
        }
    }

    // Count what the receive fetches after the interruption. Finished bytes must not be
    // fetched again.
    let fetched = loop {
        if let NodeEvent::FileReceived {
            transfer: id,
            fetched,
            ..
        } = next(&mut bob_events).await
            && id == transfer
        {
            break fetched;
        }
    };
    let elapsed = started.elapsed();

    // The file that arrived is the file that was sent.
    let saved = root.join("bob/../bob/Downloads/big.bin");
    let intact = fs::read(&saved)
        .map(|bytes| bytes.len() as u64 == total)
        .unwrap_or(false)
        && files_equal(&source, &saved);

    println!(
        "resumed in {:.1} s, fetched {:.0} MiB of the {mb} MiB",
        elapsed.as_secs_f64(),
        fetched as f64 / CHUNK as f64
    );
    println!("file intact: {intact}");
    // What was missing when the receive was paused, and how much more than that was fetched.
    // Some bytes are in flight when a receive is paused, and are not kept, so a small
    // overage is allowed: up to 2% of the file.
    let remaining = total - paused_at.min(total);
    let overage = fetched.saturating_sub(remaining);
    println!(
        "fetched beyond what was missing: {:.1} MiB",
        overage as f64 / CHUNK as f64
    );
    let passed = intact && fetched < total && overage <= total / 50;
    println!("result: {}", if passed { "PASS" } else { "FAIL" });

    alice.shutdown(Duration::from_secs(5));
    bob.shutdown(Duration::from_secs(5));
    let _ = fs::remove_dir_all(&root);
    if !passed {
        std::process::exit(1);
    }
}

async fn next(events: &mut groovy_core::EventStream) -> NodeEvent {
    events.next().await.expect("the node keeps running")
}

async fn wait_started(events: &mut groovy_core::EventStream) {
    loop {
        if let Some(NodeEvent::Started) = events.next().await {
            return;
        }
    }
}

/// Compares two files in pieces, so they are not held in memory.
fn files_equal(a: &PathBuf, b: &PathBuf) -> bool {
    use std::io::Read;
    let (Ok(mut fa), Ok(mut fb)) = (fs::File::open(a), fs::File::open(b)) else {
        return false;
    };
    let (mut ba, mut bb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let (Ok(na), Ok(nb)) = (fa.read(&mut ba), fb.read(&mut bb)) else {
            return false;
        };
        if na != nb || ba[..na] != bb[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
}
