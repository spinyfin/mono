//! Operator-facing "hold" flag for a live run.
//!
//! Companion to [`crate::background_children`]'s root-cause fix: that
//! module teaches the idle-park/auto-reap sweeps to recognize *one*
//! specific shape of "waiting, not stalled" (live descendant processes).
//! There is no way to cover every shape a human operator might need to
//! protect a worker for — an unusual long-running dependency, a worker
//! the operator is actively debugging by hand, a known-noisy signal the
//! automated checks haven't been taught about yet. `bossctl agents hold`
//! is the explicit escape hatch: an operator marks a live run as held,
//! and every idle-park/auto-reap sweep skips it until the operator
//! releases the hold (or the run ends, at which point the hold is simply
//! never consulted again).
//!
//! Deliberately does NOT protect a held run from a direct, operator-issued
//! `bossctl agents stop` / `agents reap` — those are break-glass verbs the
//! operator invoked explicitly, and a hold that blocked them would turn a
//! safety net into a source of "why won't this stop" confusion. Hold only
//! ever gates the *automated* sweeps (see
//! [`crate::completion::WorkerCompletionHandler::nudge_or_park`] and
//! [`crate::stale_worker_sweep::run_one_pass`]).
//!
//! State is in-memory only, mirroring [`crate::nudge_breaker::NudgeBreaker`]
//! and [`crate::build_wait_tracker::BuildWaitTracker`]: an engine restart
//! clears every hold. That is the safe direction — a restarted engine has
//! no way to ask the (possibly long-gone) operator whether a hold is still
//! wanted, so defaulting back to "sweeps apply" rather than silently
//! protecting a run forever is correct.

use std::collections::HashMap;
use std::sync::Mutex;

/// One held run's metadata.
#[derive(Debug, Clone)]
pub struct HoldRecord {
    /// Free-text reason the operator supplied, if any (`bossctl agents
    /// hold <agent> --reason "..."`). Surfaced on `agents list`/`agents
    /// status` so a human staring at a held row can see why without
    /// digging through engine logs.
    pub reason: Option<String>,
    /// Epoch seconds the hold was placed. Surfaced alongside `reason` for
    /// the same reason.
    pub held_at_epoch: i64,
}

/// In-memory `execution_id -> HoldRecord` registry. Thread-safe; cheap to
/// clone-share behind an `Arc`.
#[derive(Debug, Default)]
pub struct HoldRegistry {
    inner: Mutex<HashMap<String, HoldRecord>>,
}

impl HoldRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Place (or replace) a hold on `execution_id`. Idempotent —
    /// re-holding an already-held run just refreshes `reason`/timestamp.
    pub fn hold(&self, execution_id: &str, reason: Option<String>, now_epoch_secs: i64) {
        self.inner.lock().expect("HoldRegistry mutex poisoned").insert(
            execution_id.to_owned(),
            HoldRecord {
                reason,
                held_at_epoch: now_epoch_secs,
            },
        );
    }

    /// Release any hold on `execution_id`. Returns `true` if a hold was
    /// actually present (so callers can distinguish "released" from
    /// "wasn't held"). Idempotent.
    pub fn release(&self, execution_id: &str) -> bool {
        self.inner
            .lock()
            .expect("HoldRegistry mutex poisoned")
            .remove(execution_id)
            .is_some()
    }

    /// True iff `execution_id` currently has a hold in place.
    pub fn is_held(&self, execution_id: &str) -> bool {
        self.inner
            .lock()
            .expect("HoldRegistry mutex poisoned")
            .contains_key(execution_id)
    }

    /// The hold record for `execution_id`, if any. Used to surface
    /// `reason`/`held_at_epoch` on `agents list`/`agents status`.
    pub fn get(&self, execution_id: &str) -> Option<HoldRecord> {
        self.inner
            .lock()
            .expect("HoldRegistry mutex poisoned")
            .get(execution_id)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_then_is_held() {
        let registry = HoldRegistry::new();
        assert!(!registry.is_held("exec_a"));
        registry.hold("exec_a", Some("debugging by hand".to_owned()), 1_000);
        assert!(registry.is_held("exec_a"));
        let record = registry.get("exec_a").expect("hold record must exist");
        assert_eq!(record.reason.as_deref(), Some("debugging by hand"));
        assert_eq!(record.held_at_epoch, 1_000);
    }

    #[test]
    fn release_clears_hold_and_reports_whether_one_existed() {
        let registry = HoldRegistry::new();
        assert!(!registry.release("exec_a"), "releasing a never-held run reports false");
        registry.hold("exec_a", None, 1_000);
        assert!(registry.release("exec_a"), "releasing a held run reports true");
        assert!(!registry.is_held("exec_a"));
    }

    #[test]
    fn re_holding_refreshes_reason_and_timestamp() {
        let registry = HoldRegistry::new();
        registry.hold("exec_a", Some("first reason".to_owned()), 1_000);
        registry.hold("exec_a", Some("second reason".to_owned()), 2_000);
        let record = registry.get("exec_a").unwrap();
        assert_eq!(record.reason.as_deref(), Some("second reason"));
        assert_eq!(record.held_at_epoch, 2_000);
    }

    #[test]
    fn holds_are_independent_per_execution() {
        let registry = HoldRegistry::new();
        registry.hold("exec_a", None, 1_000);
        assert!(!registry.is_held("exec_b"));
    }

    #[test]
    fn hold_without_reason_is_allowed() {
        let registry = HoldRegistry::new();
        registry.hold("exec_a", None, 1_000);
        assert!(registry.is_held("exec_a"));
        assert_eq!(registry.get("exec_a").unwrap().reason, None);
    }
}
