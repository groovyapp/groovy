//! Where Groovy keeps its files on disk.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use thiserror::Error;

/// Errors from locating or creating the data directory.
#[derive(Debug, Error)]
pub enum PathsError {
    #[error("no data directory for this user; set {0} to choose one")]
    NoDataDir(String),
    #[error("failed to create {path}")]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The environment variable that overrides the data directory, derived from
/// the app name: `groovy` gives `GROOVY_DATA_DIR`.
pub fn override_env_var(app_name: &str) -> String {
    format!("{}_DATA_DIR", app_name.to_uppercase().replace('-', "_"))
}

/// Groovy's data directory and the paths inside it.
///
/// Layout:
///
/// ```text
/// config.toml
/// identity/
/// blobs/
/// db/
/// logs/
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// Uses `{APP}_DATA_DIR` if it is set, otherwise the platform's data
    /// directory for `app_name`.
    pub fn resolve(app_name: &str) -> Result<Self, PathsError> {
        Self::resolve_with_override(app_name, std::env::var_os(override_env_var(app_name)))
    }

    fn resolve_with_override(
        app_name: &str,
        override_dir: Option<OsString>,
    ) -> Result<Self, PathsError> {
        if let Some(dir) = override_dir.filter(|dir| !dir.is_empty()) {
            return Ok(Self::at(dir));
        }
        let dirs = ProjectDirs::from("", "", app_name)
            .ok_or_else(|| PathsError::NoDataDir(override_env_var(app_name)))?;
        Ok(Self::at(dirs.data_dir()))
    }

    /// A data directory rooted at `root`. Nothing is created until [`Self::create_all`].
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn identity_dir(&self) -> PathBuf {
        self.root.join("identity")
    }

    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    pub fn db_dir(&self) -> PathBuf {
        self.root.join("db")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Creates the root and every subdirectory. Existing directories are left alone.
    pub fn create_all(&self) -> Result<(), PathsError> {
        let dirs = [
            self.root.clone(),
            self.identity_dir(),
            self.blobs_dir(),
            self.db_dir(),
            self.logs_dir(),
        ];
        for dir in dirs {
            fs::create_dir_all(&dir).map_err(|source| PathsError::Create { path: dir, source })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_variable_follows_app_name() {
        assert_eq!(override_env_var("groovy"), "GROOVY_DATA_DIR");
        assert_eq!(override_env_var("gpui-template"), "GPUI_TEMPLATE_DATA_DIR");
    }

    #[test]
    fn override_wins_over_platform_directory() {
        let dir = DataDir::resolve_with_override("groovy", Some("/custom/groovy".into())).unwrap();
        assert_eq!(dir.root(), Path::new("/custom/groovy"));
    }

    #[test]
    fn empty_override_falls_back_to_platform_directory() {
        let dir = DataDir::resolve_with_override("groovy", Some(OsString::new())).unwrap();
        assert!(dir.root().ends_with("groovy"), "{:?}", dir.root());
    }

    #[test]
    fn create_all_makes_every_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::at(tmp.path().join("data"));
        dir.create_all().unwrap();
        for sub in [
            dir.identity_dir(),
            dir.blobs_dir(),
            dir.db_dir(),
            dir.logs_dir(),
        ] {
            assert!(sub.is_dir(), "{} missing", sub.display());
        }
        // Calling it again on existing directories is fine.
        dir.create_all().unwrap();
    }
}
