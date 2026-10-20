//! Who is in a group, and in what role.
//!
//! The group's membership is a set of signed records, kept in the group's iroh-docs document.
//! A record says that its signer made a change: named the group, or made someone an admin or a
//! member, or removed them. Records are replayed in time order, and a record is applied only if
//! its signer was an admin when it was made. The founder is always an admin. So a record from a
//! member who is not an admin is ignored, even though the document accepts it.
//!
//! Each group also has a key, which is the group's current epoch. Its gossip topic is derived
//! from the key, so removing a member changes the topic: the new key is given only to the
//! members who remain. The key is never written into the document, since every member can read
//! the document, including one who was removed.

use std::collections::BTreeMap;

use iroh::{EndpointId, SecretKey, Signature};
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize};

use crate::groups::GroupId;

/// What a member may do. Admins can change the membership; members can only read and write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Admin,
    Member,
}

/// A change to a group's membership or name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    /// Names the group.
    Name(String),
    /// Gives a member a role, or removes them when `role` is `None`.
    Role {
        member: EndpointId,
        role: Option<Role>,
    },
}

/// A change, signed by the endpoint that made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub group: GroupId,
    pub change: Change,
    pub by: EndpointId,
    /// Milliseconds since the Unix epoch, on the signer's clock.
    pub at: i64,
    pub signature: Signature,
}

/// A group's key for one epoch, signed by an admin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRecord {
    pub group: GroupId,
    /// Starts at 0 and goes up by one each time a member is removed.
    pub epoch: u32,
    pub key: [u8; 32],
    pub by: EndpointId,
    pub at: i64,
    pub signature: Signature,
}

/// Who is in a group, after replaying its records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Members {
    pub founder: EndpointId,
    /// The name the group was given most recently by an admin, if any.
    pub name: Option<String>,
    /// Every current member, with their role.
    pub roles: BTreeMap<EndpointId, Role>,
}

impl Members {
    /// Whether an endpoint is a current member, in any role.
    pub fn is_member(&self, endpoint: &EndpointId) -> bool {
        self.roles.contains_key(endpoint)
    }

    /// Whether an endpoint is a current admin.
    pub fn is_admin(&self, endpoint: &EndpointId) -> bool {
        self.roles.get(endpoint) == Some(&Role::Admin)
    }

    /// The members other than `me`.
    pub fn others(&self, me: &EndpointId) -> Vec<EndpointId> {
        self.roles.keys().filter(|id| *id != me).copied().collect()
    }

    /// Replays records into the group's current membership. Records for other groups, with a
    /// bad signature, or made by someone who was not an admin when they were made, are skipped.
    pub fn replay(group: GroupId, founder: EndpointId, records: &[Record]) -> Self {
        let mut ordered: Vec<&Record> = records
            .iter()
            .filter(|record| record.group == group && verify(record))
            .collect();
        ordered.sort_by(|a, b| a.at.cmp(&b.at).then(a.by.as_bytes().cmp(b.by.as_bytes())));
        let mut members = Members {
            founder,
            name: None,
            roles: BTreeMap::from([(founder, Role::Admin)]),
        };
        for record in ordered {
            if !members.is_admin(&record.by) {
                continue;
            }
            match &record.change {
                Change::Name(name) => members.name = Some(name.clone()),
                Change::Role { member, role } => {
                    // The founder is always an admin, so the founder's role is never lowered.
                    if *member == founder && *role != Some(Role::Admin) {
                        continue;
                    }
                    match role {
                        Some(role) => {
                            members.roles.insert(*member, *role);
                        }
                        None => {
                            members.roles.remove(member);
                        }
                    }
                }
            }
        }
        members
    }
}

/// What a record's signature covers.
fn record_bytes(group: &GroupId, change: &Change, by: &EndpointId, at: i64) -> Vec<u8> {
    postcard::to_stdvec(&(group, change, by, at)).expect("a record encodes")
}

fn key_bytes(group: &GroupId, epoch: u32, key: &[u8; 32], by: &EndpointId, at: i64) -> Vec<u8> {
    postcard::to_stdvec(&(group, epoch, key, by, at)).expect("a key record encodes")
}

/// Signs a membership change, as `secret`'s owner.
pub fn sign_record(secret: &SecretKey, group: GroupId, change: Change, at: i64) -> Record {
    let by = secret.public();
    let signature = secret.sign(&record_bytes(&group, &change, &by, at));
    Record {
        group,
        change,
        by,
        at,
        signature,
    }
}

/// Whether a record's signature is its signer's.
pub fn verify(record: &Record) -> bool {
    record
        .by
        .verify(
            &record_bytes(&record.group, &record.change, &record.by, record.at),
            &record.signature,
        )
        .is_ok()
}

/// Signs a group key for an epoch, as `secret`'s owner. The caller checks the signer is an admin.
pub fn sign_key(
    secret: &SecretKey,
    group: GroupId,
    epoch: u32,
    key: [u8; 32],
    at: i64,
) -> KeyRecord {
    let by = secret.public();
    let signature = secret.sign(&key_bytes(&group, epoch, &key, &by, at));
    KeyRecord {
        group,
        epoch,
        key,
        by,
        at,
        signature,
    }
}

/// Whether a key record's signature is its signer's.
pub fn verify_key(record: &KeyRecord) -> bool {
    record
        .by
        .verify(
            &key_bytes(
                &record.group,
                record.epoch,
                &record.key,
                &record.by,
                record.at,
            ),
            &record.signature,
        )
        .is_ok()
}

/// The gossip topic a group's messages travel on in one epoch.
pub fn topic_of(key: &[u8; 32]) -> TopicId {
    TopicId::from_bytes(blake3::derive_key("groovy group topic v1", key))
}

/// A new random group key.
pub fn new_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).expect("the system random source is available");
    key
}

/// The key a document entry holds a record under. Records are never overwritten, so each one
/// has its own key, made from the hash of its signature.
pub fn record_key(record: &Record) -> String {
    let hash = blake3::hash(&postcard::to_stdvec(&record.signature).expect("a signature encodes"));
    format!("record/{}", data_encoding::HEXLOWER.encode(hash.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group() -> GroupId {
        [9; 32]
    }

    fn role(member: EndpointId, role: Option<Role>, by: &SecretKey, at: i64) -> Record {
        sign_record(by, group(), Change::Role { member, role }, at)
    }

    #[test]
    fn the_founder_is_the_only_member_before_anyone_is_added() {
        let founder = SecretKey::generate();
        let members = Members::replay(group(), founder.public(), &[]);
        assert!(members.is_admin(&founder.public()));
        assert_eq!(members.roles.len(), 1);
    }

    #[test]
    fn an_admin_adds_a_member_and_a_member_cannot_add_anyone() {
        let founder = SecretKey::generate();
        let ben = SecretKey::generate();
        let cam = SecretKey::generate();
        let dan = SecretKey::generate();
        let records = [
            role(ben.public(), Some(Role::Member), &founder, 1),
            // Ben is a member, so his change to add Dan is ignored.
            role(dan.public(), Some(Role::Member), &ben, 2),
            role(cam.public(), Some(Role::Admin), &founder, 3),
            // Cam is an admin now, so this one counts.
            role(dan.public(), Some(Role::Member), &cam, 4),
        ];
        let members = Members::replay(group(), founder.public(), &records);
        assert!(members.is_member(&ben.public()));
        assert!(members.is_admin(&cam.public()));
        assert!(members.is_member(&dan.public()));
    }

    #[test]
    fn a_removed_member_is_no_longer_in_the_group_and_the_founder_stays_admin() {
        let founder = SecretKey::generate();
        let ben = SecretKey::generate();
        let records = [
            role(ben.public(), Some(Role::Member), &founder, 1),
            role(ben.public(), None, &founder, 2),
            // Removing the founder does nothing.
            role(founder.public(), None, &founder, 3),
        ];
        let members = Members::replay(group(), founder.public(), &records);
        assert!(!members.is_member(&ben.public()));
        assert!(members.is_admin(&founder.public()));
    }

    #[test]
    fn a_record_with_another_signers_name_is_ignored() {
        let founder = SecretKey::generate();
        let mallory = SecretKey::generate();
        let mut forged = sign_record(
            &mallory,
            group(),
            Change::Role {
                member: mallory.public(),
                role: Some(Role::Admin),
            },
            1,
        );
        forged.by = founder.public();
        let members = Members::replay(group(), founder.public(), &[forged]);
        assert!(!members.is_member(&mallory.public()));
    }

    #[test]
    fn the_name_is_the_latest_one_an_admin_gave() {
        let founder = SecretKey::generate();
        let records = [
            sign_record(&founder, group(), Change::Name("friends".into()), 1),
            sign_record(&founder, group(), Change::Name("family".into()), 2),
        ];
        let members = Members::replay(group(), founder.public(), &records);
        assert_eq!(members.name.as_deref(), Some("family"));
    }

    #[test]
    fn key_records_verify_for_their_signer_only() {
        let founder = SecretKey::generate();
        let key = sign_key(&founder, group(), 3, [1; 32], 5);
        assert!(verify_key(&key));
        let mut other = key.clone();
        other.epoch = 4;
        assert!(!verify_key(&other));
    }

    #[test]
    fn each_epoch_key_gives_its_own_topic() {
        assert_ne!(topic_of(&[1; 32]), topic_of(&[2; 32]));
        assert_eq!(topic_of(&[1; 32]), topic_of(&[1; 32]));
    }
}
