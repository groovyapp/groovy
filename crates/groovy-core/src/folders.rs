//! Shared folders: a set of files that members keep in step.
//!
//! A folder is an iroh-docs replica. Its keys are relative paths such as `photos/a.txt`,
//! and its values are the hashes of the files, which the blob store holds. A delete is
//! an entry with the empty hash. The replica syncs only with trusted contacts, and the
//! folder's record, including where this device saves it, stays in the local database.
//!
//! Each path has one file: the newest entry for it. A conflict between members is not
//! yet kept as a second copy.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use iroh_blobs::{BlobFormat, Hash, api::Store};
use iroh_docs::store::Query;
use iroh_docs::{DocTicket, NamespaceId};
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
}

/// One file in a folder, as the folder currently has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderEntry {
    /// The relative path, with `/` between parts.
    pub key: String,
    pub hash: Hash,
    pub size: u64,
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
                "SELECT namespace, name, path, created FROM folders ORDER BY created, rowid",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut folders = Vec::with_capacity(rows.len());
        for (namespace, name, path, created) in rows {
            let Ok(bytes) = <[u8; 32]>::try_from(namespace.as_slice()) else {
                continue;
            };
            folders.push(Folder {
                namespace: NamespaceId::from(bytes),
                name,
                path: path.map(PathBuf::from),
                created,
            });
        }
        Ok(folders)
    }

    /// The folder with this id, if this device is in it.
    pub fn folder(&self, namespace: NamespaceId) -> Result<Option<Folder>, DbError> {
        Ok(self
            .folders()?
            .into_iter()
            .find(|folder| folder.namespace == namespace))
    }
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

/// Copies a file into the blob store, and sets it at `key` in the folder.
pub async fn add_file(
    docs: &iroh_docs::protocol::Docs,
    store: &Store,
    namespace: NamespaceId,
    key: &str,
    source: &Path,
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
    let author = docs.author_default().await.map_err(replica)?;
    doc.set_hash(author, key.into_bytes(), temp.hash(), size)
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
    let author = docs.author_default().await.map_err(replica)?;
    doc.del(author, key.into_bytes()).await.map_err(replica)?;
    Ok(())
}

/// The files in the folder, as the newest entry for each path. Deleted paths are left out.
pub async fn entries(
    docs: &iroh_docs::protocol::Docs,
    namespace: NamespaceId,
) -> Result<Vec<FolderEntry>, FolderError> {
    let doc = docs
        .open(namespace)
        .await
        .map_err(replica)?
        .ok_or(FolderError::UnknownFolder)?;
    let mut stream = Box::pin(
        doc.get_many(Query::single_latest_per_key())
            .await
            .map_err(replica)?,
    );
    let mut entries = Vec::new();
    while let Some(entry) = stream.next().await {
        let entry = entry.map_err(replica)?;
        if entry.content_hash() == Hash::EMPTY {
            continue;
        }
        entries.push(FolderEntry {
            key: String::from_utf8_lossy(entry.key()).into_owned(),
            hash: entry.content_hash(),
            size: entry.content_len(),
        });
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(entries)
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
