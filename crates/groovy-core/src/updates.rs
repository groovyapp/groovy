//! Checks GitHub Releases for a newer version of Groovy.
//!
//! The check asks the releases API for the latest release and compares its tag with this
//! build's version. It sends no identity: the request is anonymous, and only the version of
//! this build is in it. Nothing is downloaded or installed here; the app shows a banner that
//! links to the release page.

use std::time::Duration;

use semver::Version;
use serde::Deserialize;
use thiserror::Error;

/// Where the latest release is looked up.
pub const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/groovyapp/groovy/releases/latest";

/// The version this build was made from.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

const TIMEOUT: Duration = Duration::from_secs(15);

/// A newer release than this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub version: Version,
    /// The release's page, where the user can download it.
    pub page: String,
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("could not reach GitHub: {0}")]
    Network(#[from] reqwest::Error),
    #[error("GitHub's answer was not a release: {0}")]
    Malformed(String),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Looks for a release newer than `current`. Returns `None` when this build is up to date.
/// Drafts and pre-releases are not offered.
pub async fn check(current: &str) -> Result<Option<Update>, UpdateError> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(format!("Groovy/{current}"))
        .build()?;
    let body = client
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    newer_than(current, &body)
}

/// [`check`], for a thread that has no async runtime of its own.
pub fn check_blocking(current: &str) -> Result<Option<Update>, UpdateError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| UpdateError::Malformed(error.to_string()))?
        .block_on(check(current))
}

/// Reads a releases API answer, and returns the release if it is newer than `current`.
pub fn newer_than(current: &str, body: &str) -> Result<Option<Update>, UpdateError> {
    let release: Release =
        serde_json::from_str(body).map_err(|error| UpdateError::Malformed(error.to_string()))?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let latest = parse_tag(&release.tag_name)?;
    let current = Version::parse(current)
        .map_err(|error| UpdateError::Malformed(format!("this build's version: {error}")))?;
    Ok((latest > current).then_some(Update {
        version: latest,
        page: release.html_url,
    }))
}

/// A release tag such as `v0.1.0` as a version.
fn parse_tag(tag: &str) -> Result<Version, UpdateError> {
    Version::parse(tag.trim_start_matches('v'))
        .map_err(|error| UpdateError::Malformed(format!("tag {tag:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool, prerelease: bool) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://example.test/r","draft":{draft},"prerelease":{prerelease}}}"#
        )
    }

    #[test]
    fn a_newer_release_is_offered_with_its_page() {
        let update = newer_than("0.1.0", &release("v0.2.0", false, false))
            .unwrap()
            .unwrap();
        assert_eq!(update.version, Version::new(0, 2, 0));
        assert_eq!(update.page, "https://example.test/r");
    }

    #[test]
    fn the_same_or_an_older_release_is_not_offered() {
        assert_eq!(
            newer_than("0.1.0", &release("v0.1.0", false, false)).unwrap(),
            None
        );
        assert_eq!(
            newer_than("0.2.0", &release("v0.1.0", false, false)).unwrap(),
            None
        );
    }

    #[test]
    fn drafts_and_pre_releases_are_not_offered() {
        assert_eq!(
            newer_than("0.1.0", &release("v0.2.0", true, false)).unwrap(),
            None
        );
        assert_eq!(
            newer_than("0.1.0", &release("v0.2.0", false, true)).unwrap(),
            None
        );
    }

    #[test]
    fn a_patch_release_is_newer_than_its_base() {
        assert!(
            newer_than("0.1.0", &release("v0.1.1", false, false))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_tag_that_is_not_a_version_is_an_error_not_an_update() {
        assert!(matches!(
            newer_than("0.1.0", &release("nightly", false, false)),
            Err(UpdateError::Malformed(_))
        ));
    }

    #[test]
    fn an_answer_that_is_not_a_release_is_an_error() {
        assert!(matches!(
            newer_than("0.1.0", r#"{"message":"Not Found"}"#),
            Err(UpdateError::Malformed(_))
        ));
    }
}
