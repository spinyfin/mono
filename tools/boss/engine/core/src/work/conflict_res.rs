use super::*;

/// The only escalation-ladder rung that exists today (design's rung 3,
/// "Full worker — unchanged fallback"). Rungs 0-2 (deterministic
/// resolvers, engine-direct mechanical rebase, the small pre-staged
/// agent) are designed but not yet built (T2/T4/T6 of
/// `merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`);
/// until they ship, every conflict this engine records was resolved by
/// a full worker doing the whole job by hand.
const RUNG_FULL_WORKER: i64 = 3;

impl WorkDb {
    /// Read the unified auto-maintenance opt-out flag for a product.
    /// Defaults to `true` when the column is unset or the product row
    /// is missing — i.e. the opt-out only takes effect when the
    /// operator has explicitly disabled it.
    ///
    /// Used by the conflict-watch (and, in later phases, ci-watch /
    /// auto-rebase) paths to skip auto-remediation for products whose
    /// owner has set `auto_pr_maintenance_enabled = 0`
    /// (`merge-conflict-handling-in-review.md` Q7 / Phase 6 #18).
    pub fn product_auto_pr_maintenance_enabled(&self, product_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let enabled: Option<i64> = conn
            .query_row(
                "SELECT auto_pr_maintenance_enabled FROM products WHERE id = ?1",
                params![product_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(enabled.map(|v| v != 0).unwrap_or(true))
    }

    /// True iff there's a non-terminal `rebase_attempts` row covering
    /// the given PR url. Used by `conflict_watch::on_conflict_detected`
    /// to defer when the `auto-rebase-stacked-prs` flow already owns
    /// the slot (design Q7).
    ///
    /// The `rebase_attempts` table ships with that flow, not this one.
    /// Until it lands, this method short-circuits to `false` so the
    /// dispatch site reads identically before and after auto-rebase
    /// is wired up.
    pub fn has_active_rebase_attempt_for_pr(&self, pr_url: &str) -> Result<bool> {
        let conn = self.connect()?;
        if !table_exists(&conn, "rebase_attempts")? {
            return Ok(false);
        }
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rebase_attempts
              WHERE dependent_pr_url = ?1
                AND status IN ('pending', 'running', 'escalated')",
            params![pr_url],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Insert a `conflict_resolutions` row with `status='pending'`
    /// alongside a `tasks.blocked_attempt_id` pointer to the new
    /// attempt id. `(work_item_id, base_sha_at_trigger, head_sha_before)`
    /// is the idempotency key — a second probe for the same triple
    /// finds the row already pending and returns `Ok(None)` (caller
    /// reads the existing row via [`Self::active_conflict_resolution_for_work_item`]).
    /// `head_sha_before` is included because `base_sha_at_trigger` mirrors
    /// GitHub's PR `baseRefOid`, which is fixed at PR-open time and does
    /// not track `main` moving under an in-review PR — keying on it alone
    /// would make every re-arm past a stale `succeeded` attempt collide
    /// forever (T2396 / PR #1874).
    ///
    /// Phase 3 of the merge-conflict design (Q4). The caller is
    /// `conflict_watch::on_conflict_detected` after the parent
    /// `tasks` row is already flipped to `blocked: merge_conflict`.
    ///
    /// Churn guard (Phase 6 #16, design Q6): if the work item has
    /// already produced ≥ [`CHURN_GUARD_THRESHOLD`] conflict_resolutions
    /// rows in the trailing [`CHURN_GUARD_WINDOW_SECS`], the new row is
    /// inserted in `status='abandoned'` with
    /// `failure_reason='churn_threshold_exceeded'` so the dispatcher
    /// skips it and the parent stays `blocked` for human attention.
    pub fn insert_conflict_resolution(
        &self,
        input: ConflictResolutionInsertInput,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let id = next_id("crz");
        let now = now_string();

        // Count the trailing-1h attempts for this work item; if we've
        // already crossed the churn threshold, the new row is
        // pre-abandoned. The count is computed in the same transaction
        // as the insert so two concurrent probes can't both squeak past
        // the bar.
        let now_secs: i64 = now.parse().unwrap_or(0);
        let cutoff_secs = now_secs - CHURN_GUARD_WINDOW_SECS;
        let recent_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM conflict_resolutions
              WHERE work_item_id = ?1
                AND CAST(created_at AS INTEGER) >= ?2",
            params![input.work_item_id, cutoff_secs],
            |row| row.get(0),
        )?;
        let churn_tripped = recent_count >= CHURN_GUARD_THRESHOLD;
        let (status, failure_reason, finished_at): (&str, Option<&str>, Option<&str>) = if churn_tripped {
            ("abandoned", Some("churn_threshold_exceeded"), Some(now.as_str()))
        } else {
            ("pending", None, None)
        };

        let rows = tx.execute(
            "INSERT OR IGNORE INTO conflict_resolutions
                (id, product_id, work_item_id, pr_url, pr_number,
                 head_branch, base_branch, base_sha_at_trigger,
                 head_sha_before, status, failure_reason, created_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id,
                input.product_id,
                input.work_item_id,
                input.pr_url,
                input.pr_number,
                input.head_branch,
                input.base_branch,
                input.base_sha_at_trigger,
                input.head_sha_before,
                status,
                failure_reason,
                now,
                finished_at,
            ],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        // Only stamp the parent's `blocked_attempt_id` for live
        // attempts; an immediately-abandoned churn-guard row would
        // mis-point the kanban at a dead attempt.
        if !churn_tripped {
            tx.execute(
                "UPDATE tasks
                    SET blocked_attempt_id = ?2,
                        updated_at         = ?3
                  WHERE id = ?1
                    AND status = 'blocked'
                    AND blocked_reason = 'merge_conflict'
                    AND deleted_at IS NULL",
                params![input.work_item_id, id, now],
            )?;
        }
        let inserted = query_conflict_resolution(&tx, &id)?
            .with_context(|| format!("unknown conflict_resolution after insert: {id}"))?;
        tx.commit()?;
        Ok(Some(inserted))
    }

    /// Fetch a single attempt row by id. `Ok(None)` if the row is
    /// missing.
    pub fn get_conflict_resolution(&self, attempt_id: &str) -> Result<Option<ConflictResolution>> {
        let conn = self.connect()?;
        query_conflict_resolution(&conn, attempt_id)
    }

    /// Most recent `conflict_resolutions` row for `work_item_id`,
    /// regardless of status. Used by the stale-base re-arm path in
    /// `conflict_watch::on_conflict_detected` to check whether the
    /// previous attempt ended in `succeeded` (eligible for re-arm when
    /// the PR is still CONFLICTING) vs `failed`/`abandoned` (not eligible
    /// — the churn guard or human owns the retry decision in that case).
    ///
    /// Returns `None` when no attempt has ever been recorded for this
    /// work item.
    pub fn latest_conflict_resolution_for_work_item(&self, work_item_id: &str) -> Result<Option<ConflictResolution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {CONFLICT_RESOLUTION_COLUMNS}
             FROM conflict_resolutions
             WHERE work_item_id = ?1
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        ))?;
        let mut rows = stmt.query_map([work_item_id], map_conflict_resolution)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Latest non-terminal attempt for `work_item_id`. Used by the
    /// conflict-detection path to detect "an attempt is already in
    /// flight" and by the worker prompt composer to find the row to
    /// embed the diagnosis from.
    pub fn active_conflict_resolution_for_work_item(&self, work_item_id: &str) -> Result<Option<ConflictResolution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {CONFLICT_RESOLUTION_COLUMNS}
             FROM conflict_resolutions
             WHERE work_item_id = ?1
               AND status IN ('pending', 'running')
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        ))?;
        let mut rows = stmt.query_map([work_item_id], map_conflict_resolution)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Store the engine-collected diagnosis JSON on a pending attempt,
    /// and derive `conflict_class` (Layer 0 telemetry) from the
    /// diagnosis's conflicted-file paths in the same update. A
    /// diagnosis that fails to parse (the `error` path in
    /// `ConflictDiagnosis`) leaves `conflict_class` untouched rather
    /// than failing the whole write — the diagnosis JSON is still
    /// useful even when it can't be classified. Idempotent — calling
    /// twice overwrites. Returns the updated row; `Ok(None)` when the
    /// id is missing.
    pub fn set_conflict_resolution_diagnosis(
        &self,
        attempt_id: &str,
        diagnosis_json: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let conflict_class = serde_json::from_str::<crate::conflict_diagnosis::ConflictDiagnosis>(diagnosis_json)
            .ok()
            .map(|diagnosis| {
                let paths: Vec<String> = diagnosis.files.into_iter().map(|f| f.path).collect();
                crate::conflict_diagnosis::classify_conflict_class(&paths).to_owned()
            });
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET conflict_diagnosis = ?2,
                    conflict_class     = COALESCE(?3, conflict_class)
              WHERE id = ?1",
            params![attempt_id, diagnosis_json, conflict_class],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Stamp the soft-FK from a `conflict_resolutions` trigger-ledger row
    /// to the `kind=revision` task the merge-conflict producer spawned
    /// (design Q2 reverse link / Phase 3 cutover). Set by
    /// `conflict_watch::on_conflict_detected` immediately after
    /// `create_revision` succeeds. Idempotent — a second call with the
    /// same id overwrites; `Ok(None)` when the attempt id is unknown.
    ///
    /// Once set, this row is owned by the revision substrate: the dormant
    /// `conflict_resolution` backfill/rescue paths skip it (their queries
    /// filter `revision_task_id IS NULL`), so the old execution kind is
    /// never re-dispatched for a revision-backed attempt.
    pub fn set_conflict_resolution_revision_task_id(
        &self,
        attempt_id: &str,
        revision_task_id: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET revision_task_id = ?2
              WHERE id = ?1",
            params![attempt_id, revision_task_id],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Flip a `pending` attempt to `running` and stamp the lease
    /// triple (`cube_lease_id`, `cube_workspace_id`, `worker_id`) the
    /// coordinator just acquired. Idempotent — a second call with the
    /// same triple is a no-op. Returns the updated row.
    pub fn mark_conflict_resolution_running(
        &self,
        attempt_id: &str,
        cube_lease_id: &str,
        cube_workspace_id: &str,
        worker_id: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET status            = 'running',
                    cube_lease_id     = ?2,
                    cube_workspace_id = ?3,
                    worker_id         = ?4,
                    started_at        = COALESCE(started_at, ?5)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, cube_lease_id, cube_workspace_id, worker_id, now],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Worker-visible terminal transition: flip an attempt to
    /// `failed` with a reason. The Boss-tier `boss engine conflicts
    /// mark-failed` CLI lands here. `Ok(None)` when the id is unknown
    /// or already terminal.
    ///
    /// The companion success path is part of the auto-retire flow
    /// elsewhere; this method intentionally only handles the failure
    /// signal a worker emits when it hits a stop condition.
    pub fn mark_conflict_resolution_failed(
        &self,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET status         = 'failed',
                    failure_reason = ?2,
                    finished_at    = COALESCE(finished_at, ?3)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, reason, now],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Auto-retire transition: flip an attempt from `pending` or `running`
    /// to `succeeded`, stamping `head_sha_after` if known and a fresh
    /// `finished_at`. Idempotent — a second call with the row already
    /// terminal returns `Ok(None)` and writes nothing. Phase 4 / design
    /// Q5: invoked by the merge poller's `on_resolved` path when
    /// GitHub reports the PR mergeable again.  Accepting `pending` covers
    /// the case where the PR becomes clean again before the worker starts.
    pub fn mark_conflict_resolution_succeeded(
        &self,
        attempt_id: &str,
        head_sha_after: Option<&str>,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET status           = 'succeeded',
                    head_sha_after   = COALESCE(?2, head_sha_after),
                    finished_at      = COALESCE(finished_at, ?3),
                    resolved_by_rung = COALESCE(resolved_by_rung, ?4)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, head_sha_after, now, RUNG_FULL_WORKER],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Engine-side abandon: flip a non-terminal attempt to `abandoned`
    /// with the provided reason. Used for "we stepped away on purpose"
    /// terminations (parent PR closed, parent merged externally,
    /// manual override) where `failed` would be misleading. Idempotent.
    pub fn mark_conflict_resolution_abandoned(
        &self,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET status         = 'abandoned',
                    failure_reason = ?2,
                    finished_at    = COALESCE(finished_at, ?3)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, reason, now],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Abandon a stale `conflict_resolutions` row for supersede when the base
    /// SHA has NOT changed. Nullifies `base_sha_at_trigger` to free the
    /// `UNIQUE (work_item_id, base_sha_at_trigger, head_sha_before)` slot so
    /// the INSERT in `on_conflict_detected` can create a fresh row with the
    /// current base SHA and the churn guard can count this supersede toward
    /// the rolling window.
    ///
    /// SQLite treats NULL as distinct from every other value (including other
    /// NULLs) in UNIQUE constraints, so clearing the column releases the slot
    /// without conflicting with any future row.
    ///
    /// Use this for same-base supersedes (head moved or revision terminal, base
    /// unchanged). For base-SHA-changed supersedes, a plain
    /// `mark_conflict_resolution_abandoned` suffices because the new row will
    /// use a different UNIQUE key.
    pub fn abandon_conflict_resolution_for_supersede(
        &self,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET status              = 'abandoned',
                    failure_reason      = ?2,
                    base_sha_at_trigger = NULL,
                    finished_at         = COALESCE(finished_at, ?3)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, reason, now],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Read-only list of `conflict_resolutions` rows for the Phase 5
    /// `boss engine conflicts list` CLI. Filters are AND-ed; an empty
    /// `status` slice means "any status." Rows come back freshest first
    /// (`created_at DESC, id DESC`) so the CLI's first row is the row a
    /// human typically wants. `limit = None` returns every match — the
    /// CLI caps with `--limit`, so the engine doesn't apply a default.
    pub fn list_conflict_resolutions(
        &self,
        product_id: Option<&str>,
        statuses: &[String],
        work_item_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<ConflictResolution>> {
        let conn = self.connect()?;
        let mut sql = format!("SELECT {CONFLICT_RESOLUTION_COLUMNS} FROM conflict_resolutions WHERE 1=1");
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(pid) = product_id {
            sql.push_str(" AND product_id = ?");
            params_vec.push(Box::new(pid.to_owned()));
        }
        if let Some(wid) = work_item_id {
            sql.push_str(" AND work_item_id = ?");
            params_vec.push(Box::new(wid.to_owned()));
        }
        if !statuses.is_empty() {
            sql.push_str(" AND status IN (");
            for (idx, status) in statuses.iter().enumerate() {
                if idx > 0 {
                    sql.push(',');
                }
                sql.push('?');
                params_vec.push(Box::new(status.clone()));
            }
            sql.push(')');
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC");
        if let Some(cap) = limit {
            sql.push_str(" LIMIT ?");
            params_vec.push(Box::new(cap as i64));
        }
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref() as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(refs.as_slice(), map_conflict_resolution)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Reset a terminal-failure attempt back to `pending` so the
    /// dispatcher re-spawns a worker. Only valid when the row's current
    /// status is `failed` or `abandoned`; the caller (CLI) is
    /// responsible for surfacing the rejection on a non-terminal row.
    ///
    /// The reset clears `failure_reason`, `head_sha_after`, the lease
    /// triple (`cube_lease_id`, `cube_workspace_id`, `worker_id`), and
    /// `finished_at`/`started_at` — i.e. it puts the row back into the
    /// shape the dispatcher expects for a fresh pending attempt. The
    /// parent work item is also re-flipped to `blocked: merge_conflict`
    /// (if currently `in_review`) and its `blocked_attempt_id` is
    /// repointed at the reset row. Returns the reset row on success;
    /// `Ok(None)` when the id is unknown or the row is non-terminal.
    pub fn retry_conflict_resolution(&self, attempt_id: &str) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET status            = 'pending',
                    failure_reason    = NULL,
                    head_sha_after    = NULL,
                    cube_lease_id     = NULL,
                    cube_workspace_id = NULL,
                    worker_id         = NULL,
                    started_at        = NULL,
                    finished_at       = NULL
              WHERE id = ?1
                AND status IN ('failed', 'abandoned')",
            params![attempt_id],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let reset = query_conflict_resolution(&tx, attempt_id)?
            .with_context(|| format!("unknown conflict_resolution after retry: {attempt_id}"))?;
        // Re-stamp the parent's blocked state so the kanban shows the
        // card in `blocked: merge_conflict` again, and so the dispatcher
        // re-picks the row up. The flip is best-effort — if the parent
        // is already `blocked: merge_conflict` (or has been moved
        // somewhere unexpected by a human), we leave it alone.
        tx.execute(
            "UPDATE tasks
                SET status             = 'blocked',
                    blocked_reason     = 'merge_conflict',
                    blocked_attempt_id = ?2,
                    last_status_actor  = 'engine',
                    updated_at         = ?3
              WHERE id = ?1
                AND status = 'in_review'
                AND pr_url = ?4
                AND deleted_at IS NULL",
            params![reset.work_item_id, reset.id, now, reset.pr_url],
        )?;
        // If the parent is already blocked: merge_conflict (e.g. the
        // retire path hasn't run because the conflict is still live),
        // just re-point the attempt id.
        tx.execute(
            "UPDATE tasks
                SET blocked_attempt_id = ?2,
                    updated_at         = ?3
              WHERE id = ?1
                AND status = 'blocked'
                AND blocked_reason = 'merge_conflict'
                AND deleted_at IS NULL",
            params![reset.work_item_id, reset.id, now],
        )?;
        tx.commit()?;
        Ok(Some(reset))
    }

    /// Record a producer-side conflict event: a normal worker's own
    /// `cube workspace rebase` reported `REBASED_WITH_CONFLICTS`
    /// mid-task and it resolved the conflict inline, without ever
    /// going through `conflict_watch` (Layer 0 / T1 of
    /// `merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`
    /// — previously the largest source of telemetry undercount).
    /// `product_id`, `work_item_id`, and any already-open PR are
    /// resolved from `input.execution_id` so the calling worker only
    /// needs to supply what it directly observed from `cube workspace
    /// rebase`'s own output. The row is inserted already terminal
    /// (`status = 'succeeded'`) — by the time a worker calls this it
    /// has already resolved the conflict, so there is no separate
    /// pending/running lifecycle to track. `pr_url`/`pr_number` fall
    /// back to the empty-string/`0` sentinel when the task has not
    /// opened a PR yet — exactly the blind spot named in the design's
    /// evidence ("conflicts a producing worker resolves before
    /// opening its PR").
    pub fn record_producer_side_conflict(&self, input: ProducerConflictInsertInput) -> Result<ConflictResolution> {
        let execution = self.get_execution(&input.execution_id)?;
        let work_item = self.get_work_item(&execution.work_item_id)?;
        let (product_id, existing_pr_url) = match work_item {
            WorkItem::Task(t) | WorkItem::Chore(t) => (t.product_id, t.pr_url),
            other => bail!(
                "execution {} work item {} is a {}, not a task/chore",
                input.execution_id,
                execution.work_item_id,
                other.primary_id(),
            ),
        };
        let pr_url = execution.pr_url.or(existing_pr_url).unwrap_or_default();
        let pr_number = boss_github::pr_url::pr_number_from_url(&pr_url)
            .map(|n| n as i64)
            .unwrap_or(0);

        let diagnosis = crate::conflict_diagnosis::ConflictDiagnosis {
            schema_version: 1,
            base_sha: "unknown".to_owned(),
            head_sha: "unknown".to_owned(),
            files: input
                .conflicted_files
                .iter()
                .map(|path| crate::conflict_diagnosis::ConflictedFile {
                    path: path.clone(),
                    marker_count: None,
                    shape: "content".to_owned(),
                })
                .collect(),
            error: None,
        };
        let conflict_class = crate::conflict_diagnosis::classify_conflict_class(&input.conflicted_files);
        let diagnosis_json =
            serde_json::to_string(&diagnosis).context("failed to serialize producer-side conflict diagnosis")?;

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let id = next_id("crz");
        let now = now_string();
        tx.execute(
            "INSERT INTO conflict_resolutions
                (id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch,
                 status, cube_lease_id, cube_workspace_id, conflict_diagnosis,
                 created_at, started_at, finished_at, event_source, conflict_class, resolved_by_rung)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'succeeded', ?8, ?9, ?10,
                     ?11, ?12, ?13, 'producer_rebase', ?14, ?15)",
            params![
                id,
                product_id,
                execution.work_item_id,
                pr_url,
                pr_number,
                input.head_branch,
                input.base_branch,
                execution.cube_lease_id,
                execution.cube_workspace_id,
                diagnosis_json,
                now,
                now,
                now,
                conflict_class,
                RUNG_FULL_WORKER,
            ],
        )?;
        let inserted = query_conflict_resolution(&tx, &id)?
            .with_context(|| format!("unknown conflict_resolution after producer-side insert: {id}"))?;
        tx.commit()?;
        Ok(inserted)
    }
}
