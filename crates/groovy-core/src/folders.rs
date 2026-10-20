//! Shared folders: a set of files that members keep in step.
//!
//! A folder is an iroh-docs replica. Its keys are relative paths such as `photos/a.txt`,
//! and its values are the hashes of the files, which the blob store holds. A delete is
//! an entry with the empty hash. The replica syncs only with trusted contacts, and the
//! folder's record, including where this device saves it, stays in the local database.
//!
//! Each path has one file: the newest entry for it. A conflict between members is not
//! yet kept as a second copy.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use iroh_blobs::{BlobFormat, Hash, api::Store};
use iroh_docs::DocTicket;
pub use iroh_docs::NamespaceId;
use iroh_docs::store::Query;
use rusqlite::params;
use thiserror::Error;

use crate::db::{Database, DbError};
use crate::files;

/// A shared folder this device is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub namespace: NamespaceId,
    pub name: String,
    /// Where this device saves the folder's files, if it does.
    pub path: Option<PathBuf>,
    /// Unix seconds.
    pub created: i64,
    /// Whether this device keeps a full copy of the folder's files. A device that does not
    /// follows the list of files, and fetches nothing until it is asked to.
    pub keep_copy: bool,
}

/// One file in a folder, as the folder currently has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderEntry {
    /// The relative path, with `/` between parts.
    pub key: String,
    pub hash: Hash,
    pub size: u64,
}

/// How big a folder is, and who it is syncing with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderStatus {
    pub files: u64,
    /// The total size of the files, in bytes.
    pub bytes: u64,
    /// How many peers the folder is syncing with now.
    pub sync_peers: usize,
}

/// What exporting a folder wrote, and what was not yet on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub written: Vec<PathBuf>,
    /// Files that are in the folder but not yet downloaded here.
    pub missing: usize,
}

/// The hashes that shared folders reference. Garbage collection keeps every one of them.
pub type Protected = std::sync::Arc<std::sync::Mutex<std::collections::HashSet<Hash>>>;

/// Errors from shared folders.
#[derive(Debug, Error)]
pub enum FolderError {
    #[error("there is no folder with that id")]
    UnknownFolder,
    #[error("{0} is not a path inside a folder")]
    InvalidPath(String),
    #[error("the file could not be read: {0}")]
    Read(String),
    #[error("the folder's replica failed: {0}")]
    Replica(String),
    #[error("the blob store failed: {0}")]
    Store(String),
    #[error("could not save the folder")]
    Database(#[from] DbError),
}

fn replica(error: impl std::fmt::Display) -> FolderError {
    FolderError::Replica(error.to_string())
}

/// The key a relative path is stored under: its safe form, with `/` between parts.
/// A path that is not already in that form is refused, so a key always names a file
/// inside the folder.
pub fn key_for(path: &str) -> Result<String, FolderError> {
    let safe: Vec<String> = files::safe_path(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let key = safe.join("/");
    if key == path {
        Ok(key)
    } else {
        Err(FolderError::InvalidPath(path.to_owned()))
    }
}

impl Database {
    /// Records a folder this device is in.
    pub fn insert_folder(&self, folder: &Folder) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO folders (namespace, name, path, created) VALUES (?1, ?2, ?3, ?4)",
                params![
                    folder.namespace.as_bytes().as_slice(),
                    folder.name,
                    folder
                        .path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    folder.created,
                ],
            )
            .map(|_| ())
        })
    }

    /// Every folder this device is in, oldest first.
    pub fn folders(&self) -> Result<Vec<Folder>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT namespace, name, path, created, keep_copy FROM folders ORDER BY created, rowid",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut folders = Vec::with_capacity(rows.len());
        for (namespace, name, path, created, keep_copy) in rows {
            let Ok(bytes) = <[u8; 32]>::try_from(namespace.as_slice()) else {
                continue;
            };
            folders.push(Folder {
                namespace: NamespaceId::from(bytes),
                name,
                path: path.map(PathBuf::from),
                created,
                keep_copy: keep_copy != 0,
            });
        }
        Ok(folders)
    }

    /// Forgets a folder this device was in.
    pub fn delete_folder(&self, namespace: NamespaceId) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM folders WHERE namespace = ?1",
                [namespace.as_bytes().as_slice()],
            )
            .map(|_| ())
        })
    }

    /// Sets the name this device shows for a folder.
    pub fn rename_folder(&self, namespace: NamespaceId, name: &str) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE folders SET name = ?2 WHERE namespace = ?1",
                rusqlite::params![namespace.as_bytes().as_slice(), name],
            )
            .map(|changed| changed > 0)
        })
    }

    /// Sets whether this device keeps a full copy of a folder.
    pub fn set_keep_copy(&self, namespace: NamespaceId, keep: bool) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE folders SET keep_copy = ?2 WHERE namespace = ?1",
                rusqlite::params![namespace.as_bytes().as_slice(), keep],
            )
            .map(|changed| changed > 0)
        })
    }

    /// The folder with this id, if this device is in it.
    pub fn folder(&self, namespace: NamespaceId) -> Result<Option<Folder>, DbError> {
        Ok(self
            .folders()?
            .into_iter()
            .find(|folder| folder.namespace == namespace))
    }
}

/// How big the folder is, and how many peers it is syncing with.
pub async fn status(
    docs: &iroh_docs::protocol::Docs,
    namespace: NamespaceId,
) -> Result<FolderStatus, FolderError> {
    let entries = entries(docs, namespace).await?;
    let doc = docs
        .open(namespace)
        .await
        .map_err(replica)?
        .ok_or(FolderError::UnknownFolder)?;
    let sync_peers = doc
        .get_sync_peers()
        .await
        .map_err(replica)?
        .map_or(0, |peers| peers.len());
    Ok(FolderStatus {
        files: entries.len() as u64,
        bytes: entries.iter().map(|entry| entry.size).sum(),
        sync_peers,
    })
}

/// Leaves a folder: syncing stops, and the folder is removed from this device. Its files
/// stay in any directory it was saved to. Other members keep the folder.
pub async fn leave(
    docs: &iroh_docs::protocol::Docs,
    database: &Database,
    namespace: NamespaceId,
) -> Result<(), FolderError> {
    let doc = docs
        .open(namespace)
        .await
        .map_err(replica)?
        .ok_or(FolderError::UnknownFolder)?;
    doc.leave().await.map_err(replica)?;
    // A replica must be closed before it can be dropped. Handles opened by earlier calls close
    // as they are dropped, which takes a moment, so the drop is tried again for a few seconds.
    doc.close().await.map_err(replica)?;
    let mut attempts = 0;
    loop {
        match docs.drop_doc(namespace).await {
            Ok(()) => break,
            Err(error) if attempts < 50 && error.to_string().contains("not closed") => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(error) => return Err(replica(error)),
        }
    }
    database.delete_folder(namespace)?;
    Ok(())
}

/// Creates a folder, and records it. The folder is saved to `path` on this device, if given.
pub async fn create(
    docs: &iroh_docs::protocol::Docs,
    database: &Database,
    name: String,
    path: Option<PathBuf>,
    now: i64,
) -> Result<Folder, FolderError> {
    let doc = docs.create().await.map_err(replica)?;
    let folder = Folder {
        namespace: doc.id(),
        name,
        path,
        created: now,
        keep_copy: true,
    };
    database.insert_folder(&folder)?;
    Ok(folder)
}

/// Joins a folder from its ticket, and records it. Syncing starts with the peers in the
/// ticket, and only trusted contacts are served.
pub async fn join(
    docs: &iroh_docs::protocol::Docs,
    database: &Database,
    ticket: &str,
    path: Option<PathBuf>,
    now: i64,
) -> Result<Folder, FolderError> {
    let ticket: DocTicket = ticket
        .trim()
        .parse()
        .map_err(|error| FolderError::Replica(format!("not a folder ticket: {error}")))?;
    let doc = docs.import(ticket).await.map_err(replica)?;
    let folder = Folder {
        namespace: doc.id(),
        name: String::new(),
        path,
        created: now,
        keep_copy: true,
    };
    database.insert_folder(&folder)?;
    Ok(folder)
}

/// What a member who gets a ticket may do in the folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderAccess {
    /// Get the files, but not change them.
    Read,
    /// Get the files, and add, change and remove them.
    Write,
}

/// A ticket another member can use to join the folder, with the access it gives.
pub async fn ticket(
    docs: &iroh_docs::protocol::Docs,
    namespace: NamespaceId,
    access: FolderAccess,
) -> Result<String, FolderError> {
    let doc = docs
        .open(namespace)
        .await
        .map_err(replica)?
        .ok_or(FolderError::UnknownFolder)?;
    let mode = match access {
        FolderAccess::Read => iroh_docs::api::protocol::ShareMode::Read,
        FolderAccess::Write => iroh_docs::api::protocol::ShareMode::Write,
    };
    let ticket = doc
        .share(
            mode,
            iroh_docs::api::protocol::AddrInfoOptions::RelayAndAddresses,
        )
        .await
        .map_err(replica)?;
    Ok(ticket.to_string())
}

/// What a deleted path holds instead of a file. It is written as content, not as an empty
/// entry, because iroh-docs deletes by key prefix, and that would also remove other keys
/// that begin with the same name.
const TOMBSTONE: &[u8] = b"groovy:deleted";

/// The key prefix of the records of which versions a write had seen. A record
/// `history:<version>:<seen>:<key>` says that `version` was written after `seen`, so `seen`
/// is superseded. Keys cannot contain `:`, so the prefix cannot clash with a file.
pub(crate) const HISTORY: &str = "history:";

/// Whether a key is a record the folder keeps about itself, not a file.
fn is_meta(key: &str) -> bool {
    key.starts_with(HISTORY) || key.starts_with(crate::replicas::REPLICA)
}

pub(crate) fn tombstone() -> Hash {
    Hash::new(TOMBSTONE)
}

/// Whether a hash is a deleted path rather than a file.
fn is_deleted(hash: Hash) -> bool {
    hash == Hash::EMPTY || hash == tombstone()
}

/// The key a file is stored under. iroh-docs removes a key's entries when a key that
/// starts with it is written, so every stored key ends in a NUL byte: no stored key is then
/// a prefix of another, and `x.txt` cannot remove `x.txt2`. Paths cannot contain NUL.
pub(crate) fn stored_key(key: &str) -> Vec<u8> {
    let mut bytes = key.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// The key of the record that `version` was written after `seen`, in stored form.
fn history_key(key: &str, version: Hash, seen: Hash) -> Vec<u8> {
    stored_key(&format!("{HISTORY}{version}:{seen}:{key}"))
}

/// The key an entry was stored under, without its terminator.
fn unstored(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// One entry in the folder, as written by one author.
#[derive(Debug, Clone)]
pub(crate) struct Version {
    pub(crate) key: String,
    pub(crate) hash: Hash,
    pub(crate) size: u64,
    pub(crate) author: String,
    pub(crate) timestamp: u64,
}

/// Every entry in the replica, including old versions and history records.
pub(crate) async fn all_entries(
    docs: &iroh_docs::protocol::Docs,
    namespace: NamespaceId,
) -> Result<Vec<Version>, FolderError> {
    let doc = docs
        .open(namespace)
        .await
        .map_err(replica)?
        .ok_or(FolderError::UnknownFolder)?;
    let mut stream = Box::pin(doc.get_many(Query::all()).await.map_err(replica)?);
    let mut all = Vec::new();
    while let Some(entry) = stream.next().await {
        let entry = entry.map_err(replica)?;
        all.push(Version {
            key: unstored(entry.key()),
            hash: entry.content_hash(),
            size: entry.content_len(),
            author: entry.author().to_string(),
            timestamp: entry.timestamp(),
        });
    }
    Ok(all)
}

/// The newest version each author wrote for `key`, without versions that another write has
/// seen and superseded, and one per distinct content.
fn leaves(all: &[Version], key: &str) -> Vec<Version> {
    let mut latest: HashMap<&str, &Version> = HashMap::new();
    for version in all.iter().filter(|version| version.key == key) {
        let slot = latest.entry(version.author.as_str()).or_insert(version);
        if version.timestamp > slot.timestamp {
            *slot = version;
        }
    }
    let seen: HashSet<Hash> = all
        .iter()
        .filter(|version| version.key.starts_with(HISTORY))
        .filter_map(|version| parse_history(&version.key))
        .filter(|(_, _, history_key)| history_key == key)
        .map(|(_, seen, _)| seen)
        .collect();
    let mut by_hash: HashMap<Hash, Version> = HashMap::new();
    for version in latest.values() {
        if seen.contains(&version.hash) {
            continue;
        }
        let keep = match by_hash.get(&version.hash) {
            Some(existing) => version.timestamp > existing.timestamp,
            None => true,
        };
        if keep {
            by_hash.insert(version.hash, (*version).clone());
        }
    }
    by_hash.into_values().collect()
}

/// Splits a history key into the version, the seen version and the file key.
fn parse_history(full: &str) -> Option<(Hash, Hash, String)> {
    let rest = full.strip_prefix(HISTORY)?;
    let mut parts = rest.splitn(3, ':');
    let version = parts.next()?.parse().ok()?;
    let seen = parts.next()?.parse().ok()?;
    let key = parts.next()?.to_owned();
    Some((version, seen, key))
}

/// The file at `key` as the replica has it: the newest live version, and the other live
/// versions written at the same time, which are conflicts.
fn current(all: &[Version], key: &str) -> (Option<Version>, Vec<Version>) {
    let mut live: Vec<Version> = leaves(all, key)
        .into_iter()
        .filter(|version| !is_deleted(version.hash))
        .collect();
    live.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(a.author.cmp(&b.author)));
    let mut live = live.into_iter();
    (live.next(), live.collect())
}

/// Copies a file into the blob store, and sets it at `key` in the folder. The write is
/// recorded as made after the latest version the folder has, so it replaces that version.
pub async fn add_file(
    docs: &iroh_docs::protocol::Docs,
    store: &Store,
    namespace: NamespaceId,
    key: &str,
    source: &Path,
) -> Result<Hash, FolderError> {
    add_file_seeing(docs, store, namespace, key, source, None).await
}

/// Like [`add_file`], but records that the write was made after `seen` rather than the
/// latest version. A write made offline, from an older copy, uses this. The versions it did
/// not see stay as conflicts. `None` means the latest version the folder has.
pub async fn add_file_seeing(
    docs: &iroh_docs::protocol::Docs,
    store: &Store,
    namespace: NamespaceId,
    key: &str,
    source: &Path,
    seen: Option<Vec<Hash>>,
) -> Result<Hash, FolderError> {
    let key = key_for(key)?;
    let doc = docs
        .open(namespace)
        .await
        .map_err(replica)?
        .ok_or(FolderError::UnknownFolder)?;
    let size = std::fs::metadata(source)
        .map_err(|error| FolderError::Read(format!("{}: {error}", source.display())))?
        .len();
    let temp = store
        .blobs()
        .add_path_with_opts(iroh_blobs::api::blobs::AddPathOptions {
            path: source.to_path_buf(),
            format: BlobFormat::Raw,
            mode: iroh_blobs::api::blobs::ImportMode::Copy,
        })
        .temp_tag()
        .await
        .map_err(|error| FolderError::Store(error.to_string()))?;
    // No tag: the folder's entries are what keeps this blob, and garbage collection reads them.
    let seen = match seen {
        Some(seen) => seen,
        None => leaves(&all_entries(docs, namespace).await?, &key)
            .into_iter()
            .map(|version| version.hash)
            .collect(),
    };
    let author = docs.author_default().await.map_err(replica)?;
    for seen in seen {
        doc.set_bytes(author, history_key(&key, temp.hash(), seen), b"1".to_vec())
            .await
            .map_err(replica)?;
    }
    doc.set_hash(author, stored_key(&key), temp.hash(), size)
        .await
        .map_err(replica)?;
    Ok(temp.hash())
}

/// Removes the file at `key`. Members see it removed when they sync.
pub async fn remove(
    docs: &iroh_docs::protocol::Docs,
    namespace: NamespaceId,
    key: &str,
) -> Result<(), FolderError> {
    let key = key_for(key)?;
    let doc = docs
        .open(namespace)
        .await
        .map_err(replica)?
        .ok_or(FolderError::UnknownFolder)?;
    let seen: Vec<Hash> = leaves(&all_entries(docs, namespace).await?, &key)
        .into_iter()
        .map(|version| version.hash)
        .collect();
    let author = docs.author_default().await.map_err(replica)?;
    for seen in seen {
        doc.set_bytes(author, history_key(&key, tombstone(), seen), b"1".to_vec())
            .await
            .map_err(replica)?;
    }
    doc.set_bytes(author, stored_key(&key), TOMBSTONE.to_vec())
        .await
        .map_err(replica)?;
    Ok(())
}

/// The files in the folder. Each path has its newest version. A path with conflicting
/// versions also gets a copy of each other version, named for who wrote it and when.
pub async fn entries(
    docs: &iroh_docs::protocol::Docs,
    namespace: NamespaceId,
) -> Result<Vec<FolderEntry>, FolderError> {
    let all = all_entries(docs, namespace).await?;
    let keys: BTreeSet<&str> = all
        .iter()
        .filter(|version| !is_meta(&version.key))
        .map(|version| version.key.as_str())
        .collect();
    let mut entries = Vec::new();
    for key in keys {
        let (winner, conflicts) = current(&all, key);
        if let Some(winner) = winner {
            entries.push(FolderEntry {
                key: key.to_owned(),
                hash: winner.hash,
                size: winner.size,
            });
        }
        for conflict in conflicts {
            entries.push(FolderEntry {
                key: conflict_name(key, &conflict),
                hash: conflict.hash,
                size: conflict.size,
            });
        }
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(entries)
}

/// The name a conflicting version is kept under: `photos/a.txt` becomes
/// `photos/a (conflict from 1a2b3c4d, 2026-10-20).txt`.
fn conflict_name(key: &str, version: &Version) -> String {
    let (dir, file) = match key.rsplit_once('/') {
        Some((dir, file)) => (Some(dir), file),
        None => (None, key),
    };
    let (stem, extension) = match file.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (file, String::new()),
    };
    let author: String = version.author.chars().take(8).collect();
    let name = format!(
        "{stem} (conflict from {author}, {}){extension}",
        date_of(version.timestamp)
    );
    match dir {
        Some(dir) => format!("{dir}/{name}"),
        None => name,
    }
}

/// The calendar date of a replica timestamp, which counts microseconds since the Unix epoch.
fn date_of(timestamp: u64) -> String {
    let days = (timestamp / 1_000_000 / 86_400) as i64;
    // Days since 1970-01-01 to a civil date, after Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Writes every file that is downloaded here into `dir`, at its path in the folder.
/// Files still downloading are counted in the report, and nothing is written for them.
pub async fn export(
    docs: &iroh_docs::protocol::Docs,
    store: &Store,
    namespace: NamespaceId,
    dir: &Path,
) -> Result<ExportReport, FolderError> {
    let mut written = Vec::new();
    let mut missing = 0;
    for entry in entries(docs, namespace).await? {
        let has = store
            .blobs()
            .has(entry.hash)
            .await
            .map_err(|error| FolderError::Store(error.to_string()))?;
        if !has {
            missing += 1;
            continue;
        }
        let target = dir.join(files::safe_path(&entry.key));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| FolderError::Read(format!("{}: {error}", parent.display())))?;
        }
        store
            .blobs()
            .export(entry.hash, &target)
            .await
            .map_err(|error| FolderError::Store(error.to_string()))?;
        written.push(target);
    }
    Ok(ExportReport { written, missing })
}
