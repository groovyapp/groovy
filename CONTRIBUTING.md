# Contributing to Groovy

Thanks for helping. Start with the [README](README.md) for the project layout and build steps. Before a pull request, make sure `cargo fmt --check`, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` pass.

## Logging

Use `tracing` macros. Log files are written to the data directory's `logs/` folder, and `RUST_LOG` sets the level (default `info`).

**Never log secrets or user content.** That includes secret keys, ticket secrets, file contents, file names from peers, and message bodies. Log identifiers (such as an endpoint ID) and sizes or counts instead.
