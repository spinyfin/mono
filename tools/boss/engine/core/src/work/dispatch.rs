use super::*;

impl WorkDb {
    /// Returns or creates a ready execution for `work_item_id`, applying any
    /// priority / preferred-workspace overrides from the request.
    ///
    /// Friendly ids (`T3`, `P7`) are resolved to primary ids before any other
    /// processing, so callers do not need to pre-resolve them.
    ///
    /// If the most recent execution for this work item is still in flight
    /// (`ready` / `running` / `waiting_*`) we update its priority and
    /// preferred_workspace_id rather than creating a duplicate. If it is
    /// terminal (or absent), we insert a fresh `ready` execution.
    pub fn request_execution(&self, input: RequestExecutionInput) -> Result<WorkExecution> {
        // No live-worker oracle → assume every non-terminal execution
        // is genuinely live (the historical behaviour, kept for tests
        // that don't stand up the live registry).
        self.request_execution_with_live_check(input, |_| true)
    }

    /// Same as `request_execution`, but the caller supplies a
    /// predicate that says whether the execution id named by an
    /// existing non-terminal row corresponds to a worker that is
    /// **actually live** in the engine's slot registry. When the
    /// predicate returns `false` we treat the existing execution as
    /// stale (mark it `abandoned`, finished now) and create a fresh
    /// `ready` execution. This is what lets a kanban drag-to-Doing
    /// re-dispatch a chore whose previous worker died with the app
    /// before reaching `done`.
    ///
    /// Idempotency contract:
    /// - existing execution terminal or absent → insert new `ready`,
    /// - existing non-terminal AND predicate returns `true` → no-op
    ///   (just refresh priority / preferred_workspace_id, same as
    ///   before),
    /// - existing non-terminal AND predicate returns `false` → mark
    ///   existing `abandoned`, insert new `ready`.
    pub fn request_execution_with_live_check<F: FnOnce(&str) -> bool>(
        &self,
        mut input: RequestExecutionInput,
        is_live: F,
    ) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        // Resolve T42 / P7 friendly ids to primary ids before any other check,
        // so callers like `bossctl work start T3` work without client-side
        // resolution. Primary ids (task_*, proj_*, prod_*) pass through unchanged.
        if let Some(resolved) = resolve_friendly_work_item_id(&conn, &input.work_item_id)? {
            input.work_item_id = resolved;
        }
        ensure_dispatch_repo_resolvable(&mut conn, &input.work_item_id)?;
        let tx = conn.transaction()?;
        let execution = request_execution_in_tx_with_live_check(&tx, input, is_live)?;
        tx.commit()?;
        Ok(execution)
    }

    /// Re-fire the automated review pipeline for `work_item_id`'s
    /// currently-open PR by enqueuing a fresh `pr_review` execution.
    ///
    /// Accepts a friendly id (`T3`) or a primary `task_…` id. The single
    /// dispatch path shared by the dead-review auto-recovery sweep
    /// ([`crate::pr_review_recovery`]) and the operator-facing `bossctl
    /// review start --pr <n>` verb — see
    /// [`dispatch_helpers::request_pr_review_in_tx`] for the full refusal
    /// conditions and idempotency contract.
    pub fn request_pr_review(&self, work_item_id: &str, pr_checker: &dyn PrStateChecker) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let resolved = resolve_friendly_work_item_id(&conn, work_item_id)?.unwrap_or_else(|| work_item_id.to_owned());
        let tx = conn.transaction()?;
        let execution = request_pr_review_in_tx(&tx, &resolved, pr_checker)?;
        tx.commit()?;
        Ok(execution)
    }

    /// Idempotently enqueue a `pr_review` execution for `work_item_id`, from
    /// the automatic reviewer-enqueue path in `finalize_pr_transition`.
    ///
    /// Unlike [`Self::request_pr_review`], this does not probe GitHub or
    /// refuse when another execution is live for the item — the caller (mid
    /// PR-completion finalisation, already holding a freshly-detected open
    /// PR) is itself the producing execution's completion, so a fresh
    /// live-execution check would spuriously trip on that very execution.
    /// It exists solely to make "does a non-terminal `pr_review` execution
    /// already exist for this item" and "insert one" atomic (T366): two
    /// independent PR-completion triggers (the Stop-hook path and the
    /// merge-poller's `pr_recheck` sweep) can each reach the reviewer-
    /// enqueue check around the same moment, before either has recorded its
    /// completion — without this atomicity both would insert a `pr_review`
    /// execution for the same unchanged head sha, producing two independent
    /// reviews and two duplicate findings revisions from a single push.
    ///
    /// The `Immediate` transaction acquires sqlite's write lock at `BEGIN`,
    /// so a concurrent caller blocks (up to the configured `busy_timeout`)
    /// until this one commits, instead of racing the same check-then-insert.
    ///
    /// Returns `(execution, is_new)`; `is_new = false` means an existing
    /// live/non-terminal `pr_review` execution was reused rather than a
    /// duplicate being created.
    pub fn create_pr_review_execution_dedup(
        &self,
        work_item_id: &str,
        repo_remote_url: &str,
    ) -> Result<(WorkExecution, bool)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = existing_nonterminal_pr_review_execution(&tx, work_item_id)? {
            tx.commit()?;
            return Ok((existing, false));
        }
        let execution = insert_execution(
            &tx,
            CreateExecutionInput::builder()
                .work_item_id(work_item_id.to_owned())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(repo_remote_url.to_owned())
                .build(),
        )?;
        tx.commit()?;
        Ok((execution, true))
    }

    /// Repo-resolution precheck that does not create or mutate any
    /// `work_executions` row. The kanban drag-to-Doing path calls this
    /// before flipping `tasks.status = 'active'` so a deterministic
    /// dispatch failure (no product default repo, no per-task
    /// override) rejects the `UpdateWorkItem` instead of leaving the
    /// card stuck in Doing with no worker (bug #679). Shares the same
    /// error text and sticky attention item that the request-execution
    /// path writes, so the kanban Attention lane sees the same shape
    /// regardless of which trigger surfaced the problem.
    pub fn precheck_dispatch_repo(&self, work_item_id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let resolved = resolve_friendly_work_item_id(&conn, work_item_id)?.unwrap_or_else(|| work_item_id.to_owned());
        ensure_dispatch_repo_resolvable(&mut conn, &resolved)
    }

    /// Demote `tasks.status = 'active'` rows that never made it past
    /// dispatch — i.e., no `work_runs` row was ever recorded for any
    /// of the work item's executions — back to `todo`. Any non-terminal
    /// executions on those work items are stamped `abandoned` in the
    /// same transaction so the dispatcher won't pick them up after the
    /// demote.
    ///
    /// This is the boot-time "ghost active" sweep: a chore can land in
    /// `tasks.status = 'active'` without ever spawning a worker if the
    /// previous engine crashed between flipping the kanban status and
    /// claiming a slot, or if a `RequestExecution` raced ahead of the
    /// dispatcher and no slot was free. The Doing column should not
    /// show those — they have no run history and should fall back to
    /// the To-Do lane so the human can retry.
    ///
    /// Demotion also stamps `last_status_actor = 'engine'` so the
    /// kanban surface can distinguish the engine's auto-demote from a
    /// human drag, and returns the per-row `product_id` so the caller
    /// can publish a work-item-changed event on the product's topic —
    /// without that event the UI keeps showing the card in Doing
    /// until the next manual refetch.
    ///
    /// Returns one [`HealedGhostActive`] per demoted row. Items whose
    /// executions already produced a run (active worker that crashed,
    /// terminated cleanly, or is still executing) are left alone —
    /// `reconcile_active_dispatch` handles those via re-dispatch.
    pub fn heal_ghost_active_chores(&self) -> Result<Vec<HealedGhostActive>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let candidates: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT t.id, t.product_id FROM tasks t
                 WHERE t.status = 'active'
                   AND t.deleted_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM work_runs wr
                       JOIN work_executions we ON wr.execution_id = we.id
                       WHERE we.work_item_id = t.id
                   )",
            )?;
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut healed = Vec::new();
        let now = now_string();
        for (work_item_id, product_id) in candidates {
            // Abandon any non-terminal executions so they don't get
            // picked up by the dispatcher after the demote. Terminal
            // executions are left alone — they're already settled.
            tx.execute(
                "UPDATE work_executions
                 SET status = 'abandoned',
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE work_item_id = ?1
                   AND status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')",
                params![work_item_id, now],
            )?;
            // Demote the kanban status. Use a guarded update so we
            // don't race a concurrent move to `done`/`archived`.
            // Stamps `last_status_actor = 'engine'` so the kanban can
            // render "demoted by engine: dispatch never reached a
            // worker" instead of attributing the move to the human who
            // last touched the row.
            let updated = tx.execute(
                "UPDATE tasks
                 SET status = 'todo',
                     last_status_actor = 'engine',
                     updated_at = ?2
                 WHERE id = ?1
                   AND status = 'active'
                   AND deleted_at IS NULL",
                params![work_item_id, now],
            )?;
            if updated > 0 {
                healed.push(HealedGhostActive {
                    work_item_id,
                    product_id,
                });
            }
        }
        tx.commit()?;
        Ok(healed)
    }

    /// Reconciliation sweep: abandon any `queued` / `ready` /
    /// `waiting_dependency` execution whose work item is a task in a
    /// terminal status (`done` / `archived` / `cancelled`) or soft-deleted.
    /// A dispatchable execution has no business existing against a closed
    /// row — it can never be picked up (the dispatcher only surfaces
    /// non-terminal work items) and just confuses `bossctl agents list` /
    /// `boss chore show` with a phantom pending run.
    ///
    /// This is the startup-time backstop for the create-time guards in
    /// `reconcile_revision_execution`, `block_pending_revisions_on_parent_close`,
    /// and `request_execution_in_tx_with_live_check`: those stop *new*
    /// stranded executions from being created, this cleans up any that
    /// slipped through before the fix shipped (or from a future regression).
    /// Run alongside [`Self::heal_ghost_active_chores`] at engine startup.
    pub fn abandon_stranded_executions_on_closed_work_items(&self) -> Result<Vec<AbandonedStrandedExecution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let candidates: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT we.id, we.work_item_id
                 FROM work_executions we
                 JOIN tasks t ON t.id = we.work_item_id
                 WHERE we.status IN ('queued', 'ready', 'waiting_dependency')
                   AND (t.status IN ('done', 'archived', 'cancelled') OR t.deleted_at IS NOT NULL)",
            )?;
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let now = now_string();
        let mut abandoned = Vec::new();
        for (execution_id, work_item_id) in candidates {
            let updated = tx.execute(
                "UPDATE work_executions
                 SET status = 'abandoned',
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE id = ?1
                   AND status IN ('queued', 'ready', 'waiting_dependency')",
                params![execution_id, now],
            )?;
            if updated > 0 {
                abandoned.push(AbandonedStrandedExecution {
                    execution_id,
                    work_item_id,
                });
            }
        }
        tx.commit()?;
        Ok(abandoned)
    }

    /// Demote a single `active` work item back to `todo` after its
    /// dispatch failed before a worker ever came up (e.g. the worker
    /// pane could not be spawned because no app session was registered,
    /// libghostty IPC dropped, or the slot was busy). Without this the
    /// card is stranded in the Doing column behind a dead execution and
    /// the orphan-active sweep keeps re-dispatching the same doomed
    /// spawn every cycle. Demoting it surfaces the failure as a return
    /// to To-Do so the human can retry deliberately.
    ///
    /// Guarded on `status = 'active'` so a concurrent move to
    /// `done`/`archived`/`blocked` is never stomped. Stamps
    /// `last_status_actor = 'engine'` (same as `heal_ghost_active_chores`)
    /// so the kanban attributes the demote to the engine, not the human
    /// who last touched the row. Returns `true` if a row was demoted.
    pub fn demote_active_work_item_to_todo(&self, work_item_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_string();
        let updated = conn.execute(
            "UPDATE tasks
             SET status = 'todo',
                 last_status_actor = 'engine',
                 updated_at = ?2
             WHERE id = ?1
               AND status = 'active'
               AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        Ok(updated > 0)
    }

    /// Bounce a work item to Backlog after a **pre-start** dispatch
    /// attempt (cube repo ensure, workspace lease, change create, run
    /// start, …) is determined non-transient — the moment
    /// `record_pre_start_failure` returns `PermanentFail` and gives up
    /// retrying. Distinct from [`Self::demote_active_work_item_to_todo`],
    /// which handles a *post*-start pane-spawn failure on an already-`active`
    /// row: a pre-start failure can strike while the task is still `todo`
    /// (it never made it to `active`), so this also accepts that status.
    ///
    /// This closes the "failing to start" vs. "waiting for a slot"
    /// ambiguity (T2130-adjacent incident): a dispatch that keeps losing
    /// the pool-claim race never reaches this method (pool exhaustion is a
    /// transient capacity wait, handled entirely by the ordinary re-scan —
    /// see `pool_exhaustion_recovers_automatically_when_slot_frees_without_manual_intervention`),
    /// but a dispatch that repeatedly fails at a concrete pre-run stage
    /// (e.g. cube's `refusing to move backwards` lease error) would
    /// otherwise loop claim → fail → release → re-queue forever while
    /// `tasks.status` never changes. This method stops that loop by:
    ///
    /// - clearing `autostart` (single-shot, mirroring the clear
    ///   `start_execution_run_on_host` does on a successful start) so the
    ///   card renders as parked in Backlog rather than "waiting for a
    ///   slot" — the loop is over; a human must retry deliberately, and
    ///   [`Self::request_execution_with_live_check`] clears the fields
    ///   below the next time that happens,
    /// - stamping `dispatch_failed_reason` / `dispatch_failed_error` /
    ///   `dispatch_failed_at` so the kanban card can render the failure
    ///   and its underlying error inline, without a trip through
    ///   dispatch-events logs.
    ///
    /// Guarded on `status IN ('todo', 'active')` so a concurrent move to
    /// `done`/`archived`/`blocked`/`in_review` is never stomped — this
    /// also naturally excludes review-phase dispatch kinds (`pr_review`,
    /// `ci_remediation`, `conflict_resolution`), which run against tasks
    /// in those other statuses and must not be bounced back to Backlog
    /// (that would erase review context). Returns `true` iff a row was
    /// actually updated.
    pub fn bounce_dispatch_failed_to_backlog(
        &self,
        work_item_id: &str,
        reason: &str,
        error_text: &str,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_string();
        let updated = conn.execute(
            "UPDATE tasks
             SET status = 'todo',
                 autostart = 0,
                 last_status_actor = 'engine',
                 dispatch_failed_reason = ?2,
                 dispatch_failed_error = ?3,
                 dispatch_failed_at = ?4,
                 updated_at = ?4
             WHERE id = ?1
               AND status IN ('todo', 'active')
               AND deleted_at IS NULL",
            params![work_item_id, reason, error_text, now],
        )?;
        Ok(updated > 0)
    }

    /// Work items previously bounced to Backlog by
    /// [`Self::bounce_dispatch_failed_to_backlog`] (a pre-spawn dispatch
    /// failure exhausted its immediate retries), still schedulable, and
    /// whose `dispatch_failed_at` is older than `min_age_secs` — the
    /// candidate set for [`crate::dispatch_failure_recovery_sweep`].
    /// Mirrors [`Self::list_orphan_active_candidates`]'s shape for the
    /// pre-spawn side of the reconciliation story.
    pub fn list_dispatch_failed_recovery_candidates(&self, min_age_secs: i64) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let now_secs: i64 = crate::epoch_time::now_epoch_secs();
        let cutoff = now_secs - min_age_secs;
        let mut stmt = conn.prepare(
            "SELECT id FROM tasks
             WHERE status IN ('todo', 'active')
               AND deleted_at IS NULL
               AND dispatch_failed_reason IS NOT NULL
               AND autostart = 0
               AND dispatch_failed_at IS NOT NULL
               AND CAST(dispatch_failed_at AS INTEGER) < ?1
             ORDER BY CAST(dispatch_failed_at AS INTEGER) ASC, id ASC",
        )?;
        let rows = stmt.query_map([cutoff], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Reverse of [`Self::bounce_dispatch_failed_to_backlog`]: re-enable
    /// `autostart` on a work item the engine previously parked after a
    /// pre-spawn dispatch failure, so the fresh `ready` execution the
    /// caller creates next renders as dispatch-pending again instead of
    /// silently sitting in Backlog. Guarded on `dispatch_failed_reason
    /// IS NOT NULL` so it's a no-op against a work item that isn't
    /// actually parked in this state (e.g. a race against a human's own
    /// retry). Returns `true` iff a row was updated.
    ///
    /// Callers that also call `request_execution` afterwards should
    /// prefer [`Self::reenable_and_request_execution_with_live_check`],
    /// which runs both in one transaction so a `request_execution`
    /// failure can't strand the item with `autostart` flipped back on
    /// but no fresh execution — see that method's docs.
    fn reenable_autostart_after_dispatch_failure_in_tx(conn: &Connection, work_item_id: &str) -> Result<bool> {
        let now = now_string();
        let updated = conn.execute(
            "UPDATE tasks
             SET autostart = 1,
                 last_status_actor = 'engine',
                 updated_at = ?2
             WHERE id = ?1
               AND status IN ('todo', 'active')
               AND deleted_at IS NULL
               AND dispatch_failed_reason IS NOT NULL",
            params![work_item_id, now],
        )?;
        Ok(updated > 0)
    }

    /// Atomic combination of [`Self::reenable_autostart_after_dispatch_failure_in_tx`]
    /// and [`Self::request_execution_with_live_check`], used by
    /// [`crate::dispatch_failure_recovery_sweep`] to give a work item
    /// bounced by [`Self::bounce_dispatch_failed_to_backlog`] another
    /// shot after its cooldown elapses.
    ///
    /// Both steps run in the *same* transaction. This matters because
    /// `request_execution_in_tx_with_live_check` can `bail!` (repo
    /// became unresolvable, a gating dependency reappeared, ...), and
    /// if the autostart re-enable had already been committed in an
    /// earlier, separate transaction, that failure would leave the
    /// item with `autostart = 1` but `dispatch_failed_reason` still
    /// set and no new execution — invisible to both
    /// `list_dispatch_failed_recovery_candidates` (requires
    /// `autostart = 0`) and `rescan_active_dispatch` (requires `status
    /// = 'active'`), i.e. permanently stranded. Running both in one
    /// transaction means a `request_execution` failure rolls the
    /// autostart flip back too, leaving the item exactly as it was
    /// before this call — still a valid candidate for the next sweep
    /// pass.
    ///
    /// Returns `Ok(None)` if the item was no longer eligible for
    /// re-enable (raced a human retry, or the row moved on) — nothing
    /// left to do. Returns `Ok(Some(execution))` on success.
    pub fn reenable_and_request_execution_with_live_check<F: FnOnce(&str) -> bool>(
        &self,
        work_item_id: &str,
        mut input: RequestExecutionInput,
        is_live: F,
    ) -> Result<Option<WorkExecution>> {
        let mut conn = self.connect()?;
        if let Some(resolved) = resolve_friendly_work_item_id(&conn, &input.work_item_id)? {
            input.work_item_id = resolved;
        }
        ensure_dispatch_repo_resolvable(&mut conn, &input.work_item_id)?;
        let tx = conn.transaction()?;
        if !Self::reenable_autostart_after_dispatch_failure_in_tx(&tx, work_item_id)? {
            tx.commit()?;
            return Ok(None);
        }
        let execution = request_execution_in_tx_with_live_check(&tx, input, is_live)?;
        tx.commit()?;
        Ok(Some(execution))
    }

    /// Re-issue `RequestExecution` for every non-deleted task / chore
    /// whose status is `active` but whose latest execution is terminal
    /// (or which has no execution). This is the engine-startup
    /// rehydration described in `work-kanban.md` §3 of the
    /// Doing-column dispatch contract: the kanban Doing column is
    /// supposed to mirror "running or queued," and after a crash the
    /// only remaining signal of "this was supposed to be running" is
    /// `tasks.status = 'active'`. Returns the work item ids that were
    /// re-dispatched so the caller can log them.
    ///
    /// `is_live` is the same predicate `request_execution_with_live_check`
    /// uses. Engine startup runs reconcile *before* any worker spawn
    /// could have happened, so the natural caller passes a closure that
    /// returns `false` for everything — every existing non-terminal
    /// execution is treated as stale and re-dispatched. Tests that
    /// don't stand up a live registry can pass `|_| true` to keep the
    /// pre-live-check semantics.
    pub fn reconcile_active_dispatch<F: Fn(&str) -> bool>(&self, is_live: F) -> Result<Vec<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        // Active, non-deleted task/chore rows are the candidate set.
        let candidate_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM tasks
                 WHERE status = 'active' AND deleted_at IS NULL",
            )?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut redispatched = Vec::new();
        for work_item_id in candidate_ids {
            // Decide whether this work item needs a fresh ready
            // execution. The candidate cases are:
            //   - no execution at all → yes,
            //   - latest execution terminal → yes,
            //   - latest execution non-terminal but `is_live`
            //     reports the slot is gone → yes (stale row).
            let existing = query_latest_execution_for_work_item(&tx, &work_item_id)?;
            let needs_dispatch = match &existing {
                Some(existing) => existing.status.is_terminal() || !is_live(&existing.id),
                None => true,
            };
            if !needs_dispatch {
                continue;
            }
            // When the predecessor was orphaned by the startup reaper
            // (worker pane died across the engine restart), default
            // the new ready row's `preferred_workspace_id` to the
            // orphan's `cube_workspace_id`. The orphan's workspace
            // typically still holds in-flight commits the human wants
            // resumed — without this hint the dispatcher would lease
            // any free workspace and the fresh worker would start
            // against `main` on an unrelated branch. Only fires for
            // orphaned predecessors; abandoned / failed / cancelled
            // ones are intentional throwaways and don't carry forward.
            // When the predecessor was orphaned, carry forward both its
            // workspace and the allow_dirty flag so the recovering worker
            // reclaims the dirty workspace in place (uncommitted WIP
            // intact) rather than cube resetting it or falling back to
            // a fresh workspace that has no patch.
            let is_orphaned_predecessor = existing
                .as_ref()
                .map(|prev| prev.status == ExecutionStatus::Orphaned)
                .unwrap_or(false);
            let preferred_workspace_id = existing
                .as_ref()
                .filter(|_| is_orphaned_predecessor)
                .and_then(|prev| prev.cube_workspace_id.clone());
            request_execution_in_tx_with_live_check(
                &tx,
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .maybe_preferred_workspace_id(preferred_workspace_id)
                    .allow_dirty(is_orphaned_predecessor)
                    .build(),
                |run_id| is_live(run_id),
            )?;
            redispatched.push(work_item_id);
        }
        tx.commit()?;
        Ok(redispatched)
    }

    /// Steady-state counterpart of [`Self::reconcile_active_dispatch`]
    /// used by the dispatcher when a worker frees up. Re-issues
    /// `RequestExecution` for every active task/chore whose latest
    /// execution is missing or terminal — i.e., the items the
    /// create-time dispatch couldn't place because the pool was full
    /// or whose worker died after the kanban moved them to `active`.
    ///
    /// Differs from `reconcile_active_dispatch` in three ways:
    ///
    /// 1. Honours the per-task `autostart` flag. Items with
    ///    `autostart=false` are deliberately parked in `active` until
    ///    a human resumes them — the on-free rescan must not
    ///    auto-restart them silently. The startup reconcile rehydrates
    ///    them once because everything is being brought back online,
    ///    but a recurring rescan would loop on a chore that died for
    ///    a reason the user already opted out of auto-handling.
    /// 2. Skips items that are dependency-gated (a `blocks` prereq is
    ///    still unmet) instead of bailing the whole transaction.
    /// 3. Orders the candidate set by `tasks.updated_at ASC` so the
    ///    rescan acts FIFO — the chore that has been waiting longest
    ///    gets the freed worker first.
    ///
    /// Items whose latest execution is still non-terminal (`ready`,
    /// `running`, `waiting_*`) are left alone — `kick()` already
    /// consumes the `ready` queue, and the others are owned by a
    /// live worker or the dependency engine. Returns the work item
    /// ids that were freshly redispatched so the caller can log them.
    pub fn rescan_active_dispatch(&self) -> Result<Vec<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        // FIFO by `updated_at` so the chore that has been waiting
        // longest gets the freed worker. `id` is the deterministic
        // tie-breaker for rows that share an updated_at second.
        let candidates: Vec<(String, bool)> = {
            let mut stmt = tx.prepare(
                "SELECT id, autostart FROM tasks
                 WHERE status = 'active' AND deleted_at IS NULL
                 ORDER BY updated_at ASC, id ASC",
            )?;
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut redispatched = Vec::new();
        for (work_item_id, autostart) in candidates {
            if !autostart {
                continue;
            }
            let needs_dispatch = match query_latest_execution_for_work_item(&tx, &work_item_id)? {
                Some(existing) => existing.status.is_terminal(),
                None => true,
            };
            if !needs_dispatch {
                continue;
            }
            // Silently skip gated items so the rescan keeps going.
            // request_execution_in_tx_with_live_check would bail and
            // roll back the entire transaction otherwise.
            if !deps::gating_prereqs_for(&tx, &work_item_id)?.is_empty() {
                continue;
            }
            request_execution_in_tx_with_live_check(
                &tx,
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
                // `|_| true` keeps any non-terminal execution intact —
                // the on-free rescan only ever fires this branch when
                // the latest execution is terminal anyway, so the
                // closure is unreachable in the redispatch path.
                |_| true,
            )?;
            redispatched.push(work_item_id);
        }
        tx.commit()?;
        Ok(redispatched)
    }

    /// Return the work item ids whose `tasks.status = 'active'` but
    /// whose latest execution is NOT in `running` (no live worker is
    /// currently driving the slot). Used by the dispatcher to surface
    /// the "active vs slot" invariant when the worker pool stalls so a
    /// human reviewing the engine log can spot a divergence between
    /// `boss chore list --status active` and `bossctl agents list`.
    ///
    /// Items whose latest execution is `ready` (queued behind a full
    /// pool) are included — they're the canonical "queued ghost" the
    /// invariant is meant to catch.
    pub fn list_active_chores_without_live_run(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT t.id FROM tasks t
             WHERE t.status = 'active'
               AND t.deleted_at IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we
                   WHERE we.work_item_id = t.id
                     AND we.status = 'running'
               )",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Return the work item ids that are candidates for orphan-active
    /// redispatch. A candidate satisfies all of:
    ///
    /// 1. `tasks.status = 'active'` and not deleted.
    /// 2. `tasks.updated_at` is more than `min_age_secs` old (guards
    ///    against false-positive on a fresh transition whose worker is
    ///    still spinning up).
    /// 3. No `ready` execution exists (if one does, it is already
    ///    queued for dispatch; no action needed).
    ///
    /// The caller is responsible for checking whether the latest
    /// non-terminal execution (if any) is claimed by a live worker
    /// slot — that check requires in-memory worker-pool state that the
    /// DB layer does not have access to.
    ///
    /// Excludes items whose latest **`pr_review`** execution is a
    /// *terminal-but-not-completed* one (`orphaned`/`abandoned`/`failed`/
    /// `cancelled`) — i.e. a reviewer pass that died (host failure,
    /// cube-lease reap, crash) without ever finalizing. The "latest"
    /// comparison is scoped to `pr_review`-kind rows so an unrelated
    /// terminal execution of a different kind created afterwards (e.g. a
    /// churned `chore_implementation` retry) never masks a still-dead
    /// review. A `running`/`waiting_human` `pr_review` is unaffected by
    /// this exclusion (it is still a candidate here, handled by the
    /// existing running-reviewer defense-in-depth check below) — only the
    /// dead ones are diverted. `execution_kind_for_work_item` has no
    /// notion of `pr_review` (it only derives the task-kind-based
    /// implementation kinds), so if this sweep redispatched a dead-review
    /// item it would wrongly spawn a fresh implementer on top of an
    /// already-open PR instead of re-running the reviewer. Those items are
    /// handled exclusively by [`crate::pr_review_recovery`], which creates
    /// the correct `pr_review` execution kind.
    pub fn list_orphan_active_candidates(&self, min_age_secs: i64) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let now_secs: i64 = crate::epoch_time::now_epoch_secs();
        let cutoff = now_secs - min_age_secs;
        // The recovery-escalation exclusion: once the transient-recovery
        // sweep has raised an open attention item because a worker's API
        // error is non-retryable (permanent/unrecognised) or the retry
        // cap was reached, this work item must NOT be blindly
        // re-dispatched — it is flagged for a human. Resolving the
        // attention item makes it a candidate again.
        // waiting_human is a live state: the worker parked for human input and
        // then exited, releasing its worker-pool slot. The execution is still
        // alive — it just isn't currently claimed. Excluding it here prevents
        // the sweep from treating an unclaimed slot as "dead worker" and
        // abandoning a valid in-flight execution.
        let stmt_sql = format!(
            "SELECT t.id FROM tasks t
             WHERE t.status = 'active'
               AND t.deleted_at IS NULL
               AND CAST(t.updated_at AS INTEGER) < ?1
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we
                   WHERE we.work_item_id = t.id
                     AND we.status IN ('ready', 'waiting_human')
               )
               AND NOT EXISTS (
                   SELECT 1 FROM work_attention_items a
                   WHERE a.work_item_id = t.id
                     AND a.status = 'open'
                     AND a.kind IN ('{permanent}', '{exhausted}')
               )
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we
                   WHERE we.work_item_id = t.id
                     AND we.kind = 'pr_review'
                     AND we.status IN ('orphaned', 'abandoned', 'failed', 'cancelled')
                     AND NOT EXISTS (
                         SELECT 1 FROM work_executions we2
                         WHERE we2.work_item_id = t.id
                           AND we2.kind = 'pr_review'
                           AND (we2.created_at > we.created_at
                                OR (we2.created_at = we.created_at AND we2.id > we.id))
                     )
               )
             ORDER BY t.updated_at ASC, t.id ASC",
            permanent = ATTENTION_KIND_RECOVERY_PERMANENT,
            exhausted = ATTENTION_KIND_RECOVERY_EXHAUSTED,
        );
        let mut stmt = conn.prepare(&stmt_sql)?;
        let rows = stmt.query_map([cutoff], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Return every non-terminal, non-deleted work item with an open PR
    /// whose latest **`pr_review`** execution reached a terminal state
    /// WITHOUT ever finalizing — see [`DeadPrReviewCandidate`]. Used by
    /// [`crate::pr_review_recovery`]'s sweep to auto-refire the review
    /// pipeline for a PR whose reviewer died mid-run (host failure,
    /// cube-lease reap, crash) so the PR is never silently left unreviewed.
    ///
    /// `we.status != 'completed'` is the detection signal:
    /// `finalize_pr_review_pass` is the ONLY path that transitions a
    /// `pr_review` execution to `completed` (success or reviewer give-up
    /// both still finalize as `completed`), so any other terminal status
    /// on the item's latest `pr_review` execution means that path never
    /// ran.
    ///
    /// The "latest" comparison is scoped to `kind = 'pr_review'` rows only
    /// (not the item's latest execution of ANY kind) — a work item can
    /// accumulate unrelated terminal executions of other kinds (e.g. a
    /// churned `chore_implementation` retry) after its review died without
    /// that superseding the dead review; scoping by kind is what makes this
    /// query answer "has a fresh review been attempted since the last one
    /// died," not "is a pr_review the single most recent row of any kind."
    pub fn list_dead_pr_review_candidates(&self) -> Result<Vec<DeadPrReviewCandidate>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT t.id, we.id, we.status
             FROM tasks t
             JOIN work_executions we ON we.work_item_id = t.id
             WHERE t.deleted_at IS NULL
               AND t.status NOT IN ('done', 'archived', 'cancelled')
               AND t.pr_url IS NOT NULL AND t.pr_url != ''
               AND we.kind = 'pr_review'
               AND we.status IN ('orphaned', 'abandoned', 'failed', 'cancelled')
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we2
                   WHERE we2.work_item_id = t.id
                     AND we2.kind = 'pr_review'
                     AND (we2.created_at > we.created_at
                          OR (we2.created_at = we.created_at AND we2.id > we.id))
               )
             ORDER BY t.updated_at ASC, t.id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let status: String = row.get(2)?;
            Ok(DeadPrReviewCandidate {
                work_item_id: row.get(0)?,
                execution_id: row.get(1)?,
                execution_status: status.parse().map_err(|e: String| {
                    rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, e.into())
                })?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Count how many terminal executions (`orphaned`, `abandoned`,
    /// `failed`) the work item has produced within the trailing
    /// `since_epoch_secs` window. Used by the orphan-active churn
    /// guard to stop auto-redispatching a work item that keeps dying.
    pub fn count_recent_terminal_executions(&self, work_item_id: &str, since_epoch_secs: i64) -> Result<i64> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT COUNT(*) FROM work_executions
              WHERE work_item_id = ?1
                AND status IN ('orphaned', 'abandoned', 'failed')
                AND CAST(created_at AS INTEGER) >= ?2",
            params![work_item_id, since_epoch_secs],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Same candidate set as [`Self::count_recent_terminal_executions`]
    /// but returns the execution ids themselves (most recent first) so a
    /// churn-guard trip can point the operator at the specific failing
    /// runs instead of just a count. See
    /// [`Self::file_churn_guard_parked_attention`].
    pub fn list_recent_terminal_execution_ids(&self, work_item_id: &str, since_epoch_secs: i64) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM work_executions
              WHERE work_item_id = ?1
                AND status IN ('orphaned', 'abandoned', 'failed')
                AND CAST(created_at AS INTEGER) >= ?2
              ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![work_item_id, since_epoch_secs], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn list_executions(&self, work_item_id: Option<&str>) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        if let Some(work_item_id) = work_item_id {
            let _ = product_id_for_work_item(&conn, work_item_id)?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                        cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                        created_at, started_at, finished_at,
                        pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since
                 FROM work_executions
                 WHERE work_item_id = ?1
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map([work_item_id], map_execution)?;
            return collect_rows(rows);
        }

        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since
             FROM work_executions
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    /// List all executions for `chain_root_id` plus every revision task in
    /// its chain. Results are ordered chronologically (created_at ASC, id
    /// ASC) across all tasks so the caller sees a unified history.
    pub fn list_executions_for_chain(&self, chain_root_id: &str) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let revision_ids = collect_chain_revision_ids(&conn, chain_root_id)?;
        let mut all_ids = vec![chain_root_id.to_owned()];
        all_ids.extend(revision_ids);

        let mut all_executions = Vec::new();
        for task_id in &all_ids {
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                        cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                        created_at, started_at, finished_at,
                        pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since
                 FROM work_executions
                 WHERE work_item_id = ?1",
            )?;
            let rows = stmt.query_map([task_id], map_execution)?;
            all_executions.extend(collect_rows(rows)?);
        }
        all_executions.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        Ok(all_executions)
    }

    pub fn get_execution(&self, id: &str) -> Result<WorkExecution> {
        let conn = self.connect()?;
        query_execution(&conn, id).require("execution", id)
    }

    /// `work_executions.host_id` for one execution — the host the
    /// scheduler attributed the run to. `None` before a run has picked a
    /// host (not yet dispatched, or a pre-migration row); the engine
    /// treats an absent value as `"local"`. Backs `bossctl work
    /// executions`, which otherwise had no way to show which host ran an
    /// execution without a raw `work_executions`/`work_runs` query.
    pub fn execution_host_id(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let host: Option<Option<String>> = conn
            .query_row(
                "SELECT host_id FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(host.flatten())
    }

    /// `work_executions.host_id` for every execution of `work_item_id`, in
    /// one query. Backs `bossctl work executions`, which previously issued
    /// one [`Self::execution_host_id`] point read per row; batching avoids
    /// the N+1 query pattern for work items with many executions.
    pub fn execution_host_ids_for_item(&self, work_item_id: &str) -> Result<HashMap<String, String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare("SELECT id, host_id FROM work_executions WHERE work_item_id = ?1")?;
        let rows = stmt.query_map(params![work_item_id], |row| {
            let id: String = row.get(0)?;
            let host_id: Option<String> = row.get(1)?;
            Ok((id, host_id.unwrap_or_else(|| "local".to_owned())))
        })?;
        collect_rows(rows).map(|entries: Vec<(String, String)>| entries.into_iter().collect())
    }

    /// Return true if `execution` is a stale prior occupant of a reused
    /// (warm-cached) cube workspace: another live (`running` /
    /// `waiting_human`) execution now claims the same `cube_workspace_id`
    /// and is more recent (by `created_at`, then `id`, matching the
    /// dispatch-ordering convention).
    ///
    /// Used by the completion handler to ignore Stop events that leaked
    /// from a stale `boss-event` hook registration left in a re-leased
    /// workspace (see [`crate::worker_setup::purge_leaked_worker_hooks`]).
    /// Without this guard a stale Stop could mis-attribute completion to
    /// the wrong run or release the live run's re-leased workspace. The
    /// newest execution is never its own predecessor, so its own Stop
    /// still finalizes it.
    pub fn execution_superseded_in_workspace(&self, execution: &WorkExecution) -> Result<bool> {
        let Some(workspace_id) = execution.cube_workspace_id.as_deref().filter(|s| !s.is_empty()) else {
            return Ok(false);
        };
        let conn = self.connect()?;
        let newest_live: Option<String> = conn
            .query_row(
                "SELECT id FROM work_executions
                 WHERE cube_workspace_id = ?1
                   AND status IN ('running', 'waiting_human')
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                rusqlite::params![workspace_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(matches!(newest_live, Some(id) if id != execution.id))
    }

    /// Find the most recent `orphaned` execution for a work item that has
    /// no `pr_url` set. Used by the runner at spawn time to detect a
    /// prior mid-flight execution whose branch the new worker should
    /// attempt to resume (startup recovery path).
    ///
    /// Returns `None` when:
    ///   - the work item has no prior executions,
    ///   - all prior executions are non-orphaned (completed, failed, etc.), or
    ///   - the latest orphaned execution already has `pr_url` set (that
    ///     case is handled by the existing `task.pr_url` resume path).
    ///
    /// The `current_execution_id` is excluded so the caller doesn't
    /// accidentally match the execution that's currently being dispatched.
    pub fn get_prior_orphaned_execution(
        &self,
        work_item_id: &str,
        current_execution_id: &str,
    ) -> Result<Option<WorkExecution>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since
             FROM work_executions
             WHERE work_item_id = ?1
               AND id != ?2
               AND status = 'orphaned'
               AND pr_url IS NULL
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            rusqlite::params![work_item_id, current_execution_id],
            map_execution,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Return the most recent `running` or `waiting_human` execution for
    /// `work_item_id`, excluding `exclude_id`. Used by the double-spawn
    /// guard in the coordinator: before spawning, if another execution is
    /// already live, the new one is redundant and should be abandoned
    /// without starting a worker.
    pub fn get_live_execution_for_work_item(
        &self,
        work_item_id: &str,
        exclude_id: &str,
    ) -> Result<Option<WorkExecution>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since
             FROM work_executions
             WHERE work_item_id = ?1
               AND id != ?2
               AND status IN ('running', 'waiting_human')
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            rusqlite::params![work_item_id, exclude_id],
            map_execution,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Return a live (`running` / `waiting_human`) execution belonging to
    /// ANY *other* work item in the same revision chain as `work_item_id` —
    /// the chain root (the task that owns the PR) plus every revision
    /// descendant of that root. Returns `None` when no other chain member
    /// is live.
    ///
    /// This is the per-PR single-writer guard. Every task in a revision
    /// chain shares the chain root's PR branch, and cube co-locates same-PR
    /// workers on ONE shared jj backing store; two live executions anywhere
    /// in the chain therefore rebase/rewrite each other's commits (the
    /// T1577 / T1815 incident: a conflict-resolution revision rebased the
    /// stack out from under a still-live implementation resume). Unlike
    /// [`Self::get_live_execution_for_work_item`], which keys on a single
    /// `work_item_id` and so cannot see a sibling revision's (or the chain
    /// root's) live worker, this walks the whole chain.
    ///
    /// The work item itself is excluded from the scan — the same-work-item
    /// "is this exact task already being worked?" question is answered by
    /// [`Self::get_live_execution_for_work_item`]; this answers the broader
    /// "is anything ELSE on this PR live?". For a work item with no revision
    /// chain (a chore with no revisions, an automation, a product/project)
    /// the chain collapses to the work item itself, so this returns `None`
    /// and the behaviour is identical to the historical per-work-item guard.
    pub fn live_execution_elsewhere_in_chain(&self, work_item_id: &str) -> Result<Option<WorkExecution>> {
        Ok(self
            .live_executions_elsewhere_in_chain(work_item_id)?
            .into_iter()
            .next())
    }

    /// Apply the optional one-shot `merge_order` dispatch stagger to a
    /// `ready` execution, returning `Some(not_before_epoch)` when a stagger was
    /// stamped (the caller should skip dispatching it this round) or `None`
    /// when the execution does not qualify (dispatch proceeds normally).
    ///
    /// This is the enforcement half of the non-blocking merge-sequencing
    /// relation (design Layer 3 / direction 2): a `merge_order` edge never
    /// gates dispatch, but for the **highest-overlap pairs** an operator may
    /// opt into a small bounded offset so two edit-overlapping siblings don't
    /// start (and therefore diff) at exactly the same time. An execution for
    /// work item X qualifies iff:
    ///
    ///   - `stagger_secs > 0` (opt-in; the config loader clamps it to
    ///     [`crate::config::MAX_MERGE_ORDER_STAGGER_SECS`]), and
    ///   - X is the **later** side of a `merge_order` pairing (the `dependent`
    ///     end of the edge — the canonical "second" task; we only ever stagger
    ///     one deterministic member of a pair, never both), and
    ///   - a **first**-side peer is still in flight (status not `done` /
    ///     `archived`) so the two would genuinely run concurrently, and
    ///   - this execution was **never staggered before**
    ///     (`dispatch_not_before IS NULL`) — a strict one-shot, so it never
    ///     re-delays and never contends with the pre-start-failure / transient
    ///     backoff paths that stamp the same column.
    ///
    /// On qualification it stamps `dispatch_not_before = now + stagger_secs`,
    /// which makes the row invisible to [`Self::list_ready_executions`] until
    /// the window elapses; the scheduler heartbeat re-kicks it afterward.
    /// **Never a block and never waits for a merge** — a bounded offset only.
    pub fn maybe_stagger_merge_order_dispatch(
        &self,
        execution_id: &str,
        work_item_id: &str,
        stagger_secs: u64,
    ) -> Result<Option<i64>> {
        if stagger_secs == 0 {
            return Ok(None);
        }
        let conn = self.connect()?;
        // Only the canonical "later" side of a pairing is eligible.
        let later_peers: Vec<deps::MergeOrderSibling> = deps::merge_order_siblings(&conn, work_item_id)?
            .into_iter()
            .filter(|s| s.work_item_is_later)
            .collect();
        if later_peers.is_empty() {
            return Ok(None);
        }
        // Require at least one first-side peer still in flight, so we only pay
        // the offset when there is real concurrency to break up.
        let mut first_in_flight = false;
        for peer in &later_peers {
            let status = deps::lookup_work_item_status(&conn, &peer.sibling_id)?;
            if matches!(status.as_deref(), Some(s) if s != "done" && s != "archived") {
                first_in_flight = true;
                break;
            }
        }
        if !first_in_flight {
            return Ok(None);
        }
        // One-shot guard: only a `ready` row that has never been deferred.
        let current: Option<Option<String>> = conn
            .query_row(
                "SELECT dispatch_not_before FROM work_executions WHERE id = ?1 AND status = 'ready'",
                params![execution_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        match current {
            Some(None) => {}                  // ready and never deferred → eligible
            Some(Some(_)) => return Ok(None), // already deferred once
            None => return Ok(None),          // not a ready row (raced away)
        }
        let not_before = crate::epoch_time::now_epoch_secs() + stagger_secs as i64;
        let updated = conn.execute(
            "UPDATE work_executions
             SET dispatch_not_before = ?2
             WHERE id = ?1 AND status = 'ready' AND dispatch_not_before IS NULL",
            params![execution_id, not_before.to_string()],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        Ok(Some(not_before))
    }

    /// Return EVERY live (`running` / `waiting_human`) execution belonging to
    /// ANY *other* work item in the same revision chain as `work_item_id` —
    /// not just the first one encountered. See
    /// [`Self::live_execution_elsewhere_in_chain`] for the chain-walk
    /// rationale and the tombstone-inclusive note; that method is now a thin
    /// "first result" wrapper around this one.
    ///
    /// Callers that need to decide "is EVERY live sibling a review" (as
    /// opposed to "is there at least one live sibling") must use this rather
    /// than the singular form: `member_ids` walks the chain root-first, so
    /// the singular form can return a root `pr_review` while masking a live
    /// writer further down the chain — exactly the gap that let a
    /// root-review bypass wrongly co-dispatch a second writer alongside a
    /// still-live descendant writer (the T1577/T1815 hazard this guard
    /// exists to prevent).
    pub fn live_executions_elsewhere_in_chain(&self, work_item_id: &str) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let root_id = chain_root(&conn, work_item_id)?;
        let mut member_ids = Vec::with_capacity(4);
        member_ids.push(root_id.clone());
        // Tombstone-inclusive: `block_pending_revisions_on_parent_close`
        // archives *and* tombstones a WIP revision in the same transaction
        // that the merge poller detects the merge, but its execution isn't
        // force-released until the poller's next step. A tombstone-filtered
        // walk could miss that still-live execution during this window and
        // let a second worker start on the same PR branch (the T1577/T1815
        // hazard this guard exists to prevent).
        member_ids.extend(collect_chain_revision_ids_including_deleted(&conn, &root_id)?);
        let mut live_executions = Vec::new();
        for member_id in &member_ids {
            if member_id == work_item_id {
                continue;
            }
            let live: Option<WorkExecution> = conn
                .query_row(
                    "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                            cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                            created_at, started_at, finished_at,
                            pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since
                     FROM work_executions
                     WHERE work_item_id = ?1
                       AND status IN ('running', 'waiting_human')
                     ORDER BY created_at DESC, id DESC
                     LIMIT 1",
                    rusqlite::params![member_id],
                    map_execution,
                )
                .optional()?;
            if let Some(live) = live {
                live_executions.push(live);
            }
        }
        Ok(live_executions)
    }

    /// Mark an execution `abandoned` without touching any other
    /// execution or task state. Used by the double-spawn guard to
    /// discard a redundant `ready` execution before it ever reaches
    /// `start_execution_run`.
    pub fn mark_execution_redundant(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        conn.execute(
            "UPDATE work_executions
             SET status = 'abandoned',
                 finished_at = COALESCE(finished_at, ?2)
             WHERE id = ?1",
            rusqlite::params![execution_id, now],
        )?;
        Ok(())
    }

    /// Atomically move a `ready` execution back to `waiting_dependency` when
    /// the dispatcher discovers at dispatch time that the work item is still
    /// gated by an unmet prereq. A no-op (returns `false`) when the execution
    /// is not in `ready` status — it may have been promoted or claimed by
    /// a concurrent path. Returns `true` when the row was actually updated.
    pub fn downgrade_ready_to_waiting_dependency(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let affected = conn.execute(
            "UPDATE work_executions
             SET status = 'waiting_dependency'
             WHERE id = ?1
               AND status = 'ready'",
            rusqlite::params![execution_id],
        )?;
        Ok(affected > 0)
    }

    /// Record `reason` as this `ready` execution's current dispatch-wait
    /// defer reason (`chain_serialized`, `pool_exhausted`, ...), mirroring
    /// the reason already logged to `dispatch_events` at the same call
    /// site. Only stamps `dispatch_wait_since` when the reason is new
    /// (previously `NULL` or a different reason) so it reflects the start
    /// of the *current* wait, not the most recent drain pass that found
    /// the same blocker still in place. Used by the kanban card to render
    /// the real wait cause instead of a generic "Waiting for a slot" (see
    /// module docs on [`Self::live_execution_elsewhere_in_chain`] for the
    /// `chain_serialized` incident this surfaces).
    pub fn set_dispatch_wait_reason(&self, execution_id: &str, reason: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        conn.execute(
            "UPDATE work_executions
             SET dispatch_wait_since = CASE
                     WHEN dispatch_wait_reason IS ?2 THEN dispatch_wait_since
                     ELSE ?3
                 END,
                 dispatch_wait_reason = ?2
             WHERE id = ?1",
            rusqlite::params![execution_id, reason, now],
        )?;
        Ok(())
    }

    /// Clear `dispatch_wait_reason` / `dispatch_wait_since` — called the
    /// moment an execution claims a worker slot (or otherwise leaves the
    /// deferred-ready state) so a stale reason doesn't linger once
    /// dispatch has actually succeeded.
    pub fn clear_dispatch_wait_reason(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions
             SET dispatch_wait_reason = NULL,
                 dispatch_wait_since = NULL
             WHERE id = ?1",
            rusqlite::params![execution_id],
        )?;
        Ok(())
    }

    /// Retire a `revision_implementation` execution that turned out to be
    /// unnecessary before it was ever dispatched: the merge-conflict fix
    /// vehicle's linked `conflict_resolutions` ledger row already
    /// transitioned to `succeeded` (the periodic merge-poller sweep found
    /// the bound PR mergeable again — see `conflict_watch::on_resolved`) in
    /// the window between this execution being queued and a worker slot
    /// becoming available. Spawning a worker here would just have it
    /// discover "nothing to do" and become the produce-a-PR nudge loop this
    /// method exists to prevent (see the `nudge_breaker` module doc).
    ///
    /// Marks the execution `abandoned` (mirrors [`Self::mark_execution_redundant`])
    /// and advances the revision task itself to `in_review` — the same
    /// terminal state a normal successful revision completion reaches via
    /// `record_worker_pr_completion` — so the task leaves the Doing/Backlog
    /// column instead of stranding there with no live worker. A revision
    /// this early in dispatch (never actually leased a workspace) is
    /// typically still `todo` — it only flips to `active` once a worker
    /// starts running — so the WHERE guard accepts either; a task a human
    /// (or a concurrent path) already moved to `blocked`/`in_review`/a
    /// terminal status is left alone. Returns `true` iff the task actually
    /// transitioned; the execution abandon always applies (best-effort,
    /// WHERE-guarded on `status = 'ready'` so a concurrent claim isn't
    /// clobbered).
    pub fn retire_stale_revision_before_dispatch(&self, execution_id: &str, task_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'abandoned',
                 finished_at = COALESCE(finished_at, ?2)
             WHERE id = ?1
               AND status = 'ready'",
            params![execution_id, now],
        )?;
        let task_rows = tx.execute(
            "UPDATE tasks
                SET status             = 'in_review',
                    updated_at         = ?2,
                    last_status_actor  = 'engine',
                    blocked_reason     = NULL,
                    blocked_attempt_id = NULL
              WHERE id = ?1
                AND status IN ('todo', 'active')",
            params![task_id, now],
        )?;
        if task_rows > 0 {
            cascade_dependents_after_prereq_status_change(&tx, task_id, "in_review", &now)?;
        }
        tx.commit()?;
        Ok(task_rows > 0)
    }

    /// Find a *stale* cube lease that the engine recorded against
    /// `workspace_id` and that is safe to force-release before a
    /// resume re-leases the same workspace.
    ///
    /// This closes the lease-reclaim half of the UI-crash recovery
    /// path (issue #962, the "mono-agent-003" scenario). When the app
    /// crashes, the dead worker's execution is marked `orphaned` but
    /// its cube workspace lease is intentionally left intact so the
    /// resume worker can recover the in-flight jj checkout via
    /// `cube workspace lease --prefer <workspace>`. The problem: cube
    /// still sees that workspace as `leased` to the dead execution, so
    /// the `--prefer` lease is refused and the hard-prefer resume fails
    /// outright -- silently stranding the local work. The dispatcher
    /// must therefore reclaim the dead lease first.
    ///
    /// Safety: returns `Some(lease_id)` **only** when the lease the
    /// caller observed cube holding (`current_lease_id`) is recorded in
    /// the engine's own `work_executions` table against a now-*terminal*
    /// execution for `workspace_id`, AND no live (`running` /
    /// `waiting_human`) execution currently claims that workspace. This
    /// guarantees we never force-release a lease backing a genuinely
    /// live worker -- only one whose owning execution the engine has
    /// already reaped. Returns `None` (do not reclaim) otherwise.
    pub fn stale_lease_to_reclaim_for_workspace(
        &self,
        workspace_id: &str,
        current_lease_id: &str,
    ) -> Result<Option<String>> {
        if workspace_id.is_empty() || current_lease_id.is_empty() {
            return Ok(None);
        }
        let conn = self.connect()?;

        // Never reclaim while a live execution still claims the
        // workspace -- that lease is legitimately in use.
        let live_holder: Option<String> = conn
            .query_row(
                "SELECT id FROM work_executions
                 WHERE cube_workspace_id = ?1
                   AND status IN ('running', 'waiting_human')
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                rusqlite::params![workspace_id],
                |row| row.get(0),
            )
            .optional()?;
        if live_holder.is_some() {
            return Ok(None);
        }

        // The lease cube reports holding the workspace must match a
        // terminal execution row the engine recorded against this same
        // workspace. Matching on both the lease id and the workspace id
        // ensures we only reclaim the dead worker's own lease, not an
        // unrelated one that happens to occupy the slot.
        let terminal_owner: Option<String> = conn
            .query_row(
                "SELECT id FROM work_executions
                 WHERE cube_workspace_id = ?1
                   AND cube_lease_id = ?2
                   AND status IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                rusqlite::params![workspace_id, current_lease_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(terminal_owner.map(|_| current_lease_id.to_owned()))
    }
}
