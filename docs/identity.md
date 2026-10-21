# Design: one user across several devices

Status: **built** (#36), with the three decisions at the end accepted as recommended. #37 depends on it.

## What a user is

A **user** is a key pair (Ed25519) generated on the first device. Its public key is the user's identity, and the name a contact knows. Each device keeps its own endpoint key, as now. A device is linked to a user by a **device certificate**: the user key signs the device's endpoint ID, a name for the device, and a creation time.

Contacts trust a user, not a device. Pairing with any one device of a user trusts all of that user's devices that hold a valid certificate.

## Certificates and what peers see

- A device certificate is stored locally and sent in the handshake, as part of the profile. Each handshake carries the sender's certificate, and the revocations the user has made.
- A peer accepts a certificate only if the signature checks against a user it already trusts, and the certificate names the endpoint that is connecting. A device with no certificate is treated as a single-device contact, as today.
- The set of a user's devices is the set of valid certificates the peer has seen. A device the peer has not heard from yet is not trusted by that peer until it connects with its certificate.

## Linking a new device

1. On an existing device, "Link a device" makes a **link ticket**: the user's public key, a random one-time secret of 32 bytes, and the device's address. It expires after ten minutes and is spent on first use.
2. The new device opens the ticket, and connects on a new protocol, `groovy-link/0`, that is served only while a link ticket is open.
3. The new device proves it has the secret, by a keyed hash over its own endpoint ID. Only then does the existing device send the user's secret key, sealed with ChaCha20-Poly1305 under a key derived from the secret. The one-time secret is never sent. Five wrong proofs close the link.
4. The new device stores the user key in the OS credential store, signs its own certificate, and the existing device signs one for the new device. Both devices now hold valid certificates for each other.

## Revoking a device

A device is revoked from any device of the user. The user key signs a **revocation** of that device's endpoint ID and a time. Revocations are kept, and sent to every contact on connect. A revoked endpoint is refused by a peer once that peer has the revocation.

## What this does and does not protect

- **Protects against:** a stolen or retired device, once the revocation has reached the peers that matter.
- **Does not protect against:** a revoked device that connects to a peer which has not yet received the revocation, or a peer that is offline when it is revoked. Such a peer learns the revocation the next time it connects to any device of the user.
- **Spreads risk:** every device holds the user key (see decision 1). A compromised device exposes the user key, and so every device the user has.

## Effect on sharing

Shared folders and messages are addressed to a user's devices through the trust rule above. A folder member is a user, so a join ticket works on any device of that user. This design does not change the folder model in `docs/storage.md`. Testing the full behaviour waits on the messaging work (#41 to #47).

## Acceptance (from #36)

- Messages and shared folders reach all of a user's linked devices. Checked with two devices of one user and a contact.
- A device linked with a ticket is trusted by the contact without re-pairing.
- A revoked device is refused by a contact that has the revocation.

## Decisions

All three were accepted as recommended.

1. **Where the user key lives.** Recommended: on every linked device, in the OS credential store. Simple, and matches "signed by existing devices". Alternative: keep it on one authority device and give the others only certificates, which means losing that device loses the ability to link or revoke.
2. **Revocation limits.** Recommended: accept the limits above, and say so in the README. Stronger guarantees need a server or a quorum.
3. **Whether contacts see the user name.** Recommended: yes, as a name the user chooses, shown with the device name. Devices are shown under the user.

## What was built

- `user.rs`: the certificate and revocation types, signing and checking, the key store (the OS credential store, or a file readable only by its owner), and the one query that decides whether a device is trusted.
- `link.rs`: the link ticket (`groovylink1…`), the `groovy-link/0` protocol, which answers only while a link is open, and the new device's side.
- The trust gate checks a certificate on every handshake, and takes it in only if it is for the connecting device and signed by the user's key. A device is trusted when a trusted contact belongs to the same user, and it has not been revoked or blocked.
- The Devices section of Settings lists the user's devices, makes and copies a link ticket, links this device from one, and revokes a device after a second click.
- A user's devices share their trusted contacts. A device that is linked receives the linking device's contacts, and two devices of the user exchange contacts each time they connect (at most every 30 seconds per pair). A contact blocked on a device stays blocked there. So a contact paired on one device can reach all of them, in both directions: the contact trusts the devices through the certificate, and the devices trust the contact.
- The link ticket is also shown as a QR code, for a device that can scan it.

## Known limits

- **A new device must greet a contact first.** A contact learns a new device's certificate from the handshake. The device is trusted from then on. A connection that skips the handshake, such as joining a shared folder by dialing the folder protocol directly, is refused until the device has connected to that contact on the Groovy protocol once.
- **A revocation reaches a contact only when a device of the user connects to it.** A contact that has not heard yet still trusts the revoked device.
- **Every linked device holds the user key.** Losing one exposes all of them until the others revoke it, and a revoked device that has the key can still sign.

## Not in this design

Key recovery if every device is lost (a backup of the user key is already possible with the identity backup in Settings, and that stays the recovery path). Changing a user's key. Linking more than one new device at a time.
