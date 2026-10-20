//! The outbox: messages waiting for a contact's device to take them.
//!
//! When a message is sent, a row is kept for each device it must reach: the contact's own
//! endpoint, and each of the contact's linked devices that has not been revoked. A row goes
//! when its device acknowledges the message. Until then the message is retried:
//!
//! - when the device connects to this node, in either direction, at once;
//! - every minute, for rows whose retry time has come, which backs off from 30 seconds to 15
//!   minutes after each failure;
//! - at startup, since the rows are in the database and survive a restart.
//!
//! A device is dialed at the address it last connected from, and by its id otherwise, so it
//! is found again when it comes back at a new address. A message that no device has taken
//! within seven days fails, and its rows are dropped.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use iroh::endpoint::Connection;
use iroh::{EndpointAddr, EndpointId, PublicKey, TransportAddr};
use rusqlite::{OptionalExtension, params};
use tracing::warn;

use crate::db::{Database, DbError};
use crate::messages::{self, MESSAGE_VERSION, Message, MessageId, WireMessage};
use crate::node::PeerGate;
use crate::protocol::{self, ProtocolError, Reply, Request};

/// The first wait after a failed delivery. It doubles after each failure.
const RETRY_FIRST_MS: i64 = 30_000;
/// The longest wait between attempts.
const RETRY_MAX_MS: i64 = 15 * 60_000;
/// How long a message is kept trying before it fails.
const EXPIRY_MS: i64 = 7 * 24 * 3_600_000;
/// How long a device must wait between flushes that a connection starts. This stops two devices
/// that both have messages for each other from dialing each other in a loop.
const COOLDOWN: Duration = Duration::from_secs(1);

/// What started a flush to a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The user sent a message. Tries at once, whatever came before.
    Send,
    /// The device connected to this node. Tries at once, but not more than once a second.
    Connected,
    /// The periodic check. Tries only the rows whose retry time has come.
    Tick,
}

/// Wait before the next attempt, after `failures` failed attempts.
fn backoff_ms(failures: i64) -> i64 {
    let shift = failures.clamp(0, 20) as u32;
    (RETRY_FIRST_MS << shift).min(RETRY_MAX_MS)
}

impl Database {
    /// Queues a message for each of `devices`. A device that already has the message keeps its
    /// row.
    pub fn enqueue(&self, message: MessageId, devices: &[EndpointId]) -> Result<(), DbError> {
        self.with_conn(|conn| {
            for device in devices {
                conn.execute(
                    "INSERT OR IGNORE INTO outbox (message, device) VALUES (?1, ?2)",
                    params![message.as_slice(), device.as_bytes().as_slice()],
                )?;
            }
            Ok(())
        })
    }

    /// The messages a device still has to take, oldest first. With `ignore_backoff`, every
    /// message is included, not only those whose retry time has come.
    pub fn queued_for(
        &self,
        device: &EndpointId,
        now: i64,
        ignore_backoff: bool,
    ) -> Result<Vec<MessageId>, DbError> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT o.message FROM outbox o JOIN messages m ON m.id = o.message
                 WHERE o.device = ?1 AND (?2 = 1 OR o.next_attempt <= ?3)
                 ORDER BY m.timestamp, m.id",
            )?;
            let rows = statement
                .query_map(
                    params![device.as_bytes().as_slice(), i64::from(ignore_backoff), now],
                    |row| row.get::<_, Vec<u8>>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|bytes| <MessageId>::try_from(bytes.as_slice()).ok())
                .collect())
        })
    }

    /// The devices with a message whose retry time has come.
    pub fn devices_due(&self, now: i64) -> Result<Vec<EndpointId>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement =
                conn.prepare("SELECT DISTINCT device FROM outbox WHERE next_attempt <= ?1")?;
            let rows = statement
                .query_map([now], |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
            .filter_map(|bytes| EndpointId::from_bytes(&bytes).ok())
            .collect())
    }

    /// Records that a device took a message: its row goes, and the message is delivered.
    pub fn acknowledge(&self, message: MessageId, device: &EndpointId) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM outbox WHERE message = ?1 AND device = ?2",
                params![message.as_slice(), device.as_bytes().as_slice()],
            )?;
            conn.execute(
                "UPDATE messages SET state = 'delivered' WHERE id = ?1",
                params![message.as_slice()],
            )?;
            Ok(())
        })
    }

    /// Records a failed attempt to a device, and when to try again.
    pub fn record_failure(
        &self,
        message: MessageId,
        device: &EndpointId,
        error: &str,
        now: i64,
    ) -> Result<(), DbError> {
        self.with_conn(|conn| {
            let failures: i64 = conn
                .query_row(
                    "SELECT attempts FROM outbox WHERE message = ?1 AND device = ?2",
                    params![message.as_slice(), device.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            conn.execute(
                "UPDATE outbox SET attempts = attempts + 1, last_error = ?3, next_attempt = ?4
                 WHERE message = ?1 AND device = ?2",
                params![
                    message.as_slice(),
                    device.as_bytes().as_slice(),
                    error,
                    now + backoff_ms(failures)
                ],
            )?;
            Ok(())
        })
    }

    /// Fails messages that no device has taken within the expiry time, and drops their rows.
    /// Returns how many messages failed.
    pub fn expire_outbox(&self, now: i64) -> Result<usize, DbError> {
        let cutoff = now - EXPIRY_MS;
        self.with_conn(|conn| {
            let failed = conn.execute(
                "UPDATE messages SET state = 'failed'
                 WHERE timestamp < ?1 AND state != 'delivered'
                   AND id IN (SELECT message FROM outbox)",
                [cutoff],
            )?;
            conn.execute(
                "DELETE FROM outbox WHERE message IN
                    (SELECT id FROM messages WHERE timestamp < ?1)",
                [cutoff],
            )?;
            Ok(failed)
        })
    }

    /// Every endpoint a message to `contact` must reach: the contact's own endpoint, and each
    /// of the contact's linked devices that is not revoked.
    pub fn contact_devices(&self, contact: &EndpointId) -> Result<Vec<EndpointId>, DbError> {
        let mut devices = vec![*contact];
        let user: Option<Vec<u8>> = self.with_conn(|conn| {
            conn.query_row(
                "SELECT user FROM contacts WHERE endpoint_id = ?1",
                [contact.as_bytes().as_slice()],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map(Option::flatten)
        })?;
        if let Some(bytes) = user
            && let Ok(bytes) = <[u8; 32]>::try_from(bytes.as_slice())
            && let Ok(user) = PublicKey::from_bytes(&bytes)
        {
            for device in self.devices_of(&user)? {
                if device.revoked.is_none() && !devices.contains(&device.cert.device) {
                    devices.push(device.cert.device);
                }
            }
        }
        Ok(devices)
    }

    /// Remembers an address a device was reached at. An address with no transport addresses
    /// is not kept.
    pub fn remember_address(&self, device: EndpointId, addr: &EndpointAddr) -> Result<(), DbError> {
        if addr.addrs.is_empty() {
            return Ok(());
        }
        let bytes = postcard::to_stdvec(addr).map_err(|error| {
            DbError::Query(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO device_addrs (device, addr) VALUES (?1, ?2)",
                params![device.as_bytes().as_slice(), bytes],
            )
            .map(|_| ())
        })
    }

    /// Every address kept, so the endpoint can reach those devices by id.
    pub fn all_addresses(&self) -> Result<Vec<EndpointAddr>, DbError> {
        let rows: Vec<Vec<u8>> = self.with_conn(|conn| {
            let mut statement = conn.prepare("SELECT addr FROM device_addrs")?;
            let rows = statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|bytes| postcard::from_bytes::<EndpointAddr>(&bytes).ok())
            .collect())
    }

    /// The last address a device was reached at, if one is kept.
    pub fn address_of(&self, device: &EndpointId) -> Result<Option<EndpointAddr>, DbError> {
        let bytes: Option<Vec<u8>> = self.with_conn(|conn| {
            conn.query_row(
                "SELECT addr FROM device_addrs WHERE device = ?1",
                [device.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
        })?;
        Ok(bytes.and_then(|bytes| postcard::from_bytes::<EndpointAddr>(&bytes).ok()))
    }
}

/// The address a connection's peer is reachable at now: every address its paths use.
pub fn address_of_connection(connection: &Connection) -> EndpointAddr {
    let addrs: BTreeSet<TransportAddr> = connection
        .paths()
        .iter()
        .map(|path| path.remote_addr().clone())
        .collect();
    EndpointAddr {
        id: connection.remote_id(),
        addrs,
    }
}

/// Writes one message on a connection, and waits for the device to store it. `on_written` runs
/// once the message is written, before the device answers.
pub async fn deliver_on(
    connection: &Connection,
    wire: &WireMessage,
    on_written: impl FnOnce(),
) -> Result<(), ProtocolError> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| ProtocolError::Connection(Box::new(error)))?;
    protocol::write_message(&mut send, &Request::Message(wire.clone())).await?;
    on_written();
    let reply: Reply = protocol::read_message(&mut recv).await?;
    match reply {
        Reply::Stored => Ok(()),
        _ => Err(ProtocolError::Closed),
    }
}

/// Per-device bookkeeping for flushes.
#[derive(Default)]
struct Slot {
    running: bool,
    finished: Option<Instant>,
}

/// The outbox of a node: the database, the gate that decides who is trusted, and which devices
/// are being flushed right now.
#[derive(Clone)]
pub struct Outbox {
    database: Arc<Database>,
    gate: Arc<dyn PeerGate>,
    slots: Arc<Mutex<HashMap<EndpointId, Slot>>>,
}

impl Outbox {
    pub fn new(database: Arc<Database>, gate: Arc<dyn PeerGate>) -> Self {
        Self {
            database,
            gate,
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Whether the device may be flushed to now. Only one flush runs for a device at a time.
    /// A connection-started flush waits out the cooldown after the last one.
    pub fn claim(&self, device: &EndpointId, trigger: Trigger) -> bool {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = slots.entry(*device).or_default();
        if slot.running {
            return false;
        }
        let cooling = slot
            .finished
            .is_some_and(|finished| finished.elapsed() < COOLDOWN);
        if trigger == Trigger::Connected && cooling {
            return false;
        }
        slot.running = true;
        true
    }

    /// Ends a flush started by [`Outbox::claim`].
    pub fn release(&self, device: &EndpointId) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = slots.entry(*device).or_default();
        slot.running = false;
        slot.finished = Some(Instant::now());
    }

    /// Whether a flush has anything to do, so that it need not dial a device for nothing.
    pub fn has_work(&self, device: &EndpointId, trigger: Trigger) -> bool {
        if !self.gate.is_trusted(device) {
            return false;
        }
        let now = messages::now_millis();
        self.database
            .queued_for(device, now, trigger != Trigger::Tick)
            .map(|queued| !queued.is_empty())
            .unwrap_or(false)
    }

    /// Writes every message due for `device` over `connection`. Stops at the first failure,
    /// which usually means the connection is gone; the rest are retried later.
    pub async fn flush_on(&self, connection: &Connection, device: EndpointId, trigger: Trigger) {
        let now = messages::now_millis();
        let queued = match self
            .database
            .queued_for(&device, now, trigger != Trigger::Tick)
        {
            Ok(queued) => queued,
            Err(error) => {
                warn!(%error, "could not read the outbox");
                return;
            }
        };
        for id in queued {
            let message: Message = match self.database.message(id) {
                Ok(Some(message)) => message,
                // A row whose message is gone has nothing to deliver.
                _ => {
                    let _ = self.database.acknowledge(id, &device);
                    continue;
                }
            };
            let wire = WireMessage {
                version: MESSAGE_VERSION,
                id,
                timestamp: message.timestamp,
                body: message.body,
                reply_to: message.reply_to,
                attachments: message.attachments,
            };
            let database = self.database.clone();
            let written = move || {
                let _ = database.mark_sent(id);
            };
            match deliver_on(connection, &wire, written).await {
                Ok(()) => {
                    let _ = self.database.acknowledge(id, &device);
                }
                Err(error) => {
                    let _ = self
                        .database
                        .record_failure(id, &device, &error.to_string(), now);
                    break;
                }
            }
        }
    }

    /// Records that a device could not be reached, for every message due for it.
    pub fn unreachable(&self, device: &EndpointId, error: &str, trigger: Trigger) {
        let now = messages::now_millis();
        let Ok(queued) = self
            .database
            .queued_for(device, now, trigger != Trigger::Tick)
        else {
            return;
        };
        for id in queued {
            let _ = self.database.record_failure(id, device, error, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_from_thirty_seconds_to_fifteen_minutes() {
        assert_eq!(backoff_ms(0), 30_000);
        assert_eq!(backoff_ms(1), 60_000);
        assert_eq!(backoff_ms(2), 120_000);
        assert_eq!(backoff_ms(5), 900_000);
        assert_eq!(backoff_ms(60), 900_000);
    }
}
