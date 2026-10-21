//! Keeps a folder's local directory and its shared replica in step.
//!
//! One task serves each folder that has a directory on this device. It does two things
//! in turn, so the two directions never race:
//!
//! - **Push:** when a file in the directory changes, the new contents become the folder's
//!   entry for that path. A deleted file removes its entry.
//! - **Pull:** when the folder's entries change, the files are written to the directory.
//!
//! The task remembers the hash each path last had on disk. A change to a file that already
//! has that hash is the task's own write, so it is not sent back. A file that has changed
//! on disk, and does not have that hash, is a local edit that has not been pushed yet, so
//! the pull leaves it alone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iroh_blobs::{Hash, api::Store};
use iroh_docs::NamespaceId;
use iroh_docs::protocol::Docs;
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::files;
use crate::folders::{self, FolderEntry, HeldAdds};

/// How often the folder is checked for changes from other members.
const PULL_INTERVAL: Duration = Duration::from_secs(1);
/// How long to wait after a change, so a save that writes several times is sent once.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Starts syncing a folder's directory. The task runs until the node stops, or until it is
/// aborted through the returned handle, as when the folder is left.
pub fn spawn(
    docs: Docs,
    store: Store,
    namespace: NamespaceId,
    dir: PathBuf,
    held: HeldAdds,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        if let Err(error) = run(docs, store, namespace, dir, held).await {
            warn!(%error, "a shared folder stopped syncing");
        }
    })
    .abort_handle()
}

async fn run(
    docs: Docs,
    store: Store,
    namespace: NamespaceId,
    dir: PathBuf,
    held: HeldAdds,
) -> Result<(), String> {
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let (tx, mut changes) = mpsc::unbounded_channel::<PathBuf>();
    // The watcher runs on its own thread; its events come back through the channel.
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result {
            for path in event.paths {
                let _ = tx.send(path);
            }
        }
    })
    .map_err(|error| error.to_string())?;
    watcher
        .watch(&dir, RecursiveMode::Recursive)
        .map_err(|error| error.to_string())?;
    info!(dir = %dir.display(), "watching a shared folder");

    let mut known: HashMap<String, Hash> = HashMap::new();
    // Start from the state on disk: what is there is pushed, and what is missing is pulled.
    pull(&docs, &store, namespace, &dir, &mut known).await;
    push_all(&docs, &store, namespace, &dir, &mut known, &held).await;

    let mut tick = tokio::time::interval(PULL_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                pull(&docs, &store, namespace, &dir, &mut known).await;
            }
            Some(path) = changes.recv() => {
                // Gather what changed in the next moment, so one save is one push.
                let mut paths = vec![path];
                tokio::time::sleep(DEBOUNCE).await;
                while let Ok(more) = changes.try_recv() {
                    paths.push(more);
                }
                push_paths(&docs, &store, namespace, &dir, &mut known, &held, paths).await;
            }
        }
    }
}

/// The hash of a file's contents, read in pieces so large files are not held in memory.
fn hash_file(path: &Path) -> Option<Hash> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(Hash::from_bytes(*hasher.finalize().as_bytes()))
}

/// The key a file in the directory is stored under, if its path is one a folder can hold.
fn key_of(dir: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(dir).ok()?;
    let parts: Vec<String> = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let key = parts.join("/");
    folders::key_for(&key).ok()
}

/// Pushes the files named by the watcher. A path that is gone removes its entry.
async fn push_paths(
    docs: &Docs,
    store: &Store,
    namespace: NamespaceId,
    dir: &Path,
    known: &mut HashMap<String, Hash>,
    held: &HeldAdds,
    paths: Vec<PathBuf>,
) {
    for path in paths {
        let Some(key) = key_of(dir, &path) else {
            continue;
        };
        if path.is_file() {
            let Some(hash) = hash_file(&path) else {
                continue;
            };
            // Our own write, or a file that has not changed since it was last synced.
            if known.get(&key) == Some(&hash) {
                continue;
            }
            match folders::add_file(docs, store, held, namespace, &key, &path).await {
                Ok(_) => {
                    known.insert(key, hash);
                }
                Err(error) => warn!(%error, key, "could not share a change"),
            }
        } else if known.contains_key(&key) {
            match folders::remove(docs, namespace, &key).await {
                Ok(()) => {
                    known.remove(&key);
                }
                Err(error) => warn!(%error, key, "could not share a deletion"),
            }
        }
    }
}

/// Pushes every file in the directory that the folder does not have yet, or has differently.
async fn push_all(
    docs: &Docs,
    store: &Store,
    namespace: NamespaceId,
    dir: &Path,
    known: &mut HashMap<String, Hash>,
    held: &HeldAdds,
) {
    let mut paths = Vec::new();
    collect_files(dir, &mut paths);
    push_paths(docs, store, namespace, dir, known, held, paths).await;
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for child in read.flatten() {
        let path = child.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_files(&path, out);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

/// Writes the folder's files to the directory, and removes the ones members deleted.
async fn pull(
    docs: &Docs,
    store: &Store,
    namespace: NamespaceId,
    dir: &Path,
    known: &mut HashMap<String, Hash>,
) {
    let entries = match folders::entries(docs, namespace).await {
        Ok(entries) => entries,
        Err(error) => {
            warn!(%error, "could not read a shared folder");
            return;
        }
    };
    for entry in &entries {
        write_entry(store, dir, entry, known).await;
    }
    // A file we wrote that the folder no longer has was deleted by a member.
    let present: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    let gone: Vec<String> = known
        .keys()
        .filter(|key| !present.contains(&key.as_str()))
        .cloned()
        .collect();
    for key in gone {
        let path = dir.join(files::safe_path(&key));
        if known
            .get(&key)
            .is_some_and(|hash| hash_file(&path) == Some(*hash))
        {
            let _ = std::fs::remove_file(&path);
        }
        known.remove(&key);
    }
}

async fn write_entry(
    store: &Store,
    dir: &Path,
    entry: &FolderEntry,
    known: &mut HashMap<String, Hash>,
) {
    if known.get(&entry.key) == Some(&entry.hash) {
        return;
    }
    let path = dir.join(files::safe_path(&entry.key));
    let local = if path.is_file() {
        hash_file(&path)
    } else {
        None
    };
    if local == Some(entry.hash) {
        known.insert(entry.key.clone(), entry.hash);
        return;
    }
    // The file on disk has changed and is not in the folder yet: a local edit to push first.
    if local.is_some() && local != known.get(&entry.key).copied() {
        return;
    }
    // Wait until the contents have arrived in the blob store.
    if !store.blobs().has(entry.hash).await.unwrap_or(false) {
        return;
    }
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    match store.blobs().export(entry.hash, &path).await {
        Ok(_) => {
            known.insert(entry.key.clone(), entry.hash);
        }
        Err(error) => warn!(%error, key = %entry.key, "could not write a shared file"),
    }
}
