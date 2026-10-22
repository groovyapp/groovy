# Changelog

All notable changes to Groovy are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [semantic versioning](https://semver.org/). While Groovy is in alpha, a minor version may change the protocol or the data format. Groovy will say so here when it does.

## [Unreleased]

## [0.1.0] - 2026-10-22

The first alpha. Groovy is for trying out with people you trust. Local data is not encrypted by Groovy itself, so keep disk encryption on. See the [threat model](docs/threat-model.md) for what is and is not protected.

### Added

- **Identity and devices.** A persistent Ed25519 identity, kept in the operating system's credential store with a file fallback, an encrypted backup, and linking of several devices to one user. A device can be revoked.
- **Contacts and pairing.** Pairing with a six-digit code that both sides must check, trust and blocking, and presence with the connection path shown.
- **Connectivity.** Address lookup that is private by default, self-hosted relay support, and reconnection after network changes, sleep and wake.
- **File sharing.** Sending files and folders to a peer, verified and resumable downloads, a transfer manager with queue, pause and cancel, and share links for one-off sharing.
- **Shared folders.** Folders synced between members, with replica status, a choice of whether each device keeps a full copy, and conflict copies when two members edit the same file while apart.
- **Storage.** Storage quotas and garbage collection that keeps what folders and messages need.
- **Messaging.** Direct messages with delivery states, an outbox that retries offline delivery, attachments fetched from the sender, and search over history.
- **Group chat.** Groups over gossip, signed membership records, admins and roles, and removal with a new group key.
- **Desktop app.** Onboarding, a sidebar with Files, Messages, Peers, Storage and Settings, desktop notifications, a system tray with background mode, and keyboard navigation.
- **Command line.** `groovy-cli` to send and receive files.
- **Linux.** An AppImage with launcher integration, `groovy://` links, and a Flatpak manifest. The Flatpak has not been built yet.
- **Windows.** A zip with the program. There is no installer, and the program is not code signed, so Windows shows a warning on first run.
- **Update check.** A check against GitHub Releases at startup, with a banner and an opt-out.

### Known limits

- Local data is not encrypted beyond the operating system's disk encryption.
- Removed group members keep the messages they already received.
- Release signatures are not in place, so downloads cannot yet be verified.
- macOS is not packaged.
