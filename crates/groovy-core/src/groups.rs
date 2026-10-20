//! Group chat: messages between a group's members over an iroh-gossip topic.
//!
//! A group has a random id, and its gossip topic is that id. Its members are the contacts
//! that have joined it. A message is signed by its author's endpoint key, and a member checks
//! the signature and that the author is a trusted contact before it stores the message. So a
//! message relayed by another member still shows who wrote it.
//!
//! Gossip only reaches the members online at the time. A member that comes back asks each
//! member it reconnects to for the messages it missed, in pages, over the Groovy protocol.
//! Each message in a page carries its signature, so the page can come from any member.
//!
//! The members come from the invitation (its ticket names some members) and from the members
//! met later. Changing who may be a member, and removing one, come with #44.

use iroh::{EndpointAddr, EndpointId, SecretKey, Signature};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::db::{Database, DbError};
use crate::messages::{MessageError, MessageId, clean_body};

/// The id of a group. Its gossip topic is the same bytes.
pub type GroupId = [u8; 32];

/// The version of [`GroupEnvelope`]. A message in another version is refused.
pub const GROUP_VERSION: u16 = 1;
/// How many messages one history page holds. Each is at most 4 KiB, so a page stays well
/// under the protocol's message limit.
pub const PAGE_SIZE: usize = 8;

const TICKET_PREFIX: &str = "groovygroup1";

/// A message as it is sent between members. The signature covers every other field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupEnvelope {
    pub version: u16,
    pub group: GroupId,
    pub id: MessageId,
    pub author: EndpointId,
    /// Milliseconds since the Unix epoch, on the author's clock.
    pub timestamp: i64,
    pub body: String,
    pub reply_to: Option<MessageId>,
    pub signature: Signature,
}

/// A group message as this device has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMessage {
    pub id: MessageId,
    pub group: GroupId,
    pub author: EndpointId,
    pub timestamp: i64,
    pub body: String,
    pub reply_to: Option<MessageId>,
}

/// A group this device is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    pub id: GroupId,
    /// What this device calls the group.
    pub name: String,
    /// Unix seconds.
    pub created: i64,
}

/// A page of messages a member answers with, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupPage {
    pub messages: Vec<GroupEnvelope>,
    /// Whether more messages follow this page.
    pub more: bool,
    /// Addresses of the group's other members that the answering member knows, so a member
    /// that has just joined can reach them.
    pub members: Vec<EndpointAddr>,
}

/// What an invitation carries: the group, and some of its members to join through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupTicket {
    pub group: GroupId,
    pub peers: Vec<EndpointAddr>,
}

/// Errors from group chat.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum GroupError {
    #[error("there is no group with that id")]
    UnknownGroup,
    #[error("the message's signature does not match its author")]
    BadSignature,
    #[error("this build does not read that version of group messages")]
    Version,
    #[error("the group ticket is not valid")]
    BadTicket,
    #[error(transparent)]
    Message(#[from] MessageError),
}

/// What a signature covers: every field of the envelope but the signature itself.
fn signed_bytes(
    version: u16,
    group: GroupId,
    id: MessageId,
    author: EndpointId,
    timestamp: i64,
    body: &str,
    reply_to: Option<MessageId>,
) -> Vec<u8> {
    postcard::to_stdvec(&(version, group, id, author, timestamp, body, reply_to))
        .expect("the fields of an envelope encode")
}

/// Writes a new message, signed with the author's key.
pub fn sign(
    secret: &SecretKey,
    group: GroupId,
    id: MessageId,
    timestamp: i64,
    body: String,
    reply_to: Option<MessageId>,
) -> GroupEnvelope {
    let author = secret.public();
    let signature = secret.sign(&signed_bytes(
        GROUP_VERSION,
        group,
        id,
        author,
        timestamp,
        &body,
        reply_to,
    ));
    GroupEnvelope {
        version: GROUP_VERSION,
        group,
        id,
        author,
        timestamp,
        body,
        reply_to,
        signature,
    }
}

/// Checks a message's version, its body, and that its signature is its author's.
pub fn verify(envelope: &GroupEnvelope) -> Result<(), GroupError> {
    if envelope.version != GROUP_VERSION {
        return Err(GroupError::Version);
    }
    clean_body(&envelope.body)?;
    envelope
        .author
        .verify(
            &signed_bytes(
                envelope.version,
                envelope.group,
                envelope.id,
                envelope.author,
                envelope.timestamp,
                &envelope.body,
                envelope.reply_to,
            ),
            &envelope.signature,
        )
        .map_err(|_| GroupError::BadSignature)
}

impl GroupTicket {
    pub fn encode(&self) -> String {
        let bytes = postcard::to_stdvec(self).expect("a ticket encodes");
        let mut out = String::from(TICKET_PREFIX);
        data_encoding::BASE32_NOPAD.encode_append(&bytes, &mut out);
        out
    }

    pub fn parse(text: &str) -> Result<Self, GroupError> {
        let text = text.trim().to_ascii_lowercase();
        let encoded = text
            .strip_prefix(TICKET_PREFIX)
            .ok_or(GroupError::BadTicket)?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(encoded.to_ascii_uppercase().as_bytes())
            .map_err(|_| GroupError::BadTicket)?;
        postcard::from_bytes(&bytes).map_err(|_| GroupError::BadTicket)
    }
}

/// A new random group id.
pub fn new_group_id() -> GroupId {
    let mut id = [0u8; 32];
    getrandom::fill(&mut id).expect("the system random source is available");
    id
}

/// The stored form of a message's signature.
fn encode_signature(signature: &Signature) -> Vec<u8> {
    postcard::to_stdvec(signature).expect("a signature encodes")
}

fn decode_signature(bytes: &[u8]) -> Option<Signature> {
    postcard::from_bytes(bytes).ok()
}

/// The columns of a stored group message.
type Row = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    String,
    Option<Vec<u8>>,
    Vec<u8>,
);

fn decode_row(row: Row) -> Option<GroupEnvelope> {
    let (id, group, author, timestamp, body, reply_to, signature) = row;
    Some(GroupEnvelope {
        version: GROUP_VERSION,
        group: <GroupId>::try_from(group.as_slice()).ok()?,
        id: <MessageId>::try_from(id.as_slice()).ok()?,
        author: EndpointId::from_bytes(&<[u8; 32]>::try_from(author.as_slice()).ok()?).ok()?,
        timestamp,
        body,
        reply_to: match reply_to {
            Some(bytes) => Some(<MessageId>::try_from(bytes.as_slice()).ok()?),
            None => None,
        },
        signature: decode_signature(&signature)?,
    })
}

const COLUMNS: &str = "id, group_id, author, timestamp, body, reply_to, signature";

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

impl Database {
    /// Records a group this device is in.
    pub fn create_group(&self, group: GroupId, name: &str, now: i64) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO chat_groups (id, name, created) VALUES (?1, ?2, ?3)",
                params![group.as_slice(), name, now],
            )
            .map(|_| ())
        })
    }

    /// Whether this device is in the group.
    pub fn is_in_group(&self, group: &GroupId) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM chat_groups WHERE id = ?1)",
                [group.as_slice()],
                |row| row.get(0),
            )
        })
    }

    /// Every group this device is in, oldest first.
    pub fn groups(&self) -> Result<Vec<GroupInfo>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement =
                conn.prepare("SELECT id, name, created FROM chat_groups ORDER BY created, rowid")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, name, created)| {
                Some(GroupInfo {
                    id: <GroupId>::try_from(id.as_slice()).ok()?,
                    name,
                    created,
                })
            })
            .collect())
    }

    /// Records that an endpoint is a member of a group.
    pub fn add_group_member(&self, group: GroupId, member: &EndpointId) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO chat_members (group_id, endpoint) VALUES (?1, ?2)",
                params![group.as_slice(), member.as_bytes().as_slice()],
            )
            .map(|_| ())
        })
    }

    /// The members of a group that this device knows of.
    pub fn group_members(&self, group: &GroupId) -> Result<Vec<EndpointId>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement =
                conn.prepare("SELECT endpoint FROM chat_members WHERE group_id = ?1")?;
            let rows = statement
                .query_map([group.as_slice()], |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|bytes| {
                EndpointId::from_bytes(&<[u8; 32]>::try_from(bytes.as_slice()).ok()?).ok()
            })
            .collect())
    }

    /// Stores a verified message. Returns `false` if it was stored before.
    pub fn insert_group_message(&self, envelope: &GroupEnvelope) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO group_messages
                    (id, group_id, author, timestamp, body, reply_to, signature)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    envelope.id.as_slice(),
                    envelope.group.as_slice(),
                    envelope.author.as_bytes().as_slice(),
                    envelope.timestamp,
                    envelope.body,
                    envelope.reply_to.as_ref().map(|id| id.as_slice()),
                    encode_signature(&envelope.signature),
                ],
            )
            .map(|changed| changed > 0)
        })
    }

    /// Every message in a group, in order.
    pub fn group_messages(&self, group: &GroupId) -> Result<Vec<GroupMessage>, DbError> {
        Ok(self
            .group_envelopes_after(group, None, usize::MAX)?
            .into_iter()
            .map(|envelope| GroupMessage {
                id: envelope.id,
                group: envelope.group,
                author: envelope.author,
                timestamp: envelope.timestamp,
                body: envelope.body,
                reply_to: envelope.reply_to,
            })
            .collect())
    }

    /// The messages after a position in the group's order, oldest first, at most `limit`.
    /// `None` starts from the first message.
    pub fn group_envelopes_after(
        &self,
        group: &GroupId,
        after: Option<(i64, MessageId)>,
        limit: usize,
    ) -> Result<Vec<GroupEnvelope>, DbError> {
        let (timestamp, id) = after.map_or((i64::MIN, Vec::new()), |(t, id)| (t, id.to_vec()));
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM group_messages
                 WHERE group_id = ?1 AND (timestamp > ?2 OR (timestamp = ?2 AND id > ?3))
                 ORDER BY timestamp, id LIMIT ?4"
            ))?;
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let rows = statement
                .query_map(params![group.as_slice(), timestamp, id, limit], read_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows.into_iter().filter_map(decode_row).collect())
    }

    /// The newest message in a group, as a position to ask for messages after.
    pub fn latest_group_position(
        &self,
        group: &GroupId,
    ) -> Result<Option<(i64, MessageId)>, DbError> {
        let row = self.with_conn(|conn| {
            conn.query_row(
                "SELECT timestamp, id FROM group_messages WHERE group_id = ?1
                 ORDER BY timestamp DESC, id DESC LIMIT 1",
                [group.as_slice()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
        })?;
        Ok(row.and_then(|(timestamp, id)| {
            Some((timestamp, <MessageId>::try_from(id.as_slice()).ok()?))
        }))
    }

    /// The timestamp of an author's latest message in a group, if it has one.
    pub fn latest_author_timestamp(
        &self,
        group: &GroupId,
        author: &EndpointId,
    ) -> Result<Option<i64>, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT MAX(timestamp) FROM group_messages WHERE group_id = ?1 AND author = ?2",
                params![group.as_slice(), author.as_bytes().as_slice()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map(Option::flatten)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(secret: &SecretKey, body: &str) -> GroupEnvelope {
        sign(secret, [7; 32], [1; 16], 10, body.to_owned(), None)
    }

    #[test]
    fn a_signed_message_verifies_and_a_changed_one_does_not() {
        let alice = SecretKey::generate();
        let message = envelope(&alice, "hello");
        assert_eq!(verify(&message), Ok(()));

        let mut changed = message.clone();
        changed.body = "goodbye".to_owned();
        assert_eq!(verify(&changed), Err(GroupError::BadSignature));

        // Claiming someone else wrote it does not verify either.
        let mut forged = message;
        forged.author = SecretKey::generate().public();
        assert_eq!(verify(&forged), Err(GroupError::BadSignature));
    }

    #[test]
    fn a_ticket_round_trips_and_other_text_is_refused() {
        let ticket = GroupTicket {
            group: [3; 32],
            peers: vec![EndpointAddr::from(SecretKey::generate().public())],
        };
        let text = ticket.encode();
        assert!(text.starts_with(TICKET_PREFIX));
        assert_eq!(GroupTicket::parse(&text), Ok(ticket));
        assert_eq!(
            GroupTicket::parse("groovylink1abc"),
            Err(GroupError::BadTicket)
        );
        assert_eq!(
            GroupTicket::parse("groovygroup1!!"),
            Err(GroupError::BadTicket)
        );
    }

    #[test]
    fn messages_are_stored_once_and_paged_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("groovy.db")).unwrap();
        let alice = SecretKey::generate();
        db.create_group([7; 32], "friends", 1).unwrap();

        let first = sign(&alice, [7; 32], [2; 16], 20, "one".into(), None);
        let second = sign(&alice, [7; 32], [1; 16], 10, "two".into(), None);

        assert!(db.insert_group_message(&first).unwrap());
        assert!(db.insert_group_message(&second).unwrap());
        assert!(!db.insert_group_message(&second).unwrap());

        let bodies: Vec<String> = db
            .group_messages(&[7; 32])
            .unwrap()
            .into_iter()
            .map(|m| m.body)
            .collect();
        assert_eq!(bodies, ["two", "one"]);
        let after_two = db
            .group_envelopes_after(&[7; 32], Some((10, [1; 16])), 8)
            .unwrap();
        assert_eq!(after_two.len(), 1);
        assert_eq!(after_two[0].body, "one");
        assert_eq!(after_two[0].signature, first.signature);
        assert_eq!(
            db.latest_group_position(&[7; 32]).unwrap(),
            Some((20, [2; 16]))
        );
        assert!(db.is_in_group(&[7; 32]).unwrap());
    }
}
