# Groovy protocol, version 0

This describes how two Groovy nodes talk over an iroh connection. The code is in `crates/groovy-core/src/protocol.rs`.

## Transport

- A Groovy connection is an iroh QUIC connection with ALPN `groovy/0`. A node only accepts connections that offer this ALPN.
- Connections are end-to-end encrypted and authenticated by endpoint ID (the Ed25519 public key). Relays forward ciphertext only.

## Version

- The ALPN carries the major version. The `groovy/0` ALPN and the `PROTOCOL_VERSION` value in the handshake must match.
- A peer on another version does not offer our ALPN, so the TLS handshake fails with `peer doesn't support any known protocol`. The node reports this as "the peer does not speak the Groovy protocol".
- If a peer does offer our ALPN but sends another version in its `Hello`, the listener still answers with its own `Hello` before closing. The dialer can then report both versions.

## Messages

Messages are a 4-byte big-endian length followed by a [postcard](https://postcard.jamesmunns.com) encoding. A message longer than 64 KiB (65,536 bytes) is refused before its body is read, and the connection is closed.

## Handshake

Each connection begins with one bidirectional stream:

1. The dialer opens the stream and sends a `Hello`.
2. The listener reads it and sends its own `Hello`.
3. If the versions differ, the side that sees the mismatch closes the connection with application code 1. The listener waits up to 5 seconds for the dialer to read its answer first.

```
Hello {
    protocol_version: u16,      // 0
    profile: PeerProfile {
        display_name: String,   // what the user chose
        avatar_hash: Option<[u8; 32]>,  // BLAKE3 of the avatar PNG
    },
}
```

The avatar itself is not sent in the handshake. Only its hash is. A picture is fetched by its hash as a blob (see Blobs).

After a successful handshake, each side emits `PeerConnected` with the other's endpoint ID and profile. The connection stays open until either side closes it, then both emit `PeerDisconnected`.

## Trust

Before a connection is accepted, the node asks its peer gate whether the remote endpoint may connect. A blocked endpoint's connection is closed with application code 2 before the handshake, so it learns nothing and uses no protocol. The same check applies when this node dials a peer, which it will not do for a blocked one.

After each successful handshake, the peer is recorded as a contact with trust `pending`. Trust is only changed by the user, through pairing (#20) or a block. A handshake never changes it.

Blob, gossip and docs protocols (#41, #43) must use the same gate before serving anyone. Blobs already do (see Blobs).

## Pairing

After the handshake, either side may open a new bidirectional stream and send a `Request`. Version 0 has one request:

```
Request::Pair
Reply::Accepted | Reply::Declined
```

The requester writes `Pair`, and the answering side replies on the same stream:

- If the requester is already trusted, the answer is `Accepted` at once.
- If the requester has sent too many pairing requests recently (3 in 60 seconds), the answer is `Declined` at once, and the user is not asked.
- Otherwise the answering node's user is asked. Their answer is sent as `Accepted` or `Declined`.

Both sides compute the same verification code from the two endpoint IDs, so the users can compare it. The code is six digits, written as `123 456`. An accepting node records the other as trusted, and so does the requester when it receives `Accepted`. A declined pairing changes nothing.

An untrusted peer may send only `Pair`. Any other request is refused, and the connection is closed.

## Presence

A trusted contact may be checked for presence with `Request::Ping`. The checking node dials the contact, runs the handshake, sends `Ping` on a new stream, and expects `Reply::Pong`. It then closes the connection, so nothing stays open between checks. A contact that is not trusted by the answering node gets no `Pong`, and the check fails.

The route is read from the connection's selected path: `Direct` if it is a direct path, `Relayed` if it goes through a relay. A contact that does not answer is checked again after a delay that starts at one minute and doubles, up to 15 minutes. Each delay varies by up to a tenth either way, so contacts do not all retry together. When this node's network changes (its address changes), every contact is checked again at once.

## Files

A trusted peer may offer files. The sender imports them into its blob store as one collection, and sends an offer on a new stream:

```
Request::Offer(FileOffer { names, root, files, bytes, from })
Reply::Accepted | Reply::Declined
```

`names` are the top-level names the user chose (a file's name, or a folder's), and `root` is the collection's hash. The collection lists every file with its path, such as `photos/sub/b.bin`, so the offer stays small however many files there are. `from` is where the sender can be reached for the blobs.

Only a trusted peer's offer is shown to the user. An untrusted peer's offer gets no answer, and its stream is dropped. When the user accepts, the node answers `Accepted`, then fetches the collection from the sender over the blobs protocol. Every block is checked against its hash as it arrives. Declining answers `Declined`, and nothing is fetched.

A contact can be set to auto-accept. Its offers are then accepted at once, and the user is not asked.

Receiving is checked as it goes, and saved to the downloads folder when it is done. Bytes already in the store are not fetched again, so an interrupted receive can be retried with the same offer. Each file is written under the path it had in the collection, with a number added if its name is taken. Pausing and cancelling a receive are local to the receiving node, and the sender is not told. A sender can cancel an offer while it waits, which closes the connection, and the recipient drops the offer. A sender can also cancel an accepted offer, which stops serving its files to everyone, trusted peers included. A sender can pause an accepted send, which stops serving its files until it is resumed. A send still waiting for its answer cannot be paused, only cancelled. Each name from a peer is made safe before anything is written: empty, `.` and `..` parts are dropped, so nothing can be written outside the downloads folder. Saving received files to a folder is part of receiving (#28).

## Blobs

Content is transferred with the `iroh-blobs` protocol on its own ALPN, served from the node's blob store in the data directory's `blobs` folder. The store is opened at startup, so content added earlier is still there after a restart.

The same trust check applies before any blob is served. A connection from an endpoint that is not trusted is closed with application code 3, so it receives no content. Blobs are fetched by hash, and a peer can only fetch what it can name.

Each blob added on this device gets a persistent tag, so it is kept across restarts and garbage collection. Garbage collection is not enabled yet. When it is, it will delete only blobs that have no tag, so content added through the node is never removed by it.

## Shares

A share offers files to anyone who has its ticket, while this node is online. The ticket is an iroh-blobs ticket: this node's address and the root hash of the share's collection.

Blob requests are decided one at a time. A trusted contact may fetch any blob this node holds. Anyone else may fetch only the root and the files of an active share, which includes the metadata that holds their names. Every other request is refused. A blocked peer is refused before any request.

Revoking a share stops it at once: new requests for its content are refused. A download that is already running is not stopped. A ticket carries an address, so it works only while the node can be reached at that address.

Previewing a ticket fetches the list of names and the sizes, not the files. Receiving a share makes it a transfer, the same as an accepted offer.

## Shared folders

A shared folder syncs over two protocols this node serves: the iroh-docs sync protocol (ALPN `/iroh-sync/1`) and iroh-gossip (ALPN `/iroh-gossip/1`). Both are served only to trusted contacts, and a connection from anyone else is closed with application code 3 before either protocol sees it.

A folder's ticket names one of its members and the folder's capability, with read or write access. Joining a folder imports it and syncs with the members named in the ticket. Each entry is a relative path and the hash of its file. A delete is an entry with the empty hash. See `docs/storage.md` for the model, and for what is not built yet (conflict copies).

## Direct messages

A message to a trusted contact is sent as `Request::Message(WireMessage)` on a new stream. The `WireMessage` carries a version (`2`, which added attachments; version 1 messages are refused), a random 16-byte id, the sender's timestamp in milliseconds, the body (at most 4 KiB, trimmed), and an optional `reply_to` id. The author is the connection's peer, so a message cannot name another author.

The receiver stores the message, unless a message with that id is already stored, and answers `Reply::Stored`. A resent message is acknowledged again without a second event. A receiver that does not trust the sender, or that does not read the version, gives no answer, and the sender sees the message as failed.

Each side keeps its copy with a delivery state: `sending` (queued, not yet written to a device), `sent` (written, not yet acknowledged), `delivered` (acknowledged by a device), or `failed` (no device took it within seven days). Conversations are ordered by `(timestamp, id)`. A sender never stamps a message earlier than its previous message to the same contact. Clock skew moves a message's place but never loses or duplicates it.

A message is queued for each device that must have it: the contact's own endpoint, and each of the contact's linked devices that has not been revoked. A queued message is retried when the device connects to this node, at startup, and every minute as its retry time comes. Retries back off from 30 seconds to 15 minutes. A device is dialed at the address it last connected from, and by its id when none is known. The state becomes `delivered` as soon as any one device acknowledges it.

## Not yet specified

### Attachments

A message can carry up to eight attachments. Each names a file, gives the root of the collection holding it, its size, and an optional preview: a small PNG for an image, at most 32 KiB, so a preview travels inside the message. The file itself is not sent with the message. The recipient fetches it on request, as a transfer from the sender, which shows progress and can be paused like any other receive. The sender keeps the collection tagged while the message refers to it.

## Group chat

A group has a random 32-byte id, and its gossip topic is that id. A group message is a `GroupEnvelope`: version, group, a random id, the author's endpoint id, timestamp, body, optional `reply_to`, and a signature by the author's key over every other field. A member accepts a message only if its signature verifies and its author is the member or a trusted contact. A relayed message therefore still shows its real author.

A ticket (`groovygroup1…`, base32 of postcard) invites someone: the group id, its founder, the membership document's ticket, the group's current key record, and up to eight members' addresses to join through. Gossip itself accepts only trusted peers, as the other gossip uses do.

A member that connects to another member asks it for what it missed with `Request::GroupHistory { group, after }`, where `after` is the newest message the asker has. The answer is a `GroupPage`: up to eight envelopes in order, a flag saying whether more follow, and the addresses of the group's other members the answering device knows. Each envelope carries its signature, so a page can come from any member. Only trusted peers get an answer.

### Membership and keys

Membership is kept in the group's iroh-docs document as signed records: a name, and a member's role (admin, member) or removal. Records are replayed in time order, and a record counts only if its signer was an admin when it was made. The founder is always an admin. The document is writable by every member, so a record from a non-admin is ignored rather than refused.

Each group has a key that starts at epoch 0. The group's gossip topic is derived from the key, so removing a member changes the topic. The admin who removes a member signs a new key at the next epoch, moves onto the new topic, and pushes the key to each remaining member with `Request::GroupKeyPush`. The key is never written into the document, since every member can read the document, including one who was removed.

A member asks for the current key with `Request::GroupKey`, and only a current member gets an answer. A device that has been invited asks an admin with `Request::GroupJoin`; the admin writes its membership record and answers with the key. A key is accepted only if it is newer, and signed by an admin or the founder.

Known limits: a member that has not yet seen a removal in the document can still give the removed member the new key, until the removal reaches it. Changing the document's authors and the mutual trust between members are not checked beyond the signatures. Roles can be changed by any admin, not only the founder.

Groups are not yet on screen (#47).

## Not yet specified

Attachments (#45) and search (#46) are not built. Each will get a section here when it is.
