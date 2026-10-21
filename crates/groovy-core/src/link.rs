//! Linking a new device to a user.
//!
//! An existing device opens a link and shows a ticket: the user's public key, a one-time secret,
//! and where to reach it. The new device connects on `groovy-link/0`, which answers only while a
//! link is open, and proves it holds the secret. The existing device then sends the user's key,
//! sealed under a key made from the secret, with the new device's certificate. A link is spent
//! the first time it works, expires after ten minutes, and closes after five wrong tries.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::contacts;
use crate::db::Database;
use crate::protocol;
use crate::user::{DeviceCert, Revocation, UserKeyStore};

/// The protocol a new device uses to get linked.
pub const LINK_ALPN: &[u8] = b"groovy-link/0";
const TICKET_PREFIX: &str = "groovylink1";
/// How long a link ticket works.
pub const LINK_TTL: Duration = Duration::from_secs(600);
/// How many wrong proofs close a link.
const MAX_ATTEMPTS: u32 = 5;
/// How long the whole exchange may take.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum LinkError {
    #[error("that is not a link ticket")]
    Ticket,
    #[error("the device that made the ticket could not be reached: {0}")]
    Connect(String),
    #[error("the device refused the link; the ticket may have expired or been used")]
    Refused,
    #[error("the link could not be completed: {0}")]
    Exchange(String),
    #[error("this device has no user key to link with")]
    NoUser,
    #[error("the user key could not be saved: {0}")]
    Store(String),
}

/// What an existing device shows so a new one can link to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkTicket {
    pub user: PublicKey,
    pub secret: [u8; 32],
    pub addr: EndpointAddr,
}

impl LinkTicket {
    pub fn encode(&self) -> String {
        let bytes = postcard::to_stdvec(self).expect("a ticket always encodes");
        let mut out = String::from(TICKET_PREFIX);
        data_encoding::BASE32_NOPAD.encode_append(&bytes, &mut out);
        out.make_ascii_lowercase();
        out
    }

    pub fn parse(text: &str) -> Result<Self, LinkError> {
        let body = text
            .trim()
            .to_ascii_lowercase()
            .strip_prefix(TICKET_PREFIX)
            .map(str::to_owned)
            .ok_or(LinkError::Ticket)?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .map_err(|_| LinkError::Ticket)?;
        postcard::from_bytes(&bytes).map_err(|_| LinkError::Ticket)
    }
}

/// A link that is open, waiting for a new device.
#[derive(Debug)]
pub struct OpenLink {
    secret: [u8; 32],
    expires: Instant,
    attempts: u32,
}

/// The one link a device has open, if any.
pub type OpenLinks = Arc<Mutex<Option<OpenLink>>>;

/// Opens a link, replacing any that was open, and returns its secret.
pub fn open_link(links: &OpenLinks) -> [u8; 32] {
    let secret = SecretKey::generate().to_bytes();
    *links.lock().unwrap_or_else(PoisonError::into_inner) = Some(OpenLink {
        secret,
        expires: Instant::now() + LINK_TTL,
        attempts: 0,
    });
    secret
}

#[derive(Debug, Serialize, Deserialize)]
struct LinkRequest {
    proof: [u8; 32],
    /// What the new device is called.
    name: String,
}

#[derive(Debug, Serialize, Deserialize)]
enum LinkReply {
    Refused,
    Linked {
        nonce: [u8; 12],
        /// The user's secret key, sealed under a key made from the link's secret.
        sealed: Vec<u8>,
        /// Every device of the user, including the new one.
        devices: Vec<DeviceCert>,
        revocations: Vec<Revocation>,
        /// The contacts the linking device trusts, so the new device trusts them too.
        contacts: Vec<crate::user::SharedContact>,
    },
}

/// What the new device sends to show it holds the link's secret.
fn proof(secret: &[u8; 32], device: &EndpointId) -> [u8; 32] {
    let mut message = b"groovy-link-proof-v1".to_vec();
    message.extend_from_slice(device.as_bytes());
    *blake3::keyed_hash(secret, &message).as_bytes()
}

fn seal_key(secret: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("groovy link seal v1", secret)
}

/// Serves linking, while a link is open.
#[derive(Clone)]
pub struct LinkProtocol {
    pub links: OpenLinks,
    pub keys: Arc<dyn UserKeyStore>,
    pub database: Arc<Database>,
}

impl std::fmt::Debug for LinkProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkProtocol").finish_non_exhaustive()
    }
}

impl LinkProtocol {
    fn answer(&self, device: EndpointId, request: LinkRequest) -> LinkReply {
        let mut links = self.links.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(open) = links.as_mut() else {
            return LinkReply::Refused;
        };
        if Instant::now() > open.expires {
            *links = None;
            return LinkReply::Refused;
        }
        // Compared as hashes, which is constant time.
        if blake3::Hash::from(proof(&open.secret, &device)) != blake3::Hash::from(request.proof) {
            open.attempts += 1;
            if open.attempts >= MAX_ATTEMPTS {
                *links = None;
            }
            return LinkReply::Refused;
        }
        let secret = open.secret;
        let Some(user) = self.keys.load() else {
            return LinkReply::Refused;
        };
        let cert = DeviceCert::issue(&user, device, &request.name, contacts::now());
        if self.database.save_device(&cert).is_err() {
            return LinkReply::Refused;
        }
        let user_public = user.public();
        let mut devices: Vec<DeviceCert> = self
            .database
            .devices_of(&user_public)
            .unwrap_or_default()
            .into_iter()
            .map(|device| device.cert)
            .collect();
        if !devices.contains(&cert) {
            devices.push(cert);
        }
        let revocations = self
            .database
            .revocations_of(&user_public)
            .unwrap_or_default();

        let nonce_bytes: [u8; 12] = SecretKey::generate().to_bytes()[..12]
            .try_into()
            .expect("twelve bytes");
        let cipher = ChaCha20Poly1305::new((&seal_key(&secret)).into());
        let Ok(sealed) =
            cipher.encrypt(Nonce::from_slice(&nonce_bytes), user.to_bytes().as_slice())
        else {
            return LinkReply::Refused;
        };
        // The link is spent.
        *links = None;
        LinkReply::Linked {
            nonce: nonce_bytes,
            sealed,
            devices,
            revocations,
            contacts: self.database.shared_contacts().unwrap_or_default(),
        }
    }
}

impl ProtocolHandler for LinkProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let device = connection.remote_id();
        let (mut send, mut recv) = connection.accept_bi().await?;
        let Ok(request) = protocol::read_message::<LinkRequest>(&mut recv).await else {
            return Ok(());
        };
        let reply = self.answer(device, request);
        if protocol::write_message(&mut send, &reply).await.is_ok() {
            let _ = send.finish();
        }
        // Give the new device time to read the reply before the connection closes.
        let _ = tokio::time::timeout(Duration::from_secs(5), connection.closed()).await;
        Ok(())
    }
}

/// Links this device to the user named in `ticket`. On success the user's key is saved with
/// `keys`, and the device's certificate and the user's other devices are saved in `database`.
/// Returns the user.
pub async fn accept_link(
    endpoint: &Endpoint,
    ticket: &LinkTicket,
    name: &str,
    keys: &dyn UserKeyStore,
    database: &Database,
) -> Result<PublicKey, LinkError> {
    let exchange = async {
        let connection = endpoint
            .connect(ticket.addr.clone(), LINK_ALPN)
            .await
            .map_err(|error| LinkError::Connect(error.to_string()))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| LinkError::Connect(error.to_string()))?;
        let request = LinkRequest {
            proof: proof(&ticket.secret, &endpoint.id()),
            name: name.to_owned(),
        };
        protocol::write_message(&mut send, &request)
            .await
            .map_err(|error| LinkError::Exchange(error.to_string()))?;
        let reply: LinkReply = protocol::read_message(&mut recv)
            .await
            .map_err(|error| LinkError::Exchange(error.to_string()))?;
        connection.close(0u32.into(), b"linked");
        Ok::<_, LinkError>(reply)
    };
    let reply = tokio::time::timeout(EXCHANGE_TIMEOUT, exchange)
        .await
        .map_err(|_| LinkError::Exchange("it took too long".to_owned()))??;

    let LinkReply::Linked {
        nonce,
        sealed,
        devices,
        revocations,
        contacts,
    } = reply
    else {
        return Err(LinkError::Refused);
    };

    let cipher = ChaCha20Poly1305::new((&seal_key(&ticket.secret)).into());
    let opened = cipher
        .decrypt(Nonce::from_slice(&nonce), sealed.as_slice())
        .map_err(|_| LinkError::Exchange("the key could not be opened".to_owned()))?;
    let bytes = <[u8; 32]>::try_from(opened.as_slice())
        .map_err(|_| LinkError::Exchange("the key has the wrong length".to_owned()))?;
    let user = SecretKey::from_bytes(&bytes);
    // The key must be the user the ticket named, or the device is being given someone else's.
    if user.public() != ticket.user {
        return Err(LinkError::Exchange(
            "the key is not the user in the ticket".to_owned(),
        ));
    }
    let own = devices
        .iter()
        .find(|cert| cert.device == endpoint.id() && cert.user == ticket.user && cert.verify());
    let Some(own) = own else {
        return Err(LinkError::Exchange(
            "no certificate for this device".to_owned(),
        ));
    };

    keys.save(&user).map_err(LinkError::Store)?;
    database
        .set_own_user(&ticket.user)
        .map_err(|error| LinkError::Store(error.to_string()))?;
    database
        .save_device(own)
        .map_err(|error| LinkError::Store(error.to_string()))?;
    for cert in devices
        .iter()
        .filter(|cert| cert.user == ticket.user && cert.verify())
    {
        let _ = database.save_device(cert);
    }
    for revocation in revocations
        .iter()
        .filter(|revocation| revocation.user == ticket.user)
    {
        let _ = database.save_revocation(revocation);
    }
    // The user's contacts become this device's contacts, so they can reach it straight away.
    let now = crate::contacts::now();
    for contact in &contacts {
        if contact.endpoint != endpoint.id()
            && !database.is_own_device(&contact.endpoint).unwrap_or(false)
        {
            let _ = database.adopt_contact(contact, now);
        }
    }
    Ok(ticket.user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_ticket_round_trips() {
        let ticket = LinkTicket {
            user: SecretKey::generate().public(),
            secret: [7; 32],
            addr: EndpointAddr::from(SecretKey::generate().public()),
        };
        assert_eq!(LinkTicket::parse(&ticket.encode()).unwrap(), ticket);
    }

    #[test]
    fn text_that_is_not_a_link_ticket_is_refused() {
        assert!(LinkTicket::parse("groovy1abc").is_err());
        assert!(LinkTicket::parse("groovylink1!!!").is_err());
        assert!(LinkTicket::parse("").is_err());
    }

    #[test]
    fn the_proof_depends_on_the_secret_and_the_device() {
        let device = SecretKey::generate().public();
        let other = SecretKey::generate().public();
        assert_eq!(proof(&[1; 32], &device), proof(&[1; 32], &device));
        assert_ne!(proof(&[1; 32], &device), proof(&[2; 32], &device));
        assert_ne!(proof(&[1; 32], &device), proof(&[1; 32], &other));
    }
}
