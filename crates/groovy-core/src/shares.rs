//! Shares: content sent to anyone who has its ticket, for one-off sharing.
//!
//! A share serves only its root and the files in its collection, and only while it
//! is active. Trusted contacts can still fetch any blob this node holds. Anyone else
//! is refused for every other hash. Revoking a share stops it at once: new requests
//! for its content are refused. A download that is already running is not stopped.

use iroh_blobs::Hash;
use rusqlite::params;

use crate::db::{Database, DbError};

/// A share, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub id: i64,
    pub root: Hash,
    /// The top-level names the share was made from, such as `photos` or `note.txt`.
    pub names: Vec<String>,
    pub files: u64,
    pub bytes: u64,
    /// The root, then every file in the collection. Only these are served.
    pub hashes: Vec<Hash>,
    /// Unix seconds.
    pub created: i64,
    /// Unix seconds when the share was revoked, or `None` while it is active.
    pub revoked: Option<i64>,
}

impl Share {
    pub fn is_active(&self) -> bool {
        self.revoked.is_none()
    }
}

impl Database {
    /// Records a new share and returns its id. The share's `id` field is ignored.
    pub fn insert_share(&self, share: &Share) -> Result<i64, DbError> {
        let hashes = encode(&share.hashes)?;
        let names = encode(&share.names)?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO shares (root, hashes, names, files, bytes, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    share.root.as_bytes().as_slice(),
                    hashes,
                    names,
                    share.files as i64,
                    share.bytes as i64,
                    share.created,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Stops a share from serving anything. Returns `false` if there is no such share
    /// or it was already revoked.
    pub fn revoke_share(&self, id: i64, now: i64) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE shares SET revoked = ?2 WHERE id = ?1 AND revoked IS NULL",
                params![id, now],
            )
            .map(|changed| changed > 0)
        })
    }

    /// Every share, oldest first, including revoked ones.
    pub fn shares(&self) -> Result<Vec<Share>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, root, hashes, names, files, bytes, created, revoked
                 FROM shares ORDER BY id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;

        let mut shares = Vec::with_capacity(rows.len());
        for (id, root, hashes, names, files, bytes, created, revoked) in rows {
            // A row that does not decode is skipped rather than failing the whole list.
            let Ok(root) = <[u8; 32]>::try_from(root.as_slice()) else {
                continue;
            };
            let (Ok(hashes), Ok(names)) = (
                postcard::from_bytes::<Vec<Hash>>(&hashes),
                postcard::from_bytes::<Vec<String>>(&names),
            ) else {
                continue;
            };
            shares.push(Share {
                id,
                root: Hash::from_bytes(root),
                names,
                files: files as u64,
                bytes: bytes as u64,
                hashes,
                created,
                revoked,
            });
        }
        Ok(shares)
    }

    /// Every hash that active shares serve.
    pub fn active_share_hashes(&self) -> Result<Vec<Hash>, DbError> {
        Ok(self
            .shares()?
            .into_iter()
            .filter(Share::is_active)
            .flat_map(|share| share.hashes)
            .collect())
    }
}

fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, DbError> {
    postcard::to_stdvec(value)
        .map_err(|error| DbError::Query(rusqlite::Error::ToSqlConversionFailure(Box::new(error))))
}
