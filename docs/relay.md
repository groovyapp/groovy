# Running a relay

A relay server carries traffic between two devices when they cannot connect directly, for example behind strict firewalls. Relays only forward ciphertext (see the README's privacy section), but they do see which endpoints talk to each other and how much data passes. Groovy uses n0's public relays by default. A relay you run yourself keeps that metadata with you.

## Pointing Groovy at a relay

In `config.toml`:

```toml
[relay]
mode = "custom"
urls = ["https://relay.example.org./"]
# Only if the relay checks a shared token (see access control below):
auth_token = "the token"
```

- `mode = "public"` (the default) uses n0's public relays.
- `mode = "disabled"` uses no relays. Peers must reach each other directly.
- A custom list must contain at least one `https://` URL. A malformed URL stops the node with a message that names the setting.

Restart Groovy after changing this. Settings → Network can switch between public relays and none.

## Running a relay

The relay server is the `iroh-relay` crate, built with its `server` feature:

```bash
cargo install iroh-relay --features server
```

It reads a TOML config file. The fields that matter for a public relay are:

- `tls` with `hostname`, and `cert_mode`. `LetsEncrypt` obtains a certificate for the hostname. `Manual` reads one you provide, from `cert_dir`, `manual_cert_path` and `manual_key_path`.
- `http_bind_addr` and the `tls` bind addresses, for the ports the relay listens on.
- `access`, which controls which endpoints may relay (below).

Serve the relay on a hostname with a valid certificate. Groovy connects over HTTPS, so a plain-HTTP relay will not work for clients.

## Access control

By default a relay lets any endpoint relay through it. Restrict it with `access`:

- `access = "everyone"` (the default).
- `access.allowlist = ["<endpoint id>", ...]`: only these endpoints may relay. Endpoint IDs are shown on each Groovy device's Settings → Identity.
- `access.denylist = ["<endpoint id>", ...]`: everyone except these.
- `access.shared_token = ["token-a", "token-b"]`: only clients presenting one of these tokens. Put the token in the client's `auth_token`.
- `access.http`: the relay asks a URL of your choice for each endpoint; it grants access only if that URL answers `true`.

An allowlist needs no change on Groovy's side. A shared token needs `auth_token` set on every device that uses the relay.

## Limits of what is verified

The Groovy client side (the `[relay]` setting, its validation, and the settings row) is tested. A full deployment, with a relay reachable from another network and a device connecting through it, has not been run as part of this project. Check it on your own relay before relying on it.
