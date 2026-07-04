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
