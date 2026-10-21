//! One user across several devices.
//!
//! A user is a key pair. Each device keeps its own endpoint key, and is linked to the user by a
//! certificate: the user's key signs the device's endpoint ID, a name, and a time. Contacts trust
//! the user, so pairing with any one device trusts every device that holds a valid certificate.
//! The user can revoke a device, with a signed record that is passed on to peers.
//!
//! See `docs/identity.md` for the design and its limits.

use std::sync::Mutex;

use iroh::{EndpointId, PublicKey, SecretKey};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::contacts::Trust;
use crate::db::{Database, DbError};
use crate::paths::DataDir;
use crate::protocol::{PeerInfo, PeerProfile};

const CERT_DOMAIN: &str = "groovy-device-certificate-v1";
const REVOCATION_DOMAIN: &str = "groovy-device-revocation-v1";
const OWN_USER_KEY: &str = "user.public";
const KEYRING_ENTRY: &str = "user";
const KEY_FILE: &str = "user_key";

/// A device's link to a user: signed by the user's key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCert {
    pub user: PublicKey,
    pub device: EndpointId,
    /// What the user calls the device, such as "laptop".
    pub name: String,
    /// Unix seconds.
    pub created: i64,
    pub signature: iroh::Signature,
}

fn cert_message(user: &PublicKey, device: &EndpointId, name: &str, created: i64) -> Vec<u8> {
    postcard::to_stdvec(&(CERT_DOMAIN, user, device, name, created))
        .expect("fixed fields always encode")
}

impl DeviceCert {
    /// Signs a certificate for `device` with the user's key.
    pub fn issue(user: &SecretKey, device: EndpointId, name: &str, created: i64) -> Self {
        let user_public = user.public();
        let message = cert_message(&user_public, &device, name, created);
        Self {
            user: user_public,
            device,
            name: name.to_owned(),
            created,
            signature: user.sign(&message),
        }
    }

    /// Whether the user's key signed this certificate.
    pub fn verify(&self) -> bool {
        let message = cert_message(&self.user, &self.device, &self.name, self.created);
        self.user.verify(&message, &self.signature).is_ok()
    }
}

/// The user's record that a device is no longer theirs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revocation {
    pub user: PublicKey,
    pub device: EndpointId,
    /// Unix seconds.
    pub at: i64,
    pub signature: iroh::Signature,
}

fn revocation_message(user: &PublicKey, device: &EndpointId, at: i64) -> Vec<u8> {
    postcard::to_stdvec(&(REVOCATION_DOMAIN, user, device, at)).expect("fixed fields always encode")
}

impl Revocation {
    pub fn issue(user: &SecretKey, device: EndpointId, at: i64) -> Self {
        let user_public = user.public();
        let message = revocation_message(&user_public, &device, at);
        Self {
            user: user_public,
            device,
            at,
            signature: user.sign(&message),
        }
    }

    pub fn verify(&self) -> bool {
        let message = revocation_message(&self.user, &self.device, self.at);
        self.user.verify(&message, &self.signature).is_ok()
    }
}

/// What a device shows in the handshake to say which user it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserClaim {
    pub certificate: DeviceCert,
    /// The devices this user has revoked, so peers refuse them too.
    pub revocations: Vec<Revocation>,
}

/// A device linked to a user, and whether it has been revoked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDevice {
    pub cert: DeviceCert,
    /// Unix seconds when the user revoked it, if they did.
    pub revoked: Option<i64>,
}

/// A place the user's secret key is kept.
pub trait UserKeyStore: Send + Sync {
    fn load(&self) -> Option<SecretKey>;
    fn save(&self, key: &SecretKey) -> Result<(), String>;
}

/// The user key kept in the OS credential store, or in a file owner-readable only when the store
/// is not available. Unlike the endpoint identity, the user key is optional, so a missing key is
/// not an error: it means this device is not linked to a user yet.
pub struct StoredUserKey {
    dir: DataDir,
    app_name: String,
}

impl StoredUserKey {
    pub fn new(dir: DataDir, app_name: &str) -> Self {
        Self {
            dir,
            app_name: app_name.to_owned(),
        }
    }

    fn file(&self) -> std::path::PathBuf {
        self.dir.identity_dir().join(KEY_FILE)
    }

    fn entry(&self) -> Option<keyring::Entry> {
        keyring::Entry::new(&self.app_name, KEYRING_ENTRY).ok()
    }
}

impl UserKeyStore for StoredUserKey {
    fn load(&self) -> Option<SecretKey> {
        if let Some(entry) = self.entry()
            && let Ok(bytes) = entry.get_secret()
            && let Ok(bytes) = <[u8; 32]>::try_from(bytes.as_slice())
        {
            return Some(SecretKey::from_bytes(&bytes));
        }
        let bytes = std::fs::read(self.file()).ok()?;
        let bytes = <[u8; 32]>::try_from(bytes.as_slice()).ok()?;
        Some(SecretKey::from_bytes(&bytes))
    }

    fn save(&self, key: &SecretKey) -> Result<(), String> {
        if let Some(entry) = self.entry()
            && entry.set_secret(&key.to_bytes()).is_ok()
        {
            return Ok(());
        }
        let path = self.file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        write_private(&path, &key.to_bytes()).map_err(|error| error.to_string())
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// A user key kept in memory only, for tests.
#[derive(Default)]
pub struct MemoryUserKey(Mutex<Option<SecretKey>>);

impl UserKeyStore for MemoryUserKey {
    fn load(&self) -> Option<SecretKey> {
        self.0.lock().ok()?.clone()
    }

    fn save(&self, key: &SecretKey) -> Result<(), String> {
        *self.0.lock().map_err(|error| error.to_string())? = Some(key.clone());
        Ok(())
    }
}

impl Database {
    /// The user this device belongs to, if it has been linked or has made one.
    pub fn own_user(&self) -> Result<Option<PublicKey>, DbError> {
        Ok(self
            .get_setting(OWN_USER_KEY)?
            .and_then(|text| text.parse().ok()))
    }

    pub fn set_own_user(&self, user: &PublicKey) -> Result<(), DbError> {
        self.set_setting(OWN_USER_KEY, &user.to_string())
    }

    /// Saves a device's certificate. A revocation already saved for it is kept.
    pub fn save_device(&self, cert: &DeviceCert) -> Result<(), DbError> {
        let encoded = encode(cert)?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO user_devices (user, device, name, created, cert)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (user, device) DO UPDATE
                 SET name = ?3, created = ?4, cert = ?5",
                params![
                    cert.user.as_bytes().as_slice(),
                    cert.device.as_bytes().as_slice(),
                    cert.name,
                    cert.created,
                    encoded,
                ],
            )
            .map(|_| ())
        })
    }

    /// Records that a user revoked a device. Does nothing if the signature is not the user's.
    pub fn save_revocation(&self, revocation: &Revocation) -> Result<(), DbError> {
        if !revocation.verify() {
            return Ok(());
        }
        let encoded = encode(revocation)?;
        self.with_conn(|conn| {
            // A device whose certificate has not been seen yet is still refused once it is.
            conn.execute(
                "INSERT OR IGNORE INTO user_devices (user, device, name, created, cert)
                 VALUES (?1, ?2, '', 0, x'')",
                params![
                    revocation.user.as_bytes().as_slice(),
                    revocation.device.as_bytes().as_slice(),
                ],
            )?;
            conn.execute(
                "UPDATE user_devices SET revoked = ?3, revocation = ?4
                 WHERE user = ?1 AND device = ?2",
                params![
                    revocation.user.as_bytes().as_slice(),
                    revocation.device.as_bytes().as_slice(),
                    revocation.at,
                    encoded,
                ],
            )
            .map(|_| ())
        })
    }

    /// The devices linked to a user, with whether each is revoked. Devices known only from a
    /// revocation have no certificate and are left out.
    pub fn devices_of(&self, user: &PublicKey) -> Result<Vec<UserDevice>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT cert, revoked FROM user_devices
                 WHERE user = ?1 AND length(cert) > 0 ORDER BY created, rowid",
            )?;
            let rows = statement
                .query_map([user.as_bytes().as_slice()], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<i64>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(cert, revoked)| {
                postcard::from_bytes::<DeviceCert>(&cert)
                    .ok()
                    .map(|cert| UserDevice { cert, revoked })
            })
            .collect())
    }

    /// The signed records of the devices a user has revoked.
    pub fn revocations_of(&self, user: &PublicKey) -> Result<Vec<Revocation>, DbError> {
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT revocation FROM user_devices WHERE user = ?1 AND revocation IS NOT NULL",
            )?;
            let rows = statement
                .query_map([user.as_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|bytes| postcard::from_bytes::<Revocation>(&bytes).ok())
            .collect())
    }

    /// Remembers which user a contact's device belongs to.
    pub fn set_contact_user(&self, device: EndpointId, user: &PublicKey) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE contacts SET user = ?2 WHERE endpoint_id = ?1",
                params![device.as_bytes().as_slice(), user.as_bytes().as_slice()],
            )
            .map(|_| ())
        })
    }

    /// Whether a device may be treated as trusted: the user paired with it, or with another
    /// device of the same user, and it has not been revoked, or blocked.
    pub fn is_trusted_device(&self, device: EndpointId) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT (
                    EXISTS (SELECT 1 FROM contacts c
                            WHERE c.endpoint_id = ?1 AND c.trust = 'trusted')
                    OR EXISTS (
                        SELECT 1 FROM user_devices d
                        WHERE d.device = ?1 AND length(d.cert) > 0
                          AND EXISTS (SELECT 1 FROM contacts t
                                      WHERE t.user = d.user AND t.trust = 'trusted'))
                 )
                 AND NOT EXISTS (SELECT 1 FROM user_devices r
                                 WHERE r.device = ?1 AND r.revoked IS NOT NULL)
                 AND NOT EXISTS (SELECT 1 FROM contacts b
                                 WHERE b.endpoint_id = ?1 AND b.trust = 'blocked')",
                [device.as_bytes().as_slice()],
                |row| row.get(0),
            )
        })
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, DbError> {
    postcard::to_stdvec(value)
        .map_err(|error| DbError::Query(rusqlite::Error::ToSqlConversionFailure(Box::new(error))))
}

/// A contact one of the user's devices trusts. The user's other devices trust it too, so a
/// contact paired on one device can reach all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedContact {
    pub endpoint: EndpointId,
    pub name: String,
}

impl Database {
    /// The contacts this device trusts, to share with the user's other devices.
    pub fn shared_contacts(&self) -> Result<Vec<SharedContact>, DbError> {
        Ok(self
            .contacts()?
            .into_iter()
            .filter(|contact| contact.trust == Trust::Trusted)
            .map(|contact| SharedContact {
                endpoint: contact.endpoint_id,
                name: contact.display_name,
            })
            .collect())
    }

    /// Trusts a contact that another of the user's devices trusts. A contact blocked on this
    /// device stays blocked. Returns whether anything changed.
    pub fn adopt_contact(&self, contact: &SharedContact, now: i64) -> Result<bool, DbError> {
        match self.contact(contact.endpoint)? {
            Some(existing) if existing.trust != Trust::Pending => Ok(false),
            Some(_) => self.set_trust(contact.endpoint, Trust::Trusted),
            None => {
                self.record_handshake(
                    &PeerInfo {
                        endpoint_id: contact.endpoint,
                        profile: PeerProfile {
                            display_name: contact.name.clone(),
                            avatar_hash: None,
                            user: None,
                        },
                    },
                    now,
                )?;
                self.set_trust(contact.endpoint, Trust::Trusted)
            }
        }
    }

    /// Whether an endpoint is another device of this device's own user, and not revoked.
    pub fn is_own_device(&self, endpoint: &EndpointId) -> Result<bool, DbError> {
        let Some(user) = self.own_user()? else {
            return Ok(false);
        };
        Ok(self
            .devices_of(&user)?
            .iter()
            .any(|device| device.cert.device == *endpoint && device.revoked.is_none()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_certificate_verifies_only_for_what_the_user_signed() {
        let user = SecretKey::generate();
        let device = SecretKey::generate().public();
        let cert = DeviceCert::issue(&user, device, "laptop", 10);
        assert!(cert.verify());

        let mut renamed = cert.clone();
        renamed.name = "desktop".into();
        assert!(!renamed.verify());
        let mut moved = cert;
        moved.device = SecretKey::generate().public();
        assert!(!moved.verify());
    }

    #[test]
    fn a_revocation_verifies_only_for_the_users_key() {
        let user = SecretKey::generate();
        let device = SecretKey::generate().public();
        let revocation = Revocation::issue(&user, device, 5);
        assert!(revocation.verify());

        let forged = Revocation {
            user: SecretKey::generate().public(),
            ..revocation
        };
        assert!(!forged.verify());
    }

    #[test]
    fn a_shared_contact_is_trusted_here_unless_it_was_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("groovy.db")).unwrap();
        let ada = SecretKey::generate().public();
        let eve = SecretKey::generate().public();
        db.record_handshake(
            &PeerInfo {
                endpoint_id: eve,
                profile: PeerProfile::default(),
            },
            1,
        )
        .unwrap();
        db.set_trust(eve, Trust::Blocked).unwrap();

        let shared = |endpoint| SharedContact {
            endpoint,
            name: "someone".to_owned(),
        };
        assert!(db.adopt_contact(&shared(ada), 2).unwrap());
        assert_eq!(db.contact(ada).unwrap().unwrap().trust, Trust::Trusted);
        assert!(!db.adopt_contact(&shared(eve), 2).unwrap());
        assert_eq!(db.contact(eve).unwrap().unwrap().trust, Trust::Blocked);
        let listed: Vec<EndpointId> = db
            .shared_contacts()
            .unwrap()
            .iter()
            .map(|c| c.endpoint)
            .collect();
        assert_eq!(listed, [ada]);
    }

    #[test]
    fn only_a_device_of_this_user_that_is_not_revoked_is_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("groovy.db")).unwrap();
        let user = SecretKey::generate();
        let (laptop, phone) = (
            SecretKey::generate().public(),
            SecretKey::generate().public(),
        );
        assert!(!db.is_own_device(&phone).unwrap(), "no user yet");
        db.set_own_user(&user.public()).unwrap();
        db.save_device(&DeviceCert::issue(&user, laptop, "laptop", 1))
            .unwrap();
        db.save_device(&DeviceCert::issue(&user, phone, "phone", 1))
            .unwrap();
        assert!(db.is_own_device(&phone).unwrap());
        db.save_revocation(&Revocation::issue(&user, phone, 2))
            .unwrap();
        assert!(!db.is_own_device(&phone).unwrap());
        assert!(db.is_own_device(&laptop).unwrap());
    }
}
