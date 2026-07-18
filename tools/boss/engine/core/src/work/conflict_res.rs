use super::*;

/// The full-worker fallback rung (design's rung 3). Rung 1 (engine-direct
/// mechanical rebase) and rung 0 (deterministic resolvers,
/// `crate::conflict_ladder::attempt_rung0`, live by default as of
/// `RUNG0_APPLY_LIVE`) are both live on the `conflict_watch` path when the
/// `conflict_ladder_mechanical_rebase` feature flag is enabled; rung 2 (the
/// small pre-staged agent) is also live, bounded to a single residual file
/// (T6 of `merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`).
/// Every conflict not resolved by a mechanical rung — or evaluated while the
/// ladder itself is disabled — still falls through to a full worker doing
/// the whole job by hand.
const RUNG_FULL_WORKER: i64 = 3;

/// One candidate row read by
/// [`WorkDb::reconcile_orphaned_conflict_ladder_attempts`]:
/// `(work_item_id, product_id, pr_url, blocked_attempt_id, attempt_status,
/// mechanical_rung_in_flight)`. `attempt_status` is `None` when
/// `blocked_attempt_id` points at a row that no longer exists.
type OrphanedLadderCandidate = (String, String, String, Option<String>, Option<String>, Option<i64>);

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
        // Excludes `event_source = 'speculative_predicted'` rows: those are
        // telemetry-only observations from the Layer 4 speculative-rebase
        // sweep (T10), never a live resolution attempt, and must not eat
        // into the churn budget that gates *real* conflict-resolution
        // cycles. Without this exclusion a burst of speculative predictions
        // for one work item could pre-abandon its next genuine attempt.
        let recent_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM conflict_resolutions
              WHERE work_item_id = ?1
                AND CAST(created_at AS INTEGER) >= ?2
                AND event_source != 'speculative_predicted'",
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
    ///
    /// Clears `resolved_by_rung` back to `NULL`: rung 2's up-front stamp
    /// (see [`Self::stamp_conflict_resolution_rung`]) records the rung that
    /// is *attempting* the resolution, not one that has resolved anything
    /// yet. A `failed` attempt was never resolved by any rung, so leaving a
    /// premature stamp on it would over-count rung 2 in telemetry that
    /// reads `resolved_by_rung` without also filtering `status =
    /// 'succeeded'`. This matches rung 1's convention, which only ever
    /// stamps a rung on the succeeded transition.
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
                SET status           = 'failed',
                    failure_reason   = ?2,
                    finished_at      = COALESCE(finished_at, ?3),
                    resolved_by_rung = NULL
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
        self.mark_conflict_resolution_succeeded_at_rung(attempt_id, head_sha_after, RUNG_FULL_WORKER)
    }

    /// Auto-retire an attempt at a specific escalation-ladder rung (design's
    /// rungs 0–3). Identical to [`Self::mark_conflict_resolution_succeeded`]
    /// but records `resolved_by_rung = rung` for a resolution the engine
    /// produced without the full-worker path — the rung-1 engine-direct
    /// mechanical rebase (T4) passes `1`, deterministic resolvers (rung 0)
    /// pass `0`. `resolved_by_rung` is `COALESCE`d so a rung already stamped
    /// on the row (e.g. by the harness before the poller's retire) wins over
    /// a later default.
    pub fn mark_conflict_resolution_succeeded_at_rung(
        &self,
        attempt_id: &str,
        head_sha_after: Option<&str>,
        rung: i64,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET status                    = 'succeeded',
                    head_sha_after            = COALESCE(?2, head_sha_after),
                    finished_at               = COALESCE(finished_at, ?3),
                    resolved_by_rung          = COALESCE(resolved_by_rung, ?4),
                    mechanical_rung_in_flight = NULL
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, head_sha_after, now, rung],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Close the revision task a resolved merge-conflict attempt spawned,
    /// so a retired `conflict_resolutions` row never leaves a stale
    /// `todo`/`active`/`blocked` revision behind it. Without this, the
    /// parent's "in revision" badge (driven by
    /// [`find_latest_active_revision_in_chain`]'s "status != done" rule)
    /// persists until the chain root's PR *actually* merges — the only
    /// other point a stale revision gets closed
    /// ([`block_pending_revisions_on_parent_close`]) — which can be long
    /// after the conflict that spawned it was resolved, or, if the
    /// revision task never advances on its own, permanently.
    ///
    /// Every revision this retire path can reach was spawned with
    /// `created_via = CREATED_VIA_MERGE_CONFLICT_PREFIX…`
    /// ([`crate::conflict_watch::maybe_spawn_conflict_revision`]) — i.e.
    /// [`is_moot_revision_kind`] by construction: a conflicting PR cannot
    /// merge while still conflicted, so by the time the conflict resolved
    /// the revision's job (if any) was already done. This mirrors
    /// [`resolve_revision_on_parent_close`]'s moot branch, just triggered
    /// by "the conflict resolved" instead of "the parent PR merged".
    ///
    /// A no-op (`Ok(None)`) when:
    /// - the task can't be found, or
    /// - it is already terminal (`done`/`archived`/`cancelled`) or
    ///   `in_review` (its commit will ride the eventual merge via
    ///   [`flip_in_review_revisions_to_done`] instead), or
    /// - a worker is still genuinely driving it (a `running` /
    ///   `waiting_human` execution) — closing the row out from under a
    ///   live worker is not this function's job; that execution's own
    ///   on-Stop completion advances the task normally when the worker's
    ///   turn ends.
    pub fn close_resolved_conflict_revision(&self, revision_task_id: &str) -> Result<Option<Task>> {
        if self.get_live_execution_for_work_item(revision_task_id, "")?.is_some() {
            return Ok(None);
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let Some(rev) = query_task(&tx, revision_task_id)? else {
            return Ok(None);
        };
        if rev.deleted_at.is_some()
            || matches!(
                rev.status,
                TaskStatus::Done | TaskStatus::Archived | TaskStatus::Cancelled | TaskStatus::InReview
            )
        {
            return Ok(None);
        }
        let now = now_string();
        let rows_changed = archive_revision_task(
            &tx,
            &rev.id,
            &now,
            "merge conflict resolved before parent PR merged; revision moot",
        )?;
        if rows_changed == 0 {
            return Ok(None);
        }
        let updated = query_task(&tx, revision_task_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Stamp `resolved_by_rung` on a still-live attempt *before* the
    /// resolution has actually completed — used by the escalation-ladder
    /// harness (T6) when it hands the conflict to a rung-2 small focused
    /// agent, so that whichever path later calls
    /// [`Self::mark_conflict_resolution_succeeded`] (which defaults to rung
    /// 3, the full-worker rung) finds `resolved_by_rung` already set and
    /// the `COALESCE` in that method preserves `2` instead of overwriting
    /// it. Does not touch `status` — the attempt stays `pending`/`running`
    /// until the spawned revision actually finishes. Idempotent no-op
    /// (`Ok(None)`) once the row is terminal.
    ///
    /// This early stamp is provisional, not a claim of success:
    /// [`Self::mark_conflict_resolution_failed`] and
    /// [`Self::mark_conflict_resolution_abandoned`] both clear
    /// `resolved_by_rung` back to `NULL` on their terminal transition, so a
    /// rung-2 attempt that never actually resolves the conflict never ends
    /// up mislabeled `resolved_by_rung = 2` — only `succeeded` rows carry a
    /// rung stamp, matching rung 1's convention.
    pub fn stamp_conflict_resolution_rung(&self, attempt_id: &str, rung: i64) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET resolved_by_rung = COALESCE(resolved_by_rung, ?2)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, rung],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Persist that a **mechanical** escalation-ladder rung (0 =
    /// deterministic resolvers, 1 = engine-direct rebase) is now in flight
    /// for this attempt, stamping the leased `cube_lease_id` /
    /// `cube_workspace_id` alongside the rung marker. The mechanical rungs
    /// (`crate::conflict_ladder`) run *inline* in the engine process with no
    /// dispatched worker and no `revision_task_id`; without this durable
    /// marker an attempt killed mid-rung by an engine restart is
    /// indistinguishable from a fresh `pending` row and is silently
    /// stranded — the 2026-07-18 flunge incident, where a rung-0 attempt
    /// vanished with no verdict and left its parent `blocked:
    /// merge_conflict` pointing at a dead attempt forever.
    ///
    /// Does not change `status` (kept `pending`/`running` so the existing
    /// fall-through-to-worker paths read identically) — only the marker and
    /// lease columns. Overwrites on a second call, so escalating rung 1 →
    /// rung 0 simply re-stamps the new rung. Guarded to non-terminal rows;
    /// `Ok(None)` once the attempt is terminal. Cleared by
    /// [`Self::clear_conflict_resolution_mechanical_rung`] the moment the
    /// rung concludes, and by every terminal transition.
    pub fn stamp_conflict_resolution_mechanical_rung(
        &self,
        attempt_id: &str,
        rung: i64,
        cube_lease_id: &str,
        cube_workspace_id: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET mechanical_rung_in_flight = ?2,
                    cube_lease_id             = ?3,
                    cube_workspace_id         = ?4
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, rung, cube_lease_id, cube_workspace_id],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Clear the mechanical-rung-in-flight marker (and the mechanical
    /// lease/workspace it stamped) once a mechanical rung has concluded —
    /// whether it retired the attempt, halted it for sign-off, or fell
    /// through to a worker. Called unconditionally by
    /// [`crate::conflict_ladder::try_mechanical_rungs`] right before it
    /// releases the leased workspace, so a row only ever carries a
    /// non-`NULL` `mechanical_rung_in_flight` while a rung is genuinely
    /// executing. Nulling the lease/workspace here is safe: the mechanical
    /// lease is released immediately afterwards, and a fall-through worker
    /// re-stamps its own lease via
    /// [`Self::mark_conflict_resolution_running`]. No status guard — this
    /// must succeed even after the retire transition already set the row
    /// terminal. `Ok(None)` when the id is unknown.
    pub fn clear_conflict_resolution_mechanical_rung(&self, attempt_id: &str) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET mechanical_rung_in_flight = NULL,
                    cube_lease_id             = NULL,
                    cube_workspace_id         = NULL
              WHERE id = ?1",
            params![attempt_id],
        )?;
        finish_attempt_update(tx, rows, attempt_id, query_conflict_resolution)
    }

    /// Engine-side abandon: flip a non-terminal attempt to `abandoned`
    /// with the provided reason. Used for "we stepped away on purpose"
    /// terminations (parent PR closed, parent merged externally,
    /// manual override) where `failed` would be misleading. Idempotent.
    ///
    /// Clears `resolved_by_rung` back to `NULL` for the same reason as
    /// [`Self::mark_conflict_resolution_failed`]: an abandoned attempt was
    /// never resolved by any rung, so a rung-2 up-front stamp must not
    /// survive onto it.
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
                SET status           = 'abandoned',
                    failure_reason   = ?2,
                    finished_at      = COALESCE(finished_at, ?3),
                    resolved_by_rung = NULL
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

    /// Invalidate a **terminal `succeeded`** attempt whose resolution did not
    /// hold: the PR is CONFLICTING again at the SAME `(base_sha_at_trigger,
    /// head_sha_before)` UNIQUE key, so the succeeded row still occupies that
    /// slot and `insert_conflict_resolution` can never create a fresh attempt
    /// for the re-conflicting PR — the `conflict_watch` stale-base re-arm
    /// wedge (mono#1398/#1764): "succeeded crz but PR still CONFLICTING",
    /// re-detected every ~6s forever.
    ///
    /// Flips the row `succeeded → failed` with `reason` and NULLs
    /// `base_sha_at_trigger` (SQLite treats NULL as distinct in UNIQUE
    /// constraints — see [`Self::abandon_conflict_resolution_for_supersede`])
    /// so the fall-through INSERT can land exactly one fresh, churn-guarded
    /// attempt at the same key. Unlike every other terminal transition here
    /// (which all guard `status IN ('pending','running')`), this deliberately
    /// targets a `succeeded` row — the false-success record — because no other
    /// primitive can free a slot a stale success is holding. Clears
    /// `resolved_by_rung` to match the failed-transition convention. Idempotent
    /// `Ok(None)` when the row is no longer `succeeded`.
    pub fn invalidate_stale_succeeded_conflict_resolution(
        &self,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET status              = 'failed',
                    failure_reason      = ?2,
                    base_sha_at_trigger = NULL,
                    resolved_by_rung    = NULL,
                    finished_at         = COALESCE(finished_at, ?3)
              WHERE id = ?1
                AND status = 'succeeded'",
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
        ListFilterQuery::new(format!(
            "SELECT {CONFLICT_RESOLUTION_COLUMNS} FROM conflict_resolutions WHERE 1=1"
        ))
        .filter_product_id(product_id)
        .filter_work_item_id(work_item_id)
        .filter_status_in(statuses)
        .order_by_created_desc()
        .limit(limit)
        .collect(&conn, map_conflict_resolution)
    }

    /// Aggregate `conflict_resolutions.conflict_diagnosis` for one
    /// product into a hotspot report (Layer 0 telemetry, T5): per-file
    /// conflict frequency, per-file-_pair_ co-conflict frequency, and
    /// per-class counts. Backs `boss engine conflicts hotspots`.
    ///
    /// Always scoped to a single `product_id` — never a cross-product
    /// blend (design's hard requirement; hotspot data is only
    /// meaningful within one repo). `top_n` caps each ranked list
    /// (file/pair frequency) to its highest-count entries; class
    /// counts are never truncated since there are only a handful of
    /// classes. Rows with no `conflict_diagnosis` still count toward
    /// `total_events` and `class_counts` (via the `conflict_class`
    /// column) but contribute nothing to file/pair frequency.
    pub fn conflict_hotspots(&self, product_id: &str, top_n: usize) -> Result<ConflictHotspotReport> {
        let conn = self.connect()?;
        let mut stmt =
            conn.prepare("SELECT conflict_diagnosis, conflict_class FROM conflict_resolutions WHERE product_id = ?1")?;
        let rows = stmt.query_map(params![product_id], |row| {
            let diagnosis_json: Option<String> = row.get(0)?;
            let conflict_class: Option<String> = row.get(1)?;
            Ok((diagnosis_json, conflict_class))
        })?;

        let mut total_events: u64 = 0;
        let mut file_counts: HashMap<String, u64> = HashMap::new();
        let mut pair_counts: HashMap<(String, String), u64> = HashMap::new();
        let mut class_counts: HashMap<String, u64> = HashMap::new();

        for row in rows {
            let (diagnosis_json, conflict_class) = row?;
            total_events += 1;
            *class_counts
                .entry(conflict_class.unwrap_or_else(|| "unknown".to_owned()))
                .or_insert(0) += 1;

            let Some(json) = diagnosis_json else { continue };
            let Ok(diagnosis) = serde_json::from_str::<crate::conflict_diagnosis::ConflictDiagnosis>(&json) else {
                continue;
            };
            let mut paths: Vec<String> = diagnosis.files.into_iter().map(|f| f.path).collect();
            paths.sort();
            paths.dedup();
            for path in &paths {
                *file_counts.entry(path.clone()).or_insert(0) += 1;
            }
            for i in 0..paths.len() {
                for j in (i + 1)..paths.len() {
                    *pair_counts.entry((paths[i].clone(), paths[j].clone())).or_insert(0) += 1;
                }
            }
        }

        let mut file_frequency: Vec<ConflictFileFrequency> = file_counts
            .into_iter()
            .map(|(path, count)| ConflictFileFrequency { path, count })
            .collect();
        file_frequency.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
        file_frequency.truncate(top_n);

        let mut file_pair_frequency: Vec<ConflictFilePairFrequency> = pair_counts
            .into_iter()
            .map(|((path_a, path_b), count)| ConflictFilePairFrequency { path_a, path_b, count })
            .collect();
        file_pair_frequency.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| (a.path_a.as_str(), a.path_b.as_str()).cmp(&(b.path_a.as_str(), b.path_b.as_str())))
        });
        file_pair_frequency.truncate(top_n);

        let mut class_counts_vec: Vec<ConflictClassCount> = class_counts
            .into_iter()
            .map(|(class, count)| ConflictClassCount { class, count })
            .collect();
        class_counts_vec.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.class.cmp(&b.class)));

        Ok(ConflictHotspotReport {
            product_id: product_id.to_owned(),
            total_events,
            file_frequency,
            file_pair_frequency,
            class_counts: class_counts_vec,
        })
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

    /// Record a speculative-conflict-prediction event: the merge poller's
    /// Layer 4 sweep (T10) ran a throwaway, no-push engine-direct rebase of
    /// an in-review PR against current `main` and it came back conflicted.
    /// This is purely a telemetry observation — it never touches the parent
    /// task's status, never gates the escalation ladder, and is inserted
    /// already terminal (`status = 'predicted'`, a status value no
    /// dispatcher or lifecycle query ever matches: `active_conflict_
    /// resolution_for_work_item` only reads `pending`/`running`, and the
    /// churn guard above explicitly excludes `event_source =
    /// 'speculative_predicted'` rows). Feeds the hotspot report
    /// (`conflict_hotspots`) with signal from a PR *before* it would
    /// otherwise reach `conflict_watch`.
    ///
    /// `head_branch`/`base_branch` are stamped `"unknown"` — the speculative
    /// sweep only has a PR number and a leased workspace, not the branch
    /// names GitHub would return from an extra probe; they don't feed
    /// classification or aggregation (only `conflict_diagnosis`'s file
    /// paths do).
    pub fn record_speculative_conflict_prediction(
        &self,
        input: SpeculativeConflictInsertInput,
    ) -> Result<ConflictResolution> {
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
            serde_json::to_string(&diagnosis).context("failed to serialize speculative conflict diagnosis")?;

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let id = next_id("crz");
        let now = now_string();
        tx.execute(
            "INSERT INTO conflict_resolutions
                (id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch,
                 status, conflict_diagnosis, created_at, finished_at, event_source, conflict_class)
             VALUES (?1, ?2, ?3, ?4, ?5, 'unknown', 'unknown', 'predicted', ?6,
                     ?7, ?8, 'speculative_predicted', ?9)",
            params![
                id,
                input.product_id,
                input.work_item_id,
                input.pr_url,
                input.pr_number,
                diagnosis_json,
                now,
                now,
                conflict_class,
            ],
        )?;
        let inserted = query_conflict_resolution(&tx, &id)?
            .with_context(|| format!("unknown conflict_resolution after speculative-prediction insert: {id}"))?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Engine-startup reconciliation for orphaned conflict-ladder attempts
    /// (the 2026-07-18 flunge restart incident). The mechanical rungs (0/1)
    /// run *inline* in the engine process — no dispatched worker, no
    /// `revision_task_id` — so if the engine restarts mid-rung the
    /// `conflict_resolutions` row is left non-terminal, the parent stays
    /// `blocked: merge_conflict` pointing at it via `blocked_attempt_id`,
    /// and nothing recovers it: the merge poller re-probes the blocked
    /// parent, but `conflict_watch::on_conflict_detected`'s re-arm path
    /// treats a pending/no-revision attempt as an "old-style crz still in
    /// flight" and declines to dispatch, forever.
    ///
    /// This one-shot startup sweep breaks that wedge. For every task
    /// `blocked: merge_conflict` whose `blocked_attempt_id` points at either
    /// (a) a row that no longer exists, or (b) a non-terminal attempt with
    /// `revision_task_id IS NULL` (an inline mechanical-rung attempt or a
    /// bare pending row that no live driver owns), it:
    ///   1. abandons the orphaned attempt and frees its UNIQUE idempotency
    ///      slot (nullifying `base_sha_at_trigger`, like
    ///      [`Self::abandon_conflict_resolution_for_supersede`]) so a fresh
    ///      attempt can land at the same key,
    ///   2. flips the parent back to `in_review` and clears
    ///      `blocked_attempt_id` + the side-table `merge_conflict` signal,
    ///      so the next merge-poller sweep re-detects the still-open
    ///      conflict and re-enters the ladder cleanly, and
    ///   3. emits an explicit trace line per recovered attempt so the death
    ///      is observable instead of silent.
    ///
    /// Revision-backed non-terminal attempts (`revision_task_id` set) are
    /// deliberately left alone: a dispatched revision worker owns them, and
    /// `conflict_watch::supersede_if_stale` already recovers one whose
    /// revision died in a restart on the next detection pass. Terminal
    /// attempts (churn-exhausted, human-owned) are also left alone — a
    /// `blocked_attempt_id` pointing at a terminal row is a legitimate
    /// resting state, not a wedge.
    ///
    /// Safe to run only at startup, before the poller/watch loops spawn:
    /// the mechanical rungs never overlap it, so any matching row is
    /// definitively orphaned (nothing in-process is driving it).
    pub fn reconcile_orphaned_conflict_ladder_attempts(&self) -> Result<Vec<RecoveredConflictLadderAttempt>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let candidates: Vec<OrphanedLadderCandidate> = {
            let mut stmt = tx.prepare(
                "SELECT t.id, t.product_id, t.pr_url, t.blocked_attempt_id,
                        cr.status, cr.mechanical_rung_in_flight
                 FROM tasks t
                 LEFT JOIN conflict_resolutions cr ON cr.id = t.blocked_attempt_id
                 WHERE t.status = 'blocked'
                   AND t.blocked_reason = 'merge_conflict'
                   AND t.blocked_attempt_id IS NOT NULL
                   AND t.deleted_at IS NULL
                   AND (
                         cr.id IS NULL
                      OR (cr.status IN ('pending', 'running') AND cr.revision_task_id IS NULL)
                   )",
            )?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,         // work_item_id
                    row.get::<_, String>(1)?,         // product_id
                    row.get::<_, String>(2)?,         // pr_url
                    row.get::<_, Option<String>>(3)?, // blocked_attempt_id
                    row.get::<_, Option<String>>(4)?, // cr.status (None when the row is missing)
                    row.get::<_, Option<i64>>(5)?,    // mechanical_rung_in_flight
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let now = now_string();
        let mut recovered = Vec::new();
        for (work_item_id, product_id, pr_url, blocked_attempt_id, attempt_status, rung) in candidates {
            // Abandon the orphaned attempt when it still exists, freeing its
            // UNIQUE (work_item_id, base_sha_at_trigger, head_sha_before)
            // slot so the fall-through re-detect can insert a fresh attempt
            // at the same key.
            if attempt_status.is_some()
                && let Some(attempt_id) = blocked_attempt_id.as_deref()
            {
                tx.execute(
                    "UPDATE conflict_resolutions
                        SET status                    = 'abandoned',
                            failure_reason            = 'engine_restart_orphaned_ladder_attempt',
                            base_sha_at_trigger       = NULL,
                            resolved_by_rung          = NULL,
                            mechanical_rung_in_flight = NULL,
                            finished_at               = COALESCE(finished_at, ?2)
                      WHERE id = ?1
                        AND status IN ('pending', 'running')",
                    params![attempt_id, now],
                )?;
            }
            // Flip the parent back to in_review + clear the attempt pointer.
            // Guarded so a row a human moved out of blocked:merge_conflict in
            // the meantime is left alone.
            let flipped = tx.execute(
                "UPDATE tasks
                    SET status             = 'in_review',
                        blocked_reason     = NULL,
                        blocked_attempt_id = NULL,
                        last_status_actor  = 'engine',
                        updated_at         = ?2
                  WHERE id = ?1
                    AND status = 'blocked'
                    AND blocked_reason = 'merge_conflict'
                    AND deleted_at IS NULL",
                params![work_item_id, now],
            )?;
            if flipped == 0 {
                continue;
            }
            // Clear the side-table merge_conflict signal so the polymorphic
            // clear dispatch doesn't re-fire (mirrors
            // `clear_chore_blocked_merge_conflict`).
            tx.execute(
                "UPDATE task_blocked_signals
                    SET cleared_at = ?2
                  WHERE work_item_id = ?1
                    AND reason = 'merge_conflict'
                    AND cleared_at IS NULL",
                params![work_item_id, now],
            )?;
            tracing::warn!(
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                attempt_id = ?blocked_attempt_id,
                mechanical_rung_in_flight = ?rung,
                "conflict_ladder: recovered an orphaned conflict-ladder attempt at startup — the previous \
                 engine died mid-attempt (restart) and left the parent blocked:merge_conflict pointing at a \
                 dead attempt; abandoned it, freed its idempotency slot, and flipped the parent back to \
                 in_review so the watcher re-detects and re-enters the ladder",
            );
            recovered.push(RecoveredConflictLadderAttempt {
                work_item_id,
                product_id,
                pr_url,
                attempt_id: blocked_attempt_id,
                rung,
            });
        }
        tx.commit()?;
        Ok(recovered)
    }
}
