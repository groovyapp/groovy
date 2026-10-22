# Threat model

This says what Groovy protects, against whom, and what it does not protect. It describes the code as it is for the alpha. The protocol is in [protocol.md](protocol.md), and the identity and device design is in [identity.md](identity.md). The decision on local data is [0002](decisions/0002-encryption-at-rest.md).

## What is worth protecting

- **Message and file contents.** Direct messages, group messages, attachments, and files in shares and shared folders.
- **Who talks to whom.** The contact list, the groups a person is in, and who is in each group.
- **Identity keys.** Each device's endpoint key, and the user key that signs its devices. Whoever holds one can act as that device or user.
- **Network metadata.** IP addresses, when devices are online, and how much they exchange.

## Who might be against it

- **A network observer,** such as a Wi-Fi operator, an ISP, or someone on the same network. They see the connections, not their contents.
- **A relay operator.** A relay forwards traffic a device cannot reach directly. It sees the addresses and endpoint IDs of both ends, and when and how much they exchange. It cannot read what they send.
- **A malicious peer that is trusted.** A contact or a device of a contact that you have accepted. It can send you anything within the protocol, keep what you send it, and pass on what it learns, such as the members of a group it is in.
- **A stranger.** An untrusted node can only ask to pair. Its connection is refused for everything else, and a message is limited to 64 KiB, so a stranger cannot make a device take in large amounts.
- **Someone with the device.** Anyone who can read the device's files, or unlock it while Groovy is signed in.

## What Groovy protects

- **Contents in transit.** Every connection is QUIC, with the peer's endpoint key. Relays see ciphertext. See [protocol.md](protocol.md#transport).
- **Who you are talking to.** Peers are identified by their keys, not by addresses. A peer that is not trusted may only ask to pair; everything else is refused. See [protocol.md](protocol.md#trust).
- **Pairing.** Both sides show the same six-digit code, which you check before accepting. A man in the middle would show a different code to each side.
- **Files and blobs.** Content is checked against its hash as it arrives, so a changed file is refused.
- **Group records.** Membership, roles and keys are signed. A record counts only if its signer was an admin when it was made, and each group message is signed by its author. See [protocol.md](protocol.md#group-chat).
- **Lost devices.** A device can be revoked, and its certificate is then refused by peers. See [identity.md](identity.md#revoking-a-device).

## What Groovy does not protect

- **Data on the device.** The database, the blob store and the downloads folder are plain files. Only the operating system's disk encryption protects them. Turn it on. See [0002](decisions/0002-encryption-at-rest.md).
- **Metadata from relays and peers.** Relays, and peers you connect to directly, learn your IP address and when you are online. Address lookup publishes your endpoint ID and addresses unless you turn it off. See the README's [privacy section](../README.md#privacy-properties-inherited-from-iroh).
- **Anything a trusted contact receives.** Groovy cannot stop a contact from keeping, copying or forwarding what you send them.
- **Removed group members.** A removed member keeps every group message it already received. Once a removal has reached the members who give out the new key, the removed member does not get it, so it cannot follow the group's new messages.
- **Deniability.** A group message is signed by its author, so a member can prove who wrote it. Direct messages are authenticated by the connection they travel on, not signed, so a recipient can show who sent them only by its own copy.
- **Anyone holding the device.** Messages on disk are not encrypted again. Whoever can read the device's files can read everything on it.

## Open questions

- **Public address lookup is on by default.** It is convenient, but it tells n0 who is looking up whom. Whether it should stay on by default is a decision for the alpha.
- **Encryption of local data beyond disk encryption** is not built. It is tracked as a follow-up in 0002.
- **Backups** are the device's data copied somewhere else. How they are encrypted is covered in the code's backup module, and is part of the security review in #57.
