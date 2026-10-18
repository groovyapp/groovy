//! The config file in the data directory.
//!
//! Unknown keys are kept when the file is saved, so settings written by a
//! newer or hand-edited file are not silently dropped.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::paths::DataDir;

/// The config version this build writes. Older files must be migrated up to it.
pub const CURRENT_VERSION: u32 = 1;

/// Errors from loading or saving the config.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read or write {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is not a valid config file")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(
        "{path} has config version {found}, but this build only supports up to {supported}; upgrade Groovy"
    )]
    NewerVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("{path} has config version {found}, which cannot be migrated")]
    UnknownVersion { path: PathBuf, found: u32 },
    #[error("failed to serialize the config")]
    Serialize(#[source] toml::ser::Error),
}

/// Whether the window follows the system's light or dark setting, or is fixed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

/// Groovy's settings, as stored in `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    /// Missing from older files, which get the default.
    #[serde(default)]
    pub appearance: Appearance,
    /// Keys this build does not know about. They are written back unchanged.
    #[serde(flatten)]
    pub unknown: toml::Table,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            appearance: Appearance::default(),
            unknown: toml::Table::new(),
        }
    }
}

impl Config {
    /// Loads `config.toml` from `dir`, creating it with defaults on first run.
    ///
    /// A file from an older version is migrated and saved back. A file from a
    /// newer version, or one that does not parse, is an error and is left untouched.
    pub fn load_or_create(dir: &DataDir) -> Result<Self, ConfigError> {
        let path = dir.config_file();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let config = Self::default();
                config.save(dir)?;
                return Ok(config);
            }
            Err(source) => return Err(ConfigError::Io { path, source }),
        };

        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        match config.version.cmp(&CURRENT_VERSION) {
            std::cmp::Ordering::Equal => Ok(config),
            std::cmp::Ordering::Greater => Err(ConfigError::NewerVersion {
                path,
                found: config.version,
                supported: CURRENT_VERSION,
            }),
            std::cmp::Ordering::Less => {
                let migrated = migrate(config, &path)?;
                migrated.save(dir)?;
                Ok(migrated)
            }
        }
    }

    /// Writes the config to `dir/config.toml`. The directory must already exist.
    /// The file is replaced atomically, so a crash cannot leave it half-written.
    pub fn save(&self, dir: &DataDir) -> Result<(), ConfigError> {
        let path = dir.config_file();
        let text = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)
            .and_then(|()| fs::rename(&tmp, &path))
            .map_err(|source| ConfigError::Io { path, source })
    }
}

/// Upgrades an older config to [`CURRENT_VERSION`].
fn migrate(config: Config, path: &Path) -> Result<Config, ConfigError> {
    // Each future version adds a step here that rewrites `config` and bumps
    // its version. There are no older versions yet, so any version below the
    // current one is unrecognised.
    Err(ConfigError::UnknownVersion {
        path: path.to_path_buf(),
        found: config.version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_dir() -> (tempfile::TempDir, DataDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::at(tmp.path());
        dir.create_all().unwrap();
        (tmp, dir)
    }

    #[test]
    fn first_run_writes_defaults() {
        let (_tmp, dir) = data_dir();

        let config = Config::load_or_create(&dir).unwrap();

        assert_eq!(config, Config::default());
        assert!(dir.config_file().is_file());
    }

    #[test]
    fn appearance_is_saved_and_loaded() {
        let (_tmp, dir) = data_dir();
        let config = Config {
            appearance: Appearance::Dark,
            ..Config::default()
        };
        config.save(&dir).unwrap();

        assert_eq!(
            Config::load_or_create(&dir).unwrap().appearance,
            Appearance::Dark
        );
    }

    #[test]
    fn files_without_an_appearance_follow_the_system() {
        let (_tmp, dir) = data_dir();
        fs::write(dir.config_file(), "version = 1\n").unwrap();

        assert_eq!(
            Config::load_or_create(&dir).unwrap().appearance,
            Appearance::System
        );
    }

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let (_tmp, dir) = data_dir();
        fs::write(
            dir.config_file(),
            "version = 1\nfuture_option = \"keep me\"\n",
        )
        .unwrap();

        let config = Config::load_or_create(&dir).unwrap();
        config.save(&dir).unwrap();

        let text = fs::read_to_string(dir.config_file()).unwrap();
        assert!(text.contains("future_option"), "{text}");
        assert!(text.contains("keep me"), "{text}");
    }

    #[test]
    fn newer_version_is_refused_and_left_alone() {
        let (_tmp, dir) = data_dir();
        let original = "version = 99\n";
        fs::write(dir.config_file(), original).unwrap();

        let err = Config::load_or_create(&dir).unwrap_err();

        assert!(
            matches!(err, ConfigError::NewerVersion { found: 99, .. }),
            "{err}"
        );
        assert_eq!(fs::read_to_string(dir.config_file()).unwrap(), original);
    }

    #[test]
    fn corrupt_file_is_an_error_and_is_not_overwritten() {
        let (_tmp, dir) = data_dir();
        let original = "version = \n";
        fs::write(dir.config_file(), original).unwrap();

        let err = Config::load_or_create(&dir).unwrap_err();

        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        assert_eq!(fs::read_to_string(dir.config_file()).unwrap(), original);
    }

    #[test]
    fn versions_below_current_are_not_migratable_yet() {
        let (_tmp, dir) = data_dir();
        fs::write(dir.config_file(), "version = 0\n").unwrap();

        let err = Config::load_or_create(&dir).unwrap_err();

        assert!(
            matches!(err, ConfigError::UnknownVersion { found: 0, .. }),
            "{err}"
        );
    }
}
