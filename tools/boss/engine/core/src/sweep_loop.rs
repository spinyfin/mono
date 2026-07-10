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

use std::future::Future;
use std::time::Duration;

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
