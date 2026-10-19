//! Presence: whether trusted contacts are reachable, and how they are reached.
//!
//! The node pings each trusted contact on a short-lived connection. A contact
//! that answers is online, reached directly or through a relay. A contact that
//! does not answer is checked again after a delay that doubles, up to
//! [`MAX_DELAY`], so an offline contact costs little.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use iroh::EndpointId;
use iroh::endpoint::Connection;

/// How long to wait between checks of a contact that is reachable, and the first
/// delay after one that is not.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// The longest delay between checks of a contact that is not reachable.
pub const MAX_DELAY: Duration = Duration::from_secs(15 * 60);

/// How a reachable contact is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Directly, over a hole-punched or local path.
    Direct,
    /// Through a relay server.
    Relayed,
}

/// The route the connection is using now.
pub fn route_of(connection: &Connection) -> Route {
    let selected_is_relay = connection
        .paths()
        .iter()
        .find(|path| path.is_selected())
        .map(|path| path.is_relay());
    match selected_is_relay {
        Some(true) => Route::Relayed,
        _ => Route::Direct,
    }
}

/// The delay before checking again, after `failures` checks in a row found the contact unreachable.
/// No failures means the normal interval.
pub fn next_delay(failures: u32) -> Duration {
    if failures == 0 {
        return CHECK_INTERVAL;
    }
    let factor = 2u32.saturating_pow(failures.min(16));
    CHECK_INTERVAL.saturating_mul(factor).min(MAX_DELAY)
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    next_check: Instant,
    failures: u32,
}

/// Decides which contacts are due for a check, and when each is next due.
#[derive(Debug, Default)]
pub struct Tracker {
    entries: HashMap<EndpointId, Entry>,
}

impl Tracker {
    /// Starts tracking `peers`, due now. Peers already tracked keep their schedule.
    pub fn track(&mut self, peers: impl IntoIterator<Item = EndpointId>, now: Instant) {
        for peer in peers {
            self.entries.entry(peer).or_insert(Entry {
                next_check: now,
                failures: 0,
            });
        }
    }

    /// Stops tracking every peer not in `keep`, such as contacts that were removed or blocked.
    pub fn retain(&mut self, keep: &[EndpointId]) {
        self.entries.retain(|peer, _| keep.contains(peer));
    }

    /// The peers due for a check at `now`.
    pub fn due(&self, now: Instant) -> Vec<EndpointId> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.next_check <= now)
            .map(|(peer, _)| *peer)
            .collect()
    }

    /// Records the result of a check. A reachable peer returns to the normal interval.
    pub fn record(&mut self, peer: EndpointId, reachable: bool, now: Instant) {
        let entry = self.entries.entry(peer).or_insert(Entry {
            next_check: now,
            failures: 0,
        });
        entry.failures = if reachable {
            0
        } else {
            entry.failures.saturating_add(1)
        };
        entry.next_check = now + next_delay(entry.failures);
    }

    /// How many checks in a row have failed for `peer`.
    pub fn failures(&self, peer: &EndpointId) -> u32 {
        self.entries.get(peer).map_or(0, |entry| entry.failures)
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    #[test]
    fn a_reachable_contact_is_checked_at_the_normal_interval() {
        assert_eq!(next_delay(0), CHECK_INTERVAL);
    }

    #[test]
    fn unreachable_contacts_are_checked_less_often_each_time_up_to_a_limit() {
        assert_eq!(next_delay(1), CHECK_INTERVAL * 2);
        assert_eq!(next_delay(2), CHECK_INTERVAL * 4);
        assert_eq!(next_delay(3), CHECK_INTERVAL * 8);
        assert_eq!(next_delay(4), MAX_DELAY);
        assert_eq!(next_delay(100), MAX_DELAY);
    }

    #[test]
    fn a_peer_is_due_once_its_delay_has_passed() {
        let peer = SecretKey::generate().public();
        let start = Instant::now();
        let mut tracker = Tracker::default();
        tracker.track([peer], start);
        assert_eq!(tracker.due(start), vec![peer]);

        tracker.record(peer, true, start);

        assert!(tracker.due(start).is_empty());
        assert_eq!(tracker.due(start + CHECK_INTERVAL), vec![peer]);
    }

    #[test]
    fn failures_back_off_and_success_resets_them() {
        let peer = SecretKey::generate().public();
        let start = Instant::now();
        let mut tracker = Tracker::default();
        tracker.track([peer], start);

        tracker.record(peer, false, start);
        tracker.record(peer, false, start);
        assert_eq!(tracker.failures(&peer), 2);
        assert!(tracker.due(start + CHECK_INTERVAL * 3).is_empty());

        tracker.record(peer, true, start);
        assert_eq!(tracker.failures(&peer), 0);
    }

    #[test]
    fn peers_that_are_no_longer_trusted_are_dropped() {
        let kept = SecretKey::generate().public();
        let dropped = SecretKey::generate().public();
        let start = Instant::now();
        let mut tracker = Tracker::default();
        tracker.track([kept, dropped], start);

        tracker.retain(&[kept]);

        assert_eq!(tracker.due(start), vec![kept]);
    }
}
