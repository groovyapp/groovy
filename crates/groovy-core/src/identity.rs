//! Groovy's endpoint identity: an Ed25519 secret key that stays the same
//! across launches. Peers find this node by the matching public key.
//!
//! The key is kept in the OS credential store. If that store cannot be used
//! on this machine, it is kept in a file in the identity directory, readable
//! only by its owner.
//!
//! A marker file records where the key lives. Once a key has been saved to
//! the credential store, a store error fails loudly rather than creating a
//! new identity, which would silently strand the contacts who know the old one.

use std::error::Error as StdError;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use iroh::{PublicKey, SecretKey};
use thiserror::Error;
use tracing::warn;

use crate::paths::DataDir;

const KEY_LEN: usize = 32;
const KEYRING_USER: &str = "identity";
const KEY_FILE: &str = "secret_key";
const LOCATION_FILE: &str = "location";

/// An error from the OS credential store.
#[derive(Debug, Error)]
#[error("the OS credential store failed")]
pub struct StoreError(#[source] Box<dyn StdError + Send + Sync>);

/// Errors from loading or creating the identity.
#[derive(Debug, Error)]
pub enum IdentityError {
    #[error(
        "this identity is stored in the OS credential store, which could not be read; not creating a new identity"
    )]
    StoreUnreadable(#[source] StoreError),
    #[error(
        "this identity is recorded in the OS credential store, but the store has no key for it; not creating a new identity"
    )]
    StoreMissing,
    #[error(
        "this identity is recorded in {path}, but that file is missing; not creating a new identity"
    )]
    FileMissing { path: PathBuf },
    #[error(
        "the OS credential store and {path} hold different identities; remove one of them before starting Groovy"
    )]
    Conflict { path: PathBuf },
    #[error("{path} does not hold a valid identity key")]
    InvalidFile { path: PathBuf },
    #[error("{path} does not record a known identity location")]
    UnknownLocation { path: PathBuf },
    #[error("failed to read or write {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Where the identity's secret key is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// The OS credential store.
    Keyring,
    /// A file in the identity directory, used because the credential store was unavailable.
    File,
}

/// A place a 32-byte secret key can be saved to and read back from.
pub trait SecretStore {
    fn load(&self) -> Result<Option<[u8; KEY_LEN]>, StoreError>;
    fn save(&self, key: &[u8; KEY_LEN]) -> Result<(), StoreError>;
}

/// The OS credential store, under the app's service name.
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub fn new(app_name: &str) -> Self {
        Self {
            service: app_name.to_owned(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(&self.service, KEYRING_USER).map_err(store_error)
    }
}

impl SecretStore for KeyringStore {
    fn load(&self) -> Result<Option<[u8; KEY_LEN]>, StoreError> {
        match self.entry()?.get_secret() {
            Ok(bytes) => {
                let key = <[u8; KEY_LEN]>::try_from(bytes.as_slice()).map_err(|_| {
                    StoreError(format!("stored key has {} bytes", bytes.len()).into())
                })?;
                Ok(Some(key))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    fn save(&self, key: &[u8; KEY_LEN]) -> Result<(), StoreError> {
        self.entry()?.set_secret(key).map_err(store_error)
    }
}

fn store_error(error: keyring::Error) -> StoreError {
    StoreError(Box::new(error))
}

/// The identity key kept in a file, owner-readable only.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<Option<[u8; KEY_LEN]>, IdentityError> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                let key = <[u8; KEY_LEN]>::try_from(bytes.as_slice()).map_err(|_| {
                    IdentityError::InvalidFile {
                        path: self.path.clone(),
                    }
                })?;
                Ok(Some(key))
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(self.io_error(source)),
        }
    }

    /// Writes the key atomically. On Unix the file is created with mode 0600.
    /// On Windows the file inherits the per-user ACL of the data directory.
    pub fn save(&self, key: &[u8; KEY_LEN]) -> Result<(), IdentityError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| self.io_error(source))?;
        }
        let tmp = self.path.with_extension("tmp");
        let _ = fs::remove_file(&tmp);
        let mut file = private_options()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|source| self.io_error(source))?;
        file.write_all(key)
            .and_then(|()| file.sync_all())
            .map_err(|source| self.io_error(source))?;
        drop(file);
        fs::rename(&tmp, &self.path).map_err(|source| self.io_error(source))
    }

    pub fn remove(&self) -> Result<(), IdentityError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(self.io_error(source)),
        }
    }

    fn io_error(&self, source: io::Error) -> IdentityError {
        IdentityError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

#[cfg(unix)]
fn private_options() -> OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.mode(0o600);
    options
}

#[cfg(not(unix))]
fn private_options() -> OpenOptions {
    OpenOptions::new()
}

/// This node's identity. Its `Debug` output shows only the public key.
pub struct Identity {
    secret: SecretKey,
    location: Location,
}

impl Identity {
    /// Loads the identity from the OS credential store, or creates it on first run.
    /// Falls back to a file in the identity directory if the store is unavailable.
    pub fn load_or_create(dir: &DataDir, app_name: &str) -> Result<Self, IdentityError> {
        let identity_dir = dir.identity_dir();
        Self::load_or_create_with(
            &KeyringStore::new(app_name),
            &FileStore::new(identity_dir.join(KEY_FILE)),
            &identity_dir.join(LOCATION_FILE),
        )
    }

    pub fn secret_key(&self) -> &SecretKey {
        &self.secret
    }

    pub fn public_key(&self) -> PublicKey {
        self.secret.public()
    }

    pub fn location(&self) -> Location {
        self.location
    }

    fn load_or_create_with(
        store: &dyn SecretStore,
        file: &FileStore,
        marker: &Path,
    ) -> Result<Self, IdentityError> {
        let (key, location) = match read_location(marker)? {
            Some(Location::Keyring) => {
                let key = store
                    .load()
                    .map_err(IdentityError::StoreUnreadable)?
                    .ok_or(IdentityError::StoreMissing)?;
                (key, Location::Keyring)
            }
            Some(Location::File) => {
                if file.load()?.is_none() {
                    return Err(IdentityError::FileMissing {
                        path: file.path.clone(),
                    });
                }
                // Move the key into the store if it is available now.
                adopt_or_create(store, file)?
            }
            // Nothing recorded, so the store has never held our key. Use it if
            // present, otherwise create one.
            None => adopt_or_create(store, file)?,
        };

        if read_location(marker)? != Some(location) {
            write_location(marker, location)?;
        }
        Ok(Self {
            secret: SecretKey::from_bytes(&key),
            location,
        })
    }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("public_key", &self.public_key())
            .field("location", &self.location)
            .finish()
    }
}

fn adopt_or_create(
    store: &dyn SecretStore,
    file: &FileStore,
) -> Result<([u8; KEY_LEN], Location), IdentityError> {
    let file_key = file.load()?;
    match store.load() {
        Ok(Some(stored)) => {
            if let Some(file_key) = file_key {
                if file_key != stored {
                    return Err(IdentityError::Conflict {
                        path: file.path.clone(),
                    });
                }
                file.remove()?;
            }
            Ok((stored, Location::Keyring))
        }
        Ok(None) => {
            let key = file_key.unwrap_or_else(|| SecretKey::generate().to_bytes());
            match store.save(&key) {
                Ok(()) => {
                    file.remove()?;
                    Ok((key, Location::Keyring))
                }
                Err(error) => {
                    warn!(%error, "OS credential store is unavailable; keeping the identity in a file");
                    if file_key.is_none() {
                        file.save(&key)?;
                    }
                    Ok((key, Location::File))
                }
            }
        }
        Err(error) => {
            warn!(%error, "OS credential store is unavailable; keeping the identity in a file");
            match file_key {
                Some(key) => Ok((key, Location::File)),
                None => {
                    let key = SecretKey::generate().to_bytes();
                    file.save(&key)?;
                    Ok((key, Location::File))
                }
            }
        }
    }
}

fn read_location(marker: &Path) -> Result<Option<Location>, IdentityError> {
    match fs::read_to_string(marker) {
        Ok(text) => match text.trim() {
            "keyring" => Ok(Some(Location::Keyring)),
            "file" => Ok(Some(Location::File)),
            _ => Err(IdentityError::UnknownLocation {
                path: marker.to_path_buf(),
            }),
        },
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(IdentityError::Io {
            path: marker.to_path_buf(),
            source,
        }),
    }
}

fn write_location(marker: &Path, location: Location) -> Result<(), IdentityError> {
    let text = match location {
        Location::Keyring => "keyring",
        Location::File => "file",
    };
    fs::write(marker, text).map_err(|source| IdentityError::Io {
        path: marker.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// A credential store that can be made to fail.
    struct FakeStore {
        key: RefCell<Option<[u8; KEY_LEN]>>,
        broken: bool,
    }

    impl FakeStore {
        fn working(key: Option<[u8; KEY_LEN]>) -> Self {
            Self {
                key: RefCell::new(key),
                broken: false,
            }
        }

        fn broken() -> Self {
            Self {
                key: RefCell::new(None),
                broken: true,
            }
        }
    }

    impl SecretStore for FakeStore {
        fn load(&self) -> Result<Option<[u8; KEY_LEN]>, StoreError> {
            if self.broken {
                return Err(StoreError("store is locked".into()));
            }
            Ok(*self.key.borrow())
        }

        fn save(&self, key: &[u8; KEY_LEN]) -> Result<(), StoreError> {
            if self.broken {
                return Err(StoreError("store is locked".into()));
            }
            *self.key.borrow_mut() = Some(*key);
            Ok(())
        }
    }

    struct Paths {
        _tmp: tempfile::TempDir,
        file: FileStore,
        marker: PathBuf,
    }

    fn paths() -> Paths {
        let tmp = tempfile::tempdir().unwrap();
        Paths {
            file: FileStore::new(tmp.path().join(KEY_FILE)),
            marker: tmp.path().join(LOCATION_FILE),
            _tmp: tmp,
        }
    }

    fn load(store: &dyn SecretStore, p: &Paths) -> Result<Identity, IdentityError> {
        Identity::load_or_create_with(store, &p.file, &p.marker)
    }

    #[test]
    fn first_run_creates_a_key_in_the_store() {
        let p = paths();
        let store = FakeStore::working(None);

        let identity = load(&store, &p).unwrap();

        assert_eq!(identity.location(), Location::Keyring);
        assert_eq!(
            store.key.borrow().unwrap(),
            identity.secret_key().to_bytes()
        );
        assert!(
            !p.file.path.exists(),
            "no identity file when the store works"
        );
    }

    #[test]
    fn later_runs_load_the_same_identity() {
        let p = paths();
        let store = FakeStore::working(None);
        let first = load(&store, &p).unwrap().public_key();

        let second = load(&store, &p).unwrap().public_key();

        assert_eq!(first, second);
    }

    #[test]
    fn store_with_a_key_but_no_marker_is_adopted() {
        let p = paths();
        let key = SecretKey::generate().to_bytes();
        let store = FakeStore::working(Some(key));

        let identity = load(&store, &p).unwrap();

        assert_eq!(identity.secret_key().to_bytes(), key);
        assert_eq!(fs::read_to_string(&p.marker).unwrap().trim(), "keyring");
    }

    #[test]
    fn unavailable_store_falls_back_to_a_file_with_the_same_identity_next_time() {
        let p = paths();
        let first = load(&FakeStore::broken(), &p).unwrap();
        assert_eq!(first.location(), Location::File);
        assert!(p.file.path.is_file());

        let second = load(&FakeStore::broken(), &p).unwrap();

        assert_eq!(first.public_key(), second.public_key());
    }

    #[cfg(unix)]
    #[test]
    fn identity_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let p = paths();
        load(&FakeStore::broken(), &p).unwrap();

        let mode = fs::metadata(&p.file.path).unwrap().permissions().mode() & 0o777;

        assert_eq!(mode, 0o600);
    }

    #[test]
    fn file_identity_moves_into_the_store_once_it_is_available() {
        let p = paths();
        let original = load(&FakeStore::broken(), &p).unwrap().public_key();

        let store = FakeStore::working(None);
        let moved = load(&store, &p).unwrap();

        assert_eq!(moved.public_key(), original);
        assert_eq!(moved.location(), Location::Keyring);
        assert!(
            !p.file.path.exists(),
            "the file copy is removed after it moves"
        );
    }

    #[test]
    fn recorded_keyring_identity_fails_loudly_when_the_store_is_locked() {
        let p = paths();
        load(&FakeStore::working(None), &p).unwrap();

        let err = load(&FakeStore::broken(), &p).unwrap_err();

        assert!(matches!(err, IdentityError::StoreUnreadable(_)), "{err}");
    }

    #[test]
    fn recorded_keyring_identity_fails_loudly_when_the_key_is_missing() {
        let p = paths();
        load(&FakeStore::working(None), &p).unwrap();

        let err = load(&FakeStore::working(None), &p).unwrap_err();

        assert!(matches!(err, IdentityError::StoreMissing), "{err}");
    }

    #[test]
    fn conflicting_store_and_file_are_refused() {
        let p = paths();
        load(&FakeStore::broken(), &p).unwrap();
        let store = FakeStore::working(Some(SecretKey::generate().to_bytes()));

        let err = load(&store, &p).unwrap_err();

        assert!(matches!(err, IdentityError::Conflict { .. }), "{err}");
    }

    #[test]
    fn corrupt_identity_file_is_an_error() {
        let p = paths();
        fs::write(&p.file.path, b"too short").unwrap();
        fs::write(&p.marker, "file").unwrap();

        let err = load(&FakeStore::broken(), &p).unwrap_err();

        assert!(matches!(err, IdentityError::InvalidFile { .. }), "{err}");
    }

    #[test]
    fn debug_output_never_shows_the_secret() {
        let p = paths();
        let identity = load(&FakeStore::working(None), &p).unwrap();
        let secret_hex: String = identity
            .secret_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let debug = format!("{identity:?}");

        assert!(!debug.contains(&secret_hex), "{debug}");
        assert!(debug.contains("public_key"), "{debug}");
    }
}
