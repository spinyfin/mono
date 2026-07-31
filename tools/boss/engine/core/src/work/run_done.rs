//! `work_executions` accessors for the run-done declaration and its
//! backstop — split out of [`super::executions_runs`], which is at its
//! configured file-size ceiling.
//!
//! Three columns, one question: did this run's worker say it was finished,
//! or did the engine stop waiting?
//!
//! `run_done_declared_at` / `run_done_outcome` are stamped by the
//! `run_done` proposal applier
//! ([`crate::work::proposal_apply`]) inside the submission transaction, so
//! the declaration and the fact the completion path gates on are one
//! commit. `run_undeclared_at` is stamped by
//! [`crate::run_done_backstop`] when it gives up waiting. A completed run
//! carries one set or the other, never both and never neither — which is
//! what lets an operator (or an audit) tell a declared ending from a
//! backstopped one without inferring it from the absence of a proposal row.
//!
//! Deliberately NOT surfaced on [`boss_protocol::WorkExecution`]: these are
//! engine-internal completion bookkeeping with exactly two readers apiece,
//! following the `metadata_fix_confirmed_at` precedent in
//! [`super::executions_runs`] rather than widening the wire type every
//! client deserializes.

use super::*;

impl WorkDb {
    /// Read this execution's terminal `run_done` declaration, if the worker
    /// has made one: `Some(outcome)` once a `run_done` proposal has applied,
    /// `None` while the run has not declared.
    ///
    /// This is the completion path's gate input. It reads the stamped
    /// column rather than re-scanning `worker_proposals` because the stamp
    /// is what the applier writes in the submission transaction — so
    /// "declared" and "the effect exists" are the same commit, exactly as
    /// every other auto-apply kind works.
    ///
    /// An unparseable stored value is `None` with a WARN, not an error: a
    /// gate that fails closed here would hang the run, and a gate that
    /// errored would take the whole Stop path down over one bad string.
    pub fn execution_run_done_outcome(&self, execution_id: &str) -> Result<Option<boss_protocol::RunDoneOutcome>> {
        let conn = self.connect()?;
        let stored: Option<String> = conn
            .query_row(
                "SELECT run_done_outcome FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(stored) = stored.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        match stored.parse::<boss_protocol::RunDoneOutcome>() {
            Ok(outcome) => Ok(Some(outcome)),
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    stored_outcome = %stored,
                    %err,
                    "run_done: stored declaration outcome did not parse; treating the run as undeclared",
                );
                Ok(None)
            }
        }
    }

    /// Stamp the backstop's "this run ended without ever declaring" marker.
    ///
    /// The counterpart to `run_done_declared_at`: between them, a completed
    /// run's stored state says which of the two endings it got, so an
    /// operator (or a later audit) never has to infer it from the absence
    /// of a proposal row. Idempotent — the first stamp wins, so a run that
    /// reaches the backstop on several Stops records when it was *first*
    /// judged undeclared rather than continually resetting to now.
    pub fn mark_execution_run_undeclared(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let affected = conn.execute(
            "UPDATE work_executions SET run_undeclared_at = ?2 \
             WHERE id = ?1 AND (run_undeclared_at IS NULL OR run_undeclared_at = '')",
            params![execution_id, now_string()],
        )?;
        if affected == 0 {
            // Either already stamped (the common case on a later Stop) or
            // the execution is gone. Distinguish, so a genuinely unknown id
            // is still an error the caller can see.
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_executions WHERE id = ?1)",
                params![execution_id],
                |row| row.get(0),
            )?;
            if !exists {
                bail!("unknown execution: {execution_id}");
            }
        }
        Ok(())
    }

    /// Whether the backstop has stamped this execution as having ended
    /// without a declaration (see [`Self::mark_execution_run_undeclared`]).
    pub fn execution_run_undeclared(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let at: Option<String> = conn
            .query_row(
                "SELECT run_undeclared_at FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(at.is_some_and(|s| !s.is_empty()))
    }
}
