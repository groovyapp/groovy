//! Groovy tickets: the text a person shares so a peer can dial them.
//!
//! A ticket is `groovy1` followed by base32 (no padding, lowercase) of a
//! postcard encoding of the endpoint ID and the addresses the node was
//! reachable at when the ticket was made. The `1` is the format version. A
//! `groovy://` link holds the same ticket and opens the app to the peer.

use std::io::Cursor;
use std::net::SocketAddr;
use std::str::FromStr;

use image::{ImageFormat, Luma};
use iroh::{EndpointAddr, EndpointId, PublicKey, RelayUrl, TransportAddr};
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const PREFIX: &str = "groovy";
const VERSION: u8 = 1;
const LINK_PREFIX: &str = "groovy://";

/// Why a pasted ticket cannot be used.
#[derive(Debug, Error)]
pub enum TicketError {
    #[error("paste a ticket first")]
    Empty,
    #[error("that is not a Groovy ticket. Groovy tickets start with \"groovy1\".")]
    NotATicket,
    #[error("this ticket is from a newer Groovy (format {0}). Update Groovy to use it.")]
    UnknownVersion(u8),
    #[error("this ticket is damaged or was cut short. Copy it again.")]
    Damaged,
    #[error("that is your own ticket. Give it to someone else, not to yourself.")]
    OwnTicket,
    #[error(
        "that ticket has no address to reach the peer. Ask them to share it again once they are online."
    )]
    NoAddresses,
    #[error("failed to make the QR code")]
    Qr(#[source] qrcode::types::QrError),
    #[error("failed to encode the QR code as a picture")]
    Image(#[source] image::ImageError),
}

/// The body of a version-1 ticket.
#[derive(Debug, Serialize, Deserialize)]
struct TicketV1 {
    id: [u8; 32],
    addrs: Vec<WireAddr>,
}

/// An address as it is written in a ticket. Other transports are not shared.
#[derive(Debug, Serialize, Deserialize)]
enum WireAddr {
    Ip(SocketAddr),
    Relay(String),
}

/// The ticket text for `addr`.
pub fn encode(addr: &EndpointAddr) -> String {
    let body = TicketV1 {
        id: *addr.id.as_bytes(),
        addrs: addr
            .addrs
            .iter()
            .filter_map(|transport| match transport {
                TransportAddr::Ip(socket) => Some(WireAddr::Ip(*socket)),
                TransportAddr::Relay(url) => Some(WireAddr::Relay(url.to_string())),
                _ => None,
            })
            .collect(),
    };
    // Serializing this plain struct cannot fail, so a failure is a bug.
    let bytes = postcard::to_allocvec(&body).expect("a ticket always serializes");
    let mut out = format!("{PREFIX}{VERSION}");
    data_encoding::BASE32_NOPAD.encode_append(&bytes, &mut out);
    out.make_ascii_lowercase();
    out
}

/// The `groovy://` link for `addr`, which opens the app to the same peer.
pub fn share_link(addr: &EndpointAddr) -> String {
    format!("{LINK_PREFIX}{}", encode(addr))
}

/// Reads a pasted ticket or link, and checks it can be dialled from this node.
pub fn parse(text: &str, our_id: EndpointId) -> Result<EndpointAddr, TicketError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(TicketError::Empty);
    }
    let body = trimmed
        .strip_prefix(LINK_PREFIX)
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let addr = decode(body)?;
    if addr.id == our_id {
        return Err(TicketError::OwnTicket);
    }
    if addr.addrs.is_empty() {
        return Err(TicketError::NoAddresses);
    }
    Ok(addr)
}

fn decode(text: &str) -> Result<EndpointAddr, TicketError> {
    let rest = text.strip_prefix(PREFIX).ok_or(TicketError::NotATicket)?;
    // The version is exactly one digit. Base32 also uses digits, so it cannot be
    // read as a run of digits.
    let mut chars = rest.chars();
    let version = chars
        .next()
        .and_then(|ch| ch.to_digit(10))
        .ok_or(TicketError::NotATicket)? as u8;
    if version != VERSION {
        return Err(TicketError::UnknownVersion(version));
    }
    let encoded = chars.as_str();
    let bytes = data_encoding::BASE32_NOPAD
        .decode(encoded.to_ascii_uppercase().as_bytes())
        .map_err(|_| TicketError::Damaged)?;
    let body: TicketV1 = postcard::from_bytes(&bytes).map_err(|_| TicketError::Damaged)?;

    let id = PublicKey::from_bytes(&body.id).map_err(|_| TicketError::Damaged)?;
    let addrs = body
        .addrs
        .into_iter()
        .map(|wire| match wire {
            WireAddr::Ip(socket) => Ok(TransportAddr::Ip(socket)),
            WireAddr::Relay(url) => RelayUrl::from_str(&url)
                .map(TransportAddr::Relay)
                .map_err(|_| TicketError::Damaged),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(EndpointAddr::from_parts(id, addrs))
}

/// A PNG of `text` as a QR code, at least 256 pixels square.
pub fn qr_png(text: &str) -> Result<Vec<u8>, TicketError> {
    let code = QrCode::new(text.as_bytes()).map_err(TicketError::Qr)?;
    let picture = code.render::<Luma<u8>>().min_dimensions(256, 256).build();
    let mut png = Vec::new();
    picture
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(TicketError::Image)?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn socket_addr() -> EndpointAddr {
        let socket: SocketAddr = "192.168.1.20:4433".parse().unwrap();
        EndpointAddr::from_parts(SecretKey::generate().public(), [TransportAddr::Ip(socket)])
    }

    fn relay_addr() -> EndpointAddr {
        let relay = RelayUrl::from_str("https://relay.example.org./").unwrap();
        EndpointAddr::from_parts(
            SecretKey::generate().public(),
            [
                TransportAddr::Relay(relay),
                TransportAddr::Ip("10.0.0.2:1234".parse().unwrap()),
            ],
        )
    }

    fn stranger() -> EndpointId {
        SecretKey::generate().public()
    }

    #[test]
    fn tickets_start_with_the_groovy_prefix_and_version() {
        assert!(encode(&socket_addr()).starts_with("groovy1"));
    }

    #[test]
    fn a_ticket_round_trips_with_direct_and_relay_addresses() {
        let addr = relay_addr();

        let parsed = parse(&encode(&addr), stranger()).unwrap();

        assert_eq!(parsed, addr);
    }

    #[test]
    fn the_link_form_and_surrounding_whitespace_are_accepted() {
        let addr = socket_addr();
        let pasted = format!("  {}  \n", share_link(&addr));

        assert_eq!(parse(&pasted, stranger()).unwrap(), addr);
    }

    #[test]
    fn an_empty_paste_is_explained() {
        assert!(matches!(parse("   ", stranger()), Err(TicketError::Empty)));
    }

    #[test]
    fn text_that_is_not_a_ticket_is_explained() {
        let err = parse("hello there", stranger()).unwrap_err();

        assert!(matches!(err, TicketError::NotATicket), "{err}");
        assert!(err.to_string().contains("not a Groovy ticket"), "{err}");
    }

    #[test]
    fn a_ticket_from_a_newer_format_asks_for_an_update() {
        let err = parse("groovy2abcdefgh", stranger()).unwrap_err();

        assert!(matches!(err, TicketError::UnknownVersion(2)), "{err}");
        assert!(err.to_string().contains("newer Groovy"), "{err}");
    }

    #[test]
    fn a_cut_short_ticket_is_called_damaged() {
        let full = encode(&socket_addr());
        let cut = &full[..full.len() - 12];

        let err = parse(cut, stranger()).unwrap_err();

        assert!(matches!(err, TicketError::Damaged), "{err}");
    }

    #[test]
    fn a_ticket_for_this_node_is_refused() {
        let addr = socket_addr();

        let err = parse(&encode(&addr), addr.id).unwrap_err();

        assert!(matches!(err, TicketError::OwnTicket), "{err}");
    }

    #[test]
    fn a_ticket_without_addresses_is_refused() {
        let bare = EndpointAddr::from_parts(SecretKey::generate().public(), []);

        let err = parse(&encode(&bare), stranger()).unwrap_err();

        assert!(matches!(err, TicketError::NoAddresses), "{err}");
    }

    #[test]
    fn the_qr_code_is_a_png() {
        let png = qr_png(&encode(&socket_addr())).unwrap();

        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
