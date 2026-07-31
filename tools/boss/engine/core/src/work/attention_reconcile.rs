//! Generic resolution pass for raised-but-never-lowered failure signals.
//!
//! Implements the automatic half of [`crate::attention_lifecycle`]: for every
//! attention kind that declares a [`ClearedBy`] variant the engine can prove,
//! resolve the open items whose evidence has since arrived, and clear the
//! `tasks.dispatch_failed_*` stamp behind the kanban card's
//! "Failed to start — …" banner on the same basis.
//!
//! Two invariants shape every query here:
//!
//! 1. **Evidence must postdate the signal.** A run that started before the
//!    attention was filed says nothing about it. Every `EXISTS` clause
//!    compares the evidence timestamp against the signal's own — so an item
//!    that is genuinely still broken accrues no evidence and stays open
//!    indefinitely, which is the point.
//! 2. **Resolution never deletes.** Rows are stamped `status = 'resolved'`
//!    with a `resolved_at`; the dispatch columns are nulled but the failure
//!    remains in the dispatch-event log and the execution's own history.
//!
//! All timestamp columns in this schema are epoch-seconds-as-text
//! (`audit_misc::now_string`), so `CAST(… AS INTEGER)` comparisons are exact
//! rather than lexicographic.

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::attention_lifecycle::{AttentionLifecycle, ClearedBy, automatically_cleared};

use super::WorkDb;
use super::audit_misc::now_string;

/// Per-pass tally from [`WorkDb::reconcile_stale_attention_signals`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AttentionReconcileOutcome {
    /// Attention items moved from `open` to `resolved` because their
    /// declared clearing evidence arrived.
    pub attentions_resolved: usize,
    /// Work items whose stale `dispatch_failed_*` stamp (the card's red
    /// "Failed to start" banner) was cleared because a run has since
    /// started for them.
    pub dispatch_banners_cleared: usize,
}

impl AttentionReconcileOutcome {
    /// Whether the pass did anything worth logging.
    pub fn is_empty(&self) -> bool {
        self.attentions_resolved == 0 && self.dispatch_banners_cleared == 0
    }
}

/// The work item an attention row is about, whichever way it is scoped.
///
/// `work_attention_items` has a `CHECK` constraint enforcing exactly one of
/// `execution_id` / `work_item_id`, and both scopes are in live use — the
/// sweeps file work-item-scoped rows, the completion handlers file
/// execution-scoped ones. Both need the same evidence lookup, so every query
/// below resolves through this expression rather than handling one scope and
/// silently ignoring the other (which is how half the kinds would have kept
/// leaking).
const ATTENTION_WORK_ITEM: &str = "COALESCE(
     work_attention_items.work_item_id,
     (SELECT we.work_item_id FROM work_executions we WHERE we.id = work_attention_items.execution_id)
 )";

/// `EXISTS` clause for [`ClearedBy::WorkResumed`]: a run for the same work
/// item started at or after the attention was raised.
///
/// The `>=` (rather than `>`) is deliberate. Timestamps have one-second
/// granularity, and a `WorkResumed` signal asserts that work is *not*
/// running; a run alive in the very second the signal was filed contradicts
/// it just as squarely as one a minute later. Requiring strict inequality
/// would leave a same-second signal stuck open forever, since `started_at`
/// never advances — reproducing the exact defect this pass exists to fix.
fn work_resumed_evidence() -> String {
    format!(
        "EXISTS (
             SELECT 1
             FROM work_runs r
             JOIN work_executions e ON e.id = r.execution_id
             WHERE e.work_item_id = {ATTENTION_WORK_ITEM}
               AND r.started_at IS NOT NULL
               AND CAST(r.started_at AS INTEGER) >= CAST(work_attention_items.created_at AS INTEGER)
         )"
    )
}

/// `EXISTS` clause for [`ClearedBy::ExecutionKindCompleted`]: an execution of
/// that kind for the same work item reached `completed` at or after the
/// attention was raised. Mirrors the shape of
/// `migrate_backfill_resolve_stale_dead_review_attentions`, which established
/// this evidence rule for `pr_review_died_without_findings`.
fn execution_kind_completed_evidence() -> String {
    format!(
        "EXISTS (
             SELECT 1
             FROM work_executions we
             WHERE we.work_item_id = {ATTENTION_WORK_ITEM}
               AND we.kind = ?3
               AND we.status = 'completed'
               AND we.finished_at IS NOT NULL
               AND CAST(we.finished_at AS INTEGER) >= CAST(work_attention_items.created_at AS INTEGER)
         )"
    )
}

fn resolve_one_kind(conn: &Connection, now: &str, lifecycle: &AttentionLifecycle) -> Result<usize> {
    let rows = match &lifecycle.cleared_by {
        ClearedBy::WorkResumed => {
            let sql = format!(
                "UPDATE work_attention_items
                 SET status = 'resolved', resolved_at = ?1
                 WHERE status = 'open'
                   AND kind = ?2
                   AND {}",
                work_resumed_evidence()
            );
            conn.execute(&sql, params![now, lifecycle.kind])?
        }
        ClearedBy::ExecutionKindCompleted(execution_kind) => {
            let sql = format!(
                "UPDATE work_attention_items
                 SET status = 'resolved', resolved_at = ?1
                 WHERE status = 'open'
                   AND kind = ?2
                   AND {}",
                execution_kind_completed_evidence()
            );
            conn.execute(&sql, params![now, lifecycle.kind, execution_kind.as_str()])?
        }
        // `automatically_cleared()` never yields these; matched explicitly so
        // adding a fifth variant is a compile error here rather than a silent
        // no-op.
        ClearedBy::ProducerReconciles | ClearedBy::HumanDecision => 0,
    };
    Ok(rows)
}

impl WorkDb {
    /// Resolve every open attention item whose declared clearing evidence has
    /// arrived, and clear stale dispatch-failure banners on the same basis.
    ///
    /// Idempotent and cheap to re-run: each statement only matches rows still
    /// in the un-cleared state, so a pass with nothing to do is a handful of
    /// indexed no-op updates. Safe to run concurrently with the producers —
    /// a kind whose producer resolves it inline simply finds nothing left.
    ///
    /// This is the *backstop*, not a replacement for the inline resolves
    /// already on the producer paths (`finalize_pr_review_pass`,
    /// `dispatch_claimed_execution`, `request_execution_in_tx_with_live_check`).
    /// Those clear a signal the instant the condition ends, which is better
    /// UX; this catches everything they structurally cannot — a condition
    /// that ended while a previous engine process was running, or via a code
    /// path nobody thought to hook.
    pub fn reconcile_stale_attention_signals(&self) -> Result<AttentionReconcileOutcome> {
        let conn = self.connect()?;
        let now = now_string();
        let mut outcome = AttentionReconcileOutcome::default();
        for lifecycle in automatically_cleared() {
            let resolved = resolve_one_kind(&conn, &now, lifecycle)?;
            if resolved > 0 {
                tracing::info!(
                    kind = lifecycle.kind,
                    resolved,
                    cleared_by = ?lifecycle.cleared_by,
                    "attention reconcile: resolved stale attention items whose clearing evidence arrived",
                );
            }
            outcome.attentions_resolved += resolved;
        }
        outcome.dispatch_banners_cleared = clear_stale_dispatch_failures(&conn, &now)?;
        Ok(outcome)
    }
}

/// Clear `tasks.dispatch_failed_reason` / `dispatch_failed_error` /
/// `dispatch_failed_at` for every work item that has since had a run start.
///
/// These columns are the sole source of the kanban card's red
/// "Failed to start — <reason>" banner (`WorkBoardCard.swift` reads
/// `task.dispatchFailedReason`; `WorkDispatchFailureBanner` renders it).
/// They are stamped by `bounce_dispatch_failed_to_backlog` when a pre-spawn
/// dispatch attempt is given up on, and the condition they assert is
/// precisely "the engine could not get an execution running for this item".
/// A run that started afterwards is the direct refutation.
///
/// `start_execution_run_on_host` performs the same clear inline, in the
/// transaction that flips the execution to `running`. This pass exists for
/// rows stamped before that inline clear existed, and for a start that
/// happened while a previous engine process was running.
fn clear_stale_dispatch_failures(conn: &Connection, now: &str) -> Result<usize> {
    let rows = conn.execute(
        "UPDATE tasks
         SET dispatch_failed_reason = NULL,
             dispatch_failed_error  = NULL,
             dispatch_failed_at     = NULL,
             updated_at             = ?1
         WHERE dispatch_failed_reason IS NOT NULL
           AND dispatch_failed_at IS NOT NULL
           AND EXISTS (
               SELECT 1
               FROM work_runs r
               JOIN work_executions e ON e.id = r.execution_id
               WHERE e.work_item_id = tasks.id
                 AND r.started_at IS NOT NULL
                 AND CAST(r.started_at AS INTEGER) >= CAST(tasks.dispatch_failed_at AS INTEGER)
           )",
        params![now],
    )?;
    if rows > 0 {
        tracing::info!(
            cleared = rows,
            "attention reconcile: cleared stale dispatch-failure banners on work items that have since started a run",
        );
    }
    Ok(rows)
}
