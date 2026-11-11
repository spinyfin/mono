//! Durable pre-start failures and repository-state holds.
use super::*;

/// One item held by a positioning refusal; see [`WorkDb::list_positioning_holds`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositioningHold {
    pub work_item_id: String,
    pub pr_url: Option<String>,
    pub restarts: bool,
}

/// How a pre-start failure is handled: retried with backoff, or a definitive
/// cube refusal that stops after one attempt and holds the item.
enum RetryPolicy<'a> {
    Retry(&'a [Duration]),
    Refused { message: &'a str },
}

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
        self.record_start_failure_with_policy(
            execution_id,
            agent_id,
            cube_repo_id,
            error_text,
            RetryPolicy::Retry(retry_delays),
        )
    }

    /// Atomically stop the execution and hold its item before publishing terminal events.
    ///
    /// `error_text` is the full failure (kept on the execution and in dispatch
    /// events); `refusal_message` is cube's own readable message, used for the
    /// blocked label and the attention body.
    ///
    /// Only `todo`/`active` rows are rewritten to `blocked`. A review-phase item
    /// (`in_review`, or blocked for a merge conflict or CI failure) keeps its status
    /// and blocked reason so the merge poller and review flow still see it; the
    /// open attention item alone holds it.
    pub fn record_workspace_positioning_refusal(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
        refusal_message: &str,
    ) -> Result<(WorkExecution, Option<WorkRun>, PreStartFailureOutcome)> {
        self.record_start_failure_with_policy(
            execution_id,
            agent_id,
            cube_repo_id,
            error_text,
            RetryPolicy::Refused {
                message: refusal_message,
            },
        )
    }

    /// Items currently held by a positioning refusal, with the PR their refused
    /// execution targeted (the recheck probe's subject). `restarts` is true when the
    /// task row itself was moved to `blocked` by the hold, so lowering it must
    /// also start the item again; review-phase rows keep their status and only
    /// need the hold lowered.
    pub fn list_positioning_holds(&self) -> Result<Vec<PositioningHold>> {
        let conn = self.connect()?;
        let refused = WORKSPACE_POSITIONING_REFUSED;
        let sql = format!(
            "{walk}
             SELECT t.id,
                    (t.status = 'blocked' AND t.blocked_reason = '{refused}'),
                    COALESCE(
                        NULLIF(t.pr_url, ''),
                        (SELECT x.pr_url FROM work_executions x
                          WHERE x.work_item_id = t.id AND x.pr_url IS NOT NULL AND x.pr_url <> ''
                          ORDER BY x.created_at DESC, x.id DESC LIMIT 1),
                        NULLIF(root.pr_url, '')
                    )
             FROM roots r
             JOIN tasks t ON t.id = r.task_id
             JOIN tasks root ON root.id = r.cycle_root_id
             ORDER BY t.id",
            walk = cycle_root_walk_cte(&format!(
                "t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
                 AND (t.blocked_reason = '{refused}'
                      OR EXISTS (SELECT 1 FROM work_attention_items a
                                 WHERE a.work_item_id = t.id AND a.kind = '{refused}' AND a.status = 'open'))"
            )),
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(PositioningHold {
                work_item_id: row.get(0)?,
                restarts: row.get(1)?,
                pr_url: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Lower the positioning hold on `work_item_id` (attention and, for a row the
    /// hold blocked, the blocked status). Returns whether a hold was present.
    pub fn clear_positioning_hold(&self, work_item_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let held = crate::dispatch_helpers::positioning_hold_active(&tx, work_item_id)?;
        if held {
            crate::dispatch_helpers::clear_positioning_hold_in_tx(&tx, work_item_id, "engine")?;
        }
        tx.commit()?;
        Ok(held)
    }

    fn record_start_failure_with_policy(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
        policy: RetryPolicy<'_>,
    ) -> Result<(WorkExecution, Option<WorkRun>, PreStartFailureOutcome)> {
        let (refusal_message, retry_delays): (Option<&str>, &[Duration]) = match policy {
            RetryPolicy::Retry(delays) => (None, delays),
            RetryPolicy::Refused { message } => (Some(message), &[]),
        };
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

        if let Some(message) = refusal_message {
            crate::task_status_audit::execute(
                &tx,
                WORKSPACE_POSITIONING_REFUSED,
                "engine",
                &format!(
                    "UPDATE tasks SET status = 'blocked', blocked_reason = '{WORKSPACE_POSITIONING_REFUSED}',
                     blocked_detail = ?2, last_status_actor = 'engine', updated_at = ?3
                     WHERE id = ?1 AND deleted_at IS NULL AND status IN ('todo', 'active')"
                ),
                params![execution.work_item_id, format!("Workspace blocked: {message}"), now],
            )?;
            let probe_pr = resolved_positioning_pr_url(&tx, &execution)?;
            if execution
                .pr_url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .is_none()
                && let Some(url) = probe_pr.as_deref()
            {
                tx.execute(
                    "UPDATE work_executions SET pr_url = ?2 WHERE id = ?1 AND (pr_url IS NULL OR pr_url = '')",
                    params![execution_id, url],
                )?;
            }
            if execution.work_item_id.starts_with("task_") {
                let restart = if probe_pr.is_some() {
                    "The item restarts on its own once the reported state is resolved, or launch it manually."
                } else {
                    "No pull request is bound, so this hold cannot clear automatically; launch the item manually once the reported state is resolved."
                };
                crate::workitems::upsert_refreshing_work_item_attention_in_tx(
                    &tx,
                    &execution.work_item_id,
                    WORKSPACE_POSITIONING_REFUSED,
                    "Workspace blocked: positioning refused",
                    &format!(
                        "Workspace blocked: {message}\n\nExecution `{execution_id}` could not position its workspace.\n\n{restart} No repository content was discarded."
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

/// PR the refused goto targeted, matching dispatch's workspace-goto lookup:
/// the execution's own URL, then the task row, then the revision chain root.
fn resolved_positioning_pr_url(conn: &Connection, execution: &WorkExecution) -> Result<Option<String>> {
    let nonempty = |url: Option<String>| url.filter(|value| !value.trim().is_empty());
    if let Some(url) = nonempty(execution.pr_url.clone()) {
        return Ok(Some(url));
    }
    let Some(task) = query_task(conn, &execution.work_item_id)? else {
        return Ok(None);
    };
    if let Some(url) = nonempty(task.pr_url) {
        return Ok(Some(url));
    }
    Ok(nonempty(
        get_chain_root_task(conn, &execution.work_item_id)?.and_then(|root| root.pr_url),
    ))
}
