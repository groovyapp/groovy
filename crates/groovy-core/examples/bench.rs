//! Measures import, small-file and loopback transfer throughput.
//!
//! Run it with one command, from the repository root:
//!
//! ```text
//! cargo run --release -p groovy-core --example bench
//! cargo run --release -p groovy-core --example bench -- --large-mb 10240 --small-files 100000
//! ```
//!
//! The data is written under `target/bench`, and removed when the run ends. Results are
//! printed as a table, so they can be copied into `docs/benchmarks.md`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use groovy_core::files;
use groovy_core::protocol::PeerProfile;
use groovy_core::{
    ContactGate, DataDir, Database, Network, Node, NodeConfig, NodeEvent, RelaySetting, SecretKey,
};
use iroh_blobs::store::fs::FsStore;

/// How long a transfer may take before the run gives up.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(3600);
/// The chunk the large file is written in.
const CHUNK: usize = 1 << 20;

struct Sizes {
    large_mb: u64,
    small_files: u64,
    small_bytes: usize,
}

fn parse_args() -> Sizes {
    let mut sizes = Sizes {
        large_mb: 1024,
        small_files: 10_000,
        small_bytes: 1024,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--large-mb" => sizes.large_mb = value.parse().expect("--large-mb is a number"),
            "--small-files" => {
                sizes.small_files = value.parse().expect("--small-files is a number")
            }
            "--small-bytes" => {
                sizes.small_bytes = value.parse().expect("--small-bytes is a number")
            }
            other => panic!("unknown flag {other}"),
        }
    }
    sizes
}

/// Writes `megabytes` of data to `path`, in chunks, without holding it all in memory.
fn write_large(path: &Path, megabytes: u64) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    let chunk: Vec<u8> = (0..CHUNK).map(|i| (i % 251) as u8).collect();
    for _ in 0..megabytes {
        file.write_all(&chunk)?;
    }
    file.sync_all()
}

/// Writes `count` small files, each `bytes` long, in folders of 100 files each.
fn write_small(root: &Path, count: u64, bytes: usize) -> std::io::Result<PathBuf> {
    let folder = root.join("small");
    for index in 0..count {
        let dir = folder.join(format!("{:05}", index / 100));
        if index % 100 == 0 {
            fs::create_dir_all(&dir)?;
        }
        let content: Vec<u8> = (0..bytes)
            .map(|i| ((i as u64 + index) % 251) as u8)
            .collect();
        fs::write(dir.join(format!("{index:08}.bin")), content)?;
    }
    Ok(folder)
}

/// Opens a blob store in `dir`, creating the folder first.
async fn open_store(dir: &Path) -> FsStore {
    fs::create_dir_all(dir).expect("the store folder can be created");
    FsStore::load(dir).await.expect("the store opens")
}

fn rate(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / 1_000_000.0 / elapsed.as_secs_f64()
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
        user_keys: None,
    }
}

#[tokio::main]
async fn main() {
    let sizes = parse_args();
    let root = std::env::current_dir()
        .expect("the current folder is known")
        .join("target/bench");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("the bench folder can be created");

    println!("# Groovy benchmark");
    println!();
    println!("| Measurement | Size | Time | Throughput |\n|---|---|---|---|");

    // 1. Import of one large file: hashing it and copying it into the store.
    let large = root.join("large.bin");
    write_large(&large, sizes.large_mb).expect("the large file can be written");
    let large_bytes = sizes.large_mb * CHUNK as u64;
    let store = open_store(&root.join("import-store")).await;
    let started = Instant::now();
    files::import(&store, std::slice::from_ref(&large), |_, _| {})
        .await
        .expect("the large file imports");
    let elapsed = started.elapsed();
    println!(
        "| Import one large file | {} MB | {:.1} s | {:.0} MB/s |",
        sizes.large_mb,
        elapsed.as_secs_f64(),
        rate(large_bytes, elapsed)
    );
    drop(store);

    // 2. Import of many small files, as one folder.
    let small = write_small(&root, sizes.small_files, sizes.small_bytes)
        .expect("the small files can be written");
    let small_total = sizes.small_files * sizes.small_bytes as u64;
    let store = open_store(&root.join("small-store")).await;
    let started = Instant::now();
    let imported = files::import(&store, std::slice::from_ref(&small), |_, _| {})
        .await
        .expect("the small files import");
    let elapsed = started.elapsed();
    println!(
        "| Import {} small files | {} B each | {:.1} s | {:.0} files/s, {:.1} MB/s |",
        imported.files,
        sizes.small_bytes,
        elapsed.as_secs_f64(),
        imported.files as f64 / elapsed.as_secs_f64(),
        rate(small_total, elapsed)
    );
    drop(store);

    // 3. Transfer over loopback: a share is made on one node and received by another.
    let alice_dir = DataDir::at(root.join("alice"));
    let bob_dir = DataDir::at(root.join("bob"));
    for dir in [&alice_dir, &bob_dir] {
        dir.create_all().expect("the node folder can be created");
    }
    let alice_db = Arc::new(Database::open(&alice_dir.database_file()).expect("database opens"));
    let bob_db = Arc::new(Database::open(&bob_dir.database_file()).expect("database opens"));
    let (alice, mut alice_events) =
        Node::start(node_config(&alice_dir, &alice_db)).expect("alice starts");
    let (bob, mut bob_events) = Node::start(node_config(&bob_dir, &bob_db)).expect("bob starts");
    wait_started(&mut alice_events).await;
    wait_started(&mut bob_events).await;

    let started = Instant::now();
    let shared = alice
        .create_share(vec![large.clone()])
        .await
        .expect("the share is made");
    let share_elapsed = started.elapsed();
    println!(
        "| Make a share of the large file | {} MB | {:.1} s | {:.0} MB/s |",
        sizes.large_mb,
        share_elapsed.as_secs_f64(),
        rate(large_bytes, share_elapsed)
    );

    let started = Instant::now();
    let transfer = bob
        .receive_share(shared.ticket)
        .await
        .expect("bob starts receiving");
    let received = tokio::time::timeout(TRANSFER_TIMEOUT, async {
        loop {
            match bob_events.next().await {
                Some(NodeEvent::FileReceived { transfer: id, .. }) if id == transfer => break,
                Some(_) => {}
                None => panic!("bob stopped before the transfer finished"),
            }
        }
    })
    .await;
    received.expect("the transfer finishes within the timeout");
    let elapsed = started.elapsed();
    println!(
        "| Transfer over loopback | {} MB | {:.1} s | {:.0} MB/s |",
        sizes.large_mb,
        elapsed.as_secs_f64(),
        rate(large_bytes, elapsed)
    );

    alice.shutdown(Duration::from_secs(5));
    bob.shutdown(Duration::from_secs(5));
    let _ = fs::remove_dir_all(&root);
}

async fn wait_started(events: &mut groovy_core::EventStream) {
    loop {
        if let Some(NodeEvent::Started) = events.next().await {
            return;
        }
    }
}
