//! Tickets: the text a person shares so a peer can dial them.
//!
//! A ticket is an iroh endpoint ticket: the endpoint ID plus the addresses the
//! node is reachable at when the ticket was made. Peers paste it, or open a
//! `groovy://` link that contains it.

use std::io::Cursor;

use image::{ImageFormat, Luma};
use iroh::{EndpointAddr, EndpointId};
use iroh_tickets::{ParseError, endpoint::EndpointTicket};
use qrcode::QrCode;
use thiserror::Error;

const LINK_PREFIX: &str = "groovy://";

/// Why a pasted ticket cannot be used.
#[derive(Debug, Error)]
pub enum TicketError {
    #[error("paste a ticket first")]
    Empty,
    #[error(
        "that is not a Groovy ticket. Tickets start with \"endpoint\" and come from the other person's Groovy."
    )]
    Malformed(#[source] ParseError),
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

/// The ticket text for `addr`.
pub fn encode(addr: &EndpointAddr) -> String {
    EndpointTicket::from(addr.clone()).to_string()
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
    let ticket: EndpointTicket = body.parse().map_err(TicketError::Malformed)?;
    let addr: EndpointAddr = ticket.into();
    if addr.id == our_id {
        return Err(TicketError::OwnTicket);
    }
    if addr.addrs.is_empty() {
        return Err(TicketError::NoAddresses);
    }
    Ok(addr)
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
    use std::net::SocketAddr;

    use iroh::{SecretKey, TransportAddr};

    use super::*;

    fn addr_with_socket() -> EndpointAddr {
        let socket: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        EndpointAddr::from_parts(SecretKey::generate().public(), [TransportAddr::Ip(socket)])
    }

    fn stranger() -> EndpointId {
        SecretKey::generate().public()
    }

    #[test]
    fn a_ticket_round_trips_to_the_same_peer() {
        let addr = addr_with_socket();

        let parsed = parse(&encode(&addr), stranger()).unwrap();

        assert_eq!(parsed, addr);
    }

    #[test]
    fn the_link_form_and_surrounding_whitespace_are_accepted() {
        let addr = addr_with_socket();
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

        assert!(matches!(err, TicketError::Malformed(_)), "{err}");
        assert!(err.to_string().contains("not a Groovy ticket"), "{err}");
    }

    #[test]
    fn a_ticket_for_this_node_is_refused() {
        let addr = addr_with_socket();

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
        let png = qr_png(&encode(&addr_with_socket())).unwrap();

        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
