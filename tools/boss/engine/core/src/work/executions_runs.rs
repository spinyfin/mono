use super::*;

/// Options for [`WorkDb::cancel_execution_with`].
///
/// Defaults match the historical `cancel_execution` contract: any
/// non-terminal row may be cancelled, with reason `"explicit cancel"`.
#[derive(Debug, Clone, Default)]
pub struct CancelExecutionOpts {
    /// Operator- or engine-supplied reason written into the
    /// terminalization log line (and, at the RPC layer, the engine
    /// audit trail). Empty/whitespace is treated as unset.
    pub reason: Option<String>,
    /// When true, refuse any execution that is not never-started
    /// (`queued` / `ready` / `waiting_dependency` / `claimed` — see
    /// [`ExecutionStatus::is_pre_run`]). A `claimed` row is still accepted
    /// here: it is in the spawn window but no run has started, and the
    /// requested-host pre-start cancel depends on being able to
    /// terminalize it. Live workers must be stopped via
    /// `bossctl agents stop` instead.
    pub queued_only: bool,
}

impl WorkDb {
    /// Mark an execution `cancelled` and stamp `finished_at`. Errors
    /// when the execution is unknown or already in a terminal status
    /// — callers shouldn't try to cancel a row that's already done.
    ///
    /// **Kanban demote policy** (aligned with
    /// [`Self::cancel_running_execution_and_demote_task`]):
    ///
    /// - Cancelling a **live** execution (`running` / `waiting_human`)
    ///   demotes an `active` work item back to `todo` **only when this
    ///   execution is still the work item's latest**, and stamps
    ///   `last_status_actor = 'engine'`. That is the explicit
    ///   stop/abandon path: tear down the current worker and return the
    ///   card to Backlog so the orphan-active sweep does not redispatch
    ///   a ghost-active row.
    /// - Cancelling a **never-started** row (`ready` / `queued` /
    ///   `waiting_dependency`) does **not** touch task status. A human
    ///   (or another execution) may have the card in Doing; yanks from
    ///   cancel of a stale never-started row are the observed bug.
    /// - `in_review`, `done`, and `archived` are always preserved:
    ///   `in_review` means a PR exists and cancel doesn't retract that
    ///   PR, and `done`/`archived` are explicit human transitions that
    ///   the auto-dispatch path is forbidden from downgrading.
    ///
    /// Workspace lease columns are intentionally left intact so the
    /// caller can hand the execution id to
    /// `WorkerCompletionHandler::force_release`, which transfers
    /// lease ownership atomically by clearing the columns itself
    /// before talking to the cube CLI. Trying to clear them inside
    /// this transaction would race the same release path.
    ///
    /// Equivalent to [`Self::cancel_execution_with`] with
    /// [`CancelExecutionOpts::default`].
    pub fn cancel_execution(&self, execution_id: &str) -> Result<WorkExecution> {
        self.cancel_execution_with(execution_id, CancelExecutionOpts::default())
    }

    /// Like [`Self::cancel_execution`], but accepts a reason and the
    /// `queued_only` gate used by `bossctl executions cancel`.
    pub fn cancel_execution_with(&self, execution_id: &str, opts: CancelExecutionOpts) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if existing.status.is_terminal() {
            bail!(
                "execution {execution_id} is already in terminal status `{}` and cannot be cancelled",
                existing.status
            );
        }
        if opts.queued_only && !existing.status.is_pre_run() {
            if existing.status.is_live() {
                bail!(
                    "execution {execution_id} is `{}` (already started); \
                     use `bossctl agents stop {execution_id}` to stop a live worker — \
                     `executions cancel` only accepts never-started \
                     (queued/ready/waiting_dependency/claimed) rows",
                    existing.status
                );
            }
            bail!(
                "execution {execution_id} is `{}` and has already left the \
                 never-started set (queued/ready/waiting_dependency/claimed); \
                 use `bossctl work cancel {execution_id}` for any non-terminal \
                 row, or `bossctl agents stop` if a live worker still backs it",
                existing.status
            );
        }
        let reason = normalize_optional_text(opts.reason).unwrap_or_else(|| "explicit cancel".to_owned());
        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'cancelled',
                 finished_at = ?2
             WHERE id = ?1",
            params![execution_id, now.as_str()],
        )?;
        // Live cancel only: demote active → todo when this execution is
        // still the work item's latest. Never-started cancels leave the
        // kanban alone (shared helper with cancel_running_execution_and_demote_task).
        if existing.status.is_live() {
            demote_active_if_latest_execution(&tx, &existing.work_item_id, execution_id, &now)?;
        }
        let updated = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution after cancel: {execution_id}"))?;
        // Canonical terminalization trace — see `mark_execution_orphaned` for
        // why every terminal-transition site emits this line: a recurrence
        // of the ack-timeout / stale-reap contradiction (a live worker whose
        // execution the engine already terminalized) must be attributable
        // regardless of which site actually fired.
        //
        // `reason` is the operator- or engine-supplied cancel reason so the
        // audit trail distinguishes deliberate cancellation from
        // `orphaned` (engine lost the run) and `abandoned`.
        tracing::warn!(
            execution_id = %execution_id,
            work_item_id = %updated.work_item_id,
            from_status = %existing.status,
            to_status = %updated.status,
            reason = %reason,
            queued_only = opts.queued_only,
            "execution terminalized: cancel",
        );
        let mut pending = PendingEvents::new();
        stage_execution_terminal(&mut pending, &tx, execution_id, &updated.work_item_id)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok(updated)
    }

    /// Transition a non-terminal execution to the `orphaned` terminal
    /// status. Used by the startup reaper and the manual `bossctl
    /// agents reap` path when a worker process has died (or is
    /// presumed dead) but the engine has no other clean signal that
    /// it should stop treating the row as live.
    ///
    /// The workspace lease columns (`cube_lease_id`,
    /// `cube_workspace_id`, `workspace_path`) are intentionally left
    /// intact. The brief is explicit: do NOT release the cube
    /// workspace lease here — the workspace may still have in-flight
    /// commits from the dead worker that a fresh execution should
    /// resume against. Lease cleanup is a separate concern (cube TTL
    /// expiry or explicit `bossctl agents stop`).
    ///
    /// Any non-terminal `work_runs` rows attached to the execution are
    /// stamped `orphaned` with the same reason recorded as
    /// `result_summary`, so the run history reflects how the row went
    /// terminal rather than leaving it `active` forever.
    ///
    /// Errors when the execution is unknown or already terminal —
    /// callers shouldn't try to reap a row that's already done.
    pub fn mark_execution_orphaned(&self, execution_id: &str, reason: &str) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if existing.status.is_terminal() {
            bail!(
                "execution {execution_id} is already in terminal status `{}` and cannot be reaped as orphaned",
                existing.status
            );
        }
        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'orphaned',
                 finished_at = COALESCE(finished_at, ?2)
             WHERE id = ?1",
            params![execution_id, now.as_str()],
        )?;
        // Stamp any still-active work_runs as orphaned so the run
        // history matches the execution status. result_summary holds
        // the reaper's reason so an operator inspecting the row can
        // see why the engine terminated it.
        tx.execute(
            "UPDATE work_runs
             SET status = 'orphaned',
                 result_summary = COALESCE(result_summary, ?3),
                 finished_at = COALESCE(finished_at, ?2)
             WHERE execution_id = ?1
               AND finished_at IS NULL",
            params![execution_id, now.as_str(), reason],
        )?;
        // Reason durability for a run row that was already closed at
        // spawn-return — e.g. `PaneSpawnRunner` stamps `work_runs.finished_at`
        // (with a generic "spawned pane" `result_summary`) the instant the
        // pane comes up, before the worker ever produces a shell. Both guards
        // on the write above (`finished_at IS NULL`, `COALESCE(result_summary,
        // …)`) are then already defeated by the time a never-started-spawn
        // reap runs, so the real orphan reason never reaches the database —
        // only a rotating log has it. `error_text` is otherwise untouched by
        // that spawn-confirm write, so stamp it on the execution's most
        // recent run row regardless of `finished_at`, without overwriting a
        // legitimate existing value (ordering-by-recency instead of widening
        // the write above to be unconditional).
        tx.execute(
            "UPDATE work_runs
             SET error_text = COALESCE(NULLIF(error_text, ''), ?2)
             WHERE id = (
                 SELECT id FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1
             )",
            params![execution_id, reason],
        )?;
        let updated = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution after orphan reap: {execution_id}"))?;
        // Canonical terminalization trace. `mark_execution_orphaned` is the
        // stale-reap / dead-pane / lost-workspace terminal-transition site;
        // its many callers log inconsistently (some not at all), so a run
        // that went terminal "with no trace line stating the actual cause"
        // was the exact instrumentation gap behind the ack-timeout /
        // stale-reap contradiction (a live worker whose execution the engine
        // had already terminalized). Emit one greppable line naming the
        // prior status and reason at the moment of the transition, so the
        // next occurrence is attributable from the trace alone.
        tracing::warn!(
            execution_id = %execution_id,
            work_item_id = %updated.work_item_id,
            from_status = %existing.status,
            to_status = %updated.status,
            reason = %reason,
            "execution terminalized: orphan reap",
        );
        let mut pending = PendingEvents::new();
        stage_execution_terminal(&mut pending, &tx, execution_id, &updated.work_item_id)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok(updated)
    }

    /// Reverse an *inferred* terminalization: put an execution the engine
    /// wrongly declared dead back into the live state its still-running worker
    /// actually occupies.
    ///
    /// ## Why a terminal status is ever reversed
    ///
    /// `orphaned` and `abandoned` are not decisions — they are **guesses**.
    /// Every site that writes them ([`Self::mark_execution_orphaned`],
    /// [`crate::spawn_ack_sweep`], [`crate::dead_pid_sweep`], the orphan
    /// sweep's `request_execution_with_live_check`) is inferring "the worker
    /// must be gone" from the absence of a signal: no ack, no pid, no pool
    /// claim, no hook. Absence of a signal is exactly what a degraded network
    /// or a slow post-sleep RPC drain produces for a worker that is perfectly
    /// alive. When that worker subsequently proves itself — a hook arrives, or
    /// its recorded pid answers a probe — the guess is *disproven*, and the
    /// only correct response is to withdraw it.
    ///
    /// Leaving it standing is what produced the 2026-07-28 duplicate-dispatch
    /// storm: the row stayed terminal, so its work item stayed eligible for
    /// re-dispatch, so a second (then third) worker was spawned on top of a
    /// live one.
    ///
    /// `cancelled`, `completed` and `failed` are NOT reversible here and the
    /// call errors on them. Those are real decisions — an operator stopped the
    /// run, or the worker finished, or it genuinely errored — and a surviving
    /// process contradicting one of them means the process should be reaped,
    /// not the record rewritten.
    ///
    /// ## What it restores
    ///
    /// - The execution goes back to the status a healthy pane-hosted worker
    ///   occupies: `running` for a `pr_review` reviewer pane, `waiting_human`
    ///   for every other kind — the same split
    ///   `runner::pane_spawn` applies at spawn time, kept in one rule so the
    ///   re-adopted row is indistinguishable from a never-lost one.
    ///   `waiting_human` additionally makes the row invisible to
    ///   [`Self::list_orphan_active_candidates`], which is what stops the
    ///   re-dispatch storm at its source.
    /// - `finished_at` is cleared: the run did not finish.
    /// - The latest `work_runs` row is un-orphaned **only if the reap was what
    ///   stamped it**. A row this execution finished legitimately is
    ///   `completed`/`failed` and is left alone; an `orphaned` one can only
    ///   have come from the reap being reversed.
    /// - A work item demoted to `todo` by the reap is put back to `active`,
    ///   but only when this execution is still its latest — the same
    ///   latest-execution guard [`demote_active_if_latest_execution`] applies
    ///   in the other direction, so a newer execution driving the row is never
    ///   clobbered.
    ///
    /// ## Refusals
    ///
    /// Errors when the execution is unknown, when its status is not an
    /// inferred terminal, or when the work item already has a DIFFERENT live
    /// execution. That last check is the anti-duplication invariant restated
    /// at the storage layer: re-adopting into a row that already has a live
    /// worker would create precisely the double-worker state this whole change
    /// exists to prevent, so it is refused here even if a caller asks for it.
    pub fn readopt_inferred_terminal_execution(&self, execution_id: &str, evidence: &str) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if !matches!(existing.status, ExecutionStatus::Orphaned | ExecutionStatus::Abandoned) {
            bail!(
                "execution {execution_id} is `{}`, which is a deliberate outcome rather than an \
                 inferred death; only `orphaned` / `abandoned` may be re-adopted",
                existing.status
            );
        }
        let conflicting: Option<String> = tx
            .query_row(
                "SELECT id FROM work_executions
                 WHERE work_item_id = ?1
                   AND id != ?2
                   AND status IN ('running', 'waiting_human')
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                params![existing.work_item_id, execution_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(conflicting) = conflicting {
            bail!(
                "refusing to re-adopt {execution_id}: work item {} already has a live execution \
                 ({conflicting}); re-adopting would put two workers on one row",
                existing.work_item_id
            );
        }

        // The status a healthy pane-hosted worker sits in: its pane is up
        // and its agent is working, whatever the execution kind. A
        // re-adopted worker is by definition one whose hooks proved it
        // alive, so restoring it to `waiting_human` would assert the one
        // thing the evidence contradicts.
        let restored = ExecutionStatus::Running;
        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = ?2,
                 finished_at = NULL
             WHERE id = ?1",
            params![execution_id, restored.as_str()],
        )?;
        // Un-orphan the latest run row, but only when the reap is what
        // stamped it. A run that ended on its own terms is `completed` /
        // `failed` and stays exactly as it is.
        tx.execute(
            "UPDATE work_runs
             SET status = 'active',
                 finished_at = NULL,
                 result_summary = NULL
             WHERE id = (
                 SELECT id FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1
             ) AND status = 'orphaned'",
            params![execution_id],
        )?;
        // Reverse a demote, under the same latest-execution guard the demote
        // itself uses.
        tx.execute(
            "UPDATE tasks
             SET status            = 'active',
                 last_status_actor = 'engine',
                 updated_at        = ?2
             WHERE id             = ?1
               AND status         = 'todo'
               AND deleted_at     IS NULL
               AND ?3 = (
                   SELECT id FROM work_executions
                   WHERE work_item_id = ?1
                   ORDER BY created_at DESC, id DESC
                   LIMIT 1
               )",
            params![existing.work_item_id, now.as_str(), execution_id],
        )?;

        let updated = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution after re-adoption: {execution_id}"))?;
        // Counterpart to the canonical "execution terminalized" trace that
        // every terminal-transition site emits. A re-adoption is the only
        // transition that runs the other way, so it gets one greppable line
        // naming the status it reversed and the evidence that disproved it —
        // otherwise a row would appear to have left a terminal state with no
        // recorded cause, which is the same instrumentation gap that made the
        // ack-timeout / stale-reap contradiction so hard to attribute.
        tracing::warn!(
            execution_id = %execution_id,
            work_item_id = %updated.work_item_id,
            from_status = %existing.status,
            to_status = %updated.status,
            evidence = %evidence,
            "execution re-adopted: inferred death disproven by a live worker",
        );
        // No bus event: `Event` carries only terminal-direction lifecycle
        // signals, and publishing `ExecutionTerminal` for a row that just LEFT
        // terminal would tell every subscriber the opposite of what happened.
        // The UI refresh is the caller's job via `publish_work_item_changed`,
        // the same layer that handles it for `force_stop_execution`.
        commit_and_publish(tx, PendingEvents::new(), &self.event_bus)?;
        Ok(updated)
    }

    /// Auto-resume a work item whose worker stalled or died on a
    /// *transient* Claude API error. In one transaction:
    ///
    ///   1. If `dead_execution_id` is still non-terminal, mark it
    ///      `orphaned` (and stamp any still-active runs `orphaned` with
    ///      `reason`). Orphaned — not abandoned — so the coordinator's
    ///      recovery path ([`Self::get_prior_orphaned_execution`]) can find
    ///      it and replay its saved recovery patch (or re-lease its
    ///      workspace dirty for in-place recovery) into the new execution.
    ///   2. Insert a fresh `ready` execution for the same work item that
    ///      **prefers the same cube workspace with `allow_dirty = true`**
    ///      (so cube's `--prefer --allow-dirty` re-leases the exact
    ///      workspace *without* resetting it, and in-progress work in the
    ///      jj workspace is not lost), carries `transient_failure_count =
    ///      new_count`, and is deferred until `dispatch_not_before_epoch`
    ///      (the backoff window — same `dispatch_not_before` gate the
    ///      pre-start retry path uses, honoured by
    ///      [`Self::list_ready_executions`]).
    ///
    ///      `allow_dirty` is always forced `true` here, regardless of
    ///      what the dead execution carried: this function exists
    ///      specifically to reclaim a workspace that has uncommitted
    ///      in-flight work, so a plain carry-forward (which defaults to
    ///      `false` for an ordinary first dispatch) would let cube's
    ///      normal clean-reset silently discard that work even on a
    ///      successful same-workspace lease. Forcing it `true` also
    ///      hardens [`crate::coordinator`]'s `lease_workspace_with_fallback`:
    ///      a failed lease on the preferred workspace becomes a hard
    ///      failure instead of a silent fallback to a different, clean
    ///      workspace.
    ///
    /// Returns the new `ready` execution. The caller releases the worker
    /// pool slot and emits the dispatch event. Because the work item now
    /// has a `ready` execution, the orphan-active sweep skips it (no
    /// double dispatch).
    pub fn request_resume_execution(
        &self,
        dead_execution_id: &str,
        new_count: i64,
        dispatch_not_before_epoch: i64,
        reason: &str,
    ) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let dead = query_execution(&tx, dead_execution_id).require("execution", dead_execution_id)?;

        let now = now_string();
        if !dead.status.is_terminal() {
            tx.execute(
                "UPDATE work_executions
                 SET status = 'orphaned',
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE id = ?1",
                params![dead_execution_id, now.as_str()],
            )?;
            tx.execute(
                "UPDATE work_runs
                 SET status = 'orphaned',
                     result_summary = COALESCE(result_summary, ?3),
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE execution_id = ?1
                   AND finished_at IS NULL",
                params![dead_execution_id, now.as_str(), reason],
            )?;
        }

        // Prefer the workspace the dead worker was actually leased into;
        // fall back to its recorded preference if the lease metadata was
        // never stamped. Hard prefer (prefer_is_soft carried from the
        // dead row) so the resume lands on the same jj checkout.
        let preferred_workspace_id = dead
            .cube_workspace_id
            .clone()
            .or_else(|| dead.preferred_workspace_id.clone());

        let new_id = next_id("exec");
        let dispatch_not_before = dispatch_not_before_epoch.to_string();
        let branch_naming_json = serde_json::to_string(&dead.branch_naming).unwrap_or_default();
        tx.execute(
            "INSERT INTO work_executions (
                id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at, prefer_is_soft,
                transient_failure_count, dispatch_not_before, allow_dirty, branch_naming
             ) VALUES (?1, ?2, ?3, 'ready', ?4, ?5, NULL, NULL, NULL, ?6, ?7, ?8, NULL, NULL, ?9, ?10, ?11, ?12, ?13)",
            params![
                new_id,
                dead.work_item_id,
                dead.kind.as_str(),
                dead.repo_remote_url,
                dead.cube_repo_id,
                dead.priority,
                preferred_workspace_id,
                now,
                dead.prefer_is_soft as i64,
                new_count,
                dispatch_not_before,
                // Always true: this row exists to reclaim a workspace
                // with uncommitted in-flight work (see the doc comment
                // above) — never carry forward the dead execution's
                // (typically `false`) value.
                true as i64,
                branch_naming_json,
            ],
        )?;

        let new_execution = query_execution(&tx, &new_id)?
            .with_context(|| format!("missing execution after resume insert: {new_id}"))?;
        tx.commit()?;
        Ok(new_execution)
    }

    /// Path of the most recent run's transcript for `execution_id`, or
    /// `None` if no run recorded one. Used by the transient-recovery
    /// sweep to read the worker's transcript tail (the ground-truth
    /// signal for whether it stalled on an API error).
    pub fn latest_transcript_path(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT transcript_path FROM work_runs
             WHERE execution_id = ?1 AND transcript_path IS NOT NULL
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            params![execution_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// Raise a work-item-scoped attention item for `work_item_id` unless
    /// one with the same `kind` is already open. Idempotent so repeated
    /// recovery-sweep passes don't pile up duplicate rows. Returns the
    /// existing or newly-created item's id. Used by the transient-recovery
    /// sweep to escalate non-retryable / retry-exhausted workers.
    ///
    /// Filing runs [`warn_if_lifecycle_undeclared`]: a signal the engine can
    /// raise but has not declared a way to lower is the defect
    /// [`crate::attention_lifecycle`] exists to prevent, so an unregistered
    /// kind is surfaced in the trace rather than passing silently.
    ///
    /// Deduping onto the open row stamps its `last_raised_at`
    /// ([`reraise_open_work_item_attention`]) so a condition that trips
    /// again while the row is still open is not resolved by evidence from
    /// before the current occurrence.
    pub fn upsert_work_item_attention(
        &self,
        work_item_id: &str,
        kind: &str,
        title: &str,
        body_markdown: &str,
    ) -> Result<String> {
        warn_if_lifecycle_undeclared(kind);
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _ = product_id_for_work_item(&tx, work_item_id)?;
        let id = match reraise_open_work_item_attention(&tx, work_item_id, kind)? {
            Some(id) => id,
            None => {
                let id = next_id("attn");
                let now = now_string();
                tx.execute(
                    "INSERT INTO work_attention_items (
                        id, execution_id, work_item_id, kind, status, title, body_markdown, created_at,
                        resolved_at, last_raised_at
                     ) VALUES (?1, NULL, ?2, ?3, 'open', ?4, ?5, ?6, NULL, ?6)",
                    params![id, work_item_id, kind, title, body_markdown, now],
                )?;
                id
            }
        };
        tx.commit()?;
        Ok(id)
    }

    /// Return the run ids that belong to `execution_id` and have not
    /// yet finished. The cancel-execution flow uses this to find any
    /// libghostty pane the execution still backs so the engine can
    /// release it in addition to the cube workspace.
    pub fn active_run_ids_for_execution(&self, execution_id: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM work_runs
             WHERE execution_id = ?1
               AND finished_at IS NULL",
        )?;
        let rows = stmt.query_map([execution_id], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Build a map from `cube_lease_id` → `execution_id` for every
    /// execution row that currently records a lease. Used by
    /// `WorkspacePoolSummary` to annotate cube's view of the pool with
    /// the engine's own knowledge of which lease is backing which
    /// execution. Rows without a lease (`cube_lease_id IS NULL`) are
    /// skipped.
    pub fn lease_to_execution_map(&self) -> Result<HashMap<String, String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT cube_lease_id, id
             FROM work_executions
             WHERE cube_lease_id IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut map = HashMap::new();
        for row in rows {
            let (lease_id, execution_id) = row?;
            map.insert(lease_id, execution_id);
        }
        Ok(map)
    }

    /// Ready executions the dispatcher's `drain_ready_queue` picks from, in
    /// dispatch order — the *only* place that order is decided, since all
    /// three worker pools (main / automation / review) share this single
    /// queue and split by per-row pool classification downstream (see
    /// `ExecutionCoordinator::pool_for_execution`).
    ///
    /// Sort key (operator directive: revisions before tasks/chores, ordered
    /// by revision kind — highest priority first):
    ///
    /// ```text
    /// (DispatchClass ASC, work_executions.priority DESC, created_at ASC, id ASC)
    /// ```
    ///
    /// `DispatchClass` (see `dispatch_class.rs`) ranks 1=merge-conflict-fixing
    /// revision, 2=CI-fixing revision, 3=automated-PR-review-finding
    /// revision, 4=any other revision, 5=any other task/chore. The `CASE`
    /// below is the SQL mirror of `DispatchClass::classify` — GLOB (not
    /// LIKE) is used deliberately because `created_via` prefixes like
    /// `pr_review:` contain a literal underscore that LIKE's `_` wildcard
    /// would otherwise treat as "any one character". Within a class, the
    /// existing `priority` column (still respected) then plain FIFO by
    /// `created_at`/`id` break ties, exactly as before this change.
    ///
    /// This orders the queue; it does not decide which pool's capacity a
    /// row may take. `drain_ready_queue` walks the returned order twice so
    /// that automation ranks below all mainline work for an *interactive*
    /// slot regardless of where it lands in this sort — see
    /// `crate::dispatch_spillover` and `DispatchClass`'s docs.
    pub fn list_ready_executions(&self) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let class_case = format!(
            "CASE \
               WHEN t.kind = 'revision' AND t.created_via GLOB '{merge_conflict}*' THEN 1 \
               WHEN t.kind = 'revision' AND t.created_via GLOB '{ci_fix}*' THEN 2 \
               WHEN t.kind = 'revision' AND t.created_via GLOB '{pr_review}*' THEN 3 \
               WHEN t.kind = 'revision' THEN 4 \
               ELSE 5 \
             END",
            merge_conflict = CREATED_VIA_MERGE_CONFLICT_PREFIX,
            ci_fix = CREATED_VIA_CI_FIX_PREFIX,
            pr_review = CREATED_VIA_PR_REVIEW_PREFIX,
        );
        let sql = format!(
            "SELECT we.id, we.work_item_id, we.kind, we.status, we.repo_remote_url, we.cube_repo_id, we.cube_lease_id, \
                    we.cube_workspace_id, we.workspace_path, we.priority, we.preferred_workspace_id, \
                    we.created_at, we.started_at, we.finished_at, \
                    we.pre_start_failure_count, we.dispatch_not_before, we.pr_url, we.pr_head_before, \
                    we.prefer_is_soft, we.worker_branch_prefix, we.transient_failure_count, we.allow_dirty, we.branch_naming, \
                    we.dispatch_wait_reason, we.dispatch_wait_since, we.driver_runtime_state, we.driver, we.model, we.effort_level, we.pr_head_after \
             FROM work_executions we \
             LEFT JOIN tasks t ON t.id = we.work_item_id \
             WHERE we.status = 'ready' \
               AND (we.dispatch_not_before IS NULL \
                    OR CAST(we.dispatch_not_before AS INTEGER) <= CAST(strftime('%s', 'now') AS INTEGER)) \
             ORDER BY {class_case} ASC, we.priority DESC, we.created_at ASC, we.id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    /// Compare-and-swap `ready → claimed` so the product reconciler cannot
    /// rewrite this row while cube setup is in flight.
    ///
    /// `Won` means this caller now owns the spawn window. `AlreadyHeld` means
    /// another drain/`force_dispatch` already claimed it — do not spawn a
    /// second worker. `Rejected` means the row left `ready` (typically
    /// `waiting_dependency` after a reconciler write that won the race).
    pub fn claim_execution_for_dispatch(&self, execution_id: &str) -> Result<DispatchClaimOutcome> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE work_executions SET status = 'claimed' WHERE id = ?1 AND status = 'ready'",
            [execution_id],
        )?;
        if updated > 0 {
            return Ok(DispatchClaimOutcome::Won);
        }
        let execution = query_execution(&conn, execution_id).require("execution", execution_id)?;
        Ok(match execution.status {
            ExecutionStatus::Claimed => DispatchClaimOutcome::AlreadyHeld,
            _ => DispatchClaimOutcome::Rejected,
        })
    }

    /// Revert `claimed → ready` when a drain path claimed the row and then
    /// deferred without spawning (chain hold, pool exhausted, inflight
    /// reservation lost). No-op if the row is no longer `claimed`.
    pub fn release_dispatch_claim(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE work_executions SET status = 'ready' WHERE id = ?1 AND status = 'claimed'",
            [execution_id],
        )?;
        Ok(updated > 0)
    }

    /// Boot-only: every leftover `claimed` row belongs to a spawn task that
    /// died with the previous engine process. Revert them to `ready` so
    /// `list_ready_executions` can pick them up; the cube lease, if any,
    /// was never persisted on the row (it is written at `start_execution_run`)
    /// and is reclaimed by cube TTL / the next lease of that workspace.
    pub fn release_stale_claimed_executions(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt =
            conn.prepare("SELECT id FROM work_executions WHERE status = 'claimed' ORDER BY created_at ASC, id ASC")?;
        let ids: Vec<String> = stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        let mut released = Vec::new();
        for id in ids {
            let updated = conn.execute(
                "UPDATE work_executions SET status = 'ready' WHERE id = ?1 AND status = 'claimed'",
                [&id],
            )?;
            if updated > 0 {
                tracing::warn!(
                    execution_id = %id,
                    "startup: reverted leftover claimed execution to ready (spawn died with the previous process)"
                );
                released.push(id);
            }
        }
        Ok(released)
    }

    /// Return every `work_executions` row the engine considers "in
    /// flight": status is non-terminal AND a cube workspace lease was
    /// recorded against it (`cube_lease_id IS NOT NULL`). The startup
    /// reconciler probes these against cube state to decide whether
    /// the underlying worker is still alive — without that probe, the
    /// existing `reconcile_active_dispatch` redispatches every
    /// non-terminal row blindly because the live-worker registry is
    /// empty at boot, which is the bug that produced the duplicate
    /// dispatch on 2026-05-07.
    pub fn list_in_flight_executions(&self) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since, driver_runtime_state, driver, model, effort_level, pr_head_after
             FROM work_executions
             WHERE status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
               AND cube_lease_id IS NOT NULL
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    /// Return every non-terminal `work_executions` row that recorded a
    /// `workspace_path` — the candidate set for the lost-workspace
    /// reconciler ([`crate::lost_workspace_sweep`]).
    ///
    /// A worker parked in `running` / `waiting_human` keeps a live cube
    /// workspace checkout at `workspace_path` for the lifetime of its pane;
    /// pre-dispatch statuses (`queued` / `ready` / `waiting_dependency` /
    /// `claimed`) never have a `workspace_path` because it is only stamped at
    /// lease time. Filtering on `workspace_path IS NOT NULL AND != ''`
    /// therefore selects exactly the rows whose liveness can be judged by
    /// whether that directory still exists on disk. Unlike
    /// [`Self::list_in_flight_executions`] this does NOT require
    /// `cube_lease_id IS NOT NULL` — a row whose lease was already released
    /// but whose `workspace_path` lingers is still a valid candidate.
    pub fn list_non_terminal_executions_with_workspace(&self) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since, driver_runtime_state, driver, model, effort_level, pr_head_after
             FROM work_executions
             WHERE status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
               AND workspace_path IS NOT NULL
               AND workspace_path != ''
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    /// Return every non-terminal `revision_implementation` execution whose
    /// task is a revision in the chain rooted at `chain_root_id`.  Used by
    /// the merge poller to find in-flight revision workers to stop after
    /// the parent PR merges.  Only executions that hold a cube workspace
    /// lease are returned (same predicate as `list_in_flight_executions`).
    ///
    /// Walks with [`collect_chain_revision_ids_including_deleted`] rather
    /// than the tombstone-filtered variant: by the time this runs, the
    /// merge poller has already called `block_pending_revisions_on_parent_close`
    /// in the same sweep, which archives *and tombstones* WIP revisions —
    /// a tombstone-filtered walk would no longer see the row whose lease
    /// still needs releasing.
    pub fn list_active_revision_executions_for_chain(&self, chain_root_id: &str) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let revision_ids = collect_chain_revision_ids_including_deleted(&conn, chain_root_id)?;
        if revision_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut executions = Vec::new();
        for rev_id in &revision_ids {
            let mut stmt = conn.prepare_cached(
                "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                        cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                        created_at, started_at, finished_at,
                        pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since, driver_runtime_state, driver, model, effort_level, pr_head_after
                 FROM work_executions
                 WHERE work_item_id = ?1
                   AND kind = 'revision_implementation'
                   AND status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
                   AND cube_lease_id IS NOT NULL
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map([rev_id], map_execution)?;
            collect_rows(rows).map(|mut v| executions.append(&mut v))?;
        }
        Ok(executions)
    }

    pub fn reconcile_product_executions(&self, product_id: &str) -> Result<ExecutionReconcileResult> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _product = query_product(&tx, product_id).require("product", product_id)?;
        let _projects = list_projects_for_product(&tx, product_id)?;
        let tasks = list_tasks_for_product(&tx, product_id)?;
        let mut result = ExecutionReconcileResult::default();
        let mut pending = PendingEvents::new();

        // Per-row repo resolution lives inside
        // `reconcile_work_item_execution` now — the product default
        // is one of several fallbacks the resolver applies, not the
        // sole signal threaded through here.

        // Bucket the product's project-bound tasks by parent. Both
        // `kind = 'design'` and `kind = 'project_task'` share the
        // same first-incomplete-is-`ready` chain — design tasks live
        // at `ordinal = 0` so they sort to the head of the list and
        // dispatch first. The execution kind diverges per-row:
        // design dispatches as `project_design`, project_tasks as
        // `task_implementation`. This is the single point where the
        // project_design lifecycle plugs into the existing per-task
        // dispatch machinery; once routed the rest of the lifecycle
        // (PR detection, in_review→done, dependency cascade) is the
        // unchanged task path.
        let mut project_tasks: HashMap<String, Vec<Task>> = HashMap::new();
        for task in tasks {
            match task.kind {
                TaskKind::Chore | TaskKind::Followup => {
                    if task_accepts_execution(&task) {
                        reconcile_work_item_execution(
                            &tx,
                            &mut result,
                            &task.id,
                            ExecutionKind::ChoreImplementation,
                            ExecutionStatus::Ready,
                        )?;
                    }
                }
                // Investigation tasks dispatch independently (no project
                // dependency chain) — each produces one standalone doc PR.
                TaskKind::Investigation => {
                    if task_accepts_execution(&task) {
                        reconcile_work_item_execution(
                            &tx,
                            &mut result,
                            &task.id,
                            ExecutionKind::InvestigationImplementation,
                            ExecutionStatus::Ready,
                        )?;
                    }
                }
                // Design postmortems dispatch independently too — they are
                // scheduled by `project_postmortem_sweep` only once the
                // project's ordinal-based task chain is fully drained, so
                // there is never a chain to serialize against.
                TaskKind::DesignPostmortem => {
                    if task_accepts_execution(&task) {
                        reconcile_work_item_execution(
                            &tx,
                            &mut result,
                            &task.id,
                            ExecutionKind::ProjectDesign,
                            ExecutionStatus::Ready,
                        )?;
                    }
                }
                // Revision tasks dispatch independently like investigations.
                // Each pushes a new commit to the *parent's* existing PR
                // branch rather than opening a new PR.  The gate checks
                // the chain root's status first (cached): if the parent PR
                // has already merged (chain root is `done`), the revision
                // is auto-blocked here rather than dispatched.
                TaskKind::Revision => {
                    if task_accepts_execution(&task) {
                        reconcile_revision_execution(&mut pending, &tx, &mut result, &task)?;
                    }
                }
                TaskKind::ProjectTask | TaskKind::Design => {
                    if let Some(project_id) = &task.project_id {
                        project_tasks.entry(project_id.clone()).or_default().push(task);
                    }
                }
                TaskKind::Task => {
                    // Plain task: no standalone execution; must be in a project.
                }
            }
        }

        for tasks in project_tasks.values_mut() {
            tasks.sort_by(|left, right| {
                left.ordinal
                    .unwrap_or(i64::MAX)
                    .cmp(&right.ordinal.unwrap_or(i64::MAX))
                    .then_with(|| left.created_at.cmp(&right.created_at))
                    .then_with(|| left.id.cmp(&right.id))
            });

            // Ordinal-chain readiness: only the single lowest-ordinal
            // incomplete task in a project is `ready`; every other project
            // task is forced to `waiting_dependency`. The declared `blocks`
            // prerequisite graph is NOT consulted here — that gate lives in
            // `reconcile_work_item_execution` (and the dispatcher). The two
            // rules disagree: a later-ordinal task with an empty prerequisite
            // list is still flipped to `waiting_dependency` by this loop.
            // That disagreement is what made the ready-window race reachable
            // (the scheduler had already picked the row up as `ready`). The
            // `claimed` status + CAS write close the race; aligning this
            // rule with the prerequisite graph is a separate change.
            let first_incomplete = tasks.iter().position(task_accepts_execution);

            for (index, task) in tasks.iter().enumerate() {
                if !task_accepts_execution(task) {
                    continue;
                }
                let desired_status = if Some(index) == first_incomplete {
                    ExecutionStatus::Ready
                } else {
                    ExecutionStatus::WaitingDependency
                };
                let execution_kind = match task.kind {
                    TaskKind::Design => ExecutionKind::ProjectDesign,
                    // Never actually reached: DesignPostmortem rows are
                    // routed to the independent-dispatch arm above and never
                    // land in `project_tasks`. Mapped defensively in case
                    // that invariant ever breaks.
                    TaskKind::DesignPostmortem => ExecutionKind::ProjectDesign,
                    // All remaining kinds in this bucket are project_task rows;
                    // the other variants are handled before being bucketed here.
                    TaskKind::ProjectTask
                    | TaskKind::Chore
                    | TaskKind::Followup
                    | TaskKind::Investigation
                    | TaskKind::Revision
                    | TaskKind::Task => ExecutionKind::TaskImplementation,
                };
                reconcile_work_item_execution(&tx, &mut result, &task.id, execution_kind, desired_status)?;
            }
        }

        commit_and_publish(tx, pending, self.event_bus())?;
        Ok(result)
    }

    pub fn start_execution_run(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: &str,
        cube_lease_id: &str,
        cube_workspace_id: &str,
        workspace_path: &str,
    ) -> Result<(WorkExecution, WorkRun)> {
        // Default to the local host. The distributed-execution dispatch
        // path (`schedule_execution`) calls `start_execution_run_on_host`
        // with the host the scheduler picked; every other caller is a
        // local-only run and inherits the `'local'` default.
        self.start_execution_run_on_host(
            execution_id,
            agent_id,
            cube_repo_id,
            cube_lease_id,
            cube_workspace_id,
            workspace_path,
            "local",
        )
    }

    /// Host-aware variant of [`Self::start_execution_run`]. Persists the
    /// scheduler-selected `host_id` onto both the new `work_runs` row and
    /// the `work_executions` row (per the distributed-execution design's
    /// "Storage Additions": the execution's `host_id` is "populated when a
    /// run first picks a host"; `work_runs.host_id` is the durable
    /// per-run attribution). `host_id = "local"` reproduces the historical
    /// behaviour exactly.
    #[allow(clippy::too_many_arguments)]
    pub fn start_execution_run_on_host(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: &str,
        cube_lease_id: &str,
        cube_workspace_id: &str,
        workspace_path: &str,
        host_id: &str,
    ) -> Result<(WorkExecution, WorkRun)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if !execution.status.can_begin_run() {
            bail!(
                "execution {execution_id} is not ready and cannot start a run from status `{}`",
                execution.status
            );
        }

        let now = now_string();
        let updated = tx.execute(
            "UPDATE work_executions
             SET status = 'running',
                 cube_repo_id = ?2,
                 cube_lease_id = ?3,
                 cube_workspace_id = ?4,
                 workspace_path = ?5,
                 host_id = ?7,
                 started_at = COALESCE(started_at, ?6),
                 finished_at = NULL
             WHERE id = ?1
               AND status IN ('ready', 'claimed')",
            params![
                execution_id,
                cube_repo_id,
                cube_lease_id,
                cube_workspace_id,
                workspace_path,
                now,
                host_id
            ],
        )?;
        if updated == 0 {
            let current = query_execution(&tx, execution_id).require("execution", execution_id)?;
            bail!(
                "execution {execution_id} is not ready and cannot start a run from status `{}`",
                current.status
            );
        }

        // Auto-advance the work item's kanban status to `active` so
        // the card moves into the Doing column when work begins.
        // Only applies to tasks/chores; products and projects use a
        // different status vocabulary and aren't rendered on the
        // kanban. Don't downgrade items already in `done` or
        // `archived` — manual transitions win.
        //
        // `in_review` needs a narrower guard than a blanket exclusion.
        // A row in Review owns an open PR. For a non-revision
        // base (chore/project_task/task) the only legitimate
        // continuation of that PR is a `kind=revision` task riding the
        // base's branch — the base itself must never re-appear in
        // Doing, whether the fresh execution landed on it by a stray
        // race (the pre-existing base-protection tests) or via a
        // deliberate direct re-dispatch (e.g. the automated reviewer
        // pass dispatches follow-up `chore_implementation` /
        // `pr_review` executions straight against the base while
        // `record_worker_pr_completion`'s
        // `PendingReview` target holds its status exactly where it
        // was — that whole mechanism depends on the base never
        // visibly flipping to Doing mid-review).
        //
        // A revision task is different: its own re-dispatch IS the
        // sanctioned continuation, and revision rows themselves rest
        // in `in_review` between attempts (e.g. `reconcile_revision`
        // settling a row once its spawning CI/conflict attempt
        // retires, or a worker that exited without pushing) —
        // blocking those from ever reaching `active` again strands a
        // live worker with no Doing card anywhere on the board. So the
        // guard is lifted only for `kind = 'revision'`, and only when
        // the revision has no non-terminal revision child of its own.
        //
        // After flat-parentage every revision's `parent_task_id` is the chain
        // root (never another revision), so the child check below is a
        // no-op for well-formed data — siblings are sequenced by the
        // chain-tail dependency gate, not nested parents. The check is
        // retained for residual pre-migration nested rows until the
        // flatten migration has run on every open DB.
        //
        // This relaxation also narrows the reverse transition (`active`
        // back to `in_review`) to two specific paths:
        // `record_worker_pr_completion` on a PR-completion signal, and
        // `reconcile_revision_execution`'s settle for conflict/CI-fix
        // revisions. A human/reviewer-created revision that reaches
        // `active` via this guard and then has its worker die without a
        // Stop or a push has no path back to `in_review` — it will sit
        // in `active` and surface via the orphan-active sweep instead of
        // as a static Review card. This is an accepted trade-off (the
        // old blanket guard's incidental loop-halt for that specific
        // failure mode is given up in exchange for the live worker
        // actually showing on the kanban), not a defect in this query.
        //
        // `autostart` is cleared here (single-shot semantics): once a
        // row has ever transitioned to Doing, the flag is consumed so
        // that moving the card back to Backlog later does not trigger
        // re-dispatch by the reconciler or orphan-active sweep.
        tx.execute(
            "UPDATE tasks
             SET status = 'active',
                 autostart = 0,
                 updated_at = ?2
             WHERE id = ?1
               AND deleted_at IS NULL
               AND status NOT IN ('done', 'archived', 'blocked')
               AND (
                 status != 'in_review'
                 OR (
                     kind = 'revision'
                     AND NOT EXISTS (
                         SELECT 1 FROM tasks child
                         WHERE child.parent_task_id = ?1
                           AND child.kind = 'revision'
                           AND child.deleted_at IS NULL
                           AND child.status NOT IN ('done', 'archived')
                     )
                 )
               )",
            params![execution.work_item_id, now],
        )?;

        // A run is starting for this work item, which refutes the one thing
        // `dispatch_failed_reason` asserts: that the engine could not get an
        // execution running for it. Clear the stamp so the card's red
        // "Failed to start — …" banner comes down.
        //
        // Before this, the columns were cleared in exactly one place —
        // `request_execution_in_tx_with_live_check` — so only a deliberate
        // re-dispatch through *that* function lowered the banner. Every other
        // way an item resumes (the reconciler minting an execution, a
        // `pr_review` / `revision_implementation` / `ci_remediation`
        // execution dispatched by the review pipeline) left it stamped
        // forever: an item could reach Merging with a bound PR and four
        // completed revisions while still showing a lease failure from hours
        // earlier. Hooking the run start instead of any one dispatch caller
        // means the rule holds for every path, present and future.
        //
        // Guarded on `dispatch_failed_reason IS NOT NULL` so it is a no-op
        // for the overwhelming majority of starts, and deliberately NOT
        // guarded on kanban status: the banner is about dispatch, not about
        // which lane the card sits in. `crate::attention_reconcile_sweep`
        // runs the same clear as a periodic backstop, for rows stamped by an
        // engine process that predates this call.
        tx.execute(
            "UPDATE tasks
             SET dispatch_failed_reason = NULL,
                 dispatch_failed_error  = NULL,
                 dispatch_failed_at     = NULL
             WHERE id = ?1
               AND dispatch_failed_reason IS NOT NULL",
            params![execution.work_item_id],
        )?;

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at, host_id
             ) VALUES (?1, ?2, ?3, 'active', NULL, NULL, NULL, NULL, ?4, ?4, NULL, ?5)",
            params![run_id, execution_id, agent_id, now, host_id],
        )?;

        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let run = query_run(&tx, &run_id)?.with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        Ok((execution, run))
    }

    /// Record the head SHA of the chore's bound PR captured at run
    /// start. Used by the Stop-boundary SHA-delta gate to decide
    /// whether a resume run actually contributed to the bound PR
    /// before falling through to the `PROBE_NO_PR` nudge. Idempotent;
    /// callers may invoke once per execution start (or skip when no
    /// PR is bound). Empty `sha` is rejected — pass `None` semantics
    /// by simply not calling.
    /// Stamp the "stop event was observed" marker for this execution.
    /// Called by `on_stop_inner` the first time a Stop event fires.
    /// The SHA-delta gate in `recheck_for_pr` checks this before running
    /// for `revision_implementation` executions: the gate must only fire
    /// *after* a Stop has been seen (as a recovery path for transient
    /// failures), never while the revision worker is still running
    /// between turns. Idempotent.
    pub fn set_execution_stop_seen(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET stop_seen = 1 WHERE id = ?1",
            params![execution_id],
        )?;
        Ok(())
    }

    /// Persist the opaque [`boss_protocol::DriverRuntimeState`] returned by
    /// [`boss_engine_driver::AgentDriver::provision_workspace`] onto the
    /// execution row. Survives engine restart, orphan recovery, and
    /// workspace release (`clear_execution_workspace` deliberately leaves
    /// this column alone). Pass `None` to clear a previously recorded
    /// value (idempotent teardown of Claude-shaped drivers that return
    /// no state typically never calls this).
    pub fn set_driver_runtime_state(
        &self,
        execution_id: &str,
        state: Option<&boss_protocol::DriverRuntimeState>,
    ) -> Result<()> {
        let conn = self.connect()?;
        let json = match state {
            Some(s) => Some(serde_json::to_string(s).context("serialize driver_runtime_state")?),
            None => None,
        };
        let affected = conn.execute(
            "UPDATE work_executions SET driver_runtime_state = ?2 WHERE id = ?1",
            params![execution_id, json],
        )?;
        if affected == 0 {
            bail!("unknown execution: {execution_id}");
        }
        Ok(())
    }

    /// Load the opaque driver-owned runtime state for `execution_id`.
    /// `None` when the row is missing, the column is NULL (Claude /
    /// pre-migration), or the stored JSON fails to parse (treated as
    /// absent so a corrupt blob cannot block teardown).
    pub fn get_driver_runtime_state(&self, execution_id: &str) -> Result<Option<boss_protocol::DriverRuntimeState>> {
        let conn = self.connect()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT driver_runtime_state FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(raw.and_then(|s| serde_json::from_str::<boss_protocol::DriverRuntimeState>(&s).ok()))
    }

    /// Every execution that still has a non-empty `driver_runtime_state`
    /// blob — the input set for driver-owned home retention (Codex
    /// per-run `CODEX_HOME` reclaim). Includes both live and terminal
    /// rows; the caller classifies liveness from `status`. Does not
    /// invent paths by scanning a provider home.
    pub fn list_executions_with_driver_runtime_state(&self) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since, driver_runtime_state, driver, model, effort_level, pr_head_after
             FROM work_executions
             WHERE driver_runtime_state IS NOT NULL
               AND driver_runtime_state != ''
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    /// Return `true` if `on_stop_inner` has been called at least once for
    /// this execution (i.e. the `stop_seen` flag is set). Returns `false`
    /// for unknown execution IDs (treat as not seen, gate stays closed).
    pub fn execution_stop_seen(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let seen: Option<i64> = conn
            .query_row(
                "SELECT stop_seen FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(seen.unwrap_or(0) != 0)
    }

    pub fn set_execution_pr_head_before(&self, execution_id: &str, sha: &str) -> Result<()> {
        if sha.is_empty() {
            bail!("set_execution_pr_head_before: sha must be non-empty");
        }
        let conn = self.connect()?;
        let affected = conn.execute(
            "UPDATE work_executions SET pr_head_before = ?2 WHERE id = ?1",
            params![execution_id, sha],
        )?;
        if affected == 0 {
            bail!("unknown execution: {execution_id}");
        }
        Ok(())
    }

    /// Stamp `revision_stop_contributed_head` to record that
    /// `on_stop_inner`'s SHA-delta `Contributed` arm observed `sha` as the
    /// current PR head for a `revision_implementation` execution. Used by
    /// `recheck_for_pr` as the T848 recovery gate: it only finalizes when the
    /// current head matches this stamped value — not on any arbitrary head
    /// movement from a concurrently-active parent worker.
    pub fn set_revision_stop_contributed_head(&self, execution_id: &str, sha: &str) -> Result<()> {
        if sha.is_empty() {
            bail!("set_revision_stop_contributed_head: sha must be non-empty");
        }
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET revision_stop_contributed_head = ?2 WHERE id = ?1",
            params![execution_id, sha],
        )?;
        Ok(())
    }

    /// Return the `revision_stop_contributed_head` value for `execution_id`,
    /// or `None` if it has never been set (on_stop_inner has not yet observed
    /// a `Contributed` outcome for this execution).
    pub fn get_revision_stop_contributed_head(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let head: Option<String> = conn
            .query_row(
                "SELECT revision_stop_contributed_head FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(head)
    }

    /// Stamp `pr_head_baseline_absorbed` for `execution_id`, recording that
    /// `on_stop_inner`'s parent-push suppression path has rewritten
    /// `pr_head_before` to a head movement it attributed to the
    /// concurrently-active parent worker rather than to this revision's own
    /// push. Never cleared for the life of the execution — once the
    /// baseline has been absorbed once, every later SHA-delta comparison is
    /// against that rewritten value, not the dispatch-time head, for good.
    pub fn mark_execution_pr_head_baseline_absorbed(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET pr_head_baseline_absorbed = 1 WHERE id = ?1",
            params![execution_id],
        )?;
        Ok(())
    }

    /// Whether `pr_head_before` has ever been rewritten by the parent-push
    /// suppression path for `execution_id` — see
    /// [`Self::mark_execution_pr_head_baseline_absorbed`]. `false` for any
    /// execution that has never gone through that path (including
    /// pre-migration rows, which default to 0).
    pub fn execution_pr_head_baseline_absorbed(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let absorbed: Option<i64> = conn
            .query_row(
                "SELECT pr_head_baseline_absorbed FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(absorbed.unwrap_or(0) != 0)
    }

    /// Snapshot the bound PR's description/body captured at run start.
    /// Baseline for the metadata-only CI-fix finalize gate (issue #1252):
    /// `on_stop` diffs the live body against this to detect an
    /// operator-visible PR-metadata delta. Unlike
    /// [`Self::set_execution_pr_head_before`] an *empty* body is a valid
    /// snapshot (a PR can legitimately start with no description, and a
    /// worker that adds one is a real delta), so empty input is stored as
    /// the empty string rather than rejected. `NULL` (never called) means
    /// "no baseline" and the gate treats it as inapplicable.
    pub fn set_execution_pr_body_before(&self, execution_id: &str, body: &str) -> Result<()> {
        let conn = self.connect()?;
        let affected = conn.execute(
            "UPDATE work_executions SET pr_body_before = ?2 WHERE id = ?1",
            params![execution_id, body],
        )?;
        if affected == 0 {
            bail!("unknown execution: {execution_id}");
        }
        Ok(())
    }

    /// Read the PR body snapshot captured at run start, or `None` when no
    /// snapshot was taken (new-PR flow, fetch failure, pre-migration row).
    pub fn get_execution_pr_body_before(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let body: Option<String> = conn
            .query_row(
                "SELECT pr_body_before FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(body)
    }

    /// Snapshot the bound PR's title captured at run start, alongside
    /// [`Self::set_execution_pr_body_before`] — same call site
    /// (`execution_started`), same baseline semantics: `boss pr body`
    /// returns both so a worker doing read-modify-write on the description
    /// can see the title without a `gh pr view` round trip.
    pub fn set_execution_pr_title_before(&self, execution_id: &str, title: &str) -> Result<()> {
        let conn = self.connect()?;
        let affected = conn.execute(
            "UPDATE work_executions SET pr_title_before = ?2 WHERE id = ?1",
            params![execution_id, title],
        )?;
        if affected == 0 {
            bail!("unknown execution: {execution_id}");
        }
        Ok(())
    }

    /// Read the PR title snapshot captured at run start, or `None` when no
    /// snapshot was taken (new-PR flow, fetch failure, pre-migration row).
    pub fn get_execution_pr_title_before(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let title: Option<String> = conn
            .query_row(
                "SELECT pr_title_before FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(title)
    }

    /// Stamp the "metadata delta observed at a clean Stop boundary" marker
    /// for the metadata-only CI-fix finalize gate (issue #1252). Set ONLY
    /// by the on-Stop handler — never the merge poller — so it is positive
    /// evidence that the worker reached a real Stop boundary (a dead /
    /// cut-off worker emits no Stop hook and so never gets marked) AND
    /// produced an operator-visible PR-metadata change. The merge poller
    /// gates its metadata-only finalize on this marker plus green CI,
    /// which is what prevents the #1262 regression (finalizing a worker
    /// that contributed nothing). Idempotent.
    pub fn mark_execution_metadata_fix_confirmed(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let affected = conn.execute(
            "UPDATE work_executions SET metadata_fix_confirmed_at = ?2 WHERE id = ?1",
            params![execution_id, now_string()],
        )?;
        if affected == 0 {
            bail!("unknown execution: {execution_id}");
        }
        Ok(())
    }

    /// Whether the metadata-only CI-fix marker has been stamped on this
    /// execution (see [`Self::mark_execution_metadata_fix_confirmed`]).
    /// The merge poller consults this before finalizing a revision whose
    /// bound PR head never moved.
    pub fn execution_metadata_fix_confirmed(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let at: Option<String> = conn
            .query_row(
                "SELECT metadata_fix_confirmed_at FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(at.map(|s| !s.is_empty()).unwrap_or(false))
    }

    pub fn fail_execution_start(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
    ) -> Result<(WorkExecution, WorkRun)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if !execution.status.can_begin_run() {
            bail!(
                "execution {execution_id} is not ready and cannot fail startup from status `{}`",
                execution.status
            );
        }
        let from_status = execution.status.clone();

        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'failed',
                 cube_repo_id = COALESCE(?2, cube_repo_id),
                 cube_lease_id = NULL,
                 cube_workspace_id = NULL,
                 workspace_path = NULL,
                 started_at = COALESCE(started_at, ?3),
                 finished_at = ?3
             WHERE id = ?1",
            params![execution_id, cube_repo_id, now],
        )?;

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, 'failed', ?4, NULL, NULL, NULL, ?5, ?5, ?5)",
            params![run_id, execution_id, agent_id, error_text, now],
        )?;

        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let run = query_run(&tx, &run_id)?.with_context(|| format!("missing run after insert: {run_id}"))?;
        // Canonical terminalization trace — see `mark_execution_orphaned`.
        tracing::warn!(
            execution_id = %execution_id,
            work_item_id = %execution.work_item_id,
            from_status = %from_status,
            to_status = %execution.status,
            reason = %error_text,
            "execution terminalized: fail start",
        );
        let mut pending = PendingEvents::new();
        stage_execution_terminal(&mut pending, &tx, execution_id, &execution.work_item_id)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok((execution, run))
    }

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
    pub fn record_pre_start_failure(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
        retry_delays: &[Duration],
    ) -> Result<(WorkExecution, Option<WorkRun>, PreStartFailureOutcome)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
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

        let (outcome, run_id) = if new_count <= max_retries {
            let delay = retry_delays[(new_count - 1) as usize];
            let dispatch_not_before =
                (boss_engine_utils::epoch_time::now_epoch_secs() as u64 + delay.as_secs()).to_string();
            tx.execute(
                "UPDATE work_executions
                 SET status = 'ready',
                     pre_start_failure_count = ?2,
                     cube_repo_id = COALESCE(?3, cube_repo_id),
                     cube_lease_id = NULL,
                     cube_workspace_id = NULL,
                     workspace_path = NULL,
                     started_at = NULL,
                     finished_at = NULL,
                     dispatch_not_before = ?4
                 WHERE id = ?1
                   AND status IN ('ready', 'claimed')",
                params![execution_id, new_count, cube_repo_id, dispatch_not_before],
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
                     cube_lease_id = NULL,
                     cube_workspace_id = NULL,
                     workspace_path = NULL,
                     started_at = COALESCE(started_at, ?4),
                     finished_at = ?4
                 WHERE id = ?1
                   AND status IN ('ready', 'claimed')",
                params![execution_id, new_count, cube_repo_id, now],
            )?;
            (PreStartFailureOutcome::PermanentFail, Some(run_id))
        };

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
        Ok((execution, run, outcome))
    }

    pub fn finish_execution_run(
        &self,
        input: FinishExecutionRunInput,
    ) -> Result<(WorkExecution, WorkRun, Option<WorkAttentionItem>)> {
        let FinishExecutionRunInput {
            execution_id,
            run_id,
            execution_status,
            run_status,
            result_summary,
            error_text,
            clear_workspace_lease,
            increment_pre_start_failure_count,
            attention,
        } = input;
        // Re-borrow the owned fields as the &str/Option<&str> shapes the
        // transaction body was written against, so the SQL below is unchanged.
        let execution_id = execution_id.as_str();
        let run_id = run_id.as_str();
        let run_status = run_status.as_str();
        let result_summary = result_summary.as_deref();
        let error_text = error_text.as_deref();

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status != ExecutionStatus::Running {
            bail!(
                "execution {execution_id} is not running and cannot finish a run from status `{}`",
                execution.status
            );
        }

        let run = query_run(&tx, run_id).require("run", run_id)?;
        if run.execution_id != execution_id {
            bail!("run {run_id} does not belong to execution {execution_id}");
        }
        if run.status != "active" {
            bail!(
                "run {run_id} is not active and cannot be finished from status `{}`",
                run.status
            );
        }

        let now = now_string();
        let execution_finished_at = if execution_status.is_terminal() {
            Some(now.as_str())
        } else {
            None
        };
        let normalized_result_summary = normalize_optional_text(result_summary.map(str::to_owned));
        let normalized_error_text = normalize_optional_text(error_text.map(str::to_owned));

        tx.execute(
            "UPDATE work_executions
             SET status = ?2,
                 cube_lease_id = CASE WHEN ?3 THEN NULL ELSE cube_lease_id END,
                 cube_workspace_id = CASE WHEN ?3 THEN NULL ELSE cube_workspace_id END,
                 workspace_path = CASE WHEN ?3 THEN NULL ELSE workspace_path END,
                 finished_at = ?4,
                 pre_start_failure_count = pre_start_failure_count + CASE WHEN ?5 THEN 1 ELSE 0 END
             WHERE id = ?1",
            params![
                execution_id,
                execution_status.as_str(),
                clear_workspace_lease,
                execution_finished_at,
                increment_pre_start_failure_count,
            ],
        )?;

        tx.execute(
            "UPDATE work_runs
             SET status = ?2,
                 error_text = ?3,
                 result_summary = ?4,
                 finished_at = ?5
             WHERE id = ?1",
            params![
                run_id,
                run_status,
                normalized_error_text,
                normalized_result_summary,
                now,
            ],
        )?;

        let attention_item = if let Some(input) = attention {
            // `finish_execution_run` only ever attaches to the
            // execution it just finished. The caller threading a
            // `work_item_id` instead is a bug — the work-item-scoped
            // attention path goes through `create_attention_item`.
            if input.work_item_id.is_some() {
                bail!(
                    "finish_execution_run attention payload must not set work_item_id (got {:?})",
                    input.work_item_id
                );
            }
            let provided = input.execution_id.as_deref().unwrap_or(execution_id);
            if provided != execution_id {
                bail!("attention item execution `{provided}` does not match finished execution `{execution_id}`",);
            }

            // Routed through `insert_attention_item_row` (rather than a
            // bespoke raw INSERT) so this filer gets the same
            // `warn_if_lifecycle_undeclared` coverage and `last_raised_at`
            // stamping as every other path — see `work/attention_filing.rs`.
            let resolved_input = CreateAttentionItemInput {
                execution_id: Some(execution_id.to_owned()),
                ..input
            };
            Some(crate::work::workitems::insert_attention_item_row(&tx, &resolved_input)?)
        } else {
            None
        };

        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let run = query_run(&tx, run_id).require("run", run_id)?;
        let mut pending = PendingEvents::new();
        if execution_status.is_terminal() {
            // Canonical terminalization trace — see `mark_execution_orphaned`.
            tracing::warn!(
                execution_id = %execution_id,
                work_item_id = %execution.work_item_id,
                from_status = %ExecutionStatus::Running,
                to_status = %execution_status,
                reason = "finish_execution_run",
                "execution terminalized: run finish",
            );
            stage_execution_terminal(&mut pending, &tx, execution_id, &execution.work_item_id)?;
        }
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok((execution, run, attention_item))
    }

    /// Unconditionally drive a pane-parked execution (`running` or
    /// `waiting_human`) to the `completed` terminal status, for a Stop-hook
    /// finalizer whose worker's real decision (a triage marker, an
    /// answer-agent reply) is resolved well after the pane's *spawn* already
    /// completed.
    ///
    /// Why this exists instead of routing through [`Self::finish_execution_run`]
    /// via [`Self::active_run_ids_for_execution`]: `PaneSpawnRunner` records
    /// the spawn itself as the run's completion the instant the pane comes up
    /// — leaving the execution live in `running` with the run's `finished_at`
    /// already stamped (see `RunWaitState::WorkerPaneAlive`). By the time the
    /// worker's actual Stop hook fires, `work_runs` normally has no row with
    /// `finished_at IS NULL` left to find, so a finalizer that only closes
    /// "active" runs is a silent no-op — the execution stays live forever,
    /// and the pane-death sweep later "reconciles" it a second time with a
    /// misleading pane-died detail (the double-finalize this closes). This
    /// instead closes any run that happens to still be open (the rarer
    /// multi-turn/nudge case) and unconditionally transitions the execution
    /// row itself, both in one transaction.
    ///
    /// Idempotent: returns `Ok(None)` without writing anything if the
    /// execution is already terminal, so it is always safe to call from a
    /// hook handler that may fire more than once, or race a concurrent
    /// reconciler.
    pub fn complete_pane_parked_execution(
        &self,
        execution_id: &str,
        run_status: &str,
        result_summary: Option<&str>,
    ) -> Result<Option<WorkExecution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status.is_terminal() {
            return Ok(None);
        }

        let now = now_string();
        let normalized_summary = normalize_optional_text(result_summary.map(str::to_owned));
        tx.execute(
            "UPDATE work_runs
             SET status = ?2,
                 result_summary = COALESCE(?3, result_summary),
                 finished_at = COALESCE(finished_at, ?4)
             WHERE execution_id = ?1
               AND finished_at IS NULL",
            params![execution_id, run_status, normalized_summary, now],
        )?;
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

        let updated = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let mut pending = PendingEvents::new();
        stage_execution_terminal(&mut pending, &tx, execution_id, &updated.work_item_id)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok(Some(updated))
    }

    /// Mirror a live worker's own awaiting-input signal onto its stored
    /// status: `running` → `waiting_human` when `awaiting` is true, and
    /// `waiting_human` → `running` when it is false.
    ///
    /// This is the ONLY writer of `waiting_human` for a pane-hosted worker,
    /// and the only clearer of it — called from `awaiting_input_status`'s
    /// two producers (the hook-dispatch mirror and the stalled-spawn sweep
    /// mirror), never directly by a runner or a completion path. The
    /// signal driving it — the driver's own awaiting-input notification,
    /// surfaced as [`boss_protocol::WorkerActivity::WaitingForInput`] — is
    /// the pane's positive evidence that a worker is blocked on a human,
    /// not an inference from tool-call timing.
    ///
    /// Deliberately narrow, and expressed as a status-guarded `UPDATE` so
    /// the guard is applied by the same statement that writes:
    ///
    /// - Only the `running` ⇄ `waiting_human` pair is ever touched. A
    ///   terminal row, a `waiting_review`/`waiting_merge` row, or a row
    ///   still pre-dispatch (`queued`/`ready`/`waiting_dependency`) is left
    ///   exactly as it is — those states are owned by the completion and
    ///   dispatch paths, and a late/duplicate hook must never drag one back
    ///   into a live status.
    /// - Idempotent: re-asserting the state the row is already in matches no
    ///   rows and returns `Ok(None)`, so a repeated `Notification` (or a
    ///   `Stop` that merely re-affirms a pending one) writes nothing.
    ///
    /// Returns `Ok(Some(execution))` with the post-write row when the
    /// transition actually fired, `Ok(None)` when it was a no-op.
    pub fn set_execution_awaiting_human(&self, execution_id: &str, awaiting: bool) -> Result<Option<WorkExecution>> {
        let (from, to) = if awaiting {
            (ExecutionStatus::Running, ExecutionStatus::WaitingHuman)
        } else {
            (ExecutionStatus::WaitingHuman, ExecutionStatus::Running)
        };
        let conn = self.connect()?;
        let rows_changed = conn.execute(
            "UPDATE work_executions
             SET status = ?2
             WHERE id = ?1
               AND status = ?3",
            params![execution_id, to.as_str(), from.as_str()],
        )?;
        if rows_changed == 0 {
            return Ok(None);
        }
        Ok(Some(
            query_execution(&conn, execution_id).require("execution", execution_id)?,
        ))
    }

    /// Unconditionally drive a pane-parked execution (`running` or
    /// `waiting_human`) to the `failed` terminal status and release its cube
    /// lease + workspace.
    ///
    /// For a driver that reported its own terminal turn boundary as an
    /// unrecoverable error (Codex's `task_complete.error`, or a stdout
    /// `turn.failed` / fatal `error` envelope) — the process already exited
    /// on its own, so there is nothing left to nudge or wait on. Unlike
    /// [`Self::mark_execution_orphaned`] (which deliberately keeps the lease
    /// so a still-possibly-alive worker's in-flight commits aren't
    /// abandoned), this is for a worker the driver has already told us is
    /// definitively done: releasing the lease here is correct, not
    /// premature.
    ///
    /// `error_text` becomes the terminal `work_runs.error_text` /
    /// `result_summary` so the run history carries the provider's own
    /// diagnostic, not just "failed".
    ///
    /// Idempotent: returns `Ok(None)` without writing anything if the
    /// execution is already terminal, mirroring
    /// [`Self::complete_pane_parked_execution`].
    pub fn fail_pane_parked_execution(&self, execution_id: &str, error_text: &str) -> Result<Option<WorkExecution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if existing.status.is_terminal() {
            return Ok(None);
        }

        let now = now_string();
        let normalized_error = normalize_optional_text(Some(error_text.to_owned()));
        tx.execute(
            "UPDATE work_runs
             SET status = 'failed',
                 error_text = COALESCE(?2, error_text),
                 result_summary = COALESCE(?2, result_summary),
                 finished_at = COALESCE(finished_at, ?3)
             WHERE execution_id = ?1
               AND finished_at IS NULL",
            params![execution_id, normalized_error, now],
        )?;
        tx.execute(
            "UPDATE work_executions
             SET status = 'failed',
                 cube_lease_id = NULL,
                 cube_workspace_id = NULL,
                 workspace_path = NULL,
                 finished_at = ?2
             WHERE id = ?1",
            params![execution_id, now],
        )?;

        let updated = query_execution(&tx, execution_id).require("execution", execution_id)?;
        // Canonical terminalization trace — see `mark_execution_orphaned`.
        tracing::warn!(
            execution_id = %execution_id,
            work_item_id = %updated.work_item_id,
            from_status = %existing.status,
            to_status = %updated.status,
            reason = %error_text,
            "execution terminalized: driver-reported fatal error",
        );
        let mut pending = PendingEvents::new();
        stage_execution_terminal(&mut pending, &tx, execution_id, &updated.work_item_id)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok(Some(updated))
    }
}

#[cfg(test)]
mod event_bus_tests {
    use boss_event_bus::TopicFilter;

    use super::*;
    use crate::test_support::{create_ready_chore_execution, create_test_product, open_db};
    use crate::work::CreateChoreInput;

    fn ready_execution(db: &WorkDb) -> WorkExecution {
        let product = create_test_product(db);
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id)
                    .name("test chore")
                    .build(),
            )
            .unwrap();
        create_ready_chore_execution(db, chore.id)
    }

    #[test]
    fn launch_config_is_frozen_per_execution_without_backfilling_unrecorded_rows() {
        let (_dir, db) = open_db();
        let execution = ready_execution(&db);

        assert_eq!(execution.driver, None, "newly queued work has not launched yet");
        let recorded = db
            .record_execution_launch_config(&execution.id, "codex", "gpt-5.5-codex", Some(EffortLevel::Large))
            .unwrap();
        assert_eq!(recorded.driver.as_deref(), Some("codex"));
        assert_eq!(recorded.model.as_deref(), Some("gpt-5.5-codex"));
        assert_eq!(recorded.effort_level, Some(EffortLevel::Large));

        let reloaded = db
            .record_execution_launch_config(&execution.id, "claude", "opus", Some(EffortLevel::Small))
            .unwrap();
        assert_eq!(reloaded.driver.as_deref(), Some("codex"));
        assert_eq!(reloaded.model.as_deref(), Some("gpt-5.5-codex"));
        assert_eq!(reloaded.effort_level, Some(EffortLevel::Large));
    }

    #[tokio::test]
    async fn cancel_execution_publishes_execution_terminal() {
        let (_dir, db) = open_db();
        let execution = ready_execution(&db);
        let mut sub = db
            .event_bus()
            .subscribe(TopicFilter::kind(boss_event_bus::EventKind::ExecutionTerminal));

        db.cancel_execution(&execution.id).unwrap();

        let event = sub.recv().await.expect("ExecutionTerminal published after cancel");
        assert_eq!(
            event,
            Event::ExecutionTerminal {
                execution_id: execution.id.clone(),
                task_id: execution.work_item_id.clone(),
                host_id: "local".to_owned(),
                pool_claim: None,
            }
        );
    }

    #[tokio::test]
    async fn mark_execution_orphaned_publishes_execution_terminal() {
        let (_dir, db) = open_db();
        let execution = ready_execution(&db);
        let mut sub = db
            .event_bus()
            .subscribe(TopicFilter::kind(boss_event_bus::EventKind::ExecutionTerminal));

        db.mark_execution_orphaned(&execution.id, "worker pane died").unwrap();

        let event = sub.recv().await.expect("ExecutionTerminal published after orphan reap");
        assert_eq!(
            event,
            Event::ExecutionTerminal {
                execution_id: execution.id.clone(),
                task_id: execution.work_item_id.clone(),
                host_id: "local".to_owned(),
                pool_claim: None,
            }
        );
    }

    #[tokio::test]
    async fn cancel_execution_does_not_publish_on_error() {
        // Cancelling an already-terminal execution bails before ever
        // touching the DB, so this test covers only the reject-before-any-write
        // path: a failed call must not leave a stray event on the bus. It does
        // NOT exercise "a staged event is dropped when the enclosing
        // transaction rolls back after staging" for a real `work/` producer —
        // that guarantee is covered generically (against a synthetic
        // producer) by `event_publish.rs`'s own `drops_events_when_commit_fails`
        // / `drops_events_when_commit_never_runs` tests.
        let (_dir, db) = open_db();
        let execution = ready_execution(&db);
        db.mark_execution_orphaned(&execution.id, "reap").unwrap();

        let mut sub = db
            .event_bus()
            .subscribe(TopicFilter::kind(boss_event_bus::EventKind::ExecutionTerminal));
        assert!(
            db.cancel_execution(&execution.id).is_err(),
            "cancelling a terminal execution must fail"
        );

        // Give any errant publish a chance to land before asserting absence.
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv())
            .await
            .expect_err("no ExecutionTerminal should be published when cancel_execution errors");
    }

    /// A run that has not delivered a turn boundary reads back as `None`.
    #[test]
    fn turn_boundary_is_absent_until_one_is_recorded() {
        let (_dir, db) = open_db();
        let execution = ready_execution(&db);
        db.start_execution_run(&execution.id, "agent", "repo", "lease", "ws", "/tmp/ws")
            .unwrap();

        assert_eq!(db.latest_run_turn_boundary_for_execution(&execution.id).unwrap(), None);
    }

    /// Last-write-wins: a driver that serves several turns from one process
    /// leaves the newest boundary, which is the one that matters.
    #[test]
    fn recording_a_turn_boundary_round_trips_and_overwrites() {
        let (_dir, db) = open_db();
        let execution = ready_execution(&db);
        db.start_execution_run(&execution.id, "agent", "repo", "lease", "ws", "/tmp/ws")
            .unwrap();

        assert!(
            db.record_run_turn_boundary_for_execution(&execution.id, "2026-07-28T00:10:00Z")
                .unwrap()
        );
        assert_eq!(
            db.latest_run_turn_boundary_for_execution(&execution.id).unwrap(),
            Some("2026-07-28T00:10:00Z".to_owned()),
        );

        db.record_run_turn_boundary_for_execution(&execution.id, "2026-07-28T00:16:58Z")
            .unwrap();
        assert_eq!(
            db.latest_run_turn_boundary_for_execution(&execution.id).unwrap(),
            Some("2026-07-28T00:16:58Z".to_owned()),
        );
    }

    /// A boundary reported before any run row exists cannot be attributed to a
    /// process, so it is dropped rather than misfiled — reported as `false`,
    /// not an error, since a hook racing the run insert is benign.
    #[test]
    fn recording_a_turn_boundary_with_no_run_row_is_a_benign_no_op() {
        let (_dir, db) = open_db();
        let execution = ready_execution(&db);

        assert!(
            !db.record_run_turn_boundary_for_execution(&execution.id, "2026-07-28T00:16:58Z")
                .unwrap()
        );
        assert_eq!(db.latest_run_turn_boundary_for_execution(&execution.id).unwrap(), None);
    }
}
