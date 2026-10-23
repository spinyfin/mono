//! The hub's clock, injectable so expiry and overlap are testable without
//! sleeping.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Source of the current wall-clock time.
///
/// Certificate validity is judged by the hub's clock, never the spoke's:
/// a spoke with a skewed clock fails its own preflight rather than being
/// believed.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> SystemTime;
}

/// The operating-system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Whole seconds since the Unix epoch (negative before it).
pub(crate) fn unix_secs(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    }
}

pub(crate) fn from_unix_secs(secs: i64) -> SystemTime {
    match u64::try_from(secs) {
        Ok(after) => UNIX_EPOCH + Duration::from_secs(after),
        Err(_) => UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_seconds_round_trip_on_both_sides_of_the_epoch() {
        for secs in [-90, 0, 1, 1_800_000_000] {
            assert_eq!(unix_secs(from_unix_secs(secs)), secs);
        }
    }
}
