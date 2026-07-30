//! Returning an execution to the dispatch queue without terminalizing it.
//!
//! Split out of `executions_runs.rs` (which is at its `file/size` ceiling)
//! along a real seam: every other transition in that module moves a row
//! *forward* to a terminal state, whereas this one deliberately moves it
//! *back* to `ready`. The distinction is the whole point of the module —
//! see the method's doc comment for why a host-environment spawn failure
//! must leave no terminal row behind.

use super::*;

impl WorkDb {
    /// Return an execution to the dispatch queue after a spawn failed for a
    /// reason belonging to the **host**, not to the work.
    ///
    /// ## Why this is not `mark_execution_orphaned`
    ///
    /// The orphan path terminalizes the row, and every terminal row counts
    /// against the work item's churn budget
    /// ([`Self::count_recent_terminal_executions`], threshold
    /// [`crate::work::ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD`] per
    /// [`crate::work::ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS`]). So an
    /// operator whose display slept three times — or once, against three
    /// work items — had those items parked as if *they* were defective. That
    /// is the work-destroying half of the 2026-07-30 incident: a spawn that
    /// failed because the machine had no active display is not evidence
    /// about the work item at all, and must leave no terminal row behind.
    ///
    /// So the row is reset to `ready` rather than terminalized: no terminal
    /// status, nothing for the churn guard to count, no dependence on the
    /// orphan sweep noticing and creating a replacement, and the work item
    /// keeps its place instead of being reset to `todo`.
    ///
    /// ## What it clears, and why
    ///
    /// The cube lease columns and `workspace_path` are cleared because the
    /// caller ([`crate::spawn_ack_sweep::reap_never_started_spawn`])
    /// force-releases the lease on this path — leaving the columns populated
    /// would point the next dispatch at a workspace this execution no longer
    /// holds. `started_at` is cleared so the row reads as never-started,
    /// which it is: no shell, no driver, no workspace mutation.
    ///
    /// Any still-open `work_runs` row is stamped `abandoned` (not
    /// `orphaned`) with `reason` as its `result_summary`, so run history
    /// records the attempt without implying the engine lost a live worker.
    ///
    /// Errors when the execution is unknown or already terminal — a row that
    /// reached a real decision must not be silently resurrected.
    pub fn requeue_execution_after_environmental_failure(
        &self,
        execution_id: &str,
        reason: &str,
    ) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if existing.status.is_terminal() {
            bail!(
                "execution {execution_id} is already in terminal status `{}` and cannot be requeued",
                existing.status
            );
        }
        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'ready',
                 started_at = NULL,
                 finished_at = NULL,
                 cube_lease_id = NULL,
                 cube_workspace_id = NULL,
                 workspace_path = NULL
             WHERE id = ?1",
            params![execution_id],
        )?;
        tx.execute(
            "UPDATE work_runs
             SET status = 'abandoned',
                 result_summary = COALESCE(result_summary, ?3),
                 finished_at = COALESCE(finished_at, ?2)
             WHERE execution_id = ?1
               AND finished_at IS NULL",
            params![execution_id, now.as_str(), reason],
        )?;
        let updated = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution after environmental requeue: {execution_id}"))?;
        // Deliberately greppable and deliberately NOT the "execution
        // terminalized" line the orphan path emits — the whole point is that
        // this row did not go terminal. A future incident asking "did we burn
        // work when the display went away?" answers itself from this line.
        tracing::warn!(
            execution_id = %execution_id,
            work_item_id = %updated.work_item_id,
            from_status = %existing.status,
            to_status = %updated.status,
            reason = %reason,
            "execution requeued: environmental spawn failure (not counted against work-item churn)",
        );
        tx.commit()?;
        Ok(updated)
    }
}
