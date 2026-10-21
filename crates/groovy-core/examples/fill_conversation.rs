//! Fills a database with one long conversation, for checking how the chat handles it.
//!
//! Stop Groovy first, then run it from the repository root:
//!
//! ```text
//! cargo run -p groovy-core --example fill_conversation -- <data dir> <own endpoint id> [count]
//! ```
//!
//! It adds a trusted contact called "Long chat" and `count` messages (10,000 by default)
//! between it and this device, alternating sides, a minute apart, ending now.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use groovy_core::protocol::PeerProfile;
use groovy_core::{DataDir, Database, EndpointId, PeerInfo, SecretKey, Trust};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .expect("the data directory is the first argument");
    let own = EndpointId::from_str(
        &args
            .next()
            .expect("this device's endpoint id is the second"),
    )
    .expect("the endpoint id is valid");
    let count: i64 = args
        .next()
        .map(|value| value.parse().expect("the count is a number"))
        .unwrap_or(10_000);

    let dir = DataDir::at(std::path::PathBuf::from(dir));
    let path = dir.database_file();
    let db = Database::open(&path).expect("the database opens");
    let contact = SecretKey::generate().public();
    db.record_handshake(
        &PeerInfo {
            endpoint_id: contact,
            profile: PeerProfile {
                display_name: "Long chat".to_owned(),
                avatar_hash: None,
                user: None,
            },
        },
        0,
    )
    .expect("the contact is recorded");
    db.set_trust(contact, Trust::Trusted)
        .expect("the contact is trusted");
    drop(db);

    // Written in one transaction, so it takes seconds rather than minutes.
    let mut conn = rusqlite::Connection::open(&path).expect("the database opens");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as i64;
    let tx = conn.transaction().expect("a transaction starts");
    for n in 0..count {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&n.to_le_bytes());
        id[8..].copy_from_slice(&contact.as_bytes()[..8]);
        let author = if n % 3 == 0 { own } else { contact };
        let body = match n % 5 {
            0 => format!("Message {n}"),
            1 => format!("Message {n}, a little longer, so the lines are not all the same width."),
            2 => format!("Message {n}\nwith a second line"),
            3 => format!("Message {n}: {}", "words ".repeat(20)),
            _ => format!("Short {n}"),
        };
        tx.execute(
            "INSERT INTO messages (id, conversation, author, timestamp, body, reply_to, state)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'delivered')",
            rusqlite::params![
                id.as_slice(),
                contact.as_bytes().as_slice(),
                author.as_bytes().as_slice(),
                now - (count - n) * 60_000,
                body,
            ],
        )
        .expect("a message is written");
    }
    tx.commit().expect("the transaction commits");
    println!("added {count} messages with a contact called Long chat ({contact})");
}
