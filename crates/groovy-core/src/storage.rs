//! Disk use: how much the transfers and folders hold, and an optional cap to warn at.
//!
//! Content is kept by tags, and garbage collection removes what nothing references. A
//! received file is kept by its transfer, and a file in a folder by the folder's entry.
//! Deleting a transfer's history, or clearing the cache, releases its content, which is
//! freed by the next collection.

use crate::db::{Database, DbError};

/// What the node holds, and the cap the user set, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageUsage {
    /// The size of every file received, and not yet deleted from history.
    pub received_bytes: u64,
    /// The size of every file in every shared folder this device is in.
    pub folder_bytes: u64,
    /// The cap the user set, in bytes. `None` means no cap.
    pub cap_bytes: Option<u64>,
}

impl StorageUsage {
    /// Everything the node holds.
    pub fn total(&self) -> u64 {
        self.received_bytes + self.folder_bytes
    }

    /// Whether usage has reached nine tenths of the cap.
    pub fn near_cap(&self) -> bool {
        self.cap_bytes
            .is_some_and(|cap| self.total().saturating_mul(10) >= cap.saturating_mul(9))
    }
}

const CAP_KEY: &str = "storage.cap_bytes";

impl Database {
    /// The cap the user set, in bytes, if any.
    pub fn storage_cap(&self) -> Result<Option<u64>, DbError> {
        Ok(self
            .get_setting(CAP_KEY)?
            .and_then(|value| value.parse().ok()))
    }

    /// Sets the cap, in bytes. `None` removes it.
    pub fn set_storage_cap(&self, cap: Option<u64>) -> Result<(), DbError> {
        match cap {
            Some(bytes) => self.set_setting(CAP_KEY, &bytes.to_string()),
            None => self.with_conn(|conn| {
                conn.execute("DELETE FROM settings WHERE key = ?1", [CAP_KEY])
                    .map(|_| ())
            }),
        }
    }
}
