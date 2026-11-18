//! Single source of truth for reading the current wall-clock time as a
//! Unix epoch offset.
//!
//! The engine reads "now as epoch seconds/millis" in dozens of places
//! (sweeps, dispatch, audit, metrics, …). Historically each site
//! reimplemented the same `SystemTime::now().duration_since(UNIX_EPOCH)`
//! incantation, which drifted in return type and error handling. These
//! helpers consolidate that computation with a consistent fallback: on a
//! pre-epoch clock (`duration_since` error) they return `0` rather than
//! panicking, matching what almost every call site already did.
//!
//! Tests that exercise retry eligibility inject a thread-local seconds
//! value through [`NowGuard`] and advance it explicitly, so they do not
//! wait on the real epoch.

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

thread_local! {
    static NOW_SECS: Cell<Option<i64>> = const { Cell::new(None) };
}

fn wall_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current time as whole seconds since the Unix epoch.
///
/// Uses a thread-local override when a [`NowGuard`] is live on this
/// thread; otherwise the wall clock. Falls back to `0` if the system
/// clock is set before the epoch.
pub fn now_epoch_secs() -> i64 {
    NOW_SECS.with(|now| now.get()).unwrap_or_else(wall_epoch_secs)
}

/// Holds a thread-local epoch-seconds override for the current thread.
///
/// Drop restores the previous override (or the wall clock). Safe for
/// current-thread tokio tests that never hop threads for the eligibility
/// reads they inject.
pub struct NowGuard {
    previous: Option<i64>,
}

impl NowGuard {
    /// Pin [`now_epoch_secs`] to `now` on this thread until the guard drops.
    pub fn set(now: i64) -> Self {
        let previous = NOW_SECS.with(|cell| {
            let previous = cell.get();
            cell.set(Some(now));
            previous
        });
        Self { previous }
    }

    /// Advance the pinned clock to `now`.
    pub fn set_to(&self, now: i64) {
        NOW_SECS.with(|cell| cell.set(Some(now)));
    }
}

impl Drop for NowGuard {
    fn drop(&mut self) {
        NOW_SECS.with(|cell| cell.set(self.previous));
    }
}

/// Current wall-clock time as whole milliseconds since the Unix epoch.
///
/// Falls back to `0` if the system clock is set before the epoch.
pub fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Current wall-clock time as whole nanoseconds since the Unix epoch.
///
/// Falls back to `0` if the system clock is set before the epoch.
pub fn now_epoch_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_guard_pins_and_restores_epoch_seconds() {
        let wall = wall_epoch_secs();
        {
            let guard = NowGuard::set(1_700_000_000);
            assert_eq!(now_epoch_secs(), 1_700_000_000);
            guard.set_to(1_700_000_042);
            assert_eq!(now_epoch_secs(), 1_700_000_042);
        }
        let restored = now_epoch_secs();
        assert!(restored >= wall, "{restored} {wall}");
        assert!(restored < wall + 5, "{restored} {wall}");
    }
}
