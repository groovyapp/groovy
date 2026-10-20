//! Replica health: which devices hold a complete copy of a shared folder.
//!
//! Each device that is in a folder writes a small record into the folder under
//! `replica:<device id>`: how many files the folder had, how many of them were complete in
//! its blob store when it last checked, and when. The records sync like any other entry, so
//! every member can see how many complete copies exist without asking anyone.
//!
//! A device checks its own copy now and then. The check compares each file with the blob
//! store: the content must be there in full, at the size the folder says. It does not read the
//! bytes again.

use std::collections::HashMap;

use iroh::EndpointId;
use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use iroh_blobs::api::proto::BlobStatus;
use iroh_docs::NamespaceId;
use iroh_docs::protocol::Docs;
use iroh_docs::store::{DownloadPolicy, FilterKind};
use serde::{Deserialize, Serialize};

use crate::db::Database;
use crate::folders::{self, FolderError, HISTORY, stored_key};

/// The key prefix of the records devices write about their own copy.
pub(crate) const REPLICA: &str = "replica:";

/// A report is written again after this long even if nothing changed, so members can see the
/// copy was checked recently.
const REFRESH_SECS: i64 = 600;

/// What a device wrote about its copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Report {
    /// Files the folder had when the device checked.
    files: u64,
    /// Files that were complete in its store.
    held: u64,
    /// Whether the device keeps a full copy.
    keeps: bool,
    /// Unix seconds.
    verified: i64,
}

/// One device's copy of a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaInfo {
    pub device: EndpointId,
    /// Whether this is the device asking.
    pub you: bool,
    /// Whether the device keeps a full copy, rather than following the file list only.
    pub keeps_copy: bool,
    pub files: u64,
    /// How many of the files the device has in full.
    pub held: u64,
    /// Unix seconds, when the device last checked its copy.
    pub verified: i64,
}

impl ReplicaInfo {
    /// Whether the device keeps the folder and has every file in full.
    pub fn complete(&self) -> bool {
        self.keeps_copy && self.held >= self.files
    }
}

/// How well a folder is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaHealth {
    /// How many devices have a complete copy, as of their last check. This device is counted
    /// by what it has now.
    pub copies: usize,
    /// Every device known to be in the folder, this one first.
    pub devices: Vec<ReplicaInfo>,
}

fn replica(error: impl std::fmt::Display) -> FolderError {
    FolderError::Replica(error.to_string())
}

fn report_key(device: EndpointId) -> Vec<u8> {
    stored_key(&format!("{REPLICA}{device}"))
}

/// Checks this device's copy of a folder against the blob store.
pub async fn check(
    docs: &Docs,
    store: &Store,
    namespace: NamespaceId,
    me: EndpointId,
    keeps: bool,
    now: i64,
) -> Result<ReplicaInfo, FolderError> {
    let entries = folders::entries(docs, namespace).await?;
    let mut held = 0;
    for entry in &entries {
        let status = store.blobs().status(entry.hash).await.map_err(replica)?;
        if matches!(status, BlobStatus::Complete { size } if size == entry.size) {
            held += 1;
        }
    }
    Ok(ReplicaInfo {
        device: me,
        you: true,
        keeps_copy: keeps,
        files: entries.len() as u64,
        held,
        verified: now,
    })
}

/// Checks this device's copy and writes the report, unless it says the same as the last one
/// and is recent. `force` writes it anyway.
pub async fn verify(
    docs: &Docs,
    store: &Store,
    namespace: NamespaceId,
    me: EndpointId,
    keeps: bool,
    now: i64,
    force: bool,
) -> Result<ReplicaInfo, FolderError> {
    let info = check(docs, store, namespace, me, keeps, now).await?;
    let report = Report {
        files: info.files,
        held: info.held,
        keeps: info.keeps_copy,
        verified: now,
    };
    if !force
        && let Some(last) = reports(docs, store, namespace)
            .await?
            .into_iter()
            .find(|(device, _)| *device == me)
            .map(|(_, last)| last)
        && last.files == report.files
        && last.held == report.held
        && last.keeps == report.keeps
        && now - last.verified < REFRESH_SECS
    {
        return Ok(info);
    }
    let doc = docs
        .open(namespace)
        .await
        .map_err(replica)?
        .ok_or(FolderError::UnknownFolder)?;
    let author = docs.author_default().await.map_err(replica)?;
    let bytes = postcard::to_stdvec(&report).map_err(replica)?;
    doc.set_bytes(author, report_key(me), bytes)
        .await
        .map_err(replica)?;
    Ok(info)
}

/// The reports in a folder, one per device, as they have arrived here.
async fn reports(
    docs: &Docs,
    store: &Store,
    namespace: NamespaceId,
) -> Result<Vec<(EndpointId, Report)>, FolderError> {
    let all = folders::all_entries(docs, namespace).await?;
    let mut newest: HashMap<&str, &folders::Version> = HashMap::new();
    for version in all.iter().filter(|v| v.key.starts_with(REPLICA)) {
        let slot = newest.entry(version.key.as_str()).or_insert(version);
        if version.timestamp > slot.timestamp {
            *slot = version;
        }
    }
    let mut reports = Vec::new();
    for (key, version) in newest {
        let Ok(device) = key[REPLICA.len()..].parse::<EndpointId>() else {
            continue;
        };
        // The record's content may not have arrived yet.
        let Ok(bytes) = store.blobs().get_bytes(version.hash).await else {
            continue;
        };
        if let Ok(report) = postcard::from_bytes::<Report>(&bytes) {
            reports.push((device, report));
        }
    }
    Ok(reports)
}

/// How many devices hold a complete copy of the folder, and what each one has. This device is
/// checked now; the others are as they last reported.
pub async fn health(
    docs: &Docs,
    store: &Store,
    namespace: NamespaceId,
    me: EndpointId,
    keeps: bool,
    now: i64,
) -> Result<ReplicaHealth, FolderError> {
    let mut devices = vec![check(docs, store, namespace, me, keeps, now).await?];
    let mut others: Vec<ReplicaInfo> = reports(docs, store, namespace)
        .await?
        .into_iter()
        .filter(|(device, _)| *device != me)
        .map(|(device, report)| ReplicaInfo {
            device,
            you: false,
            keeps_copy: report.keeps,
            files: report.files,
            held: report.held,
            verified: report.verified,
        })
        .collect();
    others.sort_by_key(|info| info.device.to_string());
    devices.extend(others);
    Ok(ReplicaHealth {
        copies: devices.iter().filter(|info| info.complete()).count(),
        devices,
    })
}

/// Every hash the folder's own records reference, so collection keeps them.
pub async fn record_hashes(docs: &Docs, namespace: NamespaceId) -> Result<Vec<Hash>, FolderError> {
    Ok(folders::all_entries(docs, namespace)
        .await?
        .into_iter()
        .filter(|version| version.key.starts_with(REPLICA) || version.key.starts_with(HISTORY))
        .map(|version| version.hash)
        .collect())
}

/// Sets whether this device keeps a full copy of a folder. A device that does not still
/// receives the file list and the reports, and fetches no file until it keeps the folder.
pub async fn set_keep_copy(
    docs: &Docs,
    database: &Database,
    namespace: NamespaceId,
    keep: bool,
) -> Result<(), FolderError> {
    let doc = docs
        .open(namespace)
        .await
        .map_err(replica)?
        .ok_or(FolderError::UnknownFolder)?;
    let policy = if keep {
        DownloadPolicy::default()
    } else {
        let prefix = |text: &str| FilterKind::Prefix(text.as_bytes().to_vec().into());
        DownloadPolicy::NothingExcept(vec![prefix(REPLICA), prefix(HISTORY)])
    };
    doc.set_download_policy(policy).await.map_err(replica)?;
    if !database.set_keep_copy(namespace, keep)? {
        return Err(FolderError::UnknownFolder);
    }
    Ok(())
}
