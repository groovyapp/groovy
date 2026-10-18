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
