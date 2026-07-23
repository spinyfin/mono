use super::*;

impl WorkDb {
    /// Record that a worker produced a PR for `execution_id`. In a single
    /// transaction:
    ///   - the linked task/chore moves to the column dictated by
    ///     `target` (`in_review` for an open PR, `done` for a PR that
    ///     was already merged at Stop time) and gets `pr_url`
    ///     populated. If the task is already past the target column
    ///     (`done`, `archived`), its status is left alone — the
    ///     `pr_url` update still applies.
    ///   - the execution transitions from `waiting_human` (or `running`)
    ///     to `completed`, the cube workspace lease columns are
    ///     cleared, `finished_at` is stamped,
    ///   - the run summary is updated if a fresh summary is provided
    ///     and the run hasn't already captured one.
    ///
    /// Returns `Ok(None)` if the execution has already been finalised
    /// (terminal status), making this safe to call from a hook handler
    /// that may fire repeatedly.
    pub fn record_worker_pr_completion(
        &self,
        execution_id: &str,
        pr_url: &str,
        result_summary: Option<&str>,
        target: WorkerPrCompletionTarget,
    ) -> Result<Option<WorkerPrCompletion>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status.is_terminal() {
            return Ok(None);
        }
        if !execution.status.is_live() {
            bail!(
                "execution {execution_id} cannot complete from worker PR signal in status `{}`",
                execution.status
            );
        }

        let original_lease_id = execution.cube_lease_id.clone();
        let original_workspace_id = execution.cube_workspace_id.clone();

        let work_item_id = execution.work_item_id.clone();
        let task =
            query_task(&tx, &work_item_id)?.with_context(|| format!("unknown task for execution: {work_item_id}"))?;
        if task.deleted_at.is_some() {
            bail!("cannot complete a deleted task: {work_item_id}");
        }

        let now = now_string();
        // Compute the new status. The chore can only advance — if it
        // is already past the target column (`done` / `archived`), we
        // keep the existing status. `PendingReview` holds the task in
        // its current status so the reviewer pass runs before human Review.
        let new_status = match target {
            _ if task.status == TaskStatus::Done || task.status == TaskStatus::Archived => task.status.clone(),
            WorkerPrCompletionTarget::InReview if task.status == TaskStatus::InReview => task.status.clone(),
            WorkerPrCompletionTarget::InReview => TaskStatus::InReview,
            WorkerPrCompletionTarget::Done => TaskStatus::Done,
            // P992: hold in current status while the reviewer runs.
            WorkerPrCompletionTarget::PendingReview => task.status.clone(),
            // incident-002 P2: halt in `blocked` pending operator sign-off.
            WorkerPrCompletionTarget::BlockedDeletionSignoff => TaskStatus::Blocked,
        };
        // Revision tasks do not own a PR — their `pr_url` must stay NULL
        // (the chain root's `pr_url` is the source of truth), *except* for
        // `PendingReview` / `BlockedDeletionSignoff` where we must stamp it so
        // the reviewer / signing-off operator can find it.
        let pr_url_for_task: Option<&str> = match target {
            WorkerPrCompletionTarget::PendingReview | WorkerPrCompletionTarget::BlockedDeletionSignoff => Some(pr_url),
            _ if task.kind == TaskKind::Revision => task.pr_url.as_deref(),
            _ => Some(pr_url),
        };
        // Deletion-signoff halt stamps `blocked_reason` instead of clearing it;
        // no attempt id (there is no auto-clearing signal — a human moves the
        // task out of `blocked`). Every other target clears the blocked columns.
        let blocked_reason_for_task: Option<&str> = match target {
            WorkerPrCompletionTarget::BlockedDeletionSignoff => Some("deletion_signoff"),
            _ => None,
        };
        tx.execute(
            "UPDATE tasks
             SET status             = ?2,
                 pr_url             = ?3,
                 updated_at         = ?4,
                 last_status_actor  = 'engine',
                 blocked_reason     = ?5,
                 blocked_attempt_id = NULL,
                 completed_at       = COALESCE(completed_at, CASE WHEN ?2 IN ('done','archived','cancelled') THEN ?4 END)
             WHERE id = ?1",
            params![task.id, new_status.as_str(), pr_url_for_task, now, blocked_reason_for_task],
        )?;

        if new_status != task.status {
            cascade_dependents_after_prereq_status_change(&tx, &task.id, new_status.as_str(), &now)?;
        }

        // Comment-intent-classification design §"Reconciliation": a
        // revision's claimed comments are addressed the moment its commit
        // is verified on the chain root's PR branch (this is that
        // detection point), not when the chain root's PR eventually merges
        // — the reviewer's re-read of the updated doc happens here, not at
        // merge time. Guarded on `status = 'in_revision'` inside
        // `reconcile_comments_for_task`, so this is a no-op on a re-fire
        // (e.g. `target` already `InReview` and this call just re-affirms
        // it). `flip_in_review_revisions_to_done`'s merge-time reconcile
        // becomes a no-op backstop for rows already resolved here.
        if new_status == TaskStatus::InReview && task.kind == TaskKind::Revision {
            comments::reconcile_comments_for_task(&tx, &task.id, comments::CommentReconcileOutcome::Resolved, &now)?;
        }

        tx.execute(
            "UPDATE work_executions
             SET status = 'completed',
                 cube_lease_id = NULL,
                 cube_workspace_id = NULL,
                 workspace_path = NULL,
                 finished_at = ?2,
                 pr_url = ?3
             WHERE id = ?1",
            params![execution_id, now, pr_url],
        )?;

        // Update the most-recent run for this execution: if a summary is
        // provided and the run's existing summary is empty, capture it.
        // The run is typically already `completed` because the
        // PaneSpawnRunner records completion immediately on spawn.
        if let Some(summary) = result_summary {
            let trimmed = summary.trim();
            if !trimmed.is_empty() {
                tx.execute(
                    "UPDATE work_runs
                     SET result_summary = COALESCE(NULLIF(result_summary, ''), ?2)
                     WHERE execution_id = ?1
                       AND id = (
                           SELECT id FROM work_runs
                           WHERE execution_id = ?1
                           ORDER BY created_at DESC, id DESC
                           LIMIT 1
                       )",
                    params![execution_id, trimmed],
                )?;
            }
        }

        let updated_execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let updated_task = query_task(&tx, &work_item_id).require("task", &work_item_id)?;
        tx.commit()?;
        Ok(Some(WorkerPrCompletion {
            execution: updated_execution,
            work_item: task_to_item(updated_task),
            released_lease_id: original_lease_id,
            released_workspace_id: original_workspace_id,
        }))
    }

    /// Record that a primary-implementation worker (`chore_implementation`
    /// / `task_implementation`) verified its assigned work is **already
    /// done** — the change is already present on `main`, the working-copy
    /// diff is empty, and there is genuinely nothing to commit, push, or
    /// open a PR for. This is the sanctioned no-op terminal (see
    /// [`crate::no_op_signal`]). In a single transaction:
    ///   - the linked task/chore moves to `done` (with **no** `pr_url` —
    ///     there is no PR), unless it is already terminal (`done` /
    ///     `archived` / `cancelled`), in which case its status is left
    ///     alone;
    ///   - the execution transitions from `waiting_human` (or `running`)
    ///     to `completed`, the cube workspace lease columns are cleared,
    ///     and `finished_at` is stamped;
    ///   - the most-recent run captures `detail` as its result summary if
    ///     it does not already have one.
    ///
    /// Mirrors [`Self::record_worker_pr_completion`] — including the
    /// dependent-cascade on a real status change and the returned
    /// lease/workspace ids for out-of-band cube release — but stamps NO
    /// `pr_url`: fabricating one would be exactly the empty PR the worker
    /// correctly refused to push.
    ///
    /// Returns `Ok(None)` if the execution has already been finalised
    /// (terminal status), making this safe to call from a hook handler
    /// that may fire repeatedly.
    pub fn record_worker_no_op_completion(
        &self,
        execution_id: &str,
        detail: &str,
    ) -> Result<Option<WorkerPrCompletion>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status.is_terminal() {
            return Ok(None);
        }
        if !execution.status.is_live() {
            bail!(
                "execution {execution_id} cannot complete from worker no-op signal in status `{}`",
                execution.status
            );
        }

        let original_lease_id = execution.cube_lease_id.clone();
        let original_workspace_id = execution.cube_workspace_id.clone();

        let work_item_id = execution.work_item_id.clone();
        let task =
            query_task(&tx, &work_item_id)?.with_context(|| format!("unknown task for execution: {work_item_id}"))?;
        if task.deleted_at.is_some() {
            bail!("cannot complete a deleted task: {work_item_id}");
        }

        let now = now_string();
        // A no-op completion closes the task as done. If it is already
        // terminal (done / archived / cancelled), leave the status alone.
        // `pr_url` is left untouched — a no-op produced none, and the
        // worker correctly refused to fabricate one.
        let new_status = if task.status.is_terminal() {
            task.status.clone()
        } else {
            TaskStatus::Done
        };
        tx.execute(
            "UPDATE tasks
             SET status             = ?2,
                 updated_at         = ?3,
                 last_status_actor  = 'engine',
                 blocked_reason     = NULL,
                 blocked_attempt_id = NULL,
                 completed_at       = COALESCE(completed_at, CASE WHEN ?2 IN ('done','archived','cancelled') THEN ?3 END)
             WHERE id = ?1",
            params![task.id, new_status.as_str(), now],
        )?;

        if new_status != task.status {
            cascade_dependents_after_prereq_status_change(&tx, &task.id, new_status.as_str(), &now)?;
        }

        tx.execute(
            "UPDATE work_executions
             SET status = 'completed',
                 cube_lease_id = NULL,
                 cube_workspace_id = NULL,
                 workspace_path = NULL,
                 finished_at = ?2
             WHERE id = ?1",
            params![execution_id, now],
        )?;

        // Capture the no-op explanation as the run summary if the run
        // hasn't already recorded one.
        let trimmed = detail.trim();
        if !trimmed.is_empty() {
            tx.execute(
                "UPDATE work_runs
                 SET result_summary = COALESCE(NULLIF(result_summary, ''), ?2)
                 WHERE execution_id = ?1
                   AND id = (
                       SELECT id FROM work_runs
                       WHERE execution_id = ?1
                       ORDER BY created_at DESC, id DESC
                       LIMIT 1
                   )",
                params![execution_id, trimmed],
            )?;
        }

        let updated_execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let updated_task = query_task(&tx, &work_item_id).require("task", &work_item_id)?;
        tx.commit()?;
        Ok(Some(WorkerPrCompletion {
            execution: updated_execution,
            work_item: task_to_item(updated_task),
            released_lease_id: original_lease_id,
            released_workspace_id: original_workspace_id,
        }))
    }

    /// Record that the engine gave up nudging a live worker: the auto-nudge
    /// circuit breaker tripped ([`crate::completion::WorkerCompletionHandler::park_for_unproductive_nudges`])
    /// because repeated Stops produced no state change — no new commit, no
    /// PR, no bound-PR anomaly resolved. Unlike
    /// [`Self::record_worker_no_op_completion`], there is no positive
    /// evidence the assigned work is done — only that further automated
    /// nudging is unproductive — so, unlike that sibling, this does **not**
    /// touch the task/chore's `status` or `pr_url` at all. Requiring the
    /// [`crate::no_op_signal`] marker for a `done` close (and never
    /// fabricating one here) is what distinguishes "verified already done"
    /// from "gave up without trying"; conflating them would be exactly the
    /// dishonest auto-close this module's own no-op gate was built to avoid.
    ///
    /// What this closes: without it, a live worker the breaker parks holds
    /// its cube lease and worker pane/slot forever — the operator has to
    /// notice and reap it by hand (incident `exec_18b932df99d17658_475`: a
    /// worker concluded a CI failure had already resolved itself, never
    /// produced a PR, and sat parked indefinitely holding its slot). In one
    /// transaction:
    ///   - the execution moves to `abandoned`, cube lease/workspace columns
    ///     cleared, `finished_at` stamped — freeing the slot/lease is the
    ///     whole point;
    ///   - the most-recent run captures `detail` (the breaker's park reason)
    ///     as its result summary if it does not already have one;
    ///   - the task/chore's `autostart` flag is cleared (mirroring
    ///     [`Self::bounce_dispatch_failed_to_backlog`]'s single-shot
    ///     convention) so [`Self::rescan_active_dispatch`] does not
    ///     immediately re-dispatch a fresh worker onto the same task the
    ///     moment this one's slot frees up — without this, a task whose
    ///     worker keeps concluding "nothing to do" without emitting the
    ///     no-op marker would loop abandon → rescan-redispatch → abandon
    ///     forever, churning a cube lease and worker slot with no human in
    ///     the loop. `status`/`pr_url` are otherwise left untouched, so the
    ///     merge poller's late-PR sweep and the dispatcher's redundant-spawn
    ///     guard continue to see it exactly as they did before — a human
    ///     reviewing the attention item this call's caller files can
    ///     explicitly re-arm `autostart` (or dispatch a fresh execution
    ///     directly) once they've decided the task is worth another try.
    ///
    /// Tolerates the task/chore row having been hard-deleted while the
    /// execution was live: the lease/pane are the resource this method
    /// exists to free, and that must happen unconditionally rather than
    /// bailing out because a best-effort metadata lookup came up empty —
    /// `work_item` is `None` in the returned completion in that case.
    ///
    /// Returns `Ok(None)` if the execution has already been finalised
    /// (terminal status), making this safe to call from a hook handler that
    /// may fire repeatedly.
    pub fn record_worker_idle_abandonment(
        &self,
        execution_id: &str,
        detail: &str,
    ) -> Result<Option<IdleAbandonmentCompletion>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status.is_terminal() {
            return Ok(None);
        }
        if !execution.status.is_live() {
            bail!(
                "execution {execution_id} cannot be idle-abandoned from status `{}`",
                execution.status
            );
        }

        let original_lease_id = execution.cube_lease_id.clone();
        let original_workspace_id = execution.cube_workspace_id.clone();

        let work_item_id = execution.work_item_id.clone();
        let task = query_task(&tx, &work_item_id)?;

        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'abandoned',
                 cube_lease_id = NULL,
                 cube_workspace_id = NULL,
                 workspace_path = NULL,
                 finished_at = ?2
             WHERE id = ?1",
            params![execution_id, now],
        )?;

        // Stop the automated re-dispatch loop: clear `autostart` so the
        // on-free rescan (`rescan_active_dispatch`) leaves this task parked
        // in `active` instead of immediately spawning a replacement worker
        // that may just abandon again the same way. Best-effort — if the
        // task row is gone there's nothing to clear.
        tx.execute(
            "UPDATE tasks SET autostart = 0 WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id],
        )?;

        // Capture the park reason as the run summary if the run hasn't
        // already recorded one — the durable "why was this abandoned" note
        // an operator finds on the row.
        let trimmed = detail.trim();
        if !trimmed.is_empty() {
            tx.execute(
                "UPDATE work_runs
                 SET result_summary = COALESCE(NULLIF(result_summary, ''), ?2)
                 WHERE execution_id = ?1
                   AND id = (
                       SELECT id FROM work_runs
                       WHERE execution_id = ?1
                       ORDER BY created_at DESC, id DESC
                       LIMIT 1
                   )",
                params![execution_id, trimmed],
            )?;
        }

        let updated_execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        tx.commit()?;
        Ok(Some(IdleAbandonmentCompletion {
            execution: updated_execution,
            work_item: task.map(task_to_item),
            released_lease_id: original_lease_id,
            released_workspace_id: original_workspace_id,
        }))
    }

    /// Chores and project_tasks currently in `in_review` whose
    /// `pr_url` is set. The merge poller iterates this list, asks
    /// GitHub whether each PR is merged, and calls
    /// [`Self::mark_chore_pr_merged`] for the ones that are. Both
    /// kinds share the `pr_url` / `status='in_review'` shape, so the
    /// poller treats them identically; `kind = 'task'` is excluded
    /// deliberately because non-project tasks don't share the
    /// PR-on-merge lifecycle yet.
    pub fn list_chores_pending_merge_check(&self) -> Result<Vec<PendingMergeCheck>> {
        self.query_pending_merge_checks("t.status = 'in_review'")
    }

    /// Executions whose bound chore is still `active` with no `pr_url`,
    /// whose execution row is `waiting_human` (i.e., the worker spawned,
    /// hit a Stop boundary, and is now idle), and that have a recorded
    /// `workspace_path` for PR detection.
    ///
    /// This is the fallback set for the merge poller's PR-open recheck:
    /// the on-Stop hook is the primary detection path but it can miss
    /// (transient `gh api` failure, GitHub's
    /// `commits/{sha}/pulls` index lagging a fresh `gh pr create`, or
    /// a Stop event that never reached the engine). Without this list
    /// the chore is stuck in `active` forever because the merge poller's
    /// other query (`list_chores_pending_merge_check`) only sees rows
    /// already in `in_review`.
    ///
    /// `CHORE_LIKE_KINDS_SQL` matches the same kinds the in-review poller
    /// watches; `task` is excluded for the same reason (non-project tasks
    /// don't share the PR lifecycle). `revision` is also included because
    /// its on-Stop hook stamps the parent pr_url, not its own.
    pub fn list_executions_pending_pr_detection(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT we.id
             FROM work_executions we
             JOIN tasks t ON t.id = we.work_item_id
             WHERE we.status = 'waiting_human'
               AND we.workspace_path IS NOT NULL
               AND we.workspace_path != ''
               AND t.deleted_at IS NULL
               AND t.kind IN ({CHORE_LIKE_KINDS_SQL}, 'revision')
               AND t.status = 'active'
               AND (t.pr_url IS NULL OR t.pr_url = '')
             ORDER BY we.created_at ASC",
        ))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        collect_rows(rows)
    }

    /// Return recently-terminal executions whose task is still `active`
    /// with no `pr_url`. These are candidates for the merge poller's
    /// late-PR-detection sweep (Bug B): when a double-spawn race causes
    /// exec_A to be abandoned before the real worker pushes its PR, the
    /// on-Stop hook returns `AlreadyTerminal` and the normal
    /// `list_executions_pending_pr_detection` query (which only watches
    /// `waiting_human`) never picks the chore back up. This query fills
    /// that gap by watching terminal executions that finished within the
    /// last `lookback_secs` seconds.
    ///
    /// Only executions with `workspace_path` set are returned — the
    /// absence of a workspace_path means the execution never reached the
    /// pane-spawn stage and therefore never pushed a branch the detector
    /// could find. Status `'cancelled'` and `'orphaned'` are excluded
    /// because those arise from human or engine actions that pre-date
    /// the pane-spawn lifecycle this sweep covers.
    pub fn list_recently_terminal_executions_pending_pr_detection(
        &self,
        lookback_secs: u64,
    ) -> Result<Vec<LatePrCandidate>> {
        let conn = self.connect()?;
        let cutoff = (boss_engine_utils::epoch_time::now_epoch_secs() as u64)
            .saturating_sub(lookback_secs)
            .to_string();
        let mut stmt = conn.prepare(&format!(
            "SELECT we.id, we.work_item_id, we.repo_remote_url, we.branch_naming, we.worker_branch_prefix
             FROM work_executions we
             JOIN tasks t ON t.id = we.work_item_id
             WHERE we.status IN ('abandoned', 'completed', 'failed')
               AND we.workspace_path IS NOT NULL
               AND we.workspace_path != ''
               AND we.finished_at IS NOT NULL
               AND CAST(we.finished_at AS INTEGER) >= ?1
               AND t.deleted_at IS NULL
               AND t.kind IN ({CHORE_LIKE_KINDS_SQL})
               AND t.status = 'active'
               AND (t.pr_url IS NULL OR t.pr_url = '')
             ORDER BY we.finished_at DESC, we.id DESC",
        ))?;
        let rows = stmt.query_map([cutoff], |row| {
            let branch_naming: BranchNaming = deserialize_json_or_default(row.get::<_, Option<String>>(3)?.as_deref());
            Ok(LatePrCandidate {
                execution_id: row.get(0)?,
                work_item_id: row.get(1)?,
                repo_remote_url: row.get(2)?,
                branch_naming,
                worker_branch_prefix: row.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
            })
        })?;
        collect_rows(rows)
    }

    /// Tasks that are held in `active` (Doing) pending an AI reviewer pass
    /// that has either finished (terminal `pr_review` execution) or timed out
    /// (non-terminal `pr_review` execution older than `stale_secs` seconds).
    ///
    /// These are the candidates for the merge poller's reviewer-fallback sweep:
    /// they should advance to `in_review` and release the hold, because either
    /// the reviewer already finished (its Stop hook never fired or failed to
    /// advance the task) or the reviewer is taking too long and we should
    /// unblock the human review lane rather than stranding the card.
    ///
    /// Returns `(task_id, product_id, pr_url)` triples.
    pub fn list_tasks_with_stalled_reviewer(&self, stale_secs: u64) -> Result<Vec<(String, String, String)>> {
        let conn = self.connect()?;
        let cutoff = (boss_engine_utils::epoch_time::now_epoch_secs() as u64)
            .saturating_sub(stale_secs)
            .to_string();
        // Tasks in `active` with a `pr_url` that have a `pr_review` execution
        // which is either:
        //   1. Terminal (reviewer finished — should have advanced the task via
        //      finalize_pr_review_pass but didn't, e.g. Stop hook was missed).
        //   2. Non-terminal but created before the stale cutoff (timeout).
        let mut stmt = conn.prepare(
            "SELECT DISTINCT t.id, t.product_id, t.pr_url
             FROM tasks t
             JOIN work_executions we ON we.work_item_id = t.id AND we.kind = 'pr_review'
             WHERE t.status = 'active'
               AND t.pr_url IS NOT NULL
               AND t.pr_url != ''
               AND t.deleted_at IS NULL
               AND (
                 -- Reviewer finished but task was not advanced (missed Stop hook)
                 we.status IN ('completed', 'abandoned', 'failed', 'cancelled', 'orphaned')
                 OR
                 -- Reviewer still running but has been running too long (timeout)
                 (we.status NOT IN ('completed', 'abandoned', 'failed', 'cancelled', 'orphaned')
                  AND we.created_at < ?1)
               )
             ORDER BY t.updated_at ASC",
        )?;
        let rows = stmt.query_map([cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        collect_rows(rows)
    }

    /// Advance a task from `active` to `in_review` as the reviewer-fallback
    /// (the reviewer finished or timed out without advancing it). Idempotent:
    /// no-ops if the task is already past `active`. Returns `true` if the task
    /// was updated.
    ///
    /// Single-live-worker guard (T1577 incident): the reviewer-fallback is
    /// only correct when the worker holding the task in `active` is actually
    /// the AI reviewer. A `pr_review` execution can be terminal/timed-out
    /// (which surfaces the task as a fallback candidate) while a DIFFERENT
    /// live execution — a `chore_implementation`/`task_implementation`/
    /// `ci_remediation` resume — is actively working the task. Advancing the
    /// lane then would strand the implementation worker in the Review column
    /// with no Doing card. So we refuse to advance while ANY live
    /// (`running`/`waiting_human`) non-`pr_review` execution exists on the
    /// task: the `NOT EXISTS` clause makes the update a no-op in that case.
    pub fn advance_pending_review_task_to_in_review(&self, work_item_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_string();
        let rows_changed = conn.execute(
            "UPDATE tasks
             SET status            = 'in_review',
                 updated_at        = ?2,
                 last_status_actor = 'engine'
             WHERE id = ?1
               AND status = 'active'
               AND pr_url IS NOT NULL
               AND pr_url != ''
               AND deleted_at IS NULL
               AND NOT EXISTS (
                 SELECT 1 FROM work_executions we
                 WHERE we.work_item_id = ?1
                   AND we.status IN ('running', 'waiting_human')
                   AND we.kind != 'pr_review'
               )",
            params![work_item_id, now],
        )?;
        Ok(rows_changed > 0)
    }

    /// Transition a task from `active` to `in_review` by binding a
    /// late-detected PR URL. Called by the merge poller's late-PR sweep
    /// when the PR was pushed after the original execution became
    /// terminal (double-spawn race). Unlike `record_worker_pr_completion`
    /// this function does not gate on execution status — the execution is
    /// already terminal; we only need to advance the task.
    ///
    /// Returns `Ok(true)` if the task was updated, `Ok(false)` if it was
    /// already past `active` (idempotent for concurrent sweeps).
    pub fn bind_pr_to_active_task_from_terminal_execution(&self, work_item_id: &str, pr_url: &str) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_string();
        let rows_changed = conn.execute(
            "UPDATE tasks
             SET status            = 'in_review',
                 pr_url            = ?2,
                 updated_at        = ?3,
                 last_status_actor = 'engine',
                 blocked_reason    = NULL,
                 blocked_attempt_id = NULL
             WHERE id = ?1
               AND deleted_at IS NULL
               AND status = 'active'
               AND (pr_url IS NULL OR pr_url = '')",
            params![work_item_id, pr_url, now],
        )?;
        Ok(rows_changed > 0)
    }

    /// Move the chore or project_task identified by `work_item_id`
    /// from `in_review` to `done`, recording `pr_url` (no-op if it
    /// was already set to the same value). Returns the updated task
    /// if a transition happened; `Ok(None)` if the row was already
    /// past `in_review` (idempotent for late-arriving merge events).
    /// Callers are expected to pre-filter on `kind` via
    /// [`Self::list_chores_pending_merge_check`]; this function
    /// itself does not gate on kind so that the SQL filter remains
    /// the single source of truth for what's mergeable.
    pub fn mark_chore_pr_merged(&self, work_item_id: &str, pr_url: &str) -> Result<Option<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let Some(task) = query_task(&tx, work_item_id)? else {
            return Ok(None);
        };
        if task.deleted_at.is_some() {
            return Ok(None);
        }
        if task.status == TaskStatus::Done || task.status == TaskStatus::Archived {
            // Already terminal — no status transition, but a task that
            // reached done/archived via a path that predates (or missed)
            // clearing merge_queue_state would otherwise sit forever in
            // `list_queued_merge_queue_members`'s membership set and
            // inflate every live card's renumbered position. Clear it here
            // too so a late-arriving merge-poller observation of an
            // already-terminal task self-heals the orphan.
            tx.execute(
                "UPDATE tasks
                 SET merge_queue_state  = NULL,
                     merge_queue_detail = NULL
                 WHERE id = ?1
                   AND merge_queue_state IS NOT NULL",
                params![task.id],
            )?;
            tx.commit()?;
            return Ok(None);
        }
        let now = now_string();
        // Clearing blocked_reason / blocked_attempt_id is load-bearing
        // for the case where the merge poller observes a force-merge
        // (branch-protection override) of a PR currently in
        // `blocked: merge_conflict`. The new state must be coherent —
        // `done` rows never carry a blocked reason.
        // Clearing merge_queue_state / merge_queue_detail here is load-bearing:
        // update_pr_poll_state (the merge poller's dequeue writer) is only
        // invoked for `Open` PRs, so a just-merged PR would otherwise keep
        // whatever queue state it carried at merge time forever. A stale
        // `merge_queue_state` on a `done` row makes the client's
        // `isInMergingSection` (see `Models.swift`) misroute the task into
        // the kanban's "Merging" section instead of the normal Done/recency
        // buckets.
        tx.execute(
            "UPDATE tasks
             SET status             = 'done',
                 pr_url             = ?2,
                 updated_at         = ?3,
                 last_status_actor  = 'engine',
                 blocked_reason     = NULL,
                 blocked_attempt_id = NULL,
                 merge_queue_state  = NULL,
                 merge_queue_detail = NULL,
                 completed_at       = COALESCE(completed_at, ?3)
             WHERE id = ?1",
            params![task.id, pr_url, now],
        )?;
        cascade_dependents_after_prereq_status_change(&tx, &task.id, "done", &now)?;
        // merge_order sequencing (direction 2): order the pair. Any in-flight
        // merge_order sibling of this just-merged task is now the "later" side
        // and owes a preserving forward-port when its base moves. This never
        // gates dispatch — it only records the ordering for the log; the
        // durable contract is the merge_order edge + this task's `done` status,
        // which the forward-port brief and the both-parents deletion tripwire
        // consume.
        record_merge_order_on_merge(&tx, &task.id)?;
        // OQ7: when a chain root reaches `done`, flip any `in_review`
        // revisions on it to `done` as well.  A revision's deliverable
        // (the commit) rode the parent PR to its terminal state.
        flip_in_review_revisions_to_done(&tx, &task.id, &now)?;
        // Invalidation: any revision still in a pre-dispatch state
        // (todo / active / waiting_dependency / blocked-for-another-reason)
        // can never push to the merged PR.  Block them now so the
        // scheduler stops dispatching them and the kanban shows why.
        block_pending_revisions_on_parent_close(&tx, &task.id, &now)?;
        // Comment-intent-classification design §"Reconciliation" (task
        // 2c): resolve any comments whose `[Revise]` batch was dispatched
        // directly to this task (the plain-chore vehicle of the
        // revision-vs-chore decision table — a revision's comments are
        // reconciled inside `flip_in_review_revisions_to_done` above,
        // since `revise_task_id` there points at the revision, not the
        // chain root).
        comments::reconcile_comments_for_task(&tx, &task.id, comments::CommentReconcileOutcome::Resolved, &now)?;
        let updated =
            query_task(&tx, work_item_id)?.with_context(|| format!("unknown task after update: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Move the chore or project_task identified by `work_item_id`
    /// from `in_review` to `done` because its bound PR was **closed
    /// without merging** — the on-close counterpart to
    /// [`Self::mark_chore_pr_merged`] (`chore-lifecycle-pr-closed-unmerged.md`).
    /// Returns the updated task if a transition happened; `Ok(None)` if the
    /// row was already past `in_review` (idempotent for late-arriving close
    /// events, and safe if a concurrent sweep already retired it).
    ///
    /// `done` is the only terminal status the current enum offers — there is
    /// no `abandoned`/`cancelled` state to distinguish "shipped" from
    /// "closed unmerged" at the status level (open question, out of scope
    /// here). Callers are expected to pre-filter on `kind` and `status =
    /// in_review` the same way [`Self::list_chores_pending_merge_check`]
    /// does for the merge path; this function itself only refuses rows
    /// already past `in_review`.
    ///
    /// Mirrors `mark_chore_pr_merged`'s terminal-transition side effects
    /// (dependent cascade, revision flip/block) since the row's resulting
    /// status is identical — but deliberately does **not** call
    /// `comments::reconcile_comments_for_task` with `Resolved`: the
    /// close-unmerged comment story is "reopen anything still `in_revision`"
    /// ([`Self::reopen_comments_for_closed_unmerged_pr`], called separately
    /// by the merge poller), which this function must not immediately
    /// undo by re-resolving those same comments.
    pub fn mark_chore_pr_closed_unmerged(&self, work_item_id: &str, pr_url: &str) -> Result<Option<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let Some(task) = query_task(&tx, work_item_id)? else {
            return Ok(None);
        };
        if task.deleted_at.is_some() {
            return Ok(None);
        }
        if task.status == TaskStatus::Done || task.status == TaskStatus::Archived {
            return Ok(None);
        }
        let now = now_string();
        tx.execute(
            "UPDATE tasks
             SET status             = 'done',
                 updated_at         = ?3,
                 last_status_actor  = 'engine',
                 blocked_reason     = NULL,
                 blocked_attempt_id = NULL,
                 completed_at       = COALESCE(completed_at, ?3)
             WHERE id = ?1
               AND pr_url = ?2",
            params![task.id, pr_url, now],
        )?;
        cascade_dependents_after_prereq_status_change(&tx, &task.id, "done", &now)?;
        flip_in_review_revisions_to_done(&tx, &task.id, &now)?;
        block_pending_revisions_on_parent_close(&tx, &task.id, &now)?;
        let updated =
            query_task(&tx, work_item_id)?.with_context(|| format!("unknown task after update: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Reopen every `in_revision` comment addressed by `work_item_id`'s PR
    /// closing without merging — the "reopen on abandon" half of
    /// comment-intent-classification design §"Reconciliation" (task 2c).
    ///
    /// `chore-lifecycle-pr-closed-unmerged` (the design this reconciliation
    /// soft-depends on) is not yet implemented, so there is no `abandoned`
    /// task status to key off of; this is the minimal comment-only hook the
    /// design's Risks section calls out as an accepted interim mitigation —
    /// it reopens comments without changing any task's own status, matching
    /// the merge poller's existing "leave the row in place" behaviour for a
    /// `ClosedUnmerged` PR.
    ///
    /// Reconciles two cases, mirroring the resolve-side fan-out in
    /// [`Self::mark_chore_pr_merged`] / [`flip_in_review_revisions_to_done`]:
    ///   - `revise_task_id = work_item_id` directly — the plain-chore
    ///     vehicle, whose own PR is exactly the one that just closed.
    ///   - `revise_task_id` = a revision in `work_item_id`'s chain — the
    ///     PR-open vehicle, whose commit rides the chain root's PR, so the
    ///     chain root's close-unmerged event is the only terminal signal a
    ///     revision-owned comment ever gets today.
    ///
    /// Returns the number of comment rows reopened (tests / logging).
    pub fn reopen_comments_for_closed_unmerged_pr(&self, work_item_id: &str) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        // The direct/plain-chore vehicle's own comments only ever resolve on
        // a genuine merge, so a late/duplicate close-unmerged sweep here
        // must never undo that (`include_resolved: false`).
        let mut affected = comments::reconcile_comments_for_task(
            &tx,
            work_item_id,
            comments::CommentReconcileOutcome::Reopened {
                include_resolved: false,
            },
            &now,
        )?;
        for rev_id in collect_chain_revision_ids(&tx, work_item_id)? {
            // A revision's comments may already be `resolved` (its commit
            // landed and reached `in_review` before the chain root's PR
            // itself resolved) — that resolve must be undone here too, since
            // the commit that produced it never merged either.
            affected += comments::reconcile_comments_for_task(
                &tx,
                &rev_id,
                comments::CommentReconcileOutcome::Reopened { include_resolved: true },
                &now,
            )?;
        }
        tx.commit()?;
        Ok(affected)
    }

    /// Update the PR poll-state columns for a single task row after a
    /// successful merge-poller probe. Stores the CI and review state strings
    /// (and optional JSON-encoded detail blobs) plus the current timestamp.
    ///
    /// Returns a [`PrPollStateOutcome`] carrying `changed` (the CI, review, or
    /// merge-queue state actually moved, so the caller should emit a change
    /// event) and `prior_ci_state` (the `ci_required_state` value stored
    /// *before* this update). `changed` is `false` when the probe confirmed
    /// the same state as before, or when the row was deleted / not found.
    /// Errors propagate from the underlying DB operations.
    ///
    /// The UPDATE is guarded by a `WHERE` clause that skips rows whose
    /// `ci_required_state` AND `review_required_state` are already set to
    /// the incoming values, so `changes() == 0` reliably means "nothing
    /// changed" — the caller does not need to issue a separate read.
    ///
    /// `prior_ci_state` is read in the same connection just before the UPDATE
    /// so the caller can detect a `fail → success` transition (CI recovered at
    /// the current head) and broadcast a `CiFailureCleared` event, reconciling
    /// a stale "ci failing" badge away during the poll we already do. Per-task
    /// poll writes are serialised by the sweep loop, so the read-then-write is
    /// race-free in practice.
    ///
    /// `input.preserve_merge_queue_state` leaves `merge_queue_state` /
    /// `merge_queue_detail` untouched regardless of `input.merge_queue_state`
    /// / `input.merge_queue_detail` — the merge poller sets this for a
    /// `trunk_queue`-mechanism task, whose merge-queue columns are owned by
    /// the Trunk submission flow (`ServerState`'s `handle_trunk_queue_merge`),
    /// not by this GitHub probe: GitHub always reports
    /// `in_merge_queue=false`/`auto_merge_enabled=false` for such a task, so
    /// without this gate every sweep would immediately wipe the optimistic
    /// `"queued"` state the trunk submission just wrote.
    pub fn update_task_pr_poll_state(&self, work_item_id: &str, input: PrPollStateInput) -> Result<PrPollStateOutcome> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let prior_ci_state: Option<String> = tx
            .query_row(
                "SELECT ci_required_state FROM tasks
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let prior_merge_queue_state: Option<String> = tx
            .query_row(
                "SELECT merge_queue_state FROM tasks
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        // Always stamp the poll timestamp so operators can observe that sweeps
        // are running even when CI/review/merge-queue state is unchanged (e.g. a
        // PR that stays CONFLICTING for an extended period). Without this,
        // pr_state_polled_at freezes the moment the state stabilises, making it
        // impossible to distinguish a frozen poller from an actively-polling one.
        tx.execute(
            "UPDATE tasks SET pr_state_polled_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        // Only write state columns (and count as changed) when CI, review,
        // merge-queue state, or merge-queue detail (position/enqueued-at,
        // which ticks every sweep while queued) differs from what's already
        // stored. COALESCE treats NULL as distinct from any non-empty string,
        // so the first probe after migration always fires the event.
        //
        // `preserve` (`?8`) short-circuits both the merge-queue SET clauses
        // (via CASE, so the stored value passes through unchanged) and the
        // merge-queue arms of the changed-check, so a preserved row's
        // `changed` outcome reflects only CI/review movement.
        let preserve = input.preserve_merge_queue_state;
        let changed = tx.execute(
            "UPDATE tasks
             SET ci_required_state      = ?2,
                 review_required_state  = ?3,
                 ci_required_detail     = ?4,
                 review_required_detail = ?5,
                 merge_queue_state      = CASE WHEN ?8 THEN merge_queue_state ELSE ?6 END,
                 merge_queue_detail     = CASE WHEN ?8 THEN merge_queue_detail ELSE ?7 END
             WHERE id = ?1
               AND deleted_at IS NULL
               AND (COALESCE(ci_required_state, '') != ?2
                    OR COALESCE(review_required_state, '') != ?3
                    OR (NOT ?8 AND (COALESCE(merge_queue_state, '') != COALESCE(?6, '')
                                    OR COALESCE(merge_queue_detail, '') != COALESCE(?7, ''))))",
            params![
                work_item_id,
                input.ci_required_state,
                input.review_required_state,
                input.ci_required_detail,
                input.review_required_detail,
                input.merge_queue_state,
                input.merge_queue_detail,
                preserve,
            ],
        )?;
        tx.commit()?;
        Ok(PrPollStateOutcome {
            changed: changed > 0,
            prior_ci_state,
            prior_merge_queue_state,
        })
    }

    /// Write the Merging-UI columns for `work_item_id` directly — the
    /// Trunk-owned counterpart to [`Self::update_task_pr_poll_state`],
    /// whose `preserve_merge_queue_state` gate deliberately keeps the
    /// GitHub probe off these two columns for a `trunk_queue` product.
    ///
    /// Two callers: `app::review::handle_trunk_queue_merge`'s optimistic
    /// write right after a successful `submitPullRequest`, and the Trunk
    /// queue poller on every observation (including the `NULL, NULL`
    /// Review snap-back when an entry leaves the queue).
    ///
    /// Returns whether the stored pair actually moved. The poller re-derives
    /// identical JSON on most sweeps, and the macOS app is push-only — so
    /// without this the poller would either publish a `work_item_changed`
    /// per PR per 15 s regardless of news, or have to re-read the row
    /// itself to tell. Does not stamp `updated_at`, consistent with
    /// `update_task_pr_poll_state` also leaving it alone.
    pub fn set_task_merge_queue_state(
        &self,
        work_item_id: &str,
        merge_queue_state: Option<&str>,
        merge_queue_detail: Option<&str>,
    ) -> Result<bool> {
        let conn = self.connect()?;
        // `IS NOT` (not `<>`) so a NULL on either side compares as a value:
        // `NULL <> 'queued'` is NULL, i.e. not true, which would silently
        // swallow the very first write and the snap-back back to NULL.
        let changed = conn.execute(
            "UPDATE tasks SET merge_queue_state = ?2, merge_queue_detail = ?3 \
             WHERE id = ?1 AND deleted_at IS NULL \
               AND (merge_queue_state IS NOT ?2 OR merge_queue_detail IS NOT ?3)",
            params![work_item_id, merge_queue_state, merge_queue_detail],
        )?;
        Ok(changed > 0)
    }
}

/// Argument bundle for [`WorkDb::update_task_pr_poll_state`] — grouped to
/// keep the method under clippy's `too_many_arguments` threshold rather than
/// passing six positional `Option<&str>`/`&str` params.
#[derive(Debug, Clone, Copy, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct PrPollStateInput<'a> {
    pub ci_required_state: &'a str,
    pub review_required_state: &'a str,
    pub ci_required_detail: Option<&'a str>,
    pub review_required_detail: Option<&'a str>,
    pub merge_queue_state: Option<&'a str>,
    pub merge_queue_detail: Option<&'a str>,
    /// When `true`, `merge_queue_state`/`merge_queue_detail` are left
    /// untouched regardless of the values above — set by the caller for a
    /// `trunk_queue`-mechanism task, whose merge-queue columns are owned by
    /// the Trunk submission flow rather than this GitHub probe. Defaults to
    /// `false` (the pre-existing behaviour) via `#[derive(Default)]`.
    pub preserve_merge_queue_state: bool,
}

/// Outcome of [`WorkDb::update_task_pr_poll_state`].
#[derive(Debug, Clone)]
pub struct PrPollStateOutcome {
    /// `true` when the CI, review, or merge-queue state actually changed
    /// (so the caller should emit a `pr_poll_state_updated` event).
    pub changed: bool,
    /// The `ci_required_state` value stored *before* this update, or `None`
    /// when the column was NULL / the row was absent. Lets the caller detect
    /// a `fail → success` transition and clear a stale "ci failing" badge.
    pub prior_ci_state: Option<String>,
    /// The `merge_queue_state` value stored *before* this update, or `None`
    /// when the column was NULL / the row was absent. Lets the caller detect
    /// merge-queue entry/exit (`_ → "queued"` or `"queued" → _`) and trigger
    /// a whole-queue renumbering pass (`merge_poller::renumber_merge_queue`)
    /// rather than patching only this row.
    pub prior_merge_queue_state: Option<String>,
}

/// One row from [`WorkDb::list_queued_merge_queue_members`]: a task
/// currently sitting in GitHub's merge queue, keyed with its raw
/// `merge_queue_detail` JSON blob so the caller can re-derive a canonical
/// ordering across every member.
#[derive(Debug, Clone)]
pub struct QueuedMergeQueueMember {
    pub task_id: String,
    pub merge_queue_detail: Option<String>,
}

impl WorkDb {
    /// Every task in `product_id` currently in GitHub's merge queue
    /// (`merge_queue_state = 'queued'`). Used by the merge poller's
    /// whole-queue renumbering pass (`merge_poller::renumber_merge_queue`)
    /// to recompute every member's displayed position whenever any one of
    /// them enters, exits, fails, or reorders — rather than leaving
    /// siblings with a stale (possibly now-duplicate or now-missing)
    /// position until their own next individual probe.
    pub fn list_queued_merge_queue_members(&self, product_id: &str) -> Result<Vec<QueuedMergeQueueMember>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, merge_queue_detail
             FROM tasks
             WHERE product_id = ?1
               AND merge_queue_state = 'queued'
               AND status NOT IN ('done', 'archived', 'cancelled')
               AND deleted_at IS NULL",
        )?;
        let rows = stmt.query_map(params![product_id], |row| {
            Ok(QueuedMergeQueueMember {
                task_id: row.get(0)?,
                merge_queue_detail: row.get(1)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Overwrite `merge_queue_detail` for one task after a whole-queue
    /// renumbering pass recomputed its canonical position. Guarded by
    /// `merge_queue_state = 'queued'` so a row that exited the queue
    /// between [`list_queued_merge_queue_members`](Self::list_queued_merge_queue_members)'s
    /// read and this write is left untouched — it will simply no longer
    /// appear in the next renumbering pass's member list either. Returns
    /// `true` when a row was actually updated (so the caller knows to emit
    /// a change event), `false` when the stored value already matched or
    /// the row is no longer a live queue member.
    pub fn update_task_merge_queue_detail(&self, task_id: &str, merge_queue_detail: &str) -> Result<bool> {
        let conn = self.connect()?;
        let changed = conn.execute(
            "UPDATE tasks
             SET merge_queue_detail = ?2
             WHERE id = ?1
               AND deleted_at IS NULL
               AND merge_queue_state = 'queued'
               AND COALESCE(merge_queue_detail, '') != ?2",
            params![task_id, merge_queue_detail],
        )?;
        Ok(changed > 0)
    }
}

impl WorkDb {
    /// Return `(review_cycle, last_reviewed_sha)` for `task_id`.
    ///
    /// `review_cycle` is the number of `pr_review` passes that have completed
    /// for this task's PR. `last_reviewed_sha` is the PR HEAD SHA recorded at
    /// the end of the most recent pass, or `None` if no pass has completed yet.
    ///
    /// Used by the cycle-bound check in [`crate::completion::WorkerCompletionHandler`]
    /// before enqueuing a new `pr_review` execution. P992 design §7, task 9.
    pub fn get_task_review_cycle_state(&self, task_id: &str) -> Result<(i64, Option<String>)> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT review_cycle, last_reviewed_sha FROM tasks WHERE id = ?1",
            [task_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .with_context(|| format!("unknown task: {task_id}"))
    }

    /// Atomically increment `review_cycle` by 1 and set `last_reviewed_sha`.
    ///
    /// Called from [`crate::completion::WorkerCompletionHandler::finalize_pr_review_pass`]
    /// after a `pr_review` execution completes, regardless of whether a
    /// revision was warranted. A missing or empty `last_reviewed_sha` records
    /// `NULL` (the reviewer could not determine the HEAD SHA).
    /// P992 design §7, task 9.
    pub fn increment_task_review_cycle(&self, task_id: &str, last_reviewed_sha: Option<&str>) -> Result<()> {
        let conn = self.connect()?;
        let rows = conn.execute(
            "UPDATE tasks
             SET review_cycle      = review_cycle + 1,
                 last_reviewed_sha = ?2,
                 updated_at        = ?3
             WHERE id = ?1
               AND deleted_at IS NULL",
            params![task_id, last_reviewed_sha.filter(|s| !s.is_empty()), now_string(),],
        )?;
        if rows == 0 {
            bail!("unknown or deleted task: {task_id}");
        }
        Ok(())
    }

    /// Resolve the task id whose `review_cycle` / `last_reviewed_sha`
    /// columns should record a completed reviewer pass triggered by
    /// `task_id`'s completion.
    ///
    /// For a `revision` task, walks to the chain root (the task that owns
    /// the PR the revision pushes commits onto) and returns *its* id
    /// instead. Each revision is a brand-new task row, so tracking cycle
    /// state on the revision itself would silently reset the cycle-bound
    /// (`max_review_cycles`) and no-op-skip gates to zero on every single
    /// revision — defeating both once revisions can trigger reviews of
    /// their own. Bookkeeping instead accumulates on one persistent row
    /// across the whole chain, mirroring how the chain root is already the
    /// source of truth for `pr_url` (see [`Self::get_revision_chain_root_pr_url`]).
    ///
    /// Returns `task_id` unchanged for a non-revision task, or when the
    /// chain root can't be resolved (broken parent link — fails open so a
    /// data anomaly degrades to "count from zero" rather than an error).
    pub(crate) fn review_cycle_root_id(&self, task_id: &str) -> String {
        let Ok(conn) = self.connect() else {
            return task_id.to_owned();
        };
        match query_task(&conn, task_id) {
            Ok(Some(t)) if t.kind == TaskKind::Revision => get_chain_root_task(&conn, task_id)
                .ok()
                .flatten()
                .map(|root| root.id)
                .unwrap_or_else(|| task_id.to_owned()),
            _ => task_id.to_owned(),
        }
    }
}
