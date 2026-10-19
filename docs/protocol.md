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

The avatar itself is not sent in the handshake. Only its hash is. Fetching a picture by hash is part of blob transfer (#26).

After a successful handshake, each side emits `PeerConnected` with the other's endpoint ID and profile. The connection stays open until either side closes it, then both emit `PeerDisconnected`.

## Trust

Before a connection is accepted, the node asks its peer gate whether the remote endpoint may connect. A blocked endpoint's connection is closed with application code 2 before the handshake, so it learns nothing and uses no protocol. The same check applies when this node dials a peer, which it will not do for a blocked one.

After each successful handshake, the peer is recorded as a contact with trust `pending`. Trust is only changed by the user, through pairing (#20) or a block. A handshake never changes it.

Blob, gossip and docs protocols (#26, #41, #43) must use the same gate before serving anyone.

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

## Not yet specified

Messages after the handshake: presence (#21), file offers and transfers (#27), chat (#41), and groups (#43). Each will get a section here when it is built.
