<p align="center">
  <img src="assets/icons/app-icon.svg" alt="Groovy icon" width="128" height="128">
</p>

<div align="center">

# Groovy

**A peer-to-peer platform for private file sharing, storage, and messaging, built on [iroh](https://iroh.computer).**

[![CI](https://github.com/thenewkenya/groovy/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/thenewkenya/groovy/actions/workflows/ci.yml)
[![Status](https://img.shields.io/badge/status-pre--alpha-red?style=flat-square)](#roadmap)
[![Version](https://img.shields.io/badge/version-0.0.1-blue?style=flat-square)](Cargo.toml)
[![Rust](https://img.shields.io/badge/rust-1.99%2B-orange?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/tools/install)
[![Edition](https://img.shields.io/badge/edition-2024-orange?style=flat-square)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)
[![Built on iroh](https://img.shields.io/badge/built%20on-iroh%201.3-7c3aed?style=flat-square)](https://iroh.computer)
[![License](https://img.shields.io/badge/license-MIT_OR_Apache--2.0-lightgrey?style=flat-square)](#license)

[Overview](#overview) · [Features](#features) · [Architecture](#architecture) · [Getting started](#getting-started) · [Roadmap](#roadmap) · [Contributing](#contributing)

</div>

---

## Overview

Groovy lets people and their devices share files, store data, and exchange messages **without relying on a central server to hold that data**. Peers connect directly, exchange end-to-end encrypted content, and keep control of where their files live.

Groovy's networking is built on [iroh](https://iroh.computer), an open-source Rust stack for connecting devices by public key. iroh establishes direct connections whenever possible, uses relay servers only as a fallback, and encrypts all traffic end to end. Groovy adds the sharing, storage, and messaging experience on top.

The goals are:

- **Privacy by default.** Content is encrypted in transit between peers, and relays forward only ciphertext.
- **No single point of control.** There is no central server that stores or can inspect user data.
- **Integrity.** Content is addressed by hash, so every received byte can be verified.
- **Simplicity.** One tool covers sharing, storage, and messaging.

> **Project status:** Groovy is in **pre-alpha**. The repository currently contains the project scaffold. The features below are design targets and are not yet implemented. See the [Roadmap](#roadmap).

## Features

| Capability | How it is built | Status |
| --- | --- | --- |
| **Peer-to-peer file sharing** | Content-addressed transfer with [`iroh-blobs`](https://docs.rs/iroh-blobs) over direct QUIC connections | 🚧 Planned |
| **Storage and replication** | Blobs held by peers, with replication policy implemented in Groovy | 🚧 Planned |
| **Direct messaging** | Custom ALPN protocol on iroh connections | 🚧 Planned |
| **Group messaging** | Topic-based broadcast with [`iroh-gossip`](https://docs.rs/iroh-gossip) | 🚧 Planned |
| **Shared state** | Multi-writer CRDT documents with [`iroh-docs`](https://docs.rs/iroh-docs) | 🚧 Planned |
| **Identity** | Ed25519 endpoint identities, used for both authentication and encryption | 🚧 Planned |
| **Peer discovery** | iroh address lookup via DNS, DHT, and local network (mDNS) | 🚧 Planned |
| **Connection sharing** | Shareable tickets via [`iroh-tickets`](https://docs.rs/iroh-tickets) | 🚧 Planned |
| **NAT traversal** | Direct hole-punched paths, with relay fallback provided by iroh | 🚧 Planned |
| **Cross-platform clients** | Command-line and desktop clients on a shared Rust core | 🚧 Planned |

## Architecture

Groovy is a layered system. The lower layers come from iroh, and Groovy builds the services and clients on top.

```text
┌──────────────────────────────────────────────────────────────┐
│  Clients         CLI · Desktop · (future: mobile)            │
├──────────────────────────────────────────────────────────────┤
│  Services        File sharing · Storage · Messaging          │
├──────────────────────────────────────────────────────────────┤
│  Data protocols  iroh-blobs · iroh-docs · iroh-gossip        │
├──────────────────────────────────────────────────────────────┤
│  Transport       iroh endpoints · QUIC · Router (ALPN)       │
├──────────────────────────────────────────────────────────────┤
│  Infrastructure  iroh-relay · DNS / DHT / mDNS lookup        │
└──────────────────────────────────────────────────────────────┘
```

| Layer | Responsibility | Provided by |
| --- | --- | --- |
| **Infrastructure** | Relay fallback for connections that cannot go direct, and address lookup for finding peers. | iroh (self-hosted relays planned for Groovy) |
| **Transport** | Authenticated, end-to-end encrypted QUIC connections. Incoming connections are routed to protocol handlers by ALPN. | iroh |
| **Data protocols** | Content transfer, shared documents, and broadcast messaging. | iroh-blobs, iroh-docs, iroh-gossip |
| **Services** | Groovy's file sharing, storage and replication policy, and messaging rules. | Groovy |
| **Clients** | User interfaces for the command line and desktop. | Groovy |

### Identity and encryption

Every Groovy node is an iroh **endpoint** identified by an Ed25519 public key. Peers dial each other by that key rather than by IP address, and all traffic is encrypted end to end.

### Privacy properties inherited from iroh

These follow from [iroh's security model](https://docs.iroh.computer/concepts/security-privacy). They are stated here so users can make informed decisions.

- **Relays forward ciphertext only.** A relay cannot read file contents, file names, or message contents. It can see the source and destination IP addresses, connection times, and the amount of data relayed, along with which device identities talk to each other.
- **Direct connections reveal IP addresses.** When two peers connect directly, they exchange their public IP addresses. Internal IP addresses are not published by default.
- **Public relays have no access control.** Any application that knows a public relay's URL can connect to it. n0 does not recommend public relays for production systems.
- **Tor is supported as a custom transport,** but both endpoints must support the same transport, so both need Tor configured.

## Getting started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) **1.99.0 or newer**, with edition 2024 support. The pinned GPUI needs a recent toolchain: its `gpui_util` crate uses `slice_as_array`, which is not stable on 1.92. CI checks this exact version. iroh 1.3 alone needs only 1.91.
- Windows or Linux. CI builds both.
- `git`, because GPUI is pulled from the [Zed repository](https://github.com/zed-industries/zed) at a pinned commit.
- On Linux, the system libraries listed in [CI](.github/workflows/ci.yml) (ALSA, fontconfig, Wayland, X11/xcb, xkbcommon, Vulkan, OpenSSL, zstd, D-Bus and pkg-config).

### Build and run

```bash
git clone https://github.com/thenewkenya/groovy.git
cd groovy
cargo build
cargo run
```

For an optimized build:

```bash
cargo build --release
```

The binary is `target/release/groovy` (`groovy.exe` on Windows).

Groovy keeps its config and data in a per-user directory: `~/.local/share/groovy` on Linux, and the platform's equivalent elsewhere. To use a different directory, for example to run two instances on one machine, set `GROOVY_DATA_DIR`:

```bash
GROOVY_DATA_DIR=/tmp/groovy-b cargo run
```

Logs go to `logs/` in the data directory (one file per day, the most recent seven kept) and to stderr. Set `RUST_LOG` to change the level, for example `RUST_LOG=debug cargo run`. See [CONTRIBUTING.md](CONTRIBUTING.md#logging) for what must never be logged.

Release builds on Windows run without a console window; debug builds keep one for logs.

### Test

```bash
cargo test
```

### Lint and format

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

### Planned dependencies

The following crates are the intended dependencies. They are not yet in `Cargo.toml`; they will be added in Phase 1.

| Crate | Version | Purpose |
| --- | --- | --- |
| [`iroh`](https://crates.io/crates/iroh) | 1.3 | Endpoints, connections, relays, address lookup |
| [`iroh-blobs`](https://crates.io/crates/iroh-blobs) | 0.103 | Content-addressed file transfer and storage |
| [`iroh-docs`](https://crates.io/crates/iroh-docs) | 0.101 | Multi-writer shared documents |
| [`iroh-gossip`](https://crates.io/crates/iroh-gossip) | 0.101 | Topic-based group messaging |
| [`iroh-tickets`](https://crates.io/crates/iroh-tickets) | 1.0 | Shareable connection tickets |
| [`iroh-relay`](https://crates.io/crates/iroh-relay) | 1.3 | Self-hosted relay server |

> **Note:** iroh's protocol crates (`iroh-blobs`, `iroh-docs`, `iroh-gossip`) are versioned independently of `iroh`. Pin exact versions in `Cargo.toml` once they are added.

## Project layout

This is a Cargo workspace. `groovy-core` holds the UI-free services and must not depend on GPUI. `groovy` is the desktop app, built on [GPUI](https://www.gpui.rs), the UI framework behind Zed.

```
crates/
  groovy-core/           Core services (networking, storage, messaging). No GPUI dependency.
    src/config.rs        Versioned config.toml with unknown keys preserved
    src/logging.rs       Daily log file, stderr output and panic logging
    src/db.rs            SQLite database with versioned migrations
    src/profile.rs       Local profile: display name and avatar, stored in the database
    src/avatar.rs        Avatar pictures reduced to 128px PNG thumbnails, with input limits
    src/backup.rs        Passphrase-encrypted (age) backups of the identity key
    src/identity.rs      Ed25519 identity in the OS credential store, with a file fallback
      ui/                  Component kit (buttons, inputs, dialogs, toasts, menus, icons); see ui/mod.rs
    src/paths.rs         Data directory layout and GROOVY_DATA_DIR override
  groovy/                Desktop app (GPUI)
    src/
      main.rs            Entry point
      data.rs            Opens the data directory and config before startup
      node.rs            Runs the Groovy node and forwards its events to the UI
      theme/             Theme struct and the Gruvbox / One Dark palettes
      title_bar/         Custom titlebar and window controls (Windows and Linux)
      window_frame.rs    Client-side decorations and native border color
      workspace/         Main window: sidebar, screens (Files, Messages, Peers, Storage, Settings) and the status bar
      onboarding.rs      First-run flow: welcome, display name, endpoint ID, backup note
      settings.rs          Settings screen: profile, appearance, network, identity, storage, advanced
      settings_profile.rs  Profile section of Settings: display name and picture
      backup.rs            Identity backup and restore: restore form and Settings section
    build.rs             Embeds the icon in the Windows executable
assets/
  icons/                 App icon: app-icon.svg (source), PNG sizes and app-icon.ico
  icons/tabler/          Tabler outline icons used by the UI (MIT, see its LICENSE)
  groovy.desktop         Linux desktop entry
scripts/
  generate_icons.py      Regenerates the PNGs and .ico from app-icon.svg
.github/workflows/       CI: fmt, clippy and release build on Windows and Linux
docs/
  decisions/             Architecture decision records (0001: SQLite for local state)
```

### Linux desktop entry

To show the icon in your launcher and taskbar, install the desktop entry and icon:

```bash
install -Dm644 assets/groovy.desktop ~/.local/share/applications/groovy.desktop
install -Dm644 assets/icons/app-icon.svg ~/.local/share/icons/hicolor/scalable/apps/groovy.svg
```

The desktop entry runs `groovy`, so put `target/release/groovy` on your `PATH`. To force client-side decorations on a compositor that draws its own:

```bash
GROOVY_WINDOW_DECORATIONS=client cargo run
```

## Customizing

### Renaming the app

The app name comes from the `name` field in `Cargo.toml`. `APP_NAME` in `src/main.rs` reads it at build time, and the Windows resource strings in `build.rs` use it too. To rename the app:

1. Change `name` in `Cargo.toml`.
2. Rename `assets/groovy.desktop` to match, and update its `Name`, `Exec`, `Icon` and `StartupWMClass` fields.
3. Update the install paths in this README.

The Linux app ID is `APP_NAME` in lowercase, and must match the desktop entry's file name.

### Changing the icon

Replace `assets/icons/app-icon.svg`, then regenerate the derived files (requires `cargo install resvg`):

```bash
python scripts/generate_icons.py
```

### Changing the theme

Each theme is a constructor in `src/theme/`. `theme::sync_with_appearance` in `src/theme/mod.rs` picks which one to use for light and dark mode. `Theme::one_dark()` is included as an alternative.

### Updating GPUI

GPUI is pinned to a commit of [Zed](https://github.com/zed-industries/zed) in `Cargo.toml`, in both `gpui` and `gpui_platform`. To move to a newer GPUI, change the `rev` in both entries, then run `cargo check` and fix any API changes.

## Roadmap

| Phase | Milestone | Status |
| --- | --- | --- |
| **0** | Project scaffold, build, and tooling | ✅ Complete |
| **1** | Identity and endpoints: persistent keys, endpoint creation, ticket exchange | ⬜ Not started |
| **2** | Connectivity: custom ALPN protocol with `Router`, self-hosted relay, discovery configuration | ⬜ Not started |
| **3** | File sharing: `iroh-blobs` integration, CLI send and receive with verified transfer | ⬜ Not started |
| **4** | Storage and replication: placement and replication policy on top of blobs | ⬜ Not started |
| **5** | Messaging: direct messages over a custom ALPN, group chat over `iroh-gossip`, shared state over `iroh-docs` | ⬜ Not started |
| **6** | Desktop client and public alpha | ⬜ Not started |

## Security

Groovy handles sensitive data, so security is a core requirement. Please **do not report security vulnerabilities in public issues**. Contact the maintainer privately so the issue can be fixed before disclosure.

Groovy depends on iroh for transport security. Vulnerabilities in iroh itself should be reported to the iroh maintainers, n0 computer, through their channels.

The project will publish its threat model and protocol design before the first public release. Until then, **do not use Groovy to protect data you cannot afford to lose or expose.**

## Contributing

Contributions are welcome once the core architecture is in place. Until then, the most helpful contributions are design discussion, and issues describing use cases, threat scenarios, or requirements.

1. Fork the repository and create a feature branch.
2. Make your change, with tests where applicable.
3. Ensure `cargo fmt --check`, `cargo test` and `cargo clippy --all-targets -- -D warnings` pass.
4. Open a pull request describing the change and its motivation.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

Groovy's dependencies are licensed separately. iroh and its protocol crates are dual-licensed under MIT OR Apache-2.0.

## Acknowledgements

Groovy is built on [iroh](https://iroh.computer) by [n0 computer](https://n0.computer).

---

<div align="center">

Built with 🦀 Rust · Powered by iroh · Made for peers

</div>
