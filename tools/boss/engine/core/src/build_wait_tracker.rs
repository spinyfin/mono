//! Time-bounded suppression tracker for the [`crate::build_wait`] signal.
//!
//! Background: see [`crate::build_wait`] for the incident this pairs with.
//! Detecting that a worker is narrating a legitimate backgrounded-build wait
//! is only half the fix — requirement #2 of that incident's remediation is
//! that genuine wedge detection must keep working: a worker that keeps
//! saying "waiting" forever without ever actually finishing (the build is
//! genuinely broken, the armed monitor itself died, …) must still eventually
//! surface to a human rather than being suppressed indefinitely.
//!
//! This tracker is the discriminator between "waiting, and still within a
//! sane window for a slow/host-contended build" and "has been saying
//! 'waiting' for an unreasonable amount of time — stop trusting it and fall
//! back to the normal nudge/park flow". It is deliberately a *much* longer
//! horizon than the auto-nudge circuit breaker's ~2-minute-to-trip cadence
//! ([`crate::nudge_breaker`]) — that cadence was calibrated for a worker
//! that is genuinely unproductive on every nudge, not one legitimately
//! blocked on a multi-minute-to-hour build under host contention.
//!
//! State is in-memory only, mirroring [`crate::nudge_breaker::NudgeBreaker`]:
//! an engine restart resets the whole tracker, which is fine — a restarted
//! engine has no memory of how long a worker has been waiting anyway, and
//! the worker's own next Stop re-establishes a fresh baseline.

use std::collections::HashMap;
use std::sync::Mutex;

/// Default horizon a continuously-reported build-wait is trusted for before
/// the tracker stops suppressing nudges and falls back to the normal
/// nudge/park flow. 45 minutes: comfortably longer than the stale-worker
/// sweep's 30-minute wedge threshold
/// ([`crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS`]) plus margin
/// for the multi-sibling-bazel-build host contention the founding incident
/// exhibited, while still bounding an indefinite suppression.
pub const DEFAULT_BUILD_WAIT_HORIZON_SECS: i64 = 45 * 60;

/// Decision returned by [`BuildWaitTracker::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildWaitDecision {
    /// Still within the horizon — suppress the nudge. `waited_secs` is how
    /// long this execution has been continuously reporting a build wait.
    Suppress { waited_secs: i64 },
    /// The horizon has elapsed — stop trusting the narration and fall back
    /// to the normal nudge/park flow. `waited_secs` is how long it waited
    /// before the tracker gave up suppressing.
    Expired { waited_secs: i64 },
}

#[derive(Debug, Clone, Copy)]
struct BuildWaitRecord {
    /// Epoch seconds of the first Stop that reported a build-wait signal
    /// for this execution, continuously since (a `forget` call resets this).
    first_seen_epoch: i64,
}

/// In-memory `execution_id -> first_seen_epoch` tracker. Thread-safe; cheap
/// to clone-share behind an `Arc`.
#[derive(Debug, Default)]
pub struct BuildWaitTracker {
    inner: Mutex<HashMap<String, BuildWaitRecord>>,
}

impl BuildWaitTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `execution_id` reported a build-wait signal at
    /// `now_epoch_secs`, bounded by `horizon_secs`.
    ///
    /// The first call for an execution id stamps `first_seen_epoch` and
    /// returns `Suppress { waited_secs: 0 }`. Subsequent calls compare
    /// `now_epoch_secs` against that stamp: still within `horizon_secs` ⇒
    /// `Suppress`; at or past it ⇒ `Expired`. Once expired, the record is
    /// left in place (not reset) — a caller that keeps calling `record`
    /// after expiry keeps getting `Expired`, since the wait has not gotten
    /// any more legitimate by continuing; only [`Self::forget`] (real
    /// progress, or the execution being finalized) starts a fresh cycle.
    pub fn record(&self, execution_id: &str, now_epoch_secs: i64, horizon_secs: i64) -> BuildWaitDecision {
        let mut guard = self.inner.lock().expect("BuildWaitTracker mutex poisoned");
        let entry = guard.entry(execution_id.to_owned()).or_insert(BuildWaitRecord {
            first_seen_epoch: now_epoch_secs,
        });
        let waited_secs = (now_epoch_secs - entry.first_seen_epoch).max(0);
        if waited_secs >= horizon_secs {
            BuildWaitDecision::Expired { waited_secs }
        } else {
            BuildWaitDecision::Suppress { waited_secs }
        }
    }

    /// Drop any tracked state for `execution_id`. Called when the worker
    /// makes real progress (a new fingerprint, a PR opens, the execution is
    /// finalized) so a later, unrelated build-wait cycle starts clean.
    /// Idempotent.
    pub fn forget(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("BuildWaitTracker mutex poisoned")
            .remove(execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_suppresses_with_zero_waited_secs() {
        let tracker = BuildWaitTracker::new();
        assert_eq!(
            tracker.record("exec_a", 1_000, 2_700),
            BuildWaitDecision::Suppress { waited_secs: 0 },
        );
    }

    #[test]
    fn stays_suppressed_within_the_horizon() {
        let tracker = BuildWaitTracker::new();
        tracker.record("exec_a", 1_000, 2_700);
        assert_eq!(
            tracker.record("exec_a", 1_000 + 1_500, 2_700),
            BuildWaitDecision::Suppress { waited_secs: 1_500 },
            "1500s < the 2700s horizon must still suppress",
        );
    }

    #[test]
    fn expires_once_the_horizon_elapses() {
        let tracker = BuildWaitTracker::new();
        tracker.record("exec_a", 1_000, 2_700);
        assert_eq!(
            tracker.record("exec_a", 1_000 + 2_700, 2_700),
            BuildWaitDecision::Expired { waited_secs: 2_700 },
            "exactly at the horizon must expire",
        );
        assert_eq!(
            tracker.record("exec_a", 1_000 + 5_000, 2_700),
            BuildWaitDecision::Expired { waited_secs: 5_000 },
            "continuing to report a wait past expiry must not re-suppress",
        );
    }

    #[test]
    fn forget_starts_a_fresh_cycle() {
        let tracker = BuildWaitTracker::new();
        tracker.record("exec_a", 1_000, 2_700);
        tracker.record("exec_a", 1_000 + 2_700, 2_700);
        tracker.forget("exec_a");
        assert_eq!(
            tracker.record("exec_a", 1_000 + 2_700, 2_700),
            BuildWaitDecision::Suppress { waited_secs: 0 },
            "forget must reset the cycle",
        );
    }

    #[test]
    fn forget_is_idempotent() {
        let tracker = BuildWaitTracker::new();
        tracker.forget("never-tracked");
        tracker.forget("never-tracked");
        assert_eq!(
            tracker.record("never-tracked", 1_000, 2_700),
            BuildWaitDecision::Suppress { waited_secs: 0 },
        );
    }

    #[test]
    fn executions_are_tracked_independently() {
        let tracker = BuildWaitTracker::new();
        tracker.record("exec_a", 1_000, 2_700);
        tracker.record("exec_a", 1_000 + 2_700, 2_700);
        assert_eq!(
            tracker.record("exec_b", 5_000, 2_700),
            BuildWaitDecision::Suppress { waited_secs: 0 },
            "a different execution starts its own cycle",
        );
    }
}
