//! Measures how fast a search runs over a large history.
//!
//! Run it in release mode from the repository root:
//!
//! ```text
//! cargo run --release -p groovy-core --example search_bench
//! cargo run --release -p groovy-core --example search_bench -- 100000
//! ```
//!
//! It writes that many direct messages, spread over 50 conversations, into a database in
//! `target/search-bench`, then times searches for a rare word and a common one. The target is
//! 200 ms per search.

use std::time::Instant;

use groovy_core::Database;

const VOCABULARY: &[&str] = &[
    "the", "and", "lunch", "meeting", "photo", "garden", "rust", "music", "travel", "plans",
    "coffee", "river", "budget", "weekend", "movie", "letter", "kestrel", "market", "train",
    "window", "recipe", "holiday", "puzzle", "garage", "signal", "anchor", "lantern", "orchard",
    "compass", "harbour",
];

/// A small deterministic generator, so each run writes the same history.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

fn main() {
    let count: u64 = std::env::args()
        .nth(1)
        .map(|value| value.parse().expect("the count is a number"))
        .unwrap_or(100_000);
    let root = std::env::current_dir()
        .expect("the current folder is known")
        .join("target/search-bench");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the bench folder can be created");
    let path = root.join("groovy.db");

    // Open once so the schema is in place, then write the rows in one transaction.
    drop(Database::open(&path).expect("the database opens"));
    let mut conn = rusqlite::Connection::open(&path).expect("the database opens");
    let conversations: Vec<[u8; 32]> = (0..50u8)
        .map(|n| {
            let mut bytes = [0u8; 32];
            bytes[0] = n;
            bytes[31] = 1;
            // Use valid endpoint keys: the derived public key of a fixed secret per conversation.
            iroh::SecretKey::from_bytes(&bytes)
                .public()
                .as_bytes()
                .to_owned()
        })
        .collect();
    let own = iroh::SecretKey::from_bytes(&[200; 32]).public();
    let started = Instant::now();
    {
        let tx = conn.transaction().expect("a transaction starts");
        let mut random = Lcg(7);
        for n in 0..count {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&n.to_le_bytes());
            let conversation = conversations[(n % conversations.len() as u64) as usize];
            let words: Vec<&str> = (0..(5 + random.next() % 8))
                .map(|_| VOCABULARY[(random.next() % VOCABULARY.len() as u64) as usize])
                .collect();
            tx.execute(
                "INSERT INTO messages (id, conversation, author, timestamp, body, reply_to, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'delivered')",
                rusqlite::params![
                    id.as_slice(),
                    conversation.as_slice(),
                    own.as_bytes().as_slice(),
                    n as i64,
                    words.join(" "),
                ],
            )
            .expect("a message is written");
        }
        tx.commit().expect("the transaction commits");
    }
    println!(
        "wrote {count} messages in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    drop(conn);

    let db = Database::open(&path).expect("the database opens");
    let mut worst = 0.0f64;
    for word in ["kestrel", "the", "harbour lantern"] {
        // Run each search a few times, and report the slowest, so a cold cache does not decide it.
        let mut slowest = 0.0f64;
        let mut hits = 0;
        for _ in 0..5 {
            let started = Instant::now();
            hits = db.search_messages(word, 50).expect("the search runs").len();
            slowest = slowest.max(started.elapsed().as_secs_f64() * 1000.0);
        }
        worst = worst.max(slowest);
        println!("search {word:?}: {hits} hits, slowest of 5 runs {slowest:.1} ms");
    }
    let verdict = if worst <= 200.0 { "PASS" } else { "FAIL" };
    println!("slowest search {worst:.1} ms against a 200 ms target: {verdict}");
    let _ = std::fs::remove_dir_all(&root);
    if worst > 200.0 {
        std::process::exit(1);
    }
}
