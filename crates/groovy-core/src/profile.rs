//! The local profile: the display name and avatar peers see for this node.
//!
//! Stored as a single row in the database. The avatar is kept as a small PNG
//! thumbnail, so only the thumbnail is ever stored or shared.

use crate::db::{Database, DbError};

const MAX_NAME_CHARS: usize = 64;

/// What the profile holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub display_name: String,
    /// A PNG thumbnail, if the user has chosen a picture.
    pub avatar_png: Option<Vec<u8>>,
}

/// The display name to keep: trimmed, and between 1 and 64 characters.
pub fn validate_display_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (1..=MAX_NAME_CHARS)
        .contains(&name.chars().count())
        .then(|| name.to_owned())
}

impl Database {
    /// The profile, or `None` before onboarding has saved one.
    pub fn profile(&self) -> Result<Option<Profile>, DbError> {
        self.with_conn(|conn| {
            let mut statement =
                conn.prepare("SELECT display_name, avatar_png FROM profile WHERE id = 1")?;
            let mut rows = statement.query_map([], |row| {
                Ok(Profile {
                    display_name: row.get(0)?,
                    avatar_png: row.get(1)?,
                })
            })?;
            rows.next().transpose()
        })
    }

    /// Saves the display name, creating the profile if needed and keeping any avatar.
    pub fn save_display_name(&self, name: &str) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO profile (id, display_name) VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET display_name = excluded.display_name",
                [name],
            )
            .map(|_| ())
        })
    }

    /// Sets or clears the avatar. Returns `false` if there is no profile yet.
    pub fn save_avatar(&self, png: Option<&[u8]>) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.execute("UPDATE profile SET avatar_png = ?1 WHERE id = 1", [png])
                .map(|updated| updated > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> (tempfile::TempDir, Database) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("groovy.sqlite")).unwrap();
        (tmp, db)
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(validate_display_name("  Ada  "), Some("Ada".to_owned()));
        assert_eq!(validate_display_name("   "), None);
        assert_eq!(validate_display_name(""), None);
        assert_eq!(
            validate_display_name(&"a".repeat(64)).map(|n| n.len()),
            Some(64)
        );
        assert_eq!(validate_display_name(&"a".repeat(65)), None);
    }

    #[test]
    fn there_is_no_profile_until_a_name_is_saved() {
        let (_tmp, db) = database();
        assert_eq!(db.profile().unwrap(), None);
    }

    #[test]
    fn name_changes_persist_and_keep_the_avatar() {
        let (_tmp, db) = database();
        db.save_display_name("Ada").unwrap();
        assert!(db.save_avatar(Some(&[1, 2, 3])).unwrap());

        db.save_display_name("Ada Lovelace").unwrap();

        let profile = db.profile().unwrap().unwrap();
        assert_eq!(profile.display_name, "Ada Lovelace");
        assert_eq!(profile.avatar_png, Some(vec![1, 2, 3]));
    }

    #[test]
    fn the_avatar_can_be_cleared() {
        let (_tmp, db) = database();
        db.save_display_name("Ada").unwrap();
        db.save_avatar(Some(&[1, 2, 3])).unwrap();

        db.save_avatar(None).unwrap();

        assert_eq!(db.profile().unwrap().unwrap().avatar_png, None);
    }

    #[test]
    fn an_avatar_needs_a_profile_first() {
        let (_tmp, db) = database();
        assert!(!db.save_avatar(Some(&[1])).unwrap());
    }
}
