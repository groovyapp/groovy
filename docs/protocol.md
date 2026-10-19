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

A contact can be set to auto-accept. Its offers are then accepted at once, and the user is not asked. Nothing is written to the user's disk by this step. Saving received files to a folder is part of receiving (#28).

## Blobs

Content is transferred with the `iroh-blobs` protocol on its own ALPN, served from the node's blob store in the data directory's `blobs` folder. The store is opened at startup, so content added earlier is still there after a restart.

The same trust check applies before any blob is served. A connection from an endpoint that is not trusted is closed with application code 3, so it receives no content. Blobs are fetched by hash, and a peer can only fetch what it can name.

Each blob added on this device gets a persistent tag, so it is kept across restarts and garbage collection. Garbage collection is not enabled yet. When it is, it will delete only blobs that have no tag, so content added through the node is never removed by it.

## Not yet specified

Messages after the handshake: transfer progress and resumption (#28, #29), chat (#41), and groups (#43). Each will get a section here when it is built.
