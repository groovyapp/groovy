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

/// Writes the files in a stored collection under `dir`, at the paths they have inside
/// it. Each path is made safe first, so a peer cannot write outside `dir`, and a file
/// whose name is taken gets a number added. Returns where each file was written.
pub async fn export(store: &Store, root: Hash, dir: &Path) -> Result<Vec<PathBuf>, FileError> {
    let mut written = Vec::new();
    for (name, hash) in list(store, root).await? {
        let target = unique_path(&dir.join(safe_path(&name)));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| FileError::Read {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        store
            .blobs()
            .export(hash, &target)
            .await
            .map_err(store_error)?;
        written.push(target);
    }
    Ok(written)
}

/// Where received files go by default: a `Groovy` folder in the user's Downloads.
pub fn default_downloads() -> Option<PathBuf> {
    directories::UserDirs::new()?
        .download_dir()
        .map(|dir| dir.join("Groovy"))
}

/// Turns a name from a peer into a relative path that stays inside the folder it is
/// written to. Empty, `.` and `..` parts are dropped, so `../../x` becomes `x`.
/// Characters that Windows does not allow are replaced, and reserved device names
/// such as `CON` get an underscore in front.
pub fn safe_path(name: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for part in name.split(['/', '\\']) {
        if let Some(clean) = safe_component(part) {
            path.push(clean);
        }
    }
    if path.as_os_str().is_empty() {
        path.push("file");
    }
    path
}

fn safe_component(part: &str) -> Option<String> {
    // Windows ignores trailing dots and spaces, so they are dropped here too.
    let trimmed = part.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return None;
    }
    let mut clean: String = trimmed
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let stem = clean.split('.').next().unwrap_or("").to_ascii_uppercase();
    if is_reserved_device(&stem) {
        clean.insert(0, '_');
    }
    Some(clean)
}

fn is_reserved_device(stem: &str) -> bool {
    matches!(stem, "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// `path` if nothing is there yet, otherwise `name (2).ext`, `name (3).ext` and so on.
fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let extension = path.extension().and_then(|e| e.to_str());
    for number in 2u32.. {
        let name = match extension {
            Some(ext) => format!("{stem} ({number}).{ext}"),
            None => format!("{stem} ({number})"),
        };
        let candidate = path.with_file_name(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("the counter runs out of numbers only after u32::MAX files of one name")
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

#[cfg(test)]
mod tests {
    use super::safe_path;
    use std::path::PathBuf;

    fn path(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn names_that_climb_out_of_the_folder_stay_inside_it() {
        assert_eq!(safe_path("../../x"), path(&["x"]));
        assert_eq!(safe_path("a/../../b.txt"), path(&["a", "b.txt"]));
        assert_eq!(safe_path("/etc/passwd"), path(&["etc", "passwd"]));
        assert_eq!(safe_path("..\\..\\x"), path(&["x"]));
        assert_eq!(safe_path("..."), path(&["file"]));
        assert_eq!(safe_path(""), path(&["file"]));
    }

    #[test]
    fn ordinary_paths_are_kept() {
        assert_eq!(
            safe_path("photos/sub/b.bin"),
            path(&["photos", "sub", "b.bin"])
        );
    }

    #[test]
    fn characters_windows_forbids_are_replaced() {
        assert_eq!(safe_path("a:b?.txt"), path(&["a_b_.txt"]));
        assert_eq!(safe_path("trailing. "), path(&["trailing"]));
    }

    #[test]
    fn reserved_device_names_are_prefixed() {
        assert_eq!(safe_path("CON.txt"), path(&["_CON.txt"]));
        assert_eq!(safe_path("com1"), path(&["_com1"]));
        assert_eq!(safe_path("LPT9.log"), path(&["_LPT9.log"]));
        // Only COM1 to COM9 and LPT1 to LPT9 are reserved.
        assert_eq!(safe_path("COM10"), path(&["COM10"]));
        assert_eq!(safe_path("CONSOLE.txt"), path(&["CONSOLE.txt"]));
    }
}
