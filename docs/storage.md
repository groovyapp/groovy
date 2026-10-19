# Storage model: shared folders and replication

Status: **proposal**, for review. Nothing here is built yet, apart from blobs (#26), shares, and transfers (#27 to #30), which this builds on. The open decisions at the end need an answer before #35 starts.

## What a shared folder is

A shared folder is an [iroh-docs](https://docs.rs/iroh-docs) replica. Each entry maps a relative path to a blob hash, plus its size and modification time. The folder's contents are the blobs those hashes name. The replica is signed by its members' keys, so an entry can be checked against the author who wrote it.

Keys are relative paths, using `/` as the separator on every platform, with no leading slash and no `..`. They pass through the same safety rules as received names (#28): a path that would leave the folder is refused.

## What lives where

| Data | Where | Why |
|---|---|---|
| Path to hash, size, modified time, author | iroh-docs replica | Syncs between members, and is signed |
| The bytes of each file | Blob store (iroh-blobs) | Content addressed, verified as they arrive |
| Which folders this device follows, and where they are saved on disk | Local database | Local choices; never shared |
| Transfer history, share tickets, contacts, trust | Local database | Already there (#29, #30) |
| Which blobs are kept, and why | Blob tags | iroh-blobs keeps only tagged content |
| Replication targets and quotas | Local database | Local policy |

Members never see each other's local paths, downloads folder, or quotas. Only the replica's entries are shared.

## Deletes, renames and conflicts

- **Delete** writes a tombstone entry for the path, rather than removing it. Tombstones let a delete reach members who are offline, and are kept until every member has seen them. The file's blob is then no longer needed by that path.
- **Rename** is a delete of the old path and a create of the new one. The blob is the same hash, so nothing is copied again.
- **Conflicts** happen when two members change the same path while apart. iroh-docs keeps both entries. Recommendation: show the entry with the later modification time as the file, and keep the other as a copy named `name (conflict from <member>, <date>).ext`. Nothing is lost, and the user decides what to do with the copy. A conflict is shown in the folder's status, not hidden.

## Keeping content alive, and garbage collection

iroh-blobs deletes content that no tag protects, when garbage collection runs. We tag, in this order of ownership:

1. **Each shared folder** holds one tag per hash it references. Removing an entry from the folder removes its tag, once no folder and no transfer needs it.
2. **Each completed transfer** holds a tag on what it received (already done in #28).
3. **Each active share** holds a tag on its root and files (#30), until it is stopped.
4. **Each file the user chose to keep offline** holds a tag, even if it is not in a folder.

Garbage collection runs at an interval, and only removes blobs with no tags. Turning it on is part of #38. Until then nothing is removed, so disk use only grows.

## What replication promises

Recommendation, for the alpha: **a folder you follow is kept in full on this device. Replication to other members is best effort.** A member's copy is available while that member is online, and the folder shows how many members currently hold all of it.

We do not promise that a file survives on the network if every member goes offline, or that a member's device keeps it forever. The status shows "held by N of M members, last seen <time>" so the user can see the risk.

## Encryption at rest

Recommendation: **not required for alpha.** Data on disk is protected by the operating system's account encryption (such as BitLocker, FileVault, or the Linux disk encryption the user chose). The identity key is already kept in the OS credential store where one exists (#16). Encrypting the database and blob store with a key we manage is a larger change, with its own key-loss risks, and belongs to the evaluation in #39.

## Open decisions

These are the owner's to answer. The recommendations above assume the answers shown.

1. **Encryption at rest for alpha.** Recommended: no. Relies on the OS.
2. **What replication promises.** Recommended: best effort, with the status shown to the user, as above.
3. **Conflict copies.** Recommended: keep both, with the later one as the file. Alternative: keep only the later one.

## Next steps

- #35 builds the replica and the folder entries, following the rules above.
- #38 turns on garbage collection and quotas, once folders hold tags.
- #39 evaluates encryption at rest, which this document defers.
