//! Durable recovery for accepted `run_done` declarations that never reached
//! a terminal write.
//!
//! The submission path finalizes a `delivered` / `blocked` declaration
//! synchronously and spawns an in-process retry when that does not resolve.
//! Both are lost to an engine restart or a panicked task, and a blocked or
//! no-PR delivery has no poller to pick it up. The declaration stamp on the
//! execution row is the durable pending-finalize record; this sweep finds
//! live executions still carrying one and re-runs the finalize, and also
//! retries retained `no-changes-needed` completion writes. See
//! [`crate::completion::WorkerCompletionHandler::sweep_pending_finalizes`].

use std::sync::Arc;
use std::time::Duration;

use crate::completion::{PendingFinalizeSweepOutcome, WorkerCompletionHandler};

/// Cadence of the periodic pass. Fires on spawn, so a declaration stranded
/// before an engine restart is recovered at boot (after the grace below).
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

/// A declaration younger than this is left to the submission path, which
/// delivers the acknowledgement before it tears the worker down.
pub const MIN_DECLARATION_AGE_SECS: i64 = 30;

impl crate::sweep_loop::SweepOutcome for PendingFinalizeSweepOutcome {
    fn has_activity(&self) -> bool {
        *self != Self::default()
    }

    fn log(&self) {
        tracing::info!(
            finalized = self.finalized,
            still_pending = self.still_pending,
            no_ops_finalized = self.no_ops_finalized,
            no_ops_still_pending = self.no_ops_still_pending,
            "run_done finalize sweep: re-ran finalize for declared-done executions that were still live",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(handler: Arc<WorkerCompletionHandler>, interval: Duration) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let handler = Arc::clone(&handler);
        async move { run_one_pass(handler.as_ref()).await }
    })
}

/// Run a single pass with the production grace period.
pub async fn run_one_pass(handler: &WorkerCompletionHandler) -> PendingFinalizeSweepOutcome {
    let cutoff = boss_engine_utils::epoch_time::now_epoch_secs() - MIN_DECLARATION_AGE_SECS;
    handler.sweep_pending_finalizes(cutoff).await
}
