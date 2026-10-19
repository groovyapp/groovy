//! Files and folders to send. They are copied into the blob store and gathered
//! into one collection, where each file keeps its path inside the folder it was
//! sent from.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use iroh_blobs::{
    BlobFormat, Hash,
    api::Store,
    api::blobs::{AddPathOptions, ImportMode},
    format::collection::Collection,
};
use thiserror::Error;

/// Errors from gathering or importing files to send.
#[derive(Debug, Error)]
pub enum FileError {
    #[error("nothing was chosen to send")]
    Empty,
    #[error("{0} has no name to send it under")]
    Unnamed(PathBuf),
    #[error("{0} has a name that is not valid UTF-8")]
    NotUtf8(PathBuf),
    #[error("two items to send are both called {0}")]
    DuplicateName(String),
    #[error("could not read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the blob store failed: {0}")]
    Store(String),
}

/// One file to import: its path inside the collection, and where it is on disk.
struct Entry {
    name: String,
    path: PathBuf,
    bytes: u64,
}

/// The result of importing: the collection, and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// The root of the collection, which the recipient fetches.
    pub root: Hash,
    /// The names of the files and folders at the top, as the user chose them.
    pub names: Vec<String>,
    /// How many files the collection holds.
    pub files: u64,
    /// The total size of those files, in bytes.
    pub bytes: u64,
}

/// Imports the files under `paths` into `store` as one collection, and tags the
/// collection so its content is kept.
///
/// `progress` is called after each file with the number of files done and the
/// number in total. Files are copied in, so later changes to them do not affect
/// what is sent.
pub async fn import(
    store: &Store,
    paths: &[PathBuf],
    mut progress: impl FnMut(u64, u64),
) -> Result<Imported, FileError> {
    let names = paths
        .iter()
        .map(|path| top_name(path))
        .collect::<Result<Vec<_>, _>>()?;
    let entries = gather(paths)?;
    let total = entries.len() as u64;
    let bytes = entries.iter().map(|entry| entry.bytes).sum();

    let mut collection = Collection::default();
    // Held until the collection is stored, so none of the files is unprotected meanwhile.
    let mut temps = Vec::with_capacity(entries.len());
    for (done, entry) in entries.into_iter().enumerate() {
        let temp = store
            .blobs()
            .add_path_with_opts(AddPathOptions {
                path: entry.path,
                format: BlobFormat::Raw,
                mode: ImportMode::Copy,
            })
            .temp_tag()
            .await
            .map_err(store_error)?;
        collection.extend([(entry.name, temp.hash())]);
        temps.push(temp);
        progress(done as u64 + 1, total);
    }

    let root_tag = collection.store(store).await.map_err(store_error)?;
    let root = root_tag.hash();
    store
        .tags()
        .create(root_tag.hash_and_format())
        .await
        .map_err(store_error)?;
    drop(temps);
    Ok(Imported {
        root,
        names,
        files: total,
        bytes,
    })
}

/// Lists the files in a stored collection, with their paths inside it.
pub async fn list(store: &Store, root: Hash) -> Result<Vec<(String, Hash)>, FileError> {
    let collection = Collection::load(root, store).await.map_err(store_error)?;
    Ok(collection.iter().cloned().collect())
}

/// Lists every file under `paths`, sorted by path so the same content always
/// makes the same collection. A folder's files are named under the folder's name.
/// Symbolic links are skipped, so nothing outside the chosen folders is sent.
fn gather(paths: &[PathBuf]) -> Result<Vec<Entry>, FileError> {
    if paths.is_empty() {
        return Err(FileError::Empty);
    }
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for path in paths {
        let name = top_name(path)?;
        if !seen.insert(name.clone()) {
            return Err(FileError::DuplicateName(name));
        }
        let meta = std::fs::symlink_metadata(path).map_err(|source| FileError::Read {
            path: path.clone(),
            source,
        })?;
        if meta.is_dir() {
            walk(path, &name, &mut entries)?;
        } else if meta.is_file() {
            entries.push(Entry {
                name,
                path: path.clone(),
                bytes: meta.len(),
            });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// Adds the files under `dir`, named under `prefix`, and the folders under it.
fn walk(dir: &Path, prefix: &str, entries: &mut Vec<Entry>) -> Result<(), FileError> {
    let read = std::fs::read_dir(dir).map_err(|source| FileError::Read {
        path: dir.to_path_buf(),
        source,
    })?;
    for child in read {
        let child = child.map_err(|source| FileError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = child.path();
        let file_name = child
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| FileError::NotUtf8(path.clone()))?;
        let name = format!("{prefix}/{file_name}");
        let meta = std::fs::symlink_metadata(&path).map_err(|source| FileError::Read {
            path: path.clone(),
            source,
        })?;
        if meta.is_dir() {
            walk(&path, &name, entries)?;
        } else if meta.is_file() {
            entries.push(Entry {
                name,
                path,
                bytes: meta.len(),
            });
        }
    }
    Ok(())
}

/// The name a file or folder is sent under: its last path component.
fn top_name(path: &Path) -> Result<String, FileError> {
    let name = path
        .file_name()
        .ok_or_else(|| FileError::Unnamed(path.to_path_buf()))?;
    name.to_str()
        .map(str::to_owned)
        .ok_or_else(|| FileError::NotUtf8(path.to_path_buf()))
}

fn store_error(error: impl std::fmt::Display) -> FileError {
    FileError::Store(error.to_string())
}
