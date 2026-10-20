//! Direct messages between two trusted contacts.
//!
//! A message is sent over the Groovy protocol to the contact, who stores it and acknowledges
//! it. The sender keeps its own copy with a delivery state: `sending` while it is in flight,
//! `delivered` once the contact has acknowledged it, and `failed` when it could not be sent.
//!
//! The author is the connection's peer, not a name written in the message: the connection
//! is already authenticated by the endpoint key, so a message cannot claim another author.
//!
//! Ordering is by `(timestamp, id)`. The timestamp is the sender's clock in milliseconds. A
//! sender never gives its own messages a timestamp earlier than its previous message in the
//! conversation, so its own order holds even if its clock steps back. A clock that runs ahead
//! or behind moves a message's place in the conversation, but never loses or duplicates it,
//! and both sides order the messages the same way.

use std::time::{SystemTime, UNIX_EPOCH};

use iroh::EndpointId;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::db::{Database, DbError};

/// The version of [`WireMessage`]. A message in another version is refused.
pub const MESSAGE_VERSION: u16 = 1;
/// Longest body accepted, in bytes. Well under the protocol's message limit.
pub const MAX_BODY_BYTES: usize = 4 * 1024;

/// The random identifier of a message.
pub type MessageId = [u8; 16];

/// Where a message is in its delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    /// Being sent. The contact has not acknowledged it yet.
    Sending,
    /// The contact stored it and acknowledged it. Also the state of every message received.
    Delivered,
    /// Could not be delivered. Retrying is handled by the outbox.
    Failed,
}

impl DeliveryState {
    fn as_str(self) -> &'static str {
        match self {
            DeliveryState::Sending => "sending",
            DeliveryState::Delivered => "delivered",
            DeliveryState::Failed => "failed",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "sending" => Some(DeliveryState::Sending),
            "delivered" => Some(DeliveryState::Delivered),
            "failed" => Some(DeliveryState::Failed),
            _ => None,
        }
    }
}

/// A message in a conversation, as this device has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    /// The contact on the other side of the conversation.
    pub conversation: EndpointId,
    /// Who wrote the message. This device's own messages have this device as the author.
    pub author: EndpointId,
    /// Milliseconds since the Unix epoch, on the author's clock.
    pub timestamp: i64,
    pub body: String,
    pub reply_to: Option<MessageId>,
    pub state: DeliveryState,
}

impl Message {
    /// Whether this device wrote the message, given its own id.
    pub fn is_outgoing(&self, own: &EndpointId) -> bool {
        self.author == *own
    }
}

/// A message as it travels between two devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireMessage {
    pub version: u16,
    pub id: MessageId,
    pub timestamp: i64,
    pub body: String,
    pub reply_to: Option<MessageId>,
}

/// Why a message is refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MessageError {
    #[error("a message needs some text")]
    Empty,
    #[error("a message can be at most {MAX_BODY_BYTES} bytes")]
    TooLong,
    #[error("this message is in a version of Groovy messages this build does not read")]
    Version,
}

/// Checks a body and returns it without surrounding whitespace.
pub fn clean_body(body: &str) -> Result<String, MessageError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(MessageError::Empty);
    }
    if trimmed.len() > MAX_BODY_BYTES {
        return Err(MessageError::TooLong);
    }
    Ok(trimmed.to_owned())
}

/// A new random message id.
pub fn new_id() -> MessageId {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("the system random source is available");
    id
}

/// The time now, in milliseconds since the Unix epoch.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Puts messages in conversation order: by timestamp, then by id so ties come out the same on
/// every device.
pub fn order(messages: &mut [Message]) {
    messages.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then(a.id.cmp(&b.id)));
}

fn id_bytes(id: &MessageId) -> &[u8] {
    id.as_slice()
}

impl Database {
    /// Stores a message. Returns `false` if a message with the same id is already stored, as
    /// when a sender resends one.
    pub fn insert_message(&self, message: &Message) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO messages
                    (id, conversation, author, timestamp, body, reply_to, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id_bytes(&message.id),
                    message.conversation.as_bytes().as_slice(),
                    message.author.as_bytes().as_slice(),
                    message.timestamp,
                    message.body,
                    message.reply_to.as_ref().map(id_bytes),
                    message.state.as_str(),
                ],
            )
            .map(|changed| changed > 0)
        })
    }

    /// Sets a message's delivery state.
    pub fn set_message_state(&self, id: MessageId, state: DeliveryState) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE messages SET state = ?2 WHERE id = ?1",
                params![id_bytes(&id), state.as_str()],
            )
            .map(|changed| changed > 0)
        })
    }

    /// The timestamp of this device's latest message in a conversation, if it has one.
    pub fn last_sent_timestamp(
        &self,
        conversation: &EndpointId,
        own: &EndpointId,
    ) -> Result<Option<i64>, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT MAX(timestamp) FROM messages WHERE conversation = ?1 AND author = ?2",
                params![
                    conversation.as_bytes().as_slice(),
                    own.as_bytes().as_slice()
                ],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map(Option::flatten)
        })
    }

    /// Every message in a conversation, in conversation order.
    pub fn messages(&self, conversation: &EndpointId) -> Result<Vec<Message>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, author, timestamp, body, reply_to, state FROM messages
                 WHERE conversation = ?1",
            )?;
            let rows = statement
                .query_map([conversation.as_bytes().as_slice()], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;

        let mut messages = Vec::with_capacity(rows.len());
        for (id, author, timestamp, body, reply_to, state) in rows {
            // A row that does not decode is skipped rather than failing the whole conversation.
            let (Ok(id), Ok(author), Some(state)) = (
                <MessageId>::try_from(id.as_slice()),
                <[u8; 32]>::try_from(author.as_slice()),
                DeliveryState::from_str(&state),
            ) else {
                continue;
            };
            let Ok(author) = EndpointId::from_bytes(&author) else {
                continue;
            };
            let reply_to = match reply_to {
                Some(bytes) => match <MessageId>::try_from(bytes.as_slice()) {
                    Ok(bytes) => Some(bytes),
                    Err(_) => continue,
                },
                None => None,
            };
            messages.push(Message {
                id,
                conversation: *conversation,
                author,
                timestamp,
                body,
                reply_to,
                state,
            });
        }
        order(&mut messages);
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn message(author: EndpointId, timestamp: i64, id: MessageId) -> Message {
        Message {
            id,
            conversation: author,
            author,
            timestamp,
            body: "hi".to_owned(),
            reply_to: None,
            state: DeliveryState::Delivered,
        }
    }

    #[test]
    fn bodies_are_trimmed_and_blank_or_long_ones_refused() {
        assert_eq!(clean_body("  hello \n").unwrap(), "hello");
        assert_eq!(clean_body(" \n\t "), Err(MessageError::Empty));
        let long = "x".repeat(MAX_BODY_BYTES + 1);
        assert_eq!(clean_body(&long), Err(MessageError::TooLong));
        assert!(clean_body(&"x".repeat(MAX_BODY_BYTES)).is_ok());
    }

    #[test]
    fn ids_are_random_and_distinct() {
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn messages_order_by_timestamp_then_id() {
        let who = SecretKey::generate().public();
        let mut messages = vec![
            message(who, 30, [1; 16]),
            message(who, 10, [9; 16]),
            message(who, 10, [2; 16]),
        ];
        order(&mut messages);
        let ids: Vec<MessageId> = messages.iter().map(|m| m.id).collect();
        assert_eq!(ids, [[2; 16], [9; 16], [1; 16]]);
    }

    #[test]
    fn a_wire_message_round_trips_through_postcard() {
        let wire = WireMessage {
            version: MESSAGE_VERSION,
            id: [4; 16],
            timestamp: 1_700_000_000_000,
            body: "hello".to_owned(),
            reply_to: Some([5; 16]),
        };
        let bytes = postcard::to_allocvec(&wire).unwrap();
        let decoded: WireMessage = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, wire);
    }

    #[test]
    fn a_stored_message_comes_back_in_order_with_its_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("groovy.db")).unwrap();
        let me = SecretKey::generate().public();
        let them = SecretKey::generate().public();
        let mut sent = message(me, 20, [2; 16]);
        sent.conversation = them;
        sent.state = DeliveryState::Sending;
        let mut received = message(them, 10, [1; 16]);
        received.conversation = them;

        assert!(db.insert_message(&sent).unwrap());
        assert!(db.insert_message(&received).unwrap());
        // The same id again is not stored twice.
        assert!(!db.insert_message(&received).unwrap());
        db.set_message_state([2; 16], DeliveryState::Delivered)
            .unwrap();

        let listed = db.messages(&them).unwrap();
        let ids: Vec<MessageId> = listed.iter().map(|m| m.id).collect();
        assert_eq!(ids, [[1; 16], [2; 16]]);
        assert_eq!(listed[1].state, DeliveryState::Delivered);
        assert!(listed[1].is_outgoing(&me));
        assert_eq!(db.last_sent_timestamp(&them, &me).unwrap(), Some(20));
        assert_eq!(db.last_sent_timestamp(&them, &them).unwrap(), Some(10));
    }
}
