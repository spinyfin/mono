//! Explicit "this execution's completion teardown is in flight" marker.
//!
//! ## The race this closes
//!
//! Every completion path terminalizes its execution row *before* it tears
//! the worker's pane down — the DB write and the pane teardown cannot be
//! one atomic step, and the row must go terminal first so nothing
//! re-dispatches the work item while the teardown runs. That leaves a
//! window in which the execution is observably **terminal with a live
//! pane**, which is exactly [`crate::terminal_work_sweep`]'s reap
//! precondition. On 2026-07-30 the window stretched to 388 s (an
//! unbounded `cube workspace release` subprocess sat inside it) and the
//! sweep reaped and SIGKILLed two workers that were still running,
//! mid-completion, on two different runs inside 45 s.
//!
//! Narrowing the window is not a fix: it is unbounded by construction,
//! and the sweep's own two-pass guard rests on the assumption that
//! "teardown still in flight would have completed within the interval",
//! which no constant can guarantee. What the sweep needs is not a longer
//! wait or a heuristic but an **explicit, written signal** that a given
//! pane already has an owner. That is this registry: a completion path
//! marks the execution before its terminalizing write and clears the mark
//! when teardown finishes, and the sweep skips any execution carrying a
//! live mark.
//!
//! ## Why the mark expires
//!
//! The sweep exists precisely to reclaim panes whose teardown never
//! landed. A mark with no bound would convert that failure mode from
//! "reaped after two passes" into "leaked forever" — the O'Brien zombie
//! this crate has already been bitten by twice. So a mark older than
//! [`MAX_TEARDOWN_PROTECTION`] stops protecting: the sweep logs the
//! overrun loudly and treats the candidate as the genuine strand it has
//! become. The bound is deliberately far above any real teardown (the
//! cube release inside it is itself timeout-bounded, and the pane release
//! is bounded by its own app-RPC and kill-grace timeouts), so it only
//! ever fires on a teardown that is actually wedged.
//!
//! ## In-memory, like its siblings
//!
//! State lives in memory only, mirroring [`crate::hold_registry`],
//! [`crate::nudge_breaker`] and [`crate::build_wait_tracker`]. An engine
//! restart clears every mark, which is the safe direction: a restarted
//! engine holds no in-flight teardown futures, so every surviving
//! terminal-with-live-pane run genuinely IS a strand and should be
//! reapable immediately.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a teardown mark suppresses sweep reclamation before the sweep
/// stops believing it. Ten minutes is roughly two orders of magnitude
/// above a healthy completion teardown and still well inside "an operator
/// would rather have the slot back".
pub const MAX_TEARDOWN_PROTECTION: Duration = Duration::from_secs(600);

/// What a sweep should conclude about an execution's teardown mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownState {
    /// No completion path currently owns this execution's teardown.
    Idle,
    /// A completion path is mid-teardown. The pane belongs to it; no
    /// sweep may reclaim it.
    InFlight { elapsed: Duration },
    /// A mark that outlived [`TeardownRegistry::max_protection`]. The
    /// teardown is wedged, so the mark stops protecting and the candidate
    /// is treated as a genuine strand.
    Expired { elapsed: Duration },
}

/// In-memory `execution_id -> teardown start` registry. Thread-safe and
/// cheap to clone-share behind an `Arc`.
#[derive(Debug)]
pub struct TeardownRegistry {
    inner: Mutex<HashMap<String, Instant>>,
    max_protection: Duration,
}

impl Default for TeardownRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TeardownRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_protection: MAX_TEARDOWN_PROTECTION,
        }
    }

    /// Registry with a non-default protection bound. Tests use
    /// `Duration::ZERO` to model an already-overrun mark without
    /// sleeping.
    pub fn with_max_protection(max_protection: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_protection,
        }
    }

    /// Mark `execution_id` as tearing down and return the RAII guard that
    /// clears the mark. Call this **before** the write that terminalizes
    /// the execution, so no sweep can ever observe the terminal row
    /// without also seeing the mark.
    ///
    /// Re-entering an already-marked execution refreshes the start
    /// instant; the mark clears when the LAST outstanding guard drops
    /// only in the sense that each guard removes the key — overlapping
    /// teardowns for one execution id do not happen in practice (the
    /// terminalizing DB write is a single-writer transaction that yields
    /// `AlreadyTerminal` to every loser).
    pub fn begin(self: &Arc<Self>, execution_id: &str) -> TeardownGuard {
        self.inner
            .lock()
            .expect("TeardownRegistry mutex poisoned")
            .insert(execution_id.to_owned(), Instant::now());
        TeardownGuard {
            registry: Arc::clone(self),
            execution_id: execution_id.to_owned(),
        }
    }

    /// Whether a sweep may reclaim `execution_id`'s pane, as of `now`.
    pub fn state(&self, execution_id: &str, now: Instant) -> TeardownState {
        let started = self
            .inner
            .lock()
            .expect("TeardownRegistry mutex poisoned")
            .get(execution_id)
            .copied();
        match started {
            None => TeardownState::Idle,
            Some(started) => {
                let elapsed = now.saturating_duration_since(started);
                if elapsed >= self.max_protection {
                    TeardownState::Expired { elapsed }
                } else {
                    TeardownState::InFlight { elapsed }
                }
            }
        }
    }

    /// Clear `execution_id`'s mark. Normally driven by [`TeardownGuard`]'s
    /// `Drop`; exposed for the guard itself.
    fn finish(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("TeardownRegistry mutex poisoned")
            .remove(execution_id);
    }
}

/// RAII handle returned by [`TeardownRegistry::begin`]. Dropping it clears
/// the mark, so every early return, `?`, panic, or dropped future on the
/// completion path releases the pane back to the sweeps rather than
/// leaking protection.
pub struct TeardownGuard {
    registry: Arc<TeardownRegistry>,
    execution_id: String,
}

impl TeardownGuard {
    /// The execution this guard protects — used by the completion paths
    /// for logging without threading the id separately.
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }
}

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        self.registry.finish(&self.execution_id);
    }
}

impl std::fmt::Debug for TeardownGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeardownGuard")
            .field("execution_id", &self.execution_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmarked_execution_is_idle() {
        let registry = Arc::new(TeardownRegistry::new());
        assert_eq!(registry.state("exec_a", Instant::now()), TeardownState::Idle);
    }

    #[test]
    fn marked_execution_is_in_flight_until_the_guard_drops() {
        let registry = Arc::new(TeardownRegistry::new());
        let guard = registry.begin("exec_a");
        assert!(matches!(
            registry.state("exec_a", Instant::now()),
            TeardownState::InFlight { .. }
        ));
        assert_eq!(guard.execution_id(), "exec_a");
        drop(guard);
        assert_eq!(
            registry.state("exec_a", Instant::now()),
            TeardownState::Idle,
            "dropping the guard must hand the pane back to the sweeps",
        );
    }

    #[test]
    fn marks_are_independent_per_execution() {
        let registry = Arc::new(TeardownRegistry::new());
        let _guard = registry.begin("exec_a");
        assert_eq!(registry.state("exec_b", Instant::now()), TeardownState::Idle);
    }

    /// The backstop guarantee: a teardown that never finishes must stop
    /// protecting its pane, or this registry would turn the sweep's
    /// zombie-reaping job off entirely.
    #[test]
    fn a_mark_past_the_protection_bound_reports_expired() {
        let registry = Arc::new(TeardownRegistry::with_max_protection(Duration::ZERO));
        let _guard = registry.begin("exec_a");
        assert!(matches!(
            registry.state("exec_a", Instant::now()),
            TeardownState::Expired { .. }
        ));
    }

    /// A guard dropped by unwinding (a panic anywhere inside the teardown)
    /// must still clear the mark — the whole point of making it RAII.
    #[test]
    fn a_panicking_teardown_still_clears_its_mark() {
        let registry = Arc::new(TeardownRegistry::new());
        let for_thread = Arc::clone(&registry);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = for_thread.begin("exec_a");
            panic!("teardown blew up");
        }));
        assert!(result.is_err(), "precondition: the closure must have panicked");
        assert_eq!(registry.state("exec_a", Instant::now()), TeardownState::Idle);
    }
}
