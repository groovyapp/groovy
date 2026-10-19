//! Pairing: two people agree that their nodes may trust each other.
//!
//! One side sends a pairing request after the handshake. The other side's user
//! sees it, compares a short verification code with the requester's, and
//! accepts or declines. Both sides compute the same code from the two endpoint
//! IDs, so the code can be read out loud or compared on screen.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use iroh::EndpointId;

/// How many pairing requests one endpoint may send in [`REQUEST_WINDOW`].
pub const MAX_REQUESTS: usize = 3;
/// The window over which [`MAX_REQUESTS`] applies.
pub const REQUEST_WINDOW: Duration = Duration::from_secs(60);

/// The six-digit code both sides of a pairing show, such as `123 456`.
///
/// It is derived from both endpoint IDs in sorted order, so each side computes
/// the same code without the two swapping roles.
pub fn verification_code(a: &EndpointId, b: &EndpointId) -> String {
    let (low, high) = if a.as_bytes() <= b.as_bytes() {
        (a, b)
    } else {
        (b, a)
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(low.as_bytes());
    hasher.update(high.as_bytes());
    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let number = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 1_000_000;
    format!("{:03} {:03}", number / 1000, number % 1000)
}

/// Limits how often each endpoint may ask to pair, so the prompts cannot be flooded.
#[derive(Debug)]
pub struct RateLimiter {
    max: usize,
    window: Duration,
    recent: HashMap<EndpointId, VecDeque<Instant>>,
}

impl RateLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            recent: HashMap::new(),
        }
    }

    /// Whether `peer` may make a request at `now`. A request that is allowed is counted.
    pub fn allow(&mut self, peer: EndpointId, now: Instant) -> bool {
        let times = self.recent.entry(peer).or_default();
        while times
            .front()
            .is_some_and(|first| now.duration_since(*first) >= self.window)
        {
            times.pop_front();
        }
        if times.len() >= self.max {
            return false;
        }
        times.push_back(now);
        true
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(MAX_REQUESTS, REQUEST_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    #[test]
    fn both_sides_compute_the_same_code() {
        let a = SecretKey::generate().public();
        let b = SecretKey::generate().public();

        assert_eq!(verification_code(&a, &b), verification_code(&b, &a));
    }

    #[test]
    fn the_code_is_six_digits_in_two_groups() {
        let code = verification_code(
            &SecretKey::generate().public(),
            &SecretKey::generate().public(),
        );

        assert_eq!(code.len(), 7);
        assert_eq!(code.as_bytes()[3], b' ');
        assert!(
            code.chars()
                .filter(|ch| *ch != ' ')
                .all(|ch| ch.is_ascii_digit())
        );
    }

    #[test]
    fn different_pairs_get_different_codes() {
        let a = SecretKey::generate().public();
        let b = SecretKey::generate().public();
        let c = SecretKey::generate().public();

        assert_ne!(verification_code(&a, &b), verification_code(&a, &c));
    }

    #[test]
    fn requests_beyond_the_limit_are_refused_until_the_window_passes() {
        let mut limiter = RateLimiter::new(2, Duration::from_secs(60));
        let peer = SecretKey::generate().public();
        let start = Instant::now();

        assert!(limiter.allow(peer, start));
        assert!(limiter.allow(peer, start));
        assert!(!limiter.allow(peer, start));
        assert!(limiter.allow(peer, start + Duration::from_secs(61)));
    }

    #[test]
    fn the_limit_is_per_endpoint() {
        let mut limiter = RateLimiter::new(1, Duration::from_secs(60));
        let start = Instant::now();

        assert!(limiter.allow(SecretKey::generate().public(), start));
        assert!(limiter.allow(SecretKey::generate().public(), start));
    }
}
