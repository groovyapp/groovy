# Contributing to Groovy

Thanks for helping. Start with the [README](README.md) for the project layout and build steps. Before a pull request, make sure `cargo fmt --check`, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` pass.

## Testing

Three layers, each with its own place:

- **Unit tests** sit next to the code they test, in a `tests` module. Core modules use fakes in place of the OS keyring, so tests never touch a real credential store.
- **Integration tests** live in `crates/groovy-core/tests/`. They start several nodes in one process, each with its own data directory, and connect them over loopback. They need no network or relay.
- **UI tests** use GPUI's `TestAppContext`, which runs headless. They live in the module of the component they test. Use `TestAppContext::single()` and plain `#[test]` functions; the `#[gpui::test]` macro overflows the compiler's recursion limit in this crate.

Run everything with `cargo test --workspace`. CI runs the same command on Windows and Linux.

### Running two instances on one machine

Set `GROOVY_DATA_DIR` to a different directory for each instance. Each gets its own identity, config and database:

```bash
GROOVY_DATA_DIR=/tmp/groovy-a cargo run
GROOVY_DATA_DIR=/tmp/groovy-b cargo run
```

## Logging

Use `tracing` macros. Log files are written to the data directory's `logs/` folder, and `RUST_LOG` sets the level (default `info`).

**Never log secrets or user content.** That includes secret keys, ticket secrets, file contents, file names from peers, and message bodies. Log identifiers (such as an endpoint ID) and sizes or counts instead.

## Errors

- **`groovy-core` returns typed errors.** Each module defines its own error enum with `thiserror`, and public functions return it. Core code does not use `anyhow`.
- **The app uses `anyhow` at its edges.** Add context with `.context("…")` as errors cross a boundary, so the message says what was being attempted.
- **No `unwrap()` or `expect()` on fallible I/O or network work,** outside tests. The exceptions are startup invariants that cannot be recovered from, and lock poisoning, which only happens after an earlier panic.
- **Show short messages to people.** `{error:#}` prints the whole chain on one line. Don't show raw `Debug` output in the UI.

## Before you start

- For anything larger than a small fix, open an issue first and describe the problem. That saves you from building something that conflicts with a decision already made.
- For a security problem, don't open an issue or a pull request. Follow [SECURITY.md](SECURITY.md).
- The [threat model](docs/threat-model.md) and [protocol design](docs/protocol.md) say what Groovy promises and how peers talk. A change that alters either must update them too.

## Pull requests

1. Fork the repository and create a branch from `master`.
2. Run the checks CI runs:

   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   cargo deny check
   ```

3. Open a pull request. The template asks what changed, why, and how it was tested.

Keep a pull request to one change. Separate a refactor from a fix, so each is easy to review.

Write commit messages in the imperative mood, such as "Add a retry for failed deliveries". The first line says what the change does; the body says why. Don't rewrite a branch after review has started, unless the reviewer asks.

## Rules for code that touches data

- **Peers are untrusted until proven otherwise.** Every protocol request that acts on data checks that the peer is trusted first. A peer that is not trusted may only ask to pair. The handlers in `crates/groovy-core/src/node.rs` show the pattern.
- **Limits on peer data.** Check what a peer sends for size and shape before using it. The wire format refuses messages over 64 KiB before reading them. A new message type needs the same.
- **Protocol changes.** Add new request types at the end of the `Request` enum. Never reorder or reuse existing ones, because peers on older builds decode them by position.
- **Database changes.** Add a new migration to `crates/groovy-core/src/db.rs`. Never edit a migration that has shipped.
- **Tests that wait on the network** must time out with a failure, not hang.
- **Timing-sensitive tests** should be run under load before they are trusted: several copies at once, for example `for i in $(seq 1 16); do cargo test <name> & done; wait`.

## Releases

Releases are tags of the form `vX.Y.Z` on `master`. The release workflow in `.github/workflows/release.yml` builds the Linux AppImage and a Windows zip, and attaches them to the release. Update [CHANGELOG.md](CHANGELOG.md) in the same pull request as the version bump.

## Licence

Groovy is dual licensed under MIT and Apache-2.0. By contributing, you agree that your contribution is licensed the same way, as described in the README.
