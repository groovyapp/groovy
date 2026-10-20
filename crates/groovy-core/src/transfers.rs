//! Transfers: the accepted file offers this node is receiving, with their state.
//!
//! Each transfer is kept in the database, so the list survives a restart. Queued
//! and active transfers are picked up again at startup, and a transfer that was
//! paused keeps the bytes it already had in the blob store.

use iroh::EndpointId;
use rusqlite::params;

use crate::db::{Database, DbError};
use crate::protocol::FileOffer;

/// Where a transfer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    /// Accepted, and waiting for a free slot.
    Queued,
    /// Receiving now.
    Active,
    /// Stopped by the user. Resuming it fetches only what is missing.
    Paused,
    /// Every file is in the store, and saved if there is a downloads folder.
    Completed,
    /// Stopped by an error. The message says what went wrong.
    Failed(String),
    /// Stopped by the user, and not expected to resume.
    Cancelled,
}

impl TransferState {
    fn to_row(&self) -> (&'static str, Option<&str>) {
        match self {
            TransferState::Queued => ("queued", None),
            TransferState::Active => ("active", None),
            TransferState::Paused => ("paused", None),
            TransferState::Completed => ("completed", None),
            TransferState::Failed(error) => ("failed", Some(error)),
            TransferState::Cancelled => ("cancelled", None),
        }
    }

    fn from_row(state: &str, error: Option<String>) -> Option<Self> {
        Some(match state {
            "queued" => TransferState::Queued,
            "active" => TransferState::Active,
            "paused" => TransferState::Paused,
            "completed" => TransferState::Completed,
            "failed" => TransferState::Failed(error.unwrap_or_default()),
            "cancelled" => TransferState::Cancelled,
            _ => return None,
        })
    }

    /// Whether the transfer is still waiting to finish, and so starts again at startup.
    pub fn is_unfinished(&self) -> bool {
        matches!(self, TransferState::Queued | TransferState::Active)
    }
}

/// Whether a transfer brings files to this device or takes them away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Files this device receives.
    Receive,
    /// Files this device offers to a peer.
    Send,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Receive => "receive",
            Direction::Send => "send",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "receive" => Some(Direction::Receive),
            "send" => Some(Direction::Send),
            _ => None,
        }
    }
}

/// One accepted offer, and how far it has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub id: i64,
    pub direction: Direction,
    /// The other side: who the files come from, or who they go to.
    pub peer: EndpointId,
    pub offer: FileOffer,
    pub state: TransferState,
    /// Payload bytes received so far, out of `offer.bytes`.
    pub done: u64,
    /// Unix seconds.
    pub created: i64,
    /// Unix seconds, of the last change.
    pub updated: i64,
}

impl Database {
    /// Records a new transfer. Its id is chosen by the caller.
    pub fn insert_transfer(&self, transfer: &Transfer) -> Result<(), DbError> {
        let encoded = encode_offer(&transfer.offer)?;
        let (state, error) = transfer.state.to_row();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO transfers (id, peer, offer, state, error, done, created, updated, direction)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    transfer.id,
                    transfer.peer.as_bytes().as_slice(),
                    encoded,
                    state,
                    error,
                    transfer.done as i64,
                    transfer.created,
                    transfer.updated,
                    transfer.direction.as_str(),
                ],
            )
            .map(|_| ())
        })
    }

    /// Records a transfer's state and how many bytes it has received.
    pub fn update_transfer(
        &self,
        id: i64,
        state: &TransferState,
        done: u64,
        now: i64,
    ) -> Result<(), DbError> {
        let (state, error) = state.to_row();
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE transfers SET state = ?2, error = ?3, done = ?4, updated = ?5 WHERE id = ?1",
                params![id, state, error, done as i64, now],
            )
            .map(|_| ())
        })
    }

    /// Deletes a transfer from the history. Its content is released by the next collection,
    /// unless something else keeps it.
    pub fn delete_transfer(&self, id: i64) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM transfers WHERE id = ?1", [id])
                .map(|_| ())
        })
    }

    /// Every transfer, oldest first.
    pub fn transfers(&self) -> Result<Vec<Transfer>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, peer, offer, state, error, done, created, updated, direction
                 FROM transfers ORDER BY id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;

        let mut transfers = Vec::with_capacity(rows.len());
        for (id, peer, offer, state, error, done, created, updated, direction) in rows {
            // A row that does not decode is skipped rather than failing the whole list.
            let Some(state) = TransferState::from_row(&state, error) else {
                continue;
            };
            let Some(direction) = Direction::from_str(&direction) else {
                continue;
            };
            let Ok(peer_key) = <[u8; 32]>::try_from(peer.as_slice()) else {
                continue;
            };
            let Ok(peer) = EndpointId::from_bytes(&peer_key) else {
                continue;
            };
            let Ok(offer) = postcard::from_bytes::<FileOffer>(&offer) else {
                continue;
            };
            transfers.push(Transfer {
                id,
                direction,
                peer,
                offer,
                state,
                done: done as u64,
                created,
                updated,
            });
        }
        Ok(transfers)
    }
}

fn encode_offer(offer: &FileOffer) -> Result<Vec<u8>, DbError> {
    postcard::to_stdvec(offer)
        .map_err(|error| DbError::Query(rusqlite::Error::ToSqlConversionFailure(Box::new(error))))
}
