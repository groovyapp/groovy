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
use iroh_blobs::Hash;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::db::{Database, DbError};

/// The version of [`WireMessage`]. A message in another version is refused.
pub const MESSAGE_VERSION: u16 = 2;
/// Longest body accepted, in bytes. Well under the protocol's message limit.
pub const MAX_BODY_BYTES: usize = 4 * 1024;
/// Most attachments a message carries.
pub const MAX_ATTACHMENTS: usize = 8;
/// Largest preview an attachment carries, in bytes. A larger one is left out.
pub const MAX_PREVIEW_BYTES: usize = 32 * 1024;
/// Longest name an attachment can have, in bytes.
pub const MAX_NAME_BYTES: usize = 255;

/// A file sent with a message. The file is fetched from the sender on request; the preview, if
/// there is one, comes with the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// The file's name, as the sender had it.
    pub name: String,
    /// The root of the collection holding the file, which the recipient fetches.
    pub root: Hash,
    /// How many files the collection holds. Always one for an attachment.
    pub files: u64,
    /// The file's size, in bytes.
    pub bytes: u64,
    /// A small PNG of an image, so the message can show it before the file is fetched.
    pub preview: Option<Vec<u8>>,
}

/// The random identifier of a message.
pub type MessageId = [u8; 16];

/// Where a message is in its delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    /// Waiting to be sent, or being sent. Not yet written to any of the contact's devices.
    Sending,
    /// Written to one of the contact's devices, which has not acknowledged it yet.
    Sent,
    /// The contact stored it and acknowledged it. Also the state of every message received.
    Delivered,
    /// Could not be delivered. Retrying is handled by the outbox.
    Failed,
}

impl DeliveryState {
    fn as_str(self) -> &'static str {
        match self {
            DeliveryState::Sending => "sending",
            DeliveryState::Sent => "sent",
            DeliveryState::Delivered => "delivered",
            DeliveryState::Failed => "failed",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "sending" => Some(DeliveryState::Sending),
            "sent" => Some(DeliveryState::Sent),
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
    /// Files sent with the message, in the order they were attached.
    pub attachments: Vec<Attachment>,
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
    pub attachments: Vec<Attachment>,
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
    #[error("a message can carry at most {MAX_ATTACHMENTS} files")]
    TooManyAttachments,
    #[error("an attachment needs a name")]
    NameMissing,
}

/// Checks a message's attachments: how many, their names, and that a preview is not too big.
/// A preview that is too big is left out, and the attachment kept.
pub fn clean_attachments(attachments: &[Attachment]) -> Result<Vec<Attachment>, MessageError> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(MessageError::TooManyAttachments);
    }
    attachments
        .iter()
        .map(|attachment| {
            let name = attachment.name.trim();
            if name.is_empty() {
                return Err(MessageError::NameMissing);
            }
            if name.len() > MAX_NAME_BYTES {
                return Err(MessageError::TooLong);
            }
            Ok(Attachment {
                name: name.to_owned(),
                preview: attachment
                    .preview
                    .clone()
                    .filter(|preview| preview.len() <= MAX_PREVIEW_BYTES),
                ..attachment.clone()
            })
        })
        .collect()
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

/// The columns of a message, as read from the table, before they are checked.
type Row = (Vec<u8>, Vec<u8>, i64, String, Option<Vec<u8>>, String);

const SELECT: &str = "SELECT id, author, timestamp, body, reply_to, state FROM messages";

/// Turns a stored row into a message. A row that does not decode is left out, rather than
/// failing the whole conversation.
fn decode(conversation: EndpointId, row: Row) -> Option<Message> {
    let (id, author, timestamp, body, reply_to, state) = row;
    let id = <MessageId>::try_from(id.as_slice()).ok()?;
    let author = EndpointId::from_bytes(&<[u8; 32]>::try_from(author.as_slice()).ok()?).ok()?;
    let state = DeliveryState::from_str(&state)?;
    let reply_to = match reply_to {
        Some(bytes) => Some(<MessageId>::try_from(bytes.as_slice()).ok()?),
        None => None,
    };
    Some(Message {
        id,
        conversation,
        author,
        timestamp,
        body,
        reply_to,
        state,
        attachments: Vec::new(),
    })
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

impl Database {
    /// Stores a message. Returns `false` if a message with the same id is already stored, as
    /// when a sender resends one.
    pub fn insert_message(&self, message: &Message) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            let changed = conn.execute(
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
            )?;
            if changed > 0 {
                for (position, attachment) in message.attachments.iter().enumerate() {
                    conn.execute(
                        "INSERT INTO message_attachments
                            (message, position, name, root, files, bytes, preview)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            id_bytes(&message.id),
                            position as i64,
                            attachment.name,
                            attachment.root.as_bytes().as_slice(),
                            attachment.files as i64,
                            attachment.bytes as i64,
                            attachment.preview,
                        ],
                    )?;
                }
            }
            Ok(changed > 0)
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

    /// Marks a message as written to a device, unless it has already been acknowledged.
    pub fn mark_sent(&self, id: MessageId) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE messages SET state = 'sent' WHERE id = ?1 AND state = 'sending'",
                params![id_bytes(&id)],
            )
            .map(|changed| changed > 0)
        })
    }

    /// The attachments of a message, in the order they were attached.
    fn attachments_of(&self, id: &MessageId) -> Result<Vec<Attachment>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT name, root, files, bytes, preview FROM message_attachments
                 WHERE message = ?1 ORDER BY position",
            )?;
            let rows = statement
                .query_map([id_bytes(id)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(name, root, files, bytes, preview)| {
                Some(Attachment {
                    name,
                    root: Hash::from_bytes(<[u8; 32]>::try_from(root.as_slice()).ok()?),
                    files: u64::try_from(files).ok()?,
                    bytes: u64::try_from(bytes).ok()?,
                    preview,
                })
            })
            .collect())
    }

    /// One message by id, if it is stored.
    pub fn message(&self, id: MessageId) -> Result<Option<Message>, DbError> {
        let found = self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, author, timestamp, body, reply_to, state, conversation
                 FROM messages WHERE id = ?1",
                [id_bytes(&id)],
                |r| Ok((row(r)?, r.get::<_, Vec<u8>>(6)?)),
            )
            .optional()
        })?;
        let Some((row, conversation)) = found else {
            return Ok(None);
        };
        let Ok(conversation) = <[u8; 32]>::try_from(conversation.as_slice())
            .map_err(|_| ())
            .and_then(|bytes| EndpointId::from_bytes(&bytes).map_err(|_| ()))
        else {
            return Ok(None);
        };
        let Some(mut message) = decode(conversation, row) else {
            return Ok(None);
        };
        message.attachments = self.attachments_of(&message.id)?;
        Ok(Some(message))
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
            let mut statement = conn.prepare(&format!("{SELECT} WHERE conversation = ?1"))?;
            let rows = statement
                .query_map([conversation.as_bytes().as_slice()], row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut messages: Vec<Message> = rows
            .into_iter()
            .filter_map(|row| decode(*conversation, row))
            .collect();
        for message in &mut messages {
            message.attachments = self.attachments_of(&message.id)?;
        }
        order(&mut messages);
        Ok(messages)
    }
}

/// Where a search hit was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    /// A direct message, in the conversation with this contact.
    Direct(EndpointId),
    /// A group message, in this group.
    Group([u8; 32]),
}

/// A message that matches a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub kind: HitKind,
    pub id: MessageId,
    pub timestamp: i64,
    pub body: String,
}

/// Where to continue reading a conversation from: the message to read before.
pub type Cursor = (i64, MessageId);

/// A search expression for the words typed: each word must match, and a word matches any word
/// that starts with it. Returns `None` when there are no words.
pub fn match_expression(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

impl Database {
    /// Messages in a conversation that match a search, newest first. Covers direct messages
    /// and group messages.
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, DbError> {
        let Some(expression) = match_expression(query) else {
            return Ok(Vec::new());
        };
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut hits = self
            .with_conn(|conn| {
                let mut statement = conn.prepare(
                    "SELECT m.id, m.conversation, m.timestamp, m.body FROM message_search s
                 JOIN messages m ON m.rowid = s.rowid
                 WHERE message_search MATCH ?1
                 ORDER BY m.timestamp DESC LIMIT ?2",
                )?;
                let rows = statement
                    .query_map(params![expression, limit], |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })?
            .into_iter()
            .filter_map(|(id, conversation, timestamp, body)| {
                Some(SearchHit {
                    kind: HitKind::Direct(
                        EndpointId::from_bytes(
                            &<[u8; 32]>::try_from(conversation.as_slice()).ok()?,
                        )
                        .ok()?,
                    ),
                    id: <MessageId>::try_from(id.as_slice()).ok()?,
                    timestamp,
                    body,
                })
            })
            .collect::<Vec<_>>();

        let group_rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT g.id, g.group_id, g.timestamp, g.body FROM group_search s
                 JOIN group_messages g ON g.rowid = s.rowid
                 WHERE group_search MATCH ?1
                 ORDER BY g.timestamp DESC LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![expression, limit], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        hits.extend(
            group_rows
                .into_iter()
                .filter_map(|(id, group, timestamp, body)| {
                    Some(SearchHit {
                        kind: HitKind::Group(<[u8; 32]>::try_from(group.as_slice()).ok()?),
                        id: <MessageId>::try_from(id.as_slice()).ok()?,
                        timestamp,
                        body,
                    })
                }),
        );
        hits.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.id.cmp(&a.id)));
        hits.truncate(limit as usize);
        Ok(hits)
    }

    /// Up to `limit` messages of a conversation before `before`, in conversation order. With no
    /// cursor it starts from the newest message, so the first page is the latest messages.
    pub fn messages_before(
        &self,
        conversation: &EndpointId,
        before: Option<Cursor>,
        limit: usize,
    ) -> Result<Vec<Message>, DbError> {
        let (timestamp, id) = before.unwrap_or((i64::MAX, [0xff; 16]));
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(&format!(
                "{SELECT} WHERE conversation = ?1
                   AND (timestamp < ?2 OR (timestamp = ?2 AND id < ?3))
                 ORDER BY timestamp DESC, id DESC LIMIT ?4"
            ))?;
            let rows = statement
                .query_map(
                    params![
                        conversation.as_bytes().as_slice(),
                        timestamp,
                        id_bytes(&id),
                        limit
                    ],
                    row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut messages: Vec<Message> = rows
            .into_iter()
            .filter_map(|row| decode(*conversation, row))
            .collect();
        for message in &mut messages {
            message.attachments = self.attachments_of(&message.id)?;
        }
        // Newest first was the order read; give the page in conversation order.
        messages.reverse();
        Ok(messages)
    }

    /// Deletes a message from this device, with its attachments and any delivery still waiting
    /// for it. Returns whether a message was deleted. This does not delete it for the contact.
    pub fn delete_message(&self, id: MessageId) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM outbox WHERE message = ?1", [id_bytes(&id)])?;
            conn.execute(
                "DELETE FROM message_attachments WHERE message = ?1",
                [id_bytes(&id)],
            )?;
            let deleted = conn.execute("DELETE FROM messages WHERE id = ?1", [id_bytes(&id)])?;
            Ok(deleted > 0)
        })
    }

    /// Deletes every message in a conversation from this device. Other devices keep their copies.
    pub fn clear_conversation(&self, conversation: &EndpointId) -> Result<usize, DbError> {
        let key = conversation.as_bytes().as_slice();
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM outbox WHERE message IN
                    (SELECT id FROM messages WHERE conversation = ?1)",
                [key],
            )?;
            conn.execute(
                "DELETE FROM message_attachments WHERE message IN
                    (SELECT id FROM messages WHERE conversation = ?1)",
                [key],
            )?;
            conn.execute("DELETE FROM messages WHERE conversation = ?1", [key])
        })
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
            attachments: Vec::new(),
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
            attachments: Vec::new(),
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
