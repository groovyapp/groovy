//! Groovy's wire protocol on iroh connections.
//!
//! The ALPN is `groovy/0`. A connection opens with a handshake on one
//! bidirectional stream: the dialer sends a [`Hello`], and the listener
//! answers with its own. Both carry the protocol version and the sender's
//! profile. Messages are a 4-byte big-endian length followed by a postcard
//! encoding, and no message may exceed [`MAX_MESSAGE_BYTES`].
//!
//! The listener always answers with its version before closing a connection
//! for a version mismatch, so the dialer can say what went wrong.

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::{EndpointAddr, EndpointId};
use iroh_blobs::Hash;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

/// The ALPN for this protocol version. A peer on another major version does
/// not offer it, so the connection fails before the handshake.
pub const ALPN: &[u8] = b"groovy/0";
/// The version carried in [`Hello`]. Must match the `0` in [`ALPN`].
pub const PROTOCOL_VERSION: u16 = 0;
/// Longest message accepted, so a peer cannot make us allocate without limit.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Application-level close code for a version mismatch.
const CLOSE_VERSION_MISMATCH: u32 = 1;
/// How long the listener waits for a refused dialer to read our answer before closing.
const ANSWER_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// What a peer shows others about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerProfile {
    pub display_name: String,
    /// BLAKE3 hash of the avatar's PNG. The picture itself is fetched separately.
    pub avatar_hash: Option<[u8; 32]>,
}

/// The first message each side sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub profile: PeerProfile,
}

impl PeerProfile {
    /// What peers see of a stored profile. The picture is shared by its hash only.
    pub fn from_profile(profile: &crate::profile::Profile) -> Self {
        Self {
            display_name: profile.display_name.clone(),
            avatar_hash: profile.avatar_png.as_deref().map(avatar_hash),
        }
    }
}

/// The first message on a stream opened after the handshake.
///
/// A peer that is not trusted may only send `Pair`. Any other request is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Pair,
    /// Asks a trusted peer whether it is reachable. Only trusted peers may send it.
    Ping,
    /// Offers files to a trusted peer. The peer answers with [`Reply::Accepted`] or
    /// [`Reply::Declined`] once its user has decided.
    Offer(FileOffer),
}

/// What a sender offers: the top-level names being sent, and the collection that
/// holds them. The full listing is in the collection, so it is fetched only after
/// the recipient accepts, which keeps this message small however many files there are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOffer {
    /// The names of the files and folders at the top of the offer.
    pub names: Vec<String>,
    /// The root of the collection holding every file.
    pub root: Hash,
    /// How many files the collection holds.
    pub files: u64,
    /// The total size of those files, in bytes.
    pub bytes: u64,
    /// Where the sender can be reached for the blobs.
    pub from: EndpointAddr,
}

/// The answer to a [`Request::Pair`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Accepted,
    Declined,
    /// The answer to [`Request::Ping`].
    Pong,
}

/// A peer we have completed a handshake with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub endpoint_id: EndpointId,
    pub profile: PeerProfile,
}

/// Errors from speaking the protocol.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("the peer runs Groovy protocol version {theirs}; this build speaks version {ours}")]
    IncompatibleVersion { ours: u16, theirs: u16 },
    #[error("the peer closed the connection before the handshake finished")]
    Closed,
    #[error("a message of {0} bytes is larger than the {MAX_MESSAGE_BYTES}-byte limit")]
    TooLarge(usize),
    #[error("failed to encode a message")]
    Encode(#[source] postcard::Error),
    #[error("failed to decode a message")]
    Decode(#[source] postcard::Error),
    #[error(
        "the peer does not speak the Groovy protocol (version {PROTOCOL_VERSION}), so it cannot connect"
    )]
    UnsupportedProtocol,
    #[error("the connection failed")]
    Connection(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// The BLAKE3 hash of an avatar's PNG bytes.
pub fn avatar_hash(png: &[u8]) -> [u8; 32] {
    *blake3::hash(png).as_bytes()
}

/// Writes one length-prefixed message.
pub async fn write_message<T: Serialize>(
    send: &mut SendStream,
    value: &T,
) -> Result<(), ProtocolError> {
    let body = postcard::to_allocvec(value).map_err(ProtocolError::Encode)?;
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::TooLarge(body.len()));
    }
    let length = (body.len() as u32).to_be_bytes();
    send.write_all(&length)
        .await
        .map_err(|error| ProtocolError::Connection(Box::new(error)))?;
    send.write_all(&body)
        .await
        .map_err(|error| ProtocolError::Connection(Box::new(error)))
}

/// Reads one length-prefixed message. Refuses messages over the size limit before reading them.
pub async fn read_message<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T, ProtocolError> {
    let mut length = [0u8; 4];
    recv.read_exact(&mut length)
        .await
        .map_err(|_| ProtocolError::Closed)?;
    let size = u32::from_be_bytes(length) as usize;
    if size > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::TooLarge(size));
    }
    let mut body = vec![0u8; size];
    recv.read_exact(&mut body)
        .await
        .map_err(|_| ProtocolError::Closed)?;
    postcard::from_bytes(&body).map_err(ProtocolError::Decode)
}

fn check_version(theirs: u16) -> Result<(), ProtocolError> {
    if theirs == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::IncompatibleVersion {
            ours: PROTOCOL_VERSION,
            theirs,
        })
    }
}

/// Dialing side: runs the handshake on an open connection and returns the peer.
pub async fn handshake_outgoing(
    connection: &Connection,
    ours: PeerProfile,
) -> Result<PeerInfo, ProtocolError> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| ProtocolError::Connection(Box::new(error)))?;
    write_message(
        &mut send,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            profile: ours,
        },
    )
    .await?;
    let theirs: Hello = read_message(&mut recv).await?;
    if let Err(error) = check_version(theirs.protocol_version) {
        connection.close(CLOSE_VERSION_MISMATCH.into(), b"protocol version");
        return Err(error);
    }
    let _ = send.finish();
    Ok(PeerInfo {
        endpoint_id: connection.remote_id(),
        profile: theirs.profile,
    })
}

/// Listening side: answers the handshake and returns the peer.
pub async fn handshake_incoming(
    connection: &Connection,
    ours: PeerProfile,
) -> Result<PeerInfo, ProtocolError> {
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|error| ProtocolError::Connection(Box::new(error)))?;
    let theirs: Hello = read_message(&mut recv).await?;
    // Answer first, even on a mismatch, so the dialer learns our version.
    write_message(
        &mut send,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            profile: ours,
        },
    )
    .await?;
    if let Err(error) = check_version(theirs.protocol_version) {
        let _ = send.finish();
        // Give the dialer time to read our answer, or it may lose it when we close.
        let _ = tokio::time::timeout(ANSWER_GRACE, connection.closed()).await;
        connection.close(CLOSE_VERSION_MISMATCH.into(), b"protocol version");
        return Err(error);
    }
    let _ = send.finish();
    Ok(PeerInfo {
        endpoint_id: connection.remote_id(),
        profile: theirs.profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips_through_postcard() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            profile: PeerProfile {
                display_name: "Ada".to_owned(),
                avatar_hash: Some([7; 32]),
            },
        };

        let bytes = postcard::to_allocvec(&hello).unwrap();
        let decoded: Hello = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(decoded, hello);
    }

    #[test]
    fn the_alpn_names_the_protocol_version() {
        assert_eq!(ALPN, format!("groovy/{PROTOCOL_VERSION}").as_bytes());
    }

    #[test]
    fn other_versions_are_refused_with_both_numbers() {
        let err = check_version(PROTOCOL_VERSION + 1).unwrap_err();

        assert!(matches!(
            err,
            ProtocolError::IncompatibleVersion { ours: 0, theirs: 1 }
        ));
    }

    #[test]
    fn a_stored_profile_is_shared_by_name_and_avatar_hash() {
        let stored = crate::profile::Profile {
            display_name: "Ada".to_owned(),
            avatar_png: Some(b"png".to_vec()),
        };

        let shared = PeerProfile::from_profile(&stored);

        assert_eq!(shared.display_name, "Ada");
        assert_eq!(shared.avatar_hash, Some(avatar_hash(b"png")));
        let without = PeerProfile::from_profile(&crate::profile::Profile {
            display_name: "Ada".to_owned(),
            avatar_png: None,
        });
        assert_eq!(without.avatar_hash, None);
    }

    #[test]
    fn avatar_hashes_are_stable_and_content_based() {
        assert_eq!(avatar_hash(b"png"), avatar_hash(b"png"));
        assert_ne!(avatar_hash(b"png"), avatar_hash(b"jpg"));
    }
}
