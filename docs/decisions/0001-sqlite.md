# 0001: Use SQLite for local state

Status: accepted

## Context

Groovy keeps structured local state that is not content-addressed: contacts, device profiles, transfer history, conversations and messages, and settings that change at runtime. Content itself lives in the blob store, see [storage](../storage.md) once it is written.

The database has to:

- search message history quickly (issue #46: under 200 ms across 100k messages);
- offer encryption at rest later, if the at-rest decision (#39) calls for it;
- evolve its schema across releases, with migrations that run on startup;
- stay easy to inspect while the project is young.

## Options

- **SQLite** (rusqlite, bundled): mature, full-text search through FTS5, an established encrypted variant (SQLCipher), and any SQLite browser can open the file.
- **redb**: a pure-Rust embedded key-value store with a simpler build and no SQL. Full-text search would need a custom index, and there is no comparable encrypted variant.

## Decision

Use **SQLite**, through `rusqlite` with its bundled library. The bundled build avoids a system dependency on Windows and Linux.

## Consequences

- The database file is `db/groovy.sqlite` in the data directory. Open it with the `sqlite3` CLI or DB Browser for SQLite to inspect it.
- Schema changes are migrations in `crates/groovy-core/src/db.rs`. Each runs in a transaction with its version bump, and a shipped migration is never edited.
- Calls block, so the database is used off the UI thread, from the node or background tasks.
- Encryption at rest, if chosen in #39, can use SQLCipher without changing the query code.
- WAL journaling and foreign-key enforcement are on.
