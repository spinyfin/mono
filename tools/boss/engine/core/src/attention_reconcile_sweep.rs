//! Periodic pass that lowers failure signals whose condition is over.
//!
//! The loop scaffold around
//! [`crate::work::WorkDb::reconcile_stale_attention_signals`], following the
//! same shape as [`crate::proposal_expiry_sweep`] (a pure `WorkDb` sweep with
//! no coordinator or dispatch-event dependency). See
//! [`crate::attention_lifecycle`] for the rule each kind declares and why.
//!
//! Reconciliation lives in the engine on purpose: deciding whether a raised
//! signal is still live is a read of execution history, and the app is a thin
//! client that renders `open` items and the `dispatch_failed_*` columns
//! without interpreting them.

use std::sync::Arc;
use std::time::Duration;

use crate::work::{AttentionReconcileOutcome, WorkDb};

/// Cadence for the periodic pass.
///
/// Every automatic rule's evidence is also acted on inline by the producer
/// path that generates it (a run start clears the dispatch banner in the same
/// transaction; `finalize_pr_review_pass` resolves the dead-review attention
/// as it completes), so this sweep is a backstop rather than the primary
/// clearing mechanism. Five minutes bounds how long a signal the inline paths
/// missed — because the engine restarted mid-condition, or because a future
/// code path forgot to hook one — can sit stale on a card, without adding
/// meaningful query load.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5 * 60);

impl crate::sweep_loop::SweepOutcome for AttentionReconcileOutcome {
    fn has_activity(&self) -> bool {
        !self.is_empty()
    }

    fn log(&self) {
        tracing::info!(
            attentions_resolved = self.attentions_resolved,
            dispatch_banners_cleared = self.dispatch_banners_cleared,
            "attention-reconcile sweep: lowered failure signals whose clearing evidence arrived",
        );
    }
}

/// How long the first pass waits for the first board to be served before
/// running anyway. The wait keeps the pass from contending with the cold
/// board load, but reconciliation must not depend on a frontend ever
/// connecting.
pub const FIRST_PASS_GRACE: Duration = Duration::from_secs(45);

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Only the first pass is gated: it waits for `first_board_served` or
/// `first_pass_grace`, whichever comes first.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    interval: Duration,
    first_board_served: Arc<tokio::sync::Notify>,
    first_pass_grace: Duration,
) -> tokio::task::JoinHandle<()> {
    let mut first = true;
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let gate = std::mem::take(&mut first).then(|| Arc::clone(&first_board_served));
        async move {
            if let Some(gate) = gate {
                // A Notify permit survives a snapshot delivered before this
                // task starts.
                let _ = tokio::time::timeout(first_pass_grace, gate.notified()).await;
            }
            run_one_pass(work_db).await
        }
    })
}

/// Run a single reconciliation pass off the async runtime. Returns the
/// tally; callers may log it. A DB error is logged and swallowed — this
/// sweep is best-effort cleanup and must never take the engine down.
pub async fn run_one_pass(work_db: Arc<WorkDb>) -> AttentionReconcileOutcome {
    crate::work::db_diagnostics::spawn_blocking(move || work_db.reconcile_stale_attention_signals())
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
        .unwrap_or_else(|err| {
            tracing::warn!(?err, "attention-reconcile sweep: pass failed; skipping");
            AttentionReconcileOutcome::default()
        })
}

#[cfg(test)]
mod tests;
