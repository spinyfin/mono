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
//!
//! `make_attempt` is invoked directly on the supervisor task (wrapped in
//! `catch_unwind`), and only the returned future is handed to a child
//! task. That means both a panic in the synchronous setup a caller does
//! before returning its future (e.g. cloning `Arc`s, re-subscribing to
//! the bus) and a panic inside the future itself go through the same
//! restart path — do not assume only the future's body is supervised.

use std::any::Any;
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

/// Delay before the first restart attempt after a panic.
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
/// Ceiling the restart delay backs off to under repeated panics.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Wraps a child attempt's `JoinHandle` so that dropping this future
/// (e.g. because the supervisor task itself was aborted while awaiting
/// it) aborts the child task too, instead of leaking it to keep running
/// detached in the background.
struct AbortOnDrop(JoinHandle<()>);

impl Future for AbortOnDrop {
    type Output = Result<(), tokio::task::JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Advances the restart backoff after a panic that started at `started`.
///
/// An attempt that ran for at least `MAX_BACKOFF` before panicking is
/// treated as having been stable, so the delay resets to the initial
/// value rather than staying pinned at the ceiling; otherwise the delay
/// doubles, capped at `MAX_BACKOFF`.
fn next_backoff(started: Instant, current: Duration) -> Duration {
    if started.elapsed() >= MAX_BACKOFF {
        INITIAL_BACKOFF
    } else {
        std::cmp::min(current.saturating_mul(2), MAX_BACKOFF)
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Spawn `make_attempt` as a supervised subscriber loop.
///
/// `make_attempt` is invoked once to start, and again every time the
/// previous attempt panics — either while constructing its future or
/// while running it. Each invocation is expected to run a full reconcile
/// pass before settling into its `Subscription::recv` loop, so a
/// post-panic restart reconciles exactly like a fresh boot — this is the
/// crash-recovery contract the event-bus design doc documents for
/// subscriber loops. `name` identifies the subscriber in the
/// panic-restart log line.
///
/// Restarts back off (starting at 100ms, doubling up to a 5s ceiling,
/// resetting once an attempt has run stably for at least that ceiling)
/// so a deterministically panicking subscriber cannot spin hot forever.
///
/// A `make_attempt` future that returns normally ends supervision:
/// subscriber loops are expected to run forever (matching the sweep-loop
/// convention), so a clean return only happens when the caller wants the
/// subscriber to stop, not on every pass.
///
/// Aborting the returned handle propagates to whichever attempt is
/// currently in flight, so a caller that aborts supervision does not
/// leak a running attempt task.
pub fn spawn_supervised<F, Fut>(name: &'static str, mut make_attempt: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            let started = Instant::now();

            let attempt_fut = match panic::catch_unwind(AssertUnwindSafe(&mut make_attempt)) {
                Ok(fut) => fut,
                Err(payload) => {
                    tracing::warn!(
                        subscriber = name,
                        panic = %panic_message(&*payload),
                        "subscriber loop panicked while starting; restarting with full reconcile"
                    );
                    backoff = next_backoff(started, backoff);
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            };

            match AbortOnDrop(tokio::spawn(attempt_fut)).await {
                Ok(()) => return,
                Err(err) if err.is_panic() => {
                    tracing::warn!(
                        subscriber = name,
                        ?err,
                        "subscriber loop panicked; restarting with full reconcile"
                    );
                    backoff = next_backoff(started, backoff);
                    tokio::time::sleep(backoff).await;
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
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::spawn_supervised;

    /// A counter that a test can `wait_at_least` on without relying on a
    /// fixed number of scheduler yields: the waiter is woken exactly when
    /// the count actually advances, and a timeout guards against a
    /// genuine hang instead of a flaky under-settle.
    struct Counter {
        value: AtomicUsize,
        notify: Notify,
    }

    impl Counter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                value: AtomicUsize::new(0),
                notify: Notify::new(),
            })
        }

        fn bump(&self) -> usize {
            let n = self.value.fetch_add(1, Ordering::SeqCst);
            self.notify.notify_waiters();
            n
        }

        fn get(&self) -> usize {
            self.value.load(Ordering::SeqCst)
        }

        async fn wait_at_least(&self, target: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let notified = self.notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.get() >= target {
                        return;
                    }
                    notified.await;
                }
            })
            .await
            .expect("attempt count did not reach target before timing out");
        }
    }

    // A panicking attempt is restarted: `make_attempt` is invoked again
    // rather than the supervisor task dying with it.
    #[tokio::test]
    async fn restarts_after_a_panic() {
        tokio::time::pause();

        let attempts = Counter::new();

        let attempts_c = attempts.clone();
        let handle = spawn_supervised("test-subscriber", move || {
            let n = attempts_c.bump();
            async move {
                if n == 0 {
                    panic!("simulated subscriber panic");
                }
                // Second attempt onward: park forever, like a real
                // subscriber's `Subscription::recv` loop.
                std::future::pending::<()>().await;
            }
        });

        attempts.wait_at_least(2).await;
        assert_eq!(
            attempts.get(),
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
        tokio::time::pause();

        let reconciles = Counter::new();
        let attempts = Arc::new(AtomicUsize::new(0));

        let reconciles_c = reconciles.clone();
        let attempts_c = attempts.clone();
        let handle = spawn_supervised("test-subscriber", move || {
            reconciles_c.bump();
            let n = attempts_c.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    panic!("simulated subscriber panic");
                }
                std::future::pending::<()>().await;
            }
        });

        reconciles.wait_at_least(3).await;
        assert_eq!(
            reconciles.get(),
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

    // Aborting the returned handle must not leak the in-flight attempt:
    // once the supervisor is aborted, the attempt task stops running too.
    #[tokio::test]
    async fn abort_stops_the_in_flight_attempt() {
        tokio::time::pause();

        let running = Arc::new(AtomicUsize::new(0));

        let running_c = running.clone();
        let handle = spawn_supervised("test-subscriber", move || {
            let running = running_c.clone();
            async move {
                running.fetch_add(1, Ordering::SeqCst);
                let _guard = DecrementOnDrop(running);
                std::future::pending::<()>().await;
            }
        });

        // Give the first attempt a chance to start and mark itself running.
        while running.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }

        handle.abort();
        // Let the abort propagate to the child task.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            running.load(Ordering::SeqCst),
            0,
            "aborting the supervisor must abort the in-flight attempt, not leak it",
        );
    }

    struct DecrementOnDrop(Arc<AtomicUsize>);

    impl Drop for DecrementOnDrop {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
