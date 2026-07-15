//! Shared scaffold for the engine's periodic reconciliation sweeps.
//!
//! Most sweep modules in `engine/core` (dead-pid, pool-claim,
//! lost-workspace, terminal-work, orphan, stale-worker, …) hand-roll the
//! identical loop body:
//!
//! ```ignore
//! tokio::spawn(async move {
//!     loop {
//!         let outcome = run_one_pass(...).await;
//!         if outcome.has_activity() {
//!             tracing::info!(...);
//!         }
//!         tokio::time::sleep(interval).await;
//!     }
//! })
//! ```
//!
//! Only the `run_one_pass` argument list and the specific `tracing`
//! fields differ. [`spawn_sweep_loop`] owns the common scaffold; each
//! sweep supplies a `pass_fn` closure that runs one pass and an
//! implementation of [`SweepOutcome`] that decides when a pass is worth
//! logging and what to log.

use std::collections::HashSet;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol::WorkExecution;

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::DispatchEventSink;
use crate::work::WorkDb;

/// Partition produced by [`confirm_two_pass`]: this pass's candidates split
/// by whether their key was also present on the previous pass.
pub(crate) struct Confirmation<T> {
    /// Candidates whose key appeared on both the previous pass and this one —
    /// confirmed across two consecutive passes, safe to act on.
    pub confirmed: Vec<T>,
    /// Candidates seen for the first time this pass — held one more interval
    /// before any action (the deferred, two-pass half).
    pub pending: Vec<T>,
}

/// Two-pass confirmation bookkeeping shared by the husk-pane and
/// terminal-work sweeps. Both build a set of candidate keys each pass, treat
/// a key absent from the prior `seen` set as pending (deferred one interval),
/// act on a key present in both the prior and current pass, then carry this
/// pass's keys forward as the next pass's `seen`.
///
/// Given the mutable `seen` set (the previous pass's candidate keys) and this
/// pass's `candidates` — each paired with the `K` it is identified by — this
/// partitions the candidates into [`Confirmation::confirmed`] (the key was
/// also in `seen`) and [`Confirmation::pending`] (first seen this pass), and
/// overwrites `seen` in place with this pass's full key set so the next pass
/// can confirm them. `seen` is not mutated mid-scan, so duplicate keys within
/// one pass are classified consistently.
///
/// Per-candidate logging and side effects (retire / reap, dispatch-event
/// emission) stay at the call sites; only this confirm-and-carry-forward
/// bookkeeping is shared. A pass the caller chooses to skip entirely (e.g. a
/// failed candidate lookup) must simply not call this, leaving `seen`
/// untouched so a transient blip does not restart the two-pass wait.
pub(crate) fn confirm_two_pass<K, T>(
    seen: &mut HashSet<K>,
    candidates: impl IntoIterator<Item = (K, T)>,
) -> Confirmation<T>
where
    K: Eq + Hash,
{
    let mut current: HashSet<K> = HashSet::new();
    let mut confirmed = Vec::new();
    let mut pending = Vec::new();

    for (key, item) in candidates {
        if seen.contains(&key) {
            confirmed.push(item);
        } else {
            pending.push(item);
        }
        current.insert(key);
    }

    *seen = current;
    Confirmation { confirmed, pending }
}

/// Look up an execution by id for a periodic sweep, logging a
/// per-sweep `warn` and returning `None` on lookup failure.
///
/// Several sweep/watch modules repeat the identical "look the execution
/// up, and if it's gone log a warning and skip this slot" block; the
/// only per-site difference is the log message. `context` is that
/// complete `warn` message (e.g. `"spawn-ack sweep: failed to look up
/// execution; skipping slot"`). On failure the emitted line preserves
/// the `execution_id` and `?err` fields so log output is unchanged.
///
/// Call sites keep their own skip behaviour via `let ... else`:
///
/// ```ignore
/// let Some(execution) =
///     lookup_execution_or_warn(&work_db, execution_id, "spawn-ack sweep: failed to look up execution; skipping slot")
/// else {
///     continue;
/// };
/// ```
pub(crate) fn lookup_execution_or_warn(work_db: &WorkDb, execution_id: &str, context: &str) -> Option<WorkExecution> {
    match work_db.get_execution(execution_id) {
        Ok(execution) => Some(execution),
        Err(err) => {
            tracing::warn!(execution_id, ?err, "{context}");
            None
        }
    }
}

/// Per-pass result of a periodic sweep. The loop scaffold uses
/// [`has_activity`](SweepOutcome::has_activity) to decide whether the
/// pass did meaningful work and [`log`](SweepOutcome::log) to emit the
/// sweep-specific structured `info` line.
pub(crate) trait SweepOutcome {
    /// Whether this pass did meaningful work worth logging.
    fn has_activity(&self) -> bool;

    /// Emit the sweep-specific structured `info` log line. Called by the
    /// loop scaffold only when [`has_activity`](Self::has_activity)
    /// returned `true`.
    fn log(&self);
}

/// Spawn a tokio task that runs `pass_fn` forever at `interval`, logging
/// each pass that reports activity. Fires immediately on spawn (no
/// initial sleep) so state left behind by a previous engine run is
/// reconciled at boot without waiting for the first interval — the
/// behaviour every sweep relies on.
pub(crate) fn spawn_sweep_loop<O, Fut, F>(interval: Duration, mut pass_fn: F) -> tokio::task::JoinHandle<()>
where
    O: SweepOutcome + Send,
    Fut: Future<Output = O> + Send,
    F: FnMut() -> Fut + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let outcome = pass_fn().await;
            if outcome.has_activity() {
                outcome.log();
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// The future a [`spawn_work_sweep_loop`] pass returns. Boxed because the
/// pass borrows the `&WorkDb` / `&dyn DispatchEventSink` the helper hands
/// it, and a higher-ranked closure cannot name that borrow's lifetime in a
/// generic future parameter. Costs one allocation per pass — once per
/// sweep interval, against a pass that hits the DB.
type SweepPassFuture<'a, O> = Pin<Box<dyn Future<Output = O> + Send + 'a>>;

/// Spawn a sweep loop over the engine's three standard sweep collaborators.
///
/// The `work_db` / `coordinator` / `dispatch_events` sweeps (dead-pane,
/// dispatch-failure recovery, orphan, PR-review recovery) all captured the
/// same `Arc` triple and re-cloned it per pass by hand. This owns that
/// capture: it clones the triple each pass and hands `pass_fn` the exact
/// shape `run_one_pass` already takes, so a call site is just
/// `Box::pin(run_one_pass(work_db, coordinator, dispatch_events))`.
///
/// `coordinator` arrives as an owned `Arc` because the kick path needs
/// `Arc<ExecutionCoordinator>`; the other two are borrows for the pass's
/// duration. A sweep needing extra per-pass collaborators (a PR-state
/// checker, a reaper) closes over them at the call site rather than
/// widening this signature.
///
/// Inherits [`spawn_sweep_loop`]'s fire-immediately-on-spawn contract.
pub(crate) fn spawn_work_sweep_loop<O, F>(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
    pass_fn: F,
) -> tokio::task::JoinHandle<()>
where
    O: SweepOutcome + Send,
    F: for<'a> Fn(&'a WorkDb, Arc<ExecutionCoordinator>, &'a dyn DispatchEventSink) -> SweepPassFuture<'a, O>
        + Send
        + Sync
        + 'static,
{
    let pass_fn = Arc::new(pass_fn);
    spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        let pass_fn = Arc::clone(&pass_fn);
        async move { pass_fn(work_db.as_ref(), coordinator, dispatch_events.as_ref()).await }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fake [`SweepOutcome`] that records how many times `log()` was
    /// invoked and reports a caller-supplied `has_activity` verdict.
    struct FakeOutcome {
        has_activity: bool,
        log_calls: Arc<AtomicUsize>,
    }

    impl SweepOutcome for FakeOutcome {
        fn has_activity(&self) -> bool {
            self.has_activity
        }

        fn log(&self) {
            self.log_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Hand control back to the runtime enough times for the spawned
    /// sweep task to be polled to its next `sleep(...).await` park point.
    /// The sweep future is always immediately ready, so a single poll
    /// completes a pass; we yield a few times purely for robustness and
    /// it consumes no virtual time, keeping the tests deterministic.
    async fn settle() {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    // (1) The boot-time reconciliation contract: `pass_fn` must fire once
    // immediately on spawn, before any interval has elapsed. We never
    // advance virtual time here, so the only way the counter can reach 1
    // is the loop running a pass before its first `sleep`.
    #[tokio::test(start_paused = true)]
    async fn fires_immediately_on_spawn_without_initial_sleep() {
        let passes = Arc::new(AtomicUsize::new(0));
        let log_calls = Arc::new(AtomicUsize::new(0));

        let passes_c = passes.clone();
        let log_c = log_calls.clone();
        let handle = spawn_sweep_loop(Duration::from_secs(60), move || {
            passes_c.fetch_add(1, Ordering::SeqCst);
            let log_c = log_c.clone();
            async move {
                FakeOutcome {
                    has_activity: false,
                    log_calls: log_c,
                }
            }
        });

        settle().await;

        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "pass_fn must fire exactly once immediately on spawn, without waiting for the interval",
        );

        handle.abort();
    }

    // (2) After each interval elapses, `pass_fn` fires again — once per
    // interval, forever. We drive virtual time forward by exactly one
    // interval at a time and check the pass count ticks up by one.
    #[tokio::test(start_paused = true)]
    async fn fires_once_per_interval_after_the_first() {
        let interval = Duration::from_secs(60);
        let passes = Arc::new(AtomicUsize::new(0));
        let log_calls = Arc::new(AtomicUsize::new(0));

        let passes_c = passes.clone();
        let log_c = log_calls.clone();
        let handle = spawn_sweep_loop(interval, move || {
            passes_c.fetch_add(1, Ordering::SeqCst);
            let log_c = log_c.clone();
            async move {
                FakeOutcome {
                    has_activity: false,
                    log_calls: log_c,
                }
            }
        });

        // Boot pass.
        settle().await;
        assert_eq!(passes.load(Ordering::SeqCst), 1, "boot pass");

        // Not yet due: advancing less than a full interval must not fire
        // another pass.
        tokio::time::advance(interval / 2).await;
        settle().await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "no pass before a full interval has elapsed",
        );

        // Completing the first interval fires the second pass.
        tokio::time::advance(interval / 2).await;
        settle().await;
        assert_eq!(passes.load(Ordering::SeqCst), 2, "pass after first interval");

        // And it keeps firing once per interval.
        tokio::time::advance(interval).await;
        settle().await;
        assert_eq!(passes.load(Ordering::SeqCst), 3, "pass after second interval");

        tokio::time::advance(interval).await;
        settle().await;
        assert_eq!(passes.load(Ordering::SeqCst), 4, "pass after third interval");

        handle.abort();
    }

    // ─── confirm_two_pass ────────────────────────────────────────────────────

    // First-seen keys land in `pending`; on a second consecutive pass the
    // same key is `confirmed`. `seen` is carried forward to this pass's keys.
    #[test]
    fn confirm_two_pass_defers_then_confirms() {
        let mut seen: HashSet<u8> = HashSet::new();

        let first = confirm_two_pass(&mut seen, vec![(1u8, "a"), (2u8, "b")]);
        assert!(first.confirmed.is_empty(), "nothing was seen last pass");
        assert_eq!(first.pending.len(), 2);
        assert_eq!(seen, HashSet::from([1, 2]));

        let second = confirm_two_pass(&mut seen, vec![(1u8, "a"), (2u8, "b")]);
        let mut confirmed = second.confirmed.clone();
        confirmed.sort_unstable();
        assert_eq!(confirmed, vec!["a", "b"], "both keys confirmed on the second pass");
        assert!(second.pending.is_empty());
    }

    // A key that drops out this pass leaves `seen`, so it cannot confirm; a
    // brand-new key this pass starts its own confirmation clock as pending.
    #[test]
    fn confirm_two_pass_drops_absent_keys_and_defers_new_ones() {
        let mut seen: HashSet<u8> = HashSet::from([1, 2]);

        // Key 1 persists (confirmed), key 2 vanished, key 3 is new (pending).
        let out = confirm_two_pass(&mut seen, vec![(1u8, "a"), (3u8, "c")]);
        assert_eq!(out.confirmed, vec!["a"]);
        assert_eq!(out.pending, vec!["c"]);
        // `seen` now reflects only this pass's keys — key 2 is gone.
        assert_eq!(seen, HashSet::from([1, 3]));
    }

    // (3) `log()` is invoked only on passes where `has_activity()` is
    // true and skipped otherwise. The fake alternates activity per pass
    // (active, inactive, active, …) so the log count only advances on the
    // active passes.
    #[tokio::test(start_paused = true)]
    async fn log_invoked_only_on_active_passes() {
        let interval = Duration::from_secs(60);
        let passes = Arc::new(AtomicUsize::new(0));
        let log_calls = Arc::new(AtomicUsize::new(0));

        let passes_c = passes.clone();
        let log_c = log_calls.clone();
        let handle = spawn_sweep_loop(interval, move || {
            // fetch_add returns the pre-increment value: 0, 1, 2, …
            let n = passes_c.fetch_add(1, Ordering::SeqCst);
            let has_activity = n.is_multiple_of(2); // passes 0, 2, 4 active; 1, 3 inactive
            let log_c = log_c.clone();
            async move {
                FakeOutcome {
                    has_activity,
                    log_calls: log_c,
                }
            }
        });

        // Pass 0 (active) → log fires.
        settle().await;
        assert_eq!(passes.load(Ordering::SeqCst), 1);
        assert_eq!(log_calls.load(Ordering::SeqCst), 1, "log() must fire on an active pass",);

        // Pass 1 (inactive) → log skipped.
        tokio::time::advance(interval).await;
        settle().await;
        assert_eq!(passes.load(Ordering::SeqCst), 2);
        assert_eq!(
            log_calls.load(Ordering::SeqCst),
            1,
            "log() must be skipped when has_activity() is false",
        );

        // Pass 2 (active) → log fires again.
        tokio::time::advance(interval).await;
        settle().await;
        assert_eq!(passes.load(Ordering::SeqCst), 3);
        assert_eq!(
            log_calls.load(Ordering::SeqCst),
            2,
            "log() must fire again on the next active pass",
        );

        // Pass 3 (inactive) → log skipped once more.
        tokio::time::advance(interval).await;
        settle().await;
        assert_eq!(passes.load(Ordering::SeqCst), 4);
        assert_eq!(
            log_calls.load(Ordering::SeqCst),
            2,
            "log() count must not advance on an inactive pass",
        );

        handle.abort();
    }
}
