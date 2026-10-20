//! Local database for state that is not content-addressed: contacts, transfer
//! history, conversations, messages, and settings that change at runtime.
//!
//! The database is SQLite, through rusqlite with its bundled library. See
//! `docs/decisions/0001-sqlite.md` for the reasons.
//!
//! Calls block the calling thread, so run them off the UI thread.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

/// Schema migrations, applied in order. Migration `n` (1-based) moves the
/// database from version `n - 1` to version `n`. Never edit a migration that
/// has shipped; add a new one instead.
const MIGRATIONS: &[&str] = &[
    // Settings that change at runtime, stored as text.
    "CREATE TABLE settings (
        key   TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL
    );",
    // The local profile, a single row. The display name saved during
    // onboarding moves here from the settings table.
    "CREATE TABLE profile (
        id           INTEGER PRIMARY KEY CHECK (id = 1),
        display_name TEXT NOT NULL,
        avatar_png   BLOB
    );
    INSERT INTO profile (id, display_name)
        SELECT 1, value FROM settings WHERE key = 'profile.display_name';
    DELETE FROM settings WHERE key = 'profile.display_name';",
    // Peers this node has handshaken with. Trust decides whether they may connect.
    "CREATE TABLE contacts (
        endpoint_id  BLOB PRIMARY KEY NOT NULL CHECK (length(endpoint_id) = 32),
        display_name TEXT NOT NULL,
        nickname     TEXT,
        avatar_hash  BLOB CHECK (avatar_hash IS NULL OR length(avatar_hash) = 32),
        trust        TEXT NOT NULL CHECK (trust IN ('pending', 'trusted', 'blocked')),
        first_seen   INTEGER NOT NULL,
        last_seen    INTEGER NOT NULL
    );",
    // Whether files from a contact are accepted without asking.
    "ALTER TABLE contacts ADD COLUMN auto_accept_files INTEGER NOT NULL DEFAULT 0
        CHECK (auto_accept_files IN (0, 1));",
    // Accepted file offers, and how far each has got. Kept so the transfer list
    // survives a restart.
    "CREATE TABLE transfers (
        id      INTEGER PRIMARY KEY AUTOINCREMENT,
        peer    BLOB NOT NULL CHECK (length(peer) = 32),
        offer   BLOB NOT NULL,
        state   TEXT NOT NULL CHECK (state IN ('queued', 'active', 'paused', 'completed', 'failed', 'cancelled')),
        error   TEXT,
        done    INTEGER NOT NULL DEFAULT 0,
        created INTEGER NOT NULL,
        updated INTEGER NOT NULL
    );",
    // Shares: content offered to anyone with the ticket, until revoked. `hashes` holds
    // the root and every file in the collection, which are the only blobs the share serves.
    "CREATE TABLE shares (
        id      INTEGER PRIMARY KEY AUTOINCREMENT,
        root    BLOB NOT NULL CHECK (length(root) = 32),
        hashes  BLOB NOT NULL,
        names   BLOB NOT NULL,
        files   INTEGER NOT NULL,
        bytes   INTEGER NOT NULL,
        created INTEGER NOT NULL,
        revoked INTEGER
    );",
    // Shared folders this device is in. `path` is where this device saves the folder's
    // files, if it does; it is never shared.
    "CREATE TABLE folders (
        namespace BLOB PRIMARY KEY NOT NULL CHECK (length(namespace) = 32),
        name      TEXT NOT NULL,
        path      TEXT,
        created   INTEGER NOT NULL
    );",
    // Whether a transfer is a receive or a send. Transfers recorded before this were receives.
    "ALTER TABLE transfers ADD COLUMN direction TEXT NOT NULL DEFAULT 'receive'
        CHECK (direction IN ('receive', 'send'));",
    // Devices linked to a user, as seen from here: each is a certificate the user's key signed.
    // `revoked` is when the user revoked the device, and `revocation` is the signed record, which
    // is passed on to peers. Contacts also remember which user a device belongs to.
    "CREATE TABLE user_devices (
        user       BLOB NOT NULL CHECK (length(user) = 32),
        device     BLOB NOT NULL CHECK (length(device) = 32),
        name       TEXT NOT NULL,
        created    INTEGER NOT NULL,
        cert       BLOB NOT NULL,
        revoked    INTEGER,
        revocation BLOB,
        PRIMARY KEY (user, device)
    );
    ALTER TABLE contacts ADD COLUMN user BLOB CHECK (user IS NULL OR length(user) = 32);",
    // Whether this device keeps a full copy of a shared folder. Folders recorded before this
    // were kept in full.
    "ALTER TABLE folders ADD COLUMN keep_copy INTEGER NOT NULL DEFAULT 1;",
    // Direct messages with contacts. `conversation` is the contact on the other side, and
    // `author` is who wrote the message. A message is `sending`, `delivered` or `failed`.
    "CREATE TABLE messages (
        id           BLOB PRIMARY KEY NOT NULL CHECK (length(id) = 16),
        conversation BLOB NOT NULL CHECK (length(conversation) = 32),
        author       BLOB NOT NULL CHECK (length(author) = 32),
        timestamp    INTEGER NOT NULL,
        body         TEXT NOT NULL,
        reply_to     BLOB CHECK (reply_to IS NULL OR length(reply_to) = 16),
        state        TEXT NOT NULL CHECK (state IN ('sending', 'delivered', 'failed'))
    );
    CREATE INDEX messages_by_conversation ON messages (conversation, timestamp, id);",
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Errors from the database.
#[derive(Debug, Error)]
pub enum DbError {
    #[error("failed to open the database at {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "database schema version {found} is newer than this build supports ({supported}); upgrade Groovy"
    )]
    NewerSchema { found: u32, supported: u32 },
    #[error("migration to schema version {version} failed")]
    Migration {
        version: u32,
        #[source]
        source: rusqlite::Error,
    },
    #[error("database query failed")]
    Query(#[from] rusqlite::Error),
    #[error("database lock was poisoned by an earlier panic")]
    Poisoned,
}

/// A handle to Groovy's database. Safe to share between threads.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Opens the database at `path`, creating it if needed, and migrates it to
    /// [`SCHEMA_VERSION`].
    pub fn open(path: &Path) -> Result<Self, DbError> {
        Self::open_with(path, MIGRATIONS)
    }

    fn open_with(path: &Path, migrations: &[&str]) -> Result<Self, DbError> {
        let mut conn = Connection::open(path).map_err(|source| DbError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn, migrations)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The value stored under `key`, if any.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
        })
    }

    /// Stores `value` under `key`, replacing any previous value.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
        })
    }

    /// Removes `key`. Returns whether it was present.
    pub fn delete_setting(&self, key: &str) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM settings WHERE key = ?1", params![key])
                .map(|removed| removed > 0)
        })
    }

    pub(crate) fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, DbError> {
        let conn = self.conn.lock().map_err(|_| DbError::Poisoned)?;
        Ok(f(&conn)?)
    }
}

fn schema_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Applies each migration that has not run yet. Each one runs in its own
/// transaction together with the version bump, so a failure leaves the
/// database at the last good version.
fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<(), DbError> {
    let current = schema_version(conn)?;
    let supported = migrations.len() as u32;
    if current > supported {
        return Err(DbError::NewerSchema {
            found: current,
            supported,
        });
    }

    for (index, sql) in migrations.iter().enumerate().skip(current as usize) {
        let version = index as u32 + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .map_err(|source| DbError::Migration { version, source })?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_path() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("groovy.sqlite");
        (tmp, path)
    }

    #[test]
    fn new_database_is_migrated_to_the_current_version() {
        let (_tmp, path) = db_path();

        let db = Database::open(&path).unwrap();

        let version = schema_version(&db.conn.lock().unwrap()).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn settings_round_trip() {
        let (_tmp, path) = db_path();
        let db = Database::open(&path).unwrap();

        assert_eq!(db.get_setting("theme").unwrap(), None);

        db.set_setting("theme", "dark").unwrap();
        assert_eq!(db.get_setting("theme").unwrap().as_deref(), Some("dark"));

        db.set_setting("theme", "light").unwrap();
        assert_eq!(db.get_setting("theme").unwrap().as_deref(), Some("light"));

        assert!(db.delete_setting("theme").unwrap());
        assert!(!db.delete_setting("theme").unwrap());
        assert_eq!(db.get_setting("theme").unwrap(), None);
    }

    #[test]
    fn data_survives_reopening() {
        let (_tmp, path) = db_path();
        Database::open(&path)
            .unwrap()
            .set_setting("name", "Danny")
            .unwrap();

        let reopened = Database::open(&path).unwrap();

        assert_eq!(
            reopened.get_setting("name").unwrap().as_deref(),
            Some("Danny")
        );
    }

    #[test]
    fn reopening_does_not_rerun_migrations() {
        let (_tmp, path) = db_path();
        // The first migration creates a table, so running it twice would fail.
        Database::open_with(&path, &["CREATE TABLE a (x INTEGER);"]).unwrap();

        Database::open_with(&path, &["CREATE TABLE a (x INTEGER);"]).unwrap();
    }

    #[test]
    fn only_new_migrations_run_on_upgrade() {
        let (_tmp, path) = db_path();
        Database::open_with(&path, &["CREATE TABLE a (x INTEGER);"]).unwrap();

        let db = Database::open_with(
            &path,
            &["CREATE TABLE a (x INTEGER);", "CREATE TABLE b (y INTEGER);"],
        )
        .unwrap();

        assert_eq!(schema_version(&db.conn.lock().unwrap()).unwrap(), 2);
    }

    #[test]
    fn failed_migration_rolls_back_to_the_last_good_version() {
        let (_tmp, path) = db_path();

        let err = Database::open_with(&path, &["CREATE TABLE a (x INTEGER);", "THIS IS NOT SQL"])
            .err()
            .expect("broken migration fails");

        assert!(
            matches!(err, DbError::Migration { version: 2, .. }),
            "{err}"
        );
        let conn = Connection::open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 1);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "migration 1 is kept");
    }

    #[test]
    fn a_name_saved_before_profiles_existed_moves_into_the_profile() {
        let (_tmp, path) = db_path();
        // Version 1 of the schema, as shipped before profiles.
        let v1 = Database::open_with(&path, &MIGRATIONS[..1]).unwrap();
        v1.set_setting("profile.display_name", "Ada").unwrap();
        drop(v1);

        let upgraded = Database::open(&path).unwrap();

        let profile = upgraded.profile().unwrap().expect("profile migrated");
        assert_eq!(profile.display_name, "Ada");
        assert_eq!(upgraded.get_setting("profile.display_name").unwrap(), None);
    }

    #[test]
    fn newer_schema_is_refused() {
        let (_tmp, path) = db_path();
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99).unwrap();
        }

        let err = Database::open(&path)
            .err()
            .expect("newer schema is refused");

        assert!(
            matches!(err, DbError::NewerSchema { found: 99, .. }),
            "{err}"
        );
    }
}
