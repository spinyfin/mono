//! Durable pre-start failures and repository-state holds.
use super::*;

impl WorkDb {
    /// Record a pre-start failure for `execution_id`, either resetting the
    /// execution to `ready` with a backoff delay (retry) or marking it
    /// permanently `failed` (and inserting a single failed `work_run`).
    ///
    /// **Why intermediate retries do not insert `work_runs`.** Each
    /// intermediate retry used to insert a `status='failed'` bookkeeping
    /// row that never hosted a worker and therefore never received a
    /// transcript path. With up to three retries that alone produced a
    /// large fraction of the fleet-wide `work_runs.transcript_path IS NULL`
    /// rows (and would have left the same hole in the per-run cost
    /// columns that share the hook-driven persist seam). Retry state is
    /// already durable on the execution (`pre_start_failure_count` /
    /// `dispatch_not_before`); the permanent-fail path still inserts one
    /// failed run so operators have a terminal history row.
    ///
    /// `retry_delays` controls how many retries are allowed and the delay
    /// between each. An empty slice means "no retries; fail immediately."
    /// The Nth element is the backoff before the (N+1)th attempt.
    ///
    /// This is the safe-to-retry alternative to `fail_execution_start`:
    /// call it for failures at `cube_repo_ensure`, `workspace_lease`,
    /// `change_create`, and `run_start` (before the worker has any
    /// side effects). Do NOT call it for failures at or after
    /// `run_started` — those require `finish_execution_run`.
    ///
    /// Returns `(execution, run, outcome)` where `run` is `Some` only on
    /// [`PreStartFailureOutcome::PermanentFail`] (the terminal failed
    /// bookkeeping row). On [`PreStartFailureOutcome::Retry`] it is
    /// `None` — no `work_runs` row was written.
    ///
    /// Lease columns are left as they stand. Dispatch persists the lease on
    /// acquire and releases it through `boss_engine::workspace_lease_release`;
    /// clearing here would hide a cube-release failure from the leftover
    /// sweep.
    pub fn record_pre_start_failure(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
        retry_delays: &[Duration],
    ) -> Result<(WorkExecution, Option<WorkRun>, PreStartFailureOutcome)> {
        self.record_start_failure_with_policy(execution_id, agent_id, cube_repo_id, error_text, Some(retry_delays))
    }

    /// Atomically stop the execution and hold its item before publishing terminal events.
    pub fn record_workspace_positioning_refusal(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
    ) -> Result<(WorkExecution, Option<WorkRun>, PreStartFailureOutcome)> {
        self.record_start_failure_with_policy(execution_id, agent_id, cube_repo_id, error_text, None)
    }

    fn record_start_failure_with_policy(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
        retry_delays: Option<&[Duration]>,
    ) -> Result<(WorkExecution, Option<WorkRun>, PreStartFailureOutcome)> {
        let positioning_refused = retry_delays.is_none();
        let retry_delays = retry_delays.unwrap_or_default();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if !execution.status.can_begin_run() {
            bail!(
                "execution {execution_id} is not ready and cannot record pre-start failure \
                 from status `{}`",
                execution.status
            );
        }

        let now = now_string();
        let new_count = execution.pre_start_failure_count + 1;
        let max_retries = retry_delays.len() as i64;
        let from_status = execution.status.clone();
        // Lease columns stay. The pre-start path persists the lease on
        // acquire and releases through `workspace_lease_release`; clearing
        // here would hide a cube-release failure from the leftover-lease sweep.

        let (outcome, run_id) = if new_count <= max_retries {
            let delay = retry_delays[(new_count - 1) as usize];
            let dispatch_not_before =
                (boss_engine_utils::epoch_time::now_epoch_secs() as u64 + delay.as_secs()).to_string();
            tx.execute(
                "UPDATE work_executions
                 SET status = 'ready',
                     pre_start_failure_count = ?2,
                     cube_repo_id = COALESCE(?3, cube_repo_id),
                     started_at = NULL,
                     finished_at = NULL,
                     dispatch_not_before = ?4,
                     last_error = ?5
                 WHERE id = ?1
                   AND status IN ('ready', 'claimed')",
                params![execution_id, new_count, cube_repo_id, dispatch_not_before, error_text],
            )?;
            (PreStartFailureOutcome::Retry { delay }, None)
        } else {
            // Terminal only: one failed bookkeeping run for operator history.
            let run_id = next_id("run");
            tx.execute(
                "INSERT INTO work_runs (
                    id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                    artifacts_path, created_at, started_at, finished_at
                 ) VALUES (?1, ?2, ?3, 'failed', ?4, NULL, NULL, NULL, ?5, ?5, ?5)",
                params![run_id, execution_id, agent_id, error_text, now],
            )?;
            tx.execute(
                "UPDATE work_executions
                 SET status = 'failed',
                     pre_start_failure_count = ?2,
                     cube_repo_id = COALESCE(?3, cube_repo_id),
                     finished_at = ?4,
                     last_error = ?5
                 WHERE id = ?1
                   AND status IN ('ready', 'claimed')",
                params![execution_id, new_count, cube_repo_id, now, error_text],
            )?;
            (PreStartFailureOutcome::PermanentFail, Some(run_id))
        };

        if positioning_refused {
            crate::task_status_audit::execute(
                &tx,
                "workspace_positioning_refused",
                "engine",
                "UPDATE tasks SET status = 'blocked', blocked_reason = 'workspace_positioning_refused',
                 blocked_detail = ?2, last_status_actor = 'engine', updated_at = ?3
                 WHERE id = ?1 AND deleted_at IS NULL AND status NOT IN ('done', 'archived')",
                params![execution.work_item_id, format!("Workspace blocked: {error_text}"), now],
            )?;
            if execution.work_item_id.starts_with("task_") {
                crate::workitems::upsert_refreshing_work_item_attention_in_tx(
                    &tx,
                    &execution.work_item_id,
                    "workspace_positioning_refused",
                    "Workspace blocked: positioning refused",
                    &format!(
                        "Execution `{execution_id}` could not position its workspace.\n\n{error_text}\n\nResolve the reported repository state, then launch the item manually. No repository content was discarded."
                    ),
                )?;
            }
        }
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let run = match run_id.as_deref() {
            Some(id) => Some(query_run(&tx, id)?.with_context(|| format!("missing run after insert: {id}"))?),
            None => None,
        };
        let mut pending = PendingEvents::new();
        if matches!(outcome, PreStartFailureOutcome::PermanentFail) {
            // Canonical terminalization trace — see `mark_execution_orphaned`.
            tracing::warn!(
                execution_id = %execution_id,
                work_item_id = %execution.work_item_id,
                from_status = %from_status,
                to_status = %execution.status,
                reason = %error_text,
                "execution terminalized: pre-start failure exhausted retries",
            );
            stage_execution_terminal(&mut pending, &tx, execution_id, &execution.work_item_id)?;
        }
        commit_and_publish(tx, pending, &self.event_bus)?;
        drop(conn);
        if matches!(outcome, PreStartFailureOutcome::PermanentFail)
            && execution.kind == ExecutionKind::PrReviewGuide
            && let Err(err) = self.finish_pr_review_guide_attempt_for_terminal_execution(
                execution_id,
                ExecutionStatus::Failed,
                error_text,
                true,
            )
        {
            tracing::warn!(
                execution_id = %execution_id,
                ?err,
                "review-guide: recorded a permanent pre-start failure but failed to fail the bound attempt",
            );
        }
        Ok((execution, run, outcome))
    }
}
