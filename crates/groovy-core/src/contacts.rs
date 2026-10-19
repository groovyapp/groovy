//! Contacts: the peers this node has handshaken with, and whether they may connect.
//!
//! A peer starts as `pending` when it first connects. Pairing (#20) moves it to
//! `trusted`, and a block moves it to `blocked`. A blocked peer is refused
//! before the handshake, so it learns nothing and can use no protocol. The
//! profile a peer shows (its name and avatar) updates on each handshake, but a
//! nickname this node has set is kept.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::{EndpointId, PublicKey};
use rusqlite::{OptionalExtension, Row, params};
use tracing::warn;

use crate::db::{Database, DbError};
use crate::node::PeerGate;
use crate::protocol::PeerInfo;

/// How much this node trusts a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Seen, but not yet paired.
    Pending,
    /// Paired: the user has accepted this peer.
    Trusted,
    /// Refused at the connection, for every protocol.
    Blocked,
}

impl Trust {
    fn as_str(self) -> &'static str {
        match self {
            Trust::Pending => "pending",
            Trust::Trusted => "trusted",
            Trust::Blocked => "blocked",
        }
    }

    fn from_str(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Trust::Pending),
            "trusted" => Some(Trust::Trusted),
            "blocked" => Some(Trust::Blocked),
            _ => None,
        }
    }
}

/// A peer this node knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub endpoint_id: EndpointId,
    /// The name the peer shows. Updated on each handshake.
    pub display_name: String,
    /// A name this node's user chose. Never replaced by a handshake.
    pub nickname: Option<String>,
    pub avatar_hash: Option<[u8; 32]>,
    pub trust: Trust,
    /// Unix seconds.
    pub first_seen: i64,
    /// Unix seconds.
    pub last_seen: i64,
}

impl Contact {
    /// The name to show: the nickname if there is one, otherwise the peer's own name.
    pub fn label(&self) -> &str {
        self.nickname
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.display_name)
    }
}

/// The current time in unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn corrupt(message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Blob,
        Box::new(std::io::Error::other(message)),
    )
}

fn contact_from_row(row: &Row<'_>) -> rusqlite::Result<Contact> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let id_array: [u8; 32] = id_bytes
        .try_into()
        .map_err(|_| corrupt("a contact's endpoint ID is not 32 bytes"))?;
    let endpoint_id = PublicKey::from_bytes(&id_array)
        .map_err(|_| corrupt("a contact's endpoint ID is invalid"))?;

    let avatar: Option<Vec<u8>> = row.get(3)?;
    let avatar_hash = match avatar {
        Some(bytes) => Some(
            bytes
                .try_into()
                .map_err(|_| corrupt("a contact's avatar hash is not 32 bytes"))?,
        ),
        None => None,
    };

    let trust_text: String = row.get(4)?;
    let trust = Trust::from_str(&trust_text).ok_or(corrupt("a contact has an unknown trust"))?;

    Ok(Contact {
        endpoint_id,
        display_name: row.get(1)?,
        nickname: row.get(2)?,
        avatar_hash,
        trust,
        first_seen: row.get(5)?,
        last_seen: row.get(6)?,
    })
}

const SELECT_CONTACT: &str = "SELECT endpoint_id, display_name, nickname, avatar_hash, trust, first_seen, last_seen FROM contacts";

impl Database {
    /// Records a handshake with `peer` at `now`, and returns the contact.
    ///
    /// A new peer starts as `pending`. For a known peer, the shown name and avatar
    /// update, and the nickname and trust are kept.
    pub fn record_handshake(&self, peer: &PeerInfo, now: i64) -> Result<Contact, DbError> {
        let id = *peer.endpoint_id.as_bytes();
        let avatar = peer.profile.avatar_hash.map(|hash| hash.to_vec());
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO contacts (endpoint_id, display_name, avatar_hash, trust, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, 'pending', ?4, ?4)
                 ON CONFLICT(endpoint_id) DO UPDATE SET
                     display_name = excluded.display_name,
                     avatar_hash = excluded.avatar_hash,
                     last_seen = excluded.last_seen",
                params![id.as_slice(), peer.profile.display_name, avatar, now],
            )?;
            conn.query_row(
                &format!("{SELECT_CONTACT} WHERE endpoint_id = ?1"),
                [id.as_slice()],
                contact_from_row,
            )
        })
    }

    /// The contact for `id`, if there is one.
    pub fn contact(&self, id: EndpointId) -> Result<Option<Contact>, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                &format!("{SELECT_CONTACT} WHERE endpoint_id = ?1"),
                [id.as_bytes().as_slice()],
                contact_from_row,
            )
            .optional()
        })
    }

    /// All contacts, most recently seen first.
    pub fn contacts(&self) -> Result<Vec<Contact>, DbError> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(&format!(
                "{SELECT_CONTACT} ORDER BY last_seen DESC, display_name"
            ))?;
            let rows = statement.query_map([], contact_from_row)?;
            rows.collect()
        })
    }

    /// Sets how much this node trusts `id`. Returns `false` if there is no such contact.
    pub fn set_trust(&self, id: EndpointId, trust: Trust) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE contacts SET trust = ?2 WHERE endpoint_id = ?1",
                params![id.as_bytes().as_slice(), trust.as_str()],
            )
            .map(|changed| changed > 0)
        })
    }

    /// Sets or clears the nickname for `id`. Returns `false` if there is no such contact.
    pub fn set_nickname(&self, id: EndpointId, nickname: Option<&str>) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE contacts SET nickname = ?2 WHERE endpoint_id = ?1",
                params![id.as_bytes().as_slice(), nickname],
            )
            .map(|changed| changed > 0)
        })
    }

    /// Removes a contact. A peer that connects again is recorded as new.
    pub fn remove_contact(&self, id: EndpointId) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM contacts WHERE endpoint_id = ?1",
                [id.as_bytes().as_slice()],
            )
            .map(|removed| removed > 0)
        })
    }

    /// Whether `id` is blocked.
    pub fn is_blocked(&self, id: EndpointId) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM contacts WHERE endpoint_id = ?1 AND trust = 'blocked')",
                [id.as_bytes().as_slice()],
                |row| row.get(0),
            )
        })
    }
}

/// Decides connections from the contacts database: blocked peers are refused,
/// and every handshake is recorded.
///
/// If the database cannot be read, the peer is refused. Failing open would let a
/// blocked peer in whenever the database is busy or damaged.
pub struct ContactGate {
    database: Arc<Database>,
}

impl ContactGate {
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
    }
}

impl PeerGate for ContactGate {
    fn may_connect(&self, peer: &EndpointId) -> bool {
        match self.database.is_blocked(*peer) {
            Ok(blocked) => !blocked,
            Err(error) => {
                warn!(%error, "could not check a peer's block; refusing it");
                false
            }
        }
    }

    fn handshook(&self, peer: &PeerInfo) {
        if let Err(error) = self.database.record_handshake(peer, now()) {
            warn!(%error, "could not record a handshake");
        }
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;
    use crate::protocol::PeerProfile;

    fn database() -> (tempfile::TempDir, Database) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("groovy.sqlite")).unwrap();
        (tmp, db)
    }

    fn peer(name: &str, avatar: Option<[u8; 32]>) -> PeerInfo {
        PeerInfo {
            endpoint_id: SecretKey::generate().public(),
            profile: PeerProfile {
                display_name: name.to_owned(),
                avatar_hash: avatar,
            },
        }
    }

    #[test]
    fn a_new_peer_is_recorded_as_pending() {
        let (_tmp, db) = database();
        let grace = peer("Grace", None);

        let contact = db.record_handshake(&grace, 100).unwrap();

        assert_eq!(contact.trust, Trust::Pending);
        assert_eq!(contact.display_name, "Grace");
        assert_eq!(contact.first_seen, 100);
        assert_eq!(contact.last_seen, 100);
        assert_eq!(db.contact(grace.endpoint_id).unwrap(), Some(contact));
    }

    #[test]
    fn a_later_handshake_updates_the_name_and_avatar_and_keeps_first_seen() {
        let (_tmp, db) = database();
        let mut grace = peer("Grace", None);
        db.record_handshake(&grace, 100).unwrap();
        grace.profile = PeerProfile {
            display_name: "Grace Hopper".to_owned(),
            avatar_hash: Some([3; 32]),
        };

        let contact = db.record_handshake(&grace, 200).unwrap();

        assert_eq!(contact.display_name, "Grace Hopper");
        assert_eq!(contact.avatar_hash, Some([3; 32]));
        assert_eq!(contact.first_seen, 100);
        assert_eq!(contact.last_seen, 200);
    }

    #[test]
    fn a_nickname_survives_handshakes_and_is_shown_in_place_of_the_name() {
        let (_tmp, db) = database();
        let grace = peer("Grace", None);
        db.record_handshake(&grace, 100).unwrap();
        assert!(db.set_nickname(grace.endpoint_id, Some("Admiral")).unwrap());

        let mut renamed = grace.clone();
        renamed.profile.display_name = "Grace Hopper".to_owned();
        let contact = db.record_handshake(&renamed, 200).unwrap();

        assert_eq!(contact.nickname.as_deref(), Some("Admiral"));
        assert_eq!(contact.label(), "Admiral");
        assert_eq!(contact.display_name, "Grace Hopper");
    }

    #[test]
    fn a_handshake_does_not_change_trust() {
        let (_tmp, db) = database();
        let grace = peer("Grace", None);
        db.record_handshake(&grace, 100).unwrap();
        db.set_trust(grace.endpoint_id, Trust::Trusted).unwrap();

        let contact = db.record_handshake(&grace, 200).unwrap();

        assert_eq!(contact.trust, Trust::Trusted);
    }

    #[test]
    fn blocking_and_unblocking_change_what_the_gate_allows() {
        let (_tmp, db) = database();
        let grace = peer("Grace", None);
        db.record_handshake(&grace, 100).unwrap();
        assert!(!db.is_blocked(grace.endpoint_id).unwrap());

        db.set_trust(grace.endpoint_id, Trust::Blocked).unwrap();
        assert!(db.is_blocked(grace.endpoint_id).unwrap());

        db.set_trust(grace.endpoint_id, Trust::Trusted).unwrap();
        assert!(!db.is_blocked(grace.endpoint_id).unwrap());
    }

    #[test]
    fn removing_a_contact_forgets_it() {
        let (_tmp, db) = database();
        let grace = peer("Grace", None);
        db.record_handshake(&grace, 100).unwrap();

        assert!(db.remove_contact(grace.endpoint_id).unwrap());

        assert_eq!(db.contact(grace.endpoint_id).unwrap(), None);
        assert!(!db.remove_contact(grace.endpoint_id).unwrap());
    }

    #[test]
    fn contacts_list_the_most_recently_seen_first() {
        let (_tmp, db) = database();
        let older = peer("Older", None);
        let newer = peer("Newer", None);
        db.record_handshake(&older, 100).unwrap();
        db.record_handshake(&newer, 300).unwrap();

        let names: Vec<_> = db
            .contacts()
            .unwrap()
            .into_iter()
            .map(|contact| contact.display_name)
            .collect();

        assert_eq!(names, ["Newer", "Older"]);
    }

    #[test]
    fn the_gate_refuses_blocked_peers_and_records_handshakes() {
        let (_tmp, db) = database();
        let db = Arc::new(db);
        let gate = ContactGate::new(db.clone());
        let blocked = peer("Mallory", None);
        let friend = peer("Grace", None);
        gate.handshook(&blocked);
        db.set_trust(blocked.endpoint_id, Trust::Blocked).unwrap();

        assert!(!gate.may_connect(&blocked.endpoint_id));
        assert!(gate.may_connect(&friend.endpoint_id));
        gate.handshook(&friend);
        assert!(db.contact(friend.endpoint_id).unwrap().is_some());
    }
}
