//! Fixed-window attempt limiting for the endpoint that accepts connections
//! from anyone: `Enroll` is answered before the caller has proved anything, so
//! it is throttled on its own, apart from the authenticated `Connect` stream.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use crate::clock::unix_secs;

/// Distinct keys tracked at once. Beyond this, a key not already tracked is
/// refused until older windows lapse, so a flood of sources cannot grow the
/// table without bound.
const MAX_KEYS: usize = 4096;

#[derive(Clone, Copy)]
struct Counter {
    started: i64,
    count: u32,
}

impl Counter {
    /// Starts a new window when the current one has lapsed.
    fn roll(&mut self, now: i64, window: i64) {
        if now >= self.started + window || now < self.started {
            *self = Counter { started: now, count: 0 };
        }
    }
}

struct State<K> {
    keys: HashMap<K, Counter>,
    overall: Counter,
}

/// Admits at most `per_key` attempts per key and `overall` attempts in total
/// per `window`.
pub struct AttemptLimiter<K> {
    window: Duration,
    per_key: u32,
    overall: u32,
    state: Mutex<State<K>>,
}

impl<K: Hash + Eq + Clone> AttemptLimiter<K> {
    pub fn new(window: Duration, per_key: u32, overall: u32) -> Self {
        Self {
            window,
            per_key,
            overall,
            state: Mutex::new(State {
                keys: HashMap::new(),
                overall: Counter { started: 0, count: 0 },
            }),
        }
    }

    /// Counts an attempt by `key`; `false` means it is over the limit and
    /// must be refused without doing any work.
    pub fn admit(&self, key: &K, now: SystemTime) -> bool {
        let now = unix_secs(now);
        let window = self.window.as_secs().max(1) as i64;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.overall.roll(now, window);
        if state.overall.count >= self.overall {
            return false;
        }
        if state.keys.len() >= MAX_KEYS && !state.keys.contains_key(key) {
            state.keys.retain(|_, counter| now < counter.started + window);
            if state.keys.len() >= MAX_KEYS {
                return false;
            }
        }
        let counter = state
            .keys
            .entry(key.clone())
            .or_insert(Counter { started: now, count: 0 });
        counter.roll(now, window);
        if counter.count >= self.per_key {
            return false;
        }
        counter.count += 1;
        state.overall.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + secs)
    }

    #[test]
    fn a_key_is_limited_per_window_and_recovers_after_it() {
        let limiter = AttemptLimiter::new(Duration::from_secs(60), 2, 100);
        assert!(limiter.admit(&"a", at(0)));
        assert!(limiter.admit(&"a", at(1)));
        assert!(!limiter.admit(&"a", at(2)), "third attempt in the window is refused");
        assert!(limiter.admit(&"b", at(2)), "another key is unaffected");
        assert!(limiter.admit(&"a", at(61)), "a new window starts");
    }

    #[test]
    fn the_overall_limit_holds_across_keys() {
        let limiter = AttemptLimiter::new(Duration::from_secs(60), 10, 3);
        let sources: Vec<IpAddr> = (1..=4).map(|n| IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))).collect();
        let admitted = sources.iter().filter(|ip| limiter.admit(*ip, at(0))).count();
        assert_eq!(admitted, 3);
        assert!(limiter.admit(&sources[0], at(60)), "the overall window also rolls");
    }

    #[test]
    fn the_table_is_bounded() {
        let limiter = AttemptLimiter::new(Duration::from_secs(60), 1, u32::MAX);
        for key in 0..MAX_KEYS {
            assert!(limiter.admit(&key, at(0)));
        }
        assert!(
            !limiter.admit(&MAX_KEYS, at(1)),
            "a new key is refused while the table is full"
        );
        assert!(limiter.admit(&MAX_KEYS, at(61)), "stale windows are reclaimed");
    }
}
