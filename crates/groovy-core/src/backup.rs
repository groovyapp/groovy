//! Encrypted backups of the identity key.
//!
//! A backup is an [age](https://age-encryption.org) file, encrypted with a
//! passphrase, so any `age` tool can open it. It contains only the 32-byte
//! secret key. Without the passphrase it reveals nothing.

use std::io::{Read, Write};
use std::iter;

use age::secrecy::SecretString;
use iroh::SecretKey;
use thiserror::Error;

/// The shortest passphrase a backup will accept.
pub const MIN_PASSPHRASE_CHARS: usize = 12;

const KEY_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum BackupError {
    #[error("the passphrase must be at least {MIN_PASSPHRASE_CHARS} characters")]
    PassphraseTooShort,
    #[error("the passphrase is wrong, or the file is damaged")]
    WrongPassphrase,
    #[error("this file is not a Groovy identity backup")]
    NotABackup,
    #[error("failed to encrypt the backup")]
    Encrypt(#[source] age::EncryptError),
    #[error("failed to read the backup")]
    Decrypt(#[source] age::DecryptError),
    #[error("failed to read or write the backup")]
    Io(#[source] std::io::Error),
}

/// Encrypts the identity key with `passphrase`.
pub fn export(secret: &SecretKey, passphrase: &str) -> Result<Vec<u8>, BackupError> {
    export_with_work_factor(secret, passphrase, None)
}

/// Decrypts a backup made by [`export`]. Returns the key it holds.
pub fn import(backup: &[u8], passphrase: &str) -> Result<SecretKey, BackupError> {
    let identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_owned()));
    let decryptor = age::Decryptor::new(backup).map_err(BackupError::Decrypt)?;
    let mut reader = decryptor
        .decrypt(iter::once(&identity as &dyn age::Identity))
        .map_err(|error| match error {
            age::DecryptError::DecryptionFailed => BackupError::WrongPassphrase,
            age::DecryptError::NoMatchingKeys => BackupError::NotABackup,
            other => BackupError::Decrypt(other),
        })?;

    let mut plaintext = Vec::new();
    reader
        .read_to_end(&mut plaintext)
        .map_err(BackupError::Io)?;
    let key: [u8; KEY_LEN] = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| BackupError::NotABackup)?;
    Ok(SecretKey::from_bytes(&key))
}

/// Refuses passphrases shorter than [`MIN_PASSPHRASE_CHARS`].
pub fn validate_passphrase(passphrase: &str) -> Result<(), BackupError> {
    if passphrase.chars().count() < MIN_PASSPHRASE_CHARS {
        Err(BackupError::PassphraseTooShort)
    } else {
        Ok(())
    }
}

/// `log_n` sets the scrypt work factor. `None` uses age's default, which is
/// deliberately slow. Tests lower it to stay fast.
pub(crate) fn export_with_work_factor(
    secret: &SecretKey,
    passphrase: &str,
    log_n: Option<u8>,
) -> Result<Vec<u8>, BackupError> {
    validate_passphrase(passphrase)?;

    let mut recipient = age::scrypt::Recipient::new(SecretString::from(passphrase.to_owned()));
    if let Some(log_n) = log_n {
        recipient.set_work_factor(log_n);
    }
    let encryptor = age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient))
        .map_err(BackupError::Encrypt)?;

    let mut output = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut output)
        .map_err(BackupError::Io)?;
    writer
        .write_all(&secret.to_bytes())
        .map_err(BackupError::Io)?;
    writer.finish().map_err(BackupError::Io)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSPHRASE: &str = "correct horse battery";

    fn fast_export(secret: &SecretKey, passphrase: &str) -> Vec<u8> {
        export_with_work_factor(secret, passphrase, Some(4)).unwrap()
    }

    #[test]
    fn a_backup_restores_the_same_key() {
        let secret = SecretKey::generate();
        let backup = fast_export(&secret, PASSPHRASE);

        let restored = import(&backup, PASSPHRASE).unwrap();

        assert_eq!(restored.to_bytes(), secret.to_bytes());
        assert_eq!(restored.public(), secret.public());
    }

    #[test]
    fn the_wrong_passphrase_is_refused() {
        let backup = fast_export(&SecretKey::generate(), PASSPHRASE);

        let err = import(&backup, "not the passphrase").unwrap_err();

        assert!(matches!(err, BackupError::WrongPassphrase), "{err}");
    }

    #[test]
    fn the_key_is_not_visible_in_the_backup() {
        let secret = SecretKey::generate();
        let backup = fast_export(&secret, PASSPHRASE);

        let key = secret.to_bytes();
        assert!(!backup.windows(key.len()).any(|window| window == key));
    }

    #[test]
    fn other_files_are_not_backups() {
        let err = import(b"just some text, not an age file", PASSPHRASE).unwrap_err();

        assert!(
            matches!(err, BackupError::Decrypt(_) | BackupError::NotABackup),
            "{err}"
        );
    }

    #[test]
    fn short_passphrases_are_refused_before_encrypting() {
        let err = export(&SecretKey::generate(), "short").unwrap_err();

        assert!(matches!(err, BackupError::PassphraseTooShort), "{err}");
    }
}
