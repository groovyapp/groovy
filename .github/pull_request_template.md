## What this changes

## Why

## How it was tested

<!-- The checks you ran, and any test you added. For behaviour between devices, say whether it's covered in crates/groovy-core/tests/nodes.rs. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo deny check`

## Checklist

- [ ] Logs contain no message contents, file contents or secret keys.
- [ ] Anything a peer sends is checked before it's used, and untrusted peers get nothing they shouldn't.
- [ ] If the protocol or database changed: the version, migration and `docs/protocol.md` are updated.
- [ ] If the threat model changed: `docs/threat-model.md` is updated.
- [ ] `CHANGELOG.md` has an entry under Unreleased, if the change is visible to users.
