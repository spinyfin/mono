//! Worker-declared, time-bounded wait that holds the produce-a-PR nudge ladder.
//!
//! Companion to [`crate::hold_registry`]: that flag is an operator exemption
//! from idle-park *and* stale-worker reap. This registry is the worker's own
//! "I am deliberately waiting" signal, submitted through `boss propose wait`.
//! It suppresses only the produce-a-PR nudge and the nudge circuit breaker.
//! The 2-hour stale-worker reap, pane-death / dead-pid reconcile, hook-trust,
//! and every other safety check keep firing.
//!
//! Each declaration is capped at [`boss_protocol::WAIT_MAX_DURATION_SECS`];
//! the sum of granted durations per execution is capped at
//! [`boss_protocol::WAIT_MAX_TOTAL_SECS_PER_EXECUTION`]. Re-running the
//! command renews the active wait and charges the new duration against the
//! total, so overlapping renewals cannot run forever.
//!
//! State is in-memory only, mirroring [`crate::hold_registry::HoldRegistry`]
//! and [`crate::nudge_breaker::NudgeBreaker`]: an engine restart clears every
//! wait, which is the safe direction — nudges resume rather than silently
//! protecting a run whose worker is no longer around to renew. The
//! `worker_proposals` row remains the durable audit of each declaration.

use std::collections::HashMap;
use std::sync::Mutex;

use boss_protocol::{WAIT_MAX_DURATION_SECS, WAIT_MAX_TOTAL_SECS_PER_EXECUTION};

/// One execution's currently granted wait, plus the cumulative budget it
/// has already consumed.
#[derive(Debug, Clone)]
pub struct WaitRecord {
    pub reason: String,
    pub waiting_on: Option<String>,
    pub declared_at_epoch: i64,
    pub expires_at_epoch: i64,
    /// Duration charged against the per-execution total for this
    /// declaration, in seconds.
    pub granted_secs: u64,
}

/// Why [`WaitRegistry::declare`] refused a wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitDeclareError {
    DurationZero,
    DurationTooLong {
        requested: u64,
        max: u64,
    },
    CapExhausted {
        requested: u64,
        remaining: u64,
        used: u64,
        cap: u64,
    },
}

impl std::fmt::Display for WaitDeclareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitDeclareError::DurationZero => {
                write!(f, "duration must be at least 1 second")
            }
            WaitDeclareError::DurationTooLong { requested, max } => {
                write!(f, "duration {requested}s exceeds the per-declaration maximum of {max}s")
            }
            WaitDeclareError::CapExhausted {
                requested,
                remaining,
                used,
                cap,
            } => write!(
                f,
                "this execution has already used {used}s of wait-extension (cap {cap}s); \
                 remaining {remaining}s is less than the requested {requested}s"
            ),
        }
    }
}

#[derive(Debug, Default)]
struct WaitState {
    active: Option<WaitRecord>,
    total_granted_secs: u64,
    expiry_logged: bool,
}

/// In-memory `execution_id -> WaitState` registry. Thread-safe; cheap to
/// clone-share behind an `Arc`.
#[derive(Debug, Default)]
pub struct WaitRegistry {
    inner: Mutex<HashMap<String, WaitState>>,
    submit_locks: Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

impl WaitRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Per-execution async lock that serialises the whole wait submission
    /// (replay lookup, budget check, DB acceptance, grant commit). Without
    /// it two concurrent declarations can both pass [`check`](Self::check)
    /// against a budget that covers only one.
    pub fn submit_lock(&self, execution_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.submit_locks
            .lock()
            .expect("WaitRegistry mutex poisoned")
            .entry(execution_id.to_owned())
            .or_default()
            .clone()
    }

    /// Read-only pre-check: would [`declare`](Self::declare) accept
    /// `duration_secs` right now? Charges nothing and changes no state, so a
    /// caller can validate before persisting its audit row and only commit
    /// the grant once the row is freshly accepted.
    pub fn check(&self, execution_id: &str, duration_secs: u64) -> Result<(), WaitDeclareError> {
        let guard = self.inner.lock().expect("WaitRegistry mutex poisoned");
        let used = guard.get(execution_id).map(|s| s.total_granted_secs).unwrap_or(0);
        Self::validate(duration_secs, used)
    }

    fn validate(duration_secs: u64, used: u64) -> Result<(), WaitDeclareError> {
        if duration_secs == 0 {
            return Err(WaitDeclareError::DurationZero);
        }
        if duration_secs > WAIT_MAX_DURATION_SECS {
            return Err(WaitDeclareError::DurationTooLong {
                requested: duration_secs,
                max: WAIT_MAX_DURATION_SECS,
            });
        }
        let remaining = WAIT_MAX_TOTAL_SECS_PER_EXECUTION.saturating_sub(used);
        if duration_secs > remaining {
            return Err(WaitDeclareError::CapExhausted {
                requested: duration_secs,
                remaining,
                used,
                cap: WAIT_MAX_TOTAL_SECS_PER_EXECUTION,
            });
        }
        Ok(())
    }

    /// Grant (or renew) a wait on `execution_id`. Charges `duration_secs`
    /// against the per-execution total even when a previous wait has not
    /// yet expired, so renewals cannot stack unbounded time.
    pub fn declare(
        &self,
        execution_id: &str,
        reason: String,
        waiting_on: Option<String>,
        duration_secs: u64,
        now_epoch_secs: i64,
    ) -> Result<WaitRecord, WaitDeclareError> {
        let mut guard = self.inner.lock().expect("WaitRegistry mutex poisoned");
        let state = guard.entry(execution_id.to_owned()).or_default();
        Self::validate(duration_secs, state.total_granted_secs)?;

        let expires_at_epoch = now_epoch_secs.saturating_add(duration_secs as i64);
        let record = WaitRecord {
            reason,
            waiting_on,
            declared_at_epoch: now_epoch_secs,
            expires_at_epoch,
            granted_secs: duration_secs,
        };
        state.total_granted_secs = state.total_granted_secs.saturating_add(duration_secs);
        state.active = Some(record.clone());
        state.expiry_logged = false;
        tracing::info!(
            execution_id,
            reason = %record.reason,
            waiting_on = record.waiting_on.as_deref().unwrap_or("(none)"),
            duration_secs,
            expires_at_epoch,
            total_granted_secs = state.total_granted_secs,
            "worker wait declared — produce-a-PR nudge ladder held until expiry"
        );
        Ok(record)
    }

    /// The unexpired wait for `execution_id`, if any. Logs expiry once the
    /// first time a previously-active wait is observed past its deadline.
    pub fn active(&self, execution_id: &str, now_epoch_secs: i64) -> Option<WaitRecord> {
        let mut guard = self.inner.lock().expect("WaitRegistry mutex poisoned");
        let state = guard.get_mut(execution_id)?;
        let record = state.active.as_ref()?;
        if record.expires_at_epoch > now_epoch_secs {
            return Some(record.clone());
        }
        if !state.expiry_logged {
            tracing::info!(
                execution_id,
                reason = %record.reason,
                expires_at_epoch = record.expires_at_epoch,
                total_granted_secs = state.total_granted_secs,
                "worker wait expired — produce-a-PR nudge ladder resumes"
            );
            state.expiry_logged = true;
        }
        None
    }

    /// Cumulative seconds already granted to `execution_id`.
    pub fn total_granted_secs(&self, execution_id: &str) -> u64 {
        self.inner
            .lock()
            .expect("WaitRegistry mutex poisoned")
            .get(execution_id)
            .map(|state| state.total_granted_secs)
            .unwrap_or(0)
    }

    /// Drop all wait state for `execution_id`. Called when the execution
    /// ends so a later occupant of the same map slot cannot inherit budget.
    pub fn forget(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("WaitRegistry mutex poisoned")
            .remove(execution_id);
        self.submit_locks
            .lock()
            .expect("WaitRegistry mutex poisoned")
            .remove(execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declare_then_active_until_expiry() {
        let registry = WaitRegistry::new();
        let record = registry
            .declare("exec_a", "bazel test".into(), None, 60, 1_000)
            .expect("declare");
        assert_eq!(record.expires_at_epoch, 1_060);
        assert!(registry.active("exec_a", 1_000).is_some());
        assert!(registry.active("exec_a", 1_059).is_some());
        assert!(
            registry.active("exec_a", 1_060).is_none(),
            "expiry is exclusive of the deadline"
        );
        assert!(registry.active("exec_a", 1_061).is_none());
    }

    #[test]
    fn renew_replaces_active_wait_and_charges_the_total() {
        let registry = WaitRegistry::new();
        registry.declare("exec_a", "first".into(), None, 60, 1_000).unwrap();
        let renewed = registry
            .declare("exec_a", "still compiling".into(), Some("pid:12".into()), 120, 1_030)
            .unwrap();
        assert_eq!(renewed.reason, "still compiling");
        assert_eq!(renewed.waiting_on.as_deref(), Some("pid:12"));
        assert_eq!(renewed.expires_at_epoch, 1_150);
        assert_eq!(registry.total_granted_secs("exec_a"), 180);
        assert_eq!(
            registry.active("exec_a", 1_030).map(|r| r.reason),
            Some("still compiling".into())
        );
    }

    #[test]
    fn per_declaration_cap_is_enforced() {
        let registry = WaitRegistry::new();
        let err = registry
            .declare("exec_a", "too long".into(), None, WAIT_MAX_DURATION_SECS + 1, 0)
            .unwrap_err();
        assert_eq!(
            err,
            WaitDeclareError::DurationTooLong {
                requested: WAIT_MAX_DURATION_SECS + 1,
                max: WAIT_MAX_DURATION_SECS,
            }
        );
        assert!(registry.active("exec_a", 0).is_none());
    }

    #[test]
    fn total_cap_is_enforced_across_renewals() {
        let registry = WaitRegistry::new();
        registry
            .declare("exec_a", "one".into(), None, WAIT_MAX_DURATION_SECS, 0)
            .unwrap();
        registry
            .declare("exec_a", "two".into(), None, WAIT_MAX_DURATION_SECS, 10)
            .unwrap();
        let err = registry.declare("exec_a", "three".into(), None, 1, 20).unwrap_err();
        assert_eq!(
            err,
            WaitDeclareError::CapExhausted {
                requested: 1,
                remaining: 0,
                used: WAIT_MAX_TOTAL_SECS_PER_EXECUTION,
                cap: WAIT_MAX_TOTAL_SECS_PER_EXECUTION,
            }
        );
        assert_eq!(
            registry.active("exec_a", 20).map(|r| r.reason),
            Some("two".into()),
            "a refused declaration must not clobber the active wait"
        );
    }

    #[test]
    fn zero_duration_is_rejected() {
        let registry = WaitRegistry::new();
        assert_eq!(
            registry.declare("exec_a", "n".into(), None, 0, 0).unwrap_err(),
            WaitDeclareError::DurationZero
        );
    }

    #[test]
    fn forget_clears_budget_and_active_wait() {
        let registry = WaitRegistry::new();
        registry.declare("exec_a", "bazel".into(), None, 60, 0).unwrap();
        registry.forget("exec_a");
        assert!(registry.active("exec_a", 0).is_none());
        assert_eq!(registry.total_granted_secs("exec_a"), 0);
        registry
            .declare("exec_a", "fresh".into(), None, WAIT_MAX_DURATION_SECS, 0)
            .expect("forgetting must restore the full budget");
    }
}
