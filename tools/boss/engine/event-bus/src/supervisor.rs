//! Crash-recovery supervision for subscriber loops.
//!
//! Every subscriber loop follows the same shape: run a full reconcile
//! pass, then process bus events forever. [`spawn_supervised`] owns the
//! restart half of that contract — if the loop's future panics, the
//! supervisor logs the panic and calls the loop-constructing closure
//! again, which is expected to redo its full reconcile pass before
//! resuming `Subscription::recv`. That is what makes an in-memory,
//! best-effort bus safe across a subscriber crash: whatever event the
//! dead subscriber missed is caught by the reconcile pass on restart,
//! the same way the boot-time sweep catches state left behind by an
//! engine restart.

use std::future::Future;

use tokio::task::JoinHandle;

/// Spawn `make_attempt` as a supervised subscriber loop.
///
/// `make_attempt` is invoked once to start, and again every time the
/// previous attempt's future panics. Each invocation is expected to run
/// a full reconcile pass before settling into its `Subscription::recv`
/// loop, so a post-panic restart reconciles exactly like a fresh boot —
/// this is the crash-recovery contract the event-bus design doc
/// documents for subscriber loops. `name` identifies the subscriber in
/// the panic-restart log line.
///
/// A `make_attempt` future that returns normally ends supervision:
/// subscriber loops are expected to run forever (matching the sweep-loop
/// convention), so a clean return only happens when the caller wants the
/// subscriber to stop, not on every pass.
pub fn spawn_supervised<F, Fut>(name: &'static str, mut make_attempt: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(make_attempt());
            match handle.await {
                Ok(()) => return,
                Err(err) if err.is_panic() => {
                    tracing::warn!(
                        subscriber = name,
                        ?err,
                        "subscriber loop panicked; restarting with full reconcile"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        subscriber = name,
                        ?err,
                        "subscriber loop task cancelled; stopping supervision"
                    );
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::spawn_supervised;

    /// Hand control back to the runtime enough times for spawned tasks
    /// (the supervisor and its supervised attempts) to run to their next
    /// await point.
    async fn settle() {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    // A panicking attempt is restarted: `make_attempt` is invoked again
    // rather than the supervisor task dying with it.
    #[tokio::test]
    async fn restarts_after_a_panic() {
        let attempts = Arc::new(AtomicUsize::new(0));

        let attempts_c = attempts.clone();
        let handle = spawn_supervised("test-subscriber", move || {
            let n = attempts_c.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    panic!("simulated subscriber panic");
                }
                // Second attempt onward: park forever, like a real
                // subscriber's `Subscription::recv` loop.
                std::future::pending::<()>().await;
            }
        });

        settle().await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "a panicked attempt must be followed by exactly one restart attempt",
        );

        handle.abort();
    }

    // The crash-recovery contract: each attempt (including the very
    // first, and every restart after a panic) must reconcile before
    // resuming its loop. We model "reconcile" as a counter bumped at the
    // top of every attempt, independent of whichever pass panics.
    #[tokio::test]
    async fn reconciles_on_every_attempt_including_restarts() {
        let reconciles = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::new(AtomicUsize::new(0));

        let reconciles_c = reconciles.clone();
        let attempts_c = attempts.clone();
        let handle = spawn_supervised("test-subscriber", move || {
            reconciles_c.fetch_add(1, Ordering::SeqCst);
            let n = attempts_c.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    panic!("simulated subscriber panic");
                }
                std::future::pending::<()>().await;
            }
        });

        settle().await;
        assert_eq!(
            reconciles.load(Ordering::SeqCst),
            3,
            "a full reconcile pass must run on the initial attempt and on both restarts after a panic",
        );

        handle.abort();
    }

    // A clean (non-panicking) return from `make_attempt`'s future ends
    // supervision instead of looping forever.
    #[tokio::test]
    async fn stops_supervising_after_a_clean_return() {
        let attempts = Arc::new(AtomicUsize::new(0));

        let attempts_c = attempts.clone();
        let handle = spawn_supervised("test-subscriber", move || {
            attempts_c.fetch_add(1, Ordering::SeqCst);
            async move {}
        });

        handle.await.expect("supervisor task must not panic itself");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a clean return must not trigger a restart",
        );
    }
}
