# 0002: Encryption at rest for local data

Status: **accepted for alpha**, recorded on behalf of the owner, who can overturn it. Relates to #39 and `docs/storage.md`.

## Context

Content is encrypted in transit: every connection is QUIC with the peer's endpoint key, and blobs are verified by hash as they arrive. On disk, the blob store, the database and the downloads folder are plain files. The identity key is kept in the OS credential store where one exists, with a file fallback, and is already protected by the operating system.

## Options

1. **Rely on operating-system full-disk encryption, and document it.** BitLocker, FileVault and Linux disk encryption cover the whole data directory, including the blob store, database, logs and any files saved to the downloads folder.
2. **Encrypt the database (SQLCipher) and keep blobs plain.** Protects contacts and history, but not the content, and adds a native dependency and a key to manage.
3. **Encrypt blobs per folder.** Protects content, but breaks content addressing and deduplication, and needs a key per folder with its own recovery story.

## Decision

Use option 1 for alpha. Document it, and do not build application-level encryption yet.

## Reasons

- Option 2 protects the least sensitive part: the content is still plain, and the metadata is small. It adds a key that loses data if it is lost, and a second cipher to maintain.
- Option 3 changes what a hash means. Two copies of one file would stop matching, which breaks the sync and transfer design in #26 to #35.
- The disk, not the app, is the usual target. Full-disk encryption covers it, and most desktop systems turn it on by default.

## Consequences

- The alpha README says that local data is protected only by the operating system's disk encryption, and that a device without it is exposed.
- The downloads folder is part of the same exposure, and is not encrypted by the app.
- If a user needs protection that full-disk encryption does not give (shared machines, backups to untrusted storage), that is a follow-up. The likely shape is option 2, and it should be its own issue with a key-recovery design.

## Follow-up

Document the requirement in the README before the alpha. Open a separate issue for option 2 if it is needed after alpha feedback.
