use super::*;

/// One scheduler decision to persist via
/// [`WorkDb::record_automation_run_and_advance`]. All timestamps are UTC
/// epoch seconds (stored as strings, matching the rest of the schema).
///
/// Uses the repo builder convention (`bon`) since it carries more than 5 fields;
/// `Option` fields default to `None`, so a caller only sets what applies to
/// the decision it is recording.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct AutomationFireRecord {
    pub automation_id: String,
    /// The cron occurrence this run satisfies (UTC epoch seconds). Doubles
    /// as the at-most-once dedupe key with `automation_id`.
    pub scheduled_for: i64,
    /// When the scheduler recorded this decision (UTC epoch seconds).
    pub started_at: i64,
    /// One of the `AUTOMATION_OUTCOME_*` discriminators.
    pub outcome: String,
    pub triage_execution_id: Option<String>,
    pub produced_task_id: Option<String>,
    pub finished_at: Option<i64>,
    pub detail: Option<String>,
    /// `Some(next_occurrence)` advances `automations.next_due_at`; `None`
    /// holds the current occurrence (used for transient-failure retry).
    pub next_due_at: Option<i64>,
}

const AUTOMATION_SELECT: &str = "
    SELECT id, short_id, product_id, name, repo_remote_url,
           trigger_kind, trigger_config, standing_instruction,
           open_task_limit, catch_up_window_secs, enabled,
           created_via, created_at, updated_at,
           last_fired_at, last_outcome, next_due_at
    FROM automations";

pub(crate) fn query_automation(conn: &Connection, id: &str) -> Result<Option<boss_protocol::Automation>> {
    let sql = format!("{AUTOMATION_SELECT} WHERE id = ?1");
    conn.query_row(&sql, [id], map_automation)
        .optional()
        .map_err(Into::into)
}

impl WorkDb {
    /// Create a new automation and return the inserted row.
    pub fn create_automation(&self, input: CreateAutomationInput) -> Result<boss_protocol::Automation> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("auto");
        let now = now_string();
        let short_id = allocate_automation_short_id(&tx, &input.product_id)?;
        let (trigger_kind, trigger_config) = automation_trigger_to_db(&input.trigger)?;
        let repo_remote_url = canonicalize_repo_remote_url(input.repo_remote_url);
        let created_via = input.created_via.unwrap_or_else(|| CREATED_VIA_UNKNOWN.to_owned());

        tx.execute(
            "INSERT INTO automations
                 (id, short_id, product_id, name, repo_remote_url,
                  trigger_kind, trigger_config, standing_instruction,
                  open_task_limit, catch_up_window_secs, enabled,
                  created_via, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
            params![
                id,
                short_id,
                input.product_id,
                input.name,
                repo_remote_url,
                trigger_kind,
                trigger_config,
                input.standing_instruction,
                input.open_task_limit,
                input.catch_up_window_secs,
                input.enabled as i64,
                created_via,
                now,
            ],
        )?;

        let automation =
            query_automation(&tx, &id)?.with_context(|| format!("missing automation after insert: {id}"))?;
        tx.commit()?;
        Ok(automation)
    }

    /// List all automations for a product, ordered by `created_at ASC`.
    pub fn list_automations(&self, product_id: &str) -> Result<Vec<boss_protocol::Automation>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let sql = format!("{AUTOMATION_SELECT} WHERE product_id = ?1 ORDER BY created_at ASC, id ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([product_id], map_automation)?;
        collect_rows(rows)
    }

    /// Like [`list_automations`] but also returns each automation's current
    /// open-task count in one round-trip using a correlated subquery.
    pub fn list_automations_with_open_task_counts(
        &self,
        product_id: &str,
    ) -> Result<Vec<(boss_protocol::Automation, i64)>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let sql = "SELECT id, short_id, product_id, name, repo_remote_url,
                    trigger_kind, trigger_config, standing_instruction,
                    open_task_limit, catch_up_window_secs, enabled,
                    created_via, created_at, updated_at,
                    last_fired_at, last_outcome, next_due_at,
                    (SELECT COUNT(*) FROM tasks
                      WHERE source_automation_id = automations.id
                        AND status IN ('todo', 'ready', 'active', 'in_review', 'blocked')
                        AND deleted_at IS NULL) AS open_task_count
             FROM automations
             WHERE product_id = ?1
             ORDER BY created_at ASC, id ASC";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([product_id], |row| {
            let automation = map_automation(row)?;
            let count: i64 = row.get(17)?;
            Ok((automation, count))
        })?;
        collect_rows(rows)
    }

    /// Fetch a single automation by its canonical id.
    pub fn get_automation(&self, id: &str) -> Result<Option<boss_protocol::Automation>> {
        let conn = self.connect()?;
        query_automation(&conn, id)
    }

    /// Apply a patch to an automation. Only `Some` fields are updated.
    pub fn update_automation(&self, id: &str, patch: AutomationPatch) -> Result<boss_protocol::Automation> {
        let conn = self.connect()?;
        let existing = query_automation(&conn, id).require("automation", id)?;

        let now = now_string();

        // Resolve trigger columns only when the trigger is being updated.
        let (trigger_kind, trigger_config) = if let Some(ref trigger) = patch.trigger {
            let (k, c) = automation_trigger_to_db(trigger)?;
            (Some(k), Some(c))
        } else {
            (None, None)
        };

        // Build SET clauses dynamically so we only touch provided fields.
        let mut sets: Vec<String> = vec!["updated_at = ?1".to_owned()];
        let mut params_raw: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now.clone())];
        let mut idx = 2usize;

        macro_rules! push_opt {
            ($field:expr, $val:expr) => {
                if let Some(v) = $val {
                    sets.push(format!("{} = ?{idx}", $field));
                    params_raw.push(Box::new(v));
                    idx += 1;
                }
            };
        }

        push_opt!("name", patch.name.clone());
        push_opt!(
            "repo_remote_url",
            canonicalize_repo_remote_url(patch.repo_remote_url.clone())
        );
        if let (Some(trigger_kind), Some(trigger_config)) = (trigger_kind, trigger_config) {
            sets.push(format!("trigger_kind = ?{idx}"));
            params_raw.push(Box::new(trigger_kind));
            idx += 1;
            sets.push(format!("trigger_config = ?{idx}"));
            params_raw.push(Box::new(trigger_config));
            idx += 1;
            // Reset next_due_at so the scheduler recomputes the first occurrence
            // from the new cron expression instead of using a stale value from
            // the old schedule.
            sets.push("next_due_at = NULL".to_owned());
        }
        push_opt!("standing_instruction", patch.standing_instruction.clone());
        push_opt!("open_task_limit", patch.open_task_limit);
        // catch_up_window_secs: Option<Option<i64>> would be needed for
        // "clear to null", but AutomationPatch uses Option<i64> which means
        // "set to this value" (None = leave unchanged). Scheduler can still
        // fall back to the engine default if the column is NULL.
        push_opt!("catch_up_window_secs", patch.catch_up_window_secs);
        push_opt!("enabled", patch.enabled.map(|b| b as i64));

        // id param goes at the end
        params_raw.push(Box::new(existing.id.clone()));
        let id_idx = idx;

        let sql = format!("UPDATE automations SET {} WHERE id = ?{id_idx}", sets.join(", "));

        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_raw.iter().map(|b| b.as_ref() as &dyn rusqlite::ToSql).collect();
        conn.execute(&sql, params_refs.as_slice())?;

        query_automation(&conn, id)?.with_context(|| format!("missing automation after update: {id}"))
    }

    /// Set `enabled = true` on an automation.
    pub fn enable_automation(&self, id: &str) -> Result<boss_protocol::Automation> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, id).require("automation", id)?;
        let now = now_string();
        conn.execute(
            "UPDATE automations SET enabled = 1, updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        query_automation(&conn, id)?.with_context(|| format!("missing automation after enable: {id}"))
    }

    /// Set `enabled = false` on an automation.
    pub fn disable_automation(&self, id: &str) -> Result<boss_protocol::Automation> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, id).require("automation", id)?;
        let now = now_string();
        conn.execute(
            "UPDATE automations SET enabled = 0, updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        query_automation(&conn, id)?.with_context(|| format!("missing automation after disable: {id}"))
    }

    /// Hard-delete an automation row. Also removes any `automation_runs` rows
    /// (ON DELETE CASCADE would handle this, but the FK is not `ON DELETE`
    /// constrained in the schema; we delete explicitly for safety).
    /// Tasks that were produced by this automation keep their
    /// `source_automation_id` value — they are orphaned from the automation
    /// but continue through their lifecycle normally.
    pub fn delete_automation(&self, id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _existing = query_automation(&tx, id).require("automation", id)?;
        tx.execute("DELETE FROM automation_runs WHERE automation_id = ?1", [id])?;
        tx.execute("DELETE FROM automations WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Count how many tasks produced by `automation_id` are currently open.
    /// "Open" = any non-terminal status: `todo`, `ready`, `active` (doing),
    /// `in_review`, `blocked`. Terminal statuses (`done`, `cancelled`,
    /// `archived`) are excluded. Note: the kanban label "doing" maps to the
    /// DB value `active`; the query uses the stored value.
    /// Used by the scheduler to enforce `open_task_limit` at fire time.
    pub fn count_open_tasks_for_automation(&self, automation_id: &str) -> Result<i64> {
        let conn = self.connect()?;
        let sql = format!(
            "SELECT COUNT(*) FROM tasks
              WHERE source_automation_id = ?1
                AND status IN ({OPEN_SIBLING_STATUSES})
                AND deleted_at IS NULL"
        );
        let count: i64 = conn.query_row(&sql, [automation_id], |row| row.get(0))?;
        Ok(count)
    }

    /// Find the most recently created open task for an automation, or `None` if
    /// none exist. "Open" = any non-terminal status (`todo`, `ready`, `active`,
    /// `in_review`, `blocked`). Ordered by `created_at DESC` so the newest task
    /// is returned when multiple open tasks exist (possible when
    /// `open_task_limit > 1`).
    ///
    /// Used by the triage finalizer's marker-recovery path: when a triage run
    /// ends without a decision marker but DID create a task (the worker ran
    /// `boss task create --automation` then stopped before emitting the marker),
    /// the finalizer calls this to record `produced_task` instead of
    /// `failed_will_retry`, preventing the retry loop from over-producing
    /// duplicate tasks until the open-task cap is full.
    pub fn find_most_recent_open_task_for_automation(
        &self,
        automation_id: &str,
    ) -> Result<Option<boss_protocol::Task>> {
        let conn = self.connect()?;
        let sql = format!(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state, merge_queue_detail, driver,
                    source_automation_id
               FROM tasks
              WHERE source_automation_id = ?1
                AND status IN ({OPEN_SIBLING_STATUSES})
                AND deleted_at IS NULL
              ORDER BY created_at DESC, id DESC
              LIMIT 1"
        );
        conn.query_row(&sql, [automation_id], map_task_with_source_automation_id)
            .optional()
            .map_err(Into::into)
    }

    /// Everything this automation is already tracking: open tasks, plus
    /// tasks it resolved within the last [`SIBLING_RESOLVED_WINDOW_SECS`].
    ///
    /// Feeds the triage preamble. A triage agent is spawned fresh on every
    /// fire with no memory of previous fires, so left to itself it will
    /// re-derive — and re-file — the same finding indefinitely; that is
    /// what produced the audited duplicate clusters. Handing it this list
    /// is the difference between "there is a 3000-line file here" and
    /// "there is a 3000-line file here, and an open task already covers
    /// it".
    ///
    /// Resolved rows are included precisely because the hard gate in
    /// [`Self::create_automation_task`] cannot use them: a finding that
    /// really has recurred after being fixed must stay fileable, so
    /// closed siblings inform the agent's judgement rather than binding
    /// it.
    pub fn list_automation_sibling_tasks(&self, automation_id: &str) -> Result<Vec<AutomationSiblingTask>> {
        let conn = self.connect()?;
        let cutoff = boss_engine_utils::epoch_time::now_epoch_secs() - SIBLING_RESOLVED_WINDOW_SECS;
        // Open rows always qualify however old they are — an ancient open
        // task is *more* worth surfacing, not less. Resolved rows are
        // windowed on completion time, falling back to `updated_at` for
        // rows that predate the `completed_at` column. Open rows are
        // ordered ahead of resolved ones before the LIMIT is applied, so a
        // long-standing open task can never be pushed out by a pile of
        // recently-resolved ones — the hard gate in
        // `Self::create_automation_task` only ever refuses against open
        // rows, so those are exactly the ones truncation must not drop.
        let sql = format!(
            "SELECT short_id, name, status, pr_url
               FROM tasks
              WHERE source_automation_id = ?1
                AND deleted_at IS NULL
                AND (
                     status IN ({OPEN_SIBLING_STATUSES})
                     OR CAST(COALESCE(completed_at, updated_at) AS INTEGER) >= ?2
                )
              ORDER BY (status IN ({OPEN_SIBLING_STATUSES})) DESC, CAST(created_at AS INTEGER) DESC, id DESC
              LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![automation_id, cutoff, SIBLING_LIST_LIMIT], |row| {
            Ok(AutomationSiblingTask {
                short_id: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                name: row.get(1)?,
                status: row.get(2)?,
                pr_url: row.get(3)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Every task the dedup gate turned away for this automation, newest
    /// first. See [`AutomationDedupSuppression`] for why a gate whose
    /// whole effect is an absence needs a reader.
    pub fn list_automation_dedup_suppressions(&self, automation_id: &str) -> Result<Vec<AutomationDedupSuppression>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT automation_id, surviving_task_id, attempted_name, matched_on, match_key, created_at
               FROM automation_dedup_suppressions
              WHERE automation_id = ?1
              ORDER BY CAST(created_at AS INTEGER) DESC, id DESC",
        )?;
        let rows = stmt.query_map([automation_id], |row| {
            Ok(AutomationDedupSuppression {
                automation_id: row.get(0)?,
                surviving_task_id: row.get(1)?,
                attempted_name: row.get(2)?,
                matched_on: row.get(3)?,
                match_key: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Raw rows examined per [`list_automation_runs`] call, before
    /// retry-chain collapsing. Bounds the query itself (not just what gets
    /// rendered) against an automation with a very long fire history —
    /// the 2000+-row `redundant_spawn` storm this exists to survive.
    const AUTOMATION_RUNS_RAW_FETCH_LIMIT: i64 = 500;

    /// Collapsed rows returned to the caller (UI / CLI) after retry-chain
    /// collapsing. Small enough that a plain, non-virtualized list view
    /// never has to scroll through history to stay responsive.
    const AUTOMATION_RUNS_DISPLAY_LIMIT: usize = 50;

    /// List `automation_runs` rows for an automation, newest first,
    /// collapsing consecutive rows that share the same `outcome` and
    /// `produced_task_id` into a single entry with `repeat_count` set —
    /// e.g. 23 consecutive `failed_will_retry` rows become one "failed,
    /// retried 23x" entry instead of 23 rows. `produced_task_id` is part
    /// of the grouping key so distinct produced tasks are never merged
    /// together even if they happen to sit outcome-adjacent.
    pub fn list_automation_runs(&self, automation_id: &str) -> Result<Vec<boss_protocol::AutomationRun>> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, automation_id).require("automation", automation_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, automation_id, scheduled_for, started_at, finished_at,
                    triage_execution_id, outcome, produced_task_id, detail
               FROM automation_runs
              WHERE automation_id = ?1
              ORDER BY scheduled_for DESC, started_at DESC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![automation_id, Self::AUTOMATION_RUNS_RAW_FETCH_LIMIT],
            map_automation_run,
        )?;
        let raw: Vec<boss_protocol::AutomationRun> = collect_rows(rows)?;
        let collapsed = collapse_automation_run_retries(raw);
        Ok(collapsed
            .into_iter()
            .take(Self::AUTOMATION_RUNS_DISPLAY_LIMIT)
            .collect())
    }

    /// List tasks produced by an automation (`source_automation_id = ?`),
    /// ordered by `created_at DESC`. Includes non-deleted rows only.
    pub fn list_tasks_for_automation(&self, automation_id: &str) -> Result<Vec<boss_protocol::Task>> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, automation_id).require("automation", automation_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state, merge_queue_detail, driver,
                    source_automation_id
               FROM tasks
              WHERE source_automation_id = ?1
                AND deleted_at IS NULL
              ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([automation_id], map_task_with_source_automation_id)?;
        collect_rows(rows)
    }

    /// List open tasks produced by *any* automation for a product (cross-
    /// automation, unlike [`list_tasks_for_automation`] which is scoped to
    /// one), ordered newest first. Used by the triage preamble's layer-0
    /// context injection (automation-duplicate-work investigation, 2026-07-14)
    /// so a firing triage run can see in-flight work filed by *other*
    /// automations on the same product, not just its own — the
    /// cross-automation blindness that let two overlapping automations both
    /// file duplicate work items in the 2026-07-13 incident.
    pub fn list_open_automation_tasks_for_product(&self, product_id: &str) -> Result<Vec<boss_protocol::Task>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state, merge_queue_detail, driver,
                    source_automation_id
               FROM tasks
              WHERE product_id = ?1
                AND source_automation_id IS NOT NULL
                AND status IN ('todo', 'ready', 'active', 'in_review', 'blocked')
                AND deleted_at IS NULL
              ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([product_id], map_task_with_source_automation_id)?;
        collect_rows(rows)
    }

    /// List automation-sourced tasks for a product that reached `done` with a
    /// PR at or after `since_epoch` (UTC seconds), newest first. Used by the
    /// triage preamble's layer-0 context injection to show recently merged
    /// automation work — the "stale brief" half of the automation-duplicate-work
    /// investigation (§1.4): a run re-derives a target another automation
    /// already swept and merged hours earlier, because closed rows drop out of
    /// [`list_open_automation_tasks_for_product`] the moment they finish.
    /// `updated_at` is stored as an epoch-seconds string (see `now_string`),
    /// so the comparison casts it to INTEGER like [`list_due_automations`] does
    /// for `next_due_at`.
    pub fn list_recently_completed_automation_tasks_for_product(
        &self,
        product_id: &str,
        since_epoch: i64,
    ) -> Result<Vec<boss_protocol::Task>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state, merge_queue_detail, driver,
                    source_automation_id
               FROM tasks
              WHERE product_id = ?1
                AND source_automation_id IS NOT NULL
                AND status = 'done'
                AND pr_url IS NOT NULL AND pr_url != ''
                AND CAST(updated_at AS INTEGER) >= ?2
                AND deleted_at IS NULL
              ORDER BY CAST(updated_at AS INTEGER) DESC",
        )?;
        let rows = stmt.query_map(params![product_id, since_epoch], map_task_with_source_automation_id)?;
        collect_rows(rows)
    }

    /// List automations the scheduler should evaluate this tick: enabled,
    /// `trigger_kind = 'schedule'`, and either never-scheduled
    /// (`next_due_at IS NULL`, needs initialisation) or due
    /// (`next_due_at <= now_epoch`). Ordered oldest-first for stable
    /// iteration. `now_epoch` is UTC seconds; `next_due_at` is stored as an
    /// epoch-seconds string, so the comparison casts it to INTEGER.
    pub fn list_due_automations(&self, now_epoch: i64) -> Result<Vec<boss_protocol::Automation>> {
        let conn = self.connect()?;
        let sql = format!(
            "{AUTOMATION_SELECT}
              WHERE enabled = 1
                AND trigger_kind = 'schedule'
                AND (next_due_at IS NULL OR CAST(next_due_at AS INTEGER) <= ?1)
              ORDER BY created_at ASC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([now_epoch], map_automation)?;
        collect_rows(rows)
    }

    /// Initialise an automation's `next_due_at` (epoch seconds) without
    /// recording a fire. Used the first time the scheduler sees an
    /// automation whose `next_due_at` is still NULL: it computes the next
    /// occurrence and parks it here so the next tick can fire on time.
    /// Deliberately does NOT touch `updated_at` (which tracks user/config
    /// edits) or the `last_*` fire bookkeeping.
    pub fn initialize_automation_next_due_at(&self, id: &str, next_due_epoch: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE automations SET next_due_at = ?2 WHERE id = ?1",
            params![id, next_due_epoch.to_string()],
        )?;
        Ok(())
    }

    /// Return scheduling data for the automation scheduler's sleep computation:
    /// the minimum **future** `next_due_at` epoch (strictly after `now_epoch`)
    /// across all enabled `schedule` automations whose `next_due_at` has been
    /// initialized, and whether any enabled `schedule` automations are still
    /// uninitialized (`next_due_at IS NULL`).
    ///
    /// Used by the scheduler after each pass to compute how long to sleep before
    /// the next evaluation: sleep until `min_next_due`, capped at a maximum,
    /// but use a short poll interval when uninitialized automations are present.
    ///
    /// **Future-only is deliberate.** An occurrence still sitting at or before
    /// `now_epoch` *after* a pass has just run is one the pass could not act on
    /// — it is held for retry, or its cron/timezone is unparseable. Including it
    /// here would pin the sleep to its floor of one second and spin the loop at
    /// 1 Hz for as long as the blocker lasts (the paused-scheduler defect: ~0.96
    /// passes/s and ~2.4 `automation_runs` write transactions/s, sustained for
    /// the whole pause). The scheduler paces those cases explicitly instead, via
    /// [`crate::automation_scheduler::AutomationSchedulerPass::wake_hint`].
    pub fn list_min_future_next_due_at_for_scheduler(&self, now_epoch: i64) -> Result<(Option<i64>, bool)> {
        let conn = self.connect()?;
        let min_next_due: Option<i64> = conn.query_row(
            "SELECT MIN(CAST(next_due_at AS INTEGER))
               FROM automations
              WHERE enabled = 1 AND trigger_kind = 'schedule' AND next_due_at IS NOT NULL
                AND CAST(next_due_at AS INTEGER) > ?1",
            [now_epoch],
            |row| row.get(0),
        )?;
        let uninitialized_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM automations
              WHERE enabled = 1 AND trigger_kind = 'schedule' AND next_due_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok((min_next_due, uninitialized_count > 0))
    }

    /// Fetch the `automation_runs` row for a specific occurrence, if one
    /// exists. The `(automation_id, scheduled_for)` pair is the
    /// at-most-once dedupe key for a fired occurrence.
    pub fn automation_run_for_occurrence(
        &self,
        automation_id: &str,
        scheduled_for_epoch: i64,
    ) -> Result<Option<boss_protocol::AutomationRun>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, automation_id, scheduled_for, started_at, finished_at,
                    triage_execution_id, outcome, produced_task_id, detail
               FROM automation_runs
              WHERE automation_id = ?1 AND scheduled_for = ?2",
            params![automation_id, scheduled_for_epoch.to_string()],
            map_automation_run,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Record one scheduler decision and advance the automation's
    /// bookkeeping, atomically.
    ///
    /// The `automation_runs` write is an **upsert** keyed on
    /// `(automation_id, scheduled_for)`: a fresh occurrence inserts a row;
    /// re-recording the same occurrence (e.g. a held `failed_will_retry`
    /// the scheduler re-attempts) updates the existing row in place rather
    /// than piling up duplicates — preserving the at-most-once-per-occurrence
    /// invariant.
    ///
    /// `last_fired_at` and `last_outcome` are always updated to mirror this
    /// decision. `next_due_at` advances only when `record.next_due_at` is
    /// `Some` — a transient pre-start failure passes `None` to *hold* the
    /// occurrence for retry rather than skip past it.
    pub fn record_automation_run_and_advance(&self, record: AutomationFireRecord) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let scheduled_for = record.scheduled_for.to_string();
        let started_at = record.started_at.to_string();
        let finished_at = record.finished_at.map(|v| v.to_string());

        let existing_id: Option<String> = tx
            .query_row(
                "SELECT id FROM automation_runs
                  WHERE automation_id = ?1 AND scheduled_for = ?2",
                params![record.automation_id, scheduled_for],
                |row| row.get(0),
            )
            .optional()?;

        match existing_id {
            Some(id) => {
                tx.execute(
                    "UPDATE automation_runs
                        SET started_at = ?2, finished_at = ?3,
                            triage_execution_id = ?4, outcome = ?5,
                            produced_task_id = ?6, detail = ?7
                      WHERE id = ?1",
                    params![
                        id,
                        started_at,
                        finished_at,
                        record.triage_execution_id,
                        record.outcome,
                        record.produced_task_id,
                        record.detail,
                    ],
                )?;
            }
            None => {
                let run_id = next_id("autorun");
                tx.execute(
                    "INSERT INTO automation_runs
                         (id, automation_id, scheduled_for, started_at, finished_at,
                          triage_execution_id, outcome, produced_task_id, detail)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        run_id,
                        record.automation_id,
                        scheduled_for,
                        started_at,
                        finished_at,
                        record.triage_execution_id,
                        record.outcome,
                        record.produced_task_id,
                        record.detail,
                    ],
                )?;
            }
        }

        // Advance bookkeeping. `next_due_at` is only rewritten when the
        // caller wants to move past this occurrence.
        match record.next_due_at {
            Some(next_due) => {
                tx.execute(
                    "UPDATE automations
                        SET last_fired_at = ?2, last_outcome = ?3, next_due_at = ?4
                      WHERE id = ?1",
                    params![
                        record.automation_id,
                        record.started_at.to_string(),
                        record.outcome,
                        next_due.to_string(),
                    ],
                )?;
            }
            None => {
                tx.execute(
                    "UPDATE automations
                        SET last_fired_at = ?2, last_outcome = ?3
                      WHERE id = ?1",
                    params![record.automation_id, record.started_at.to_string(), record.outcome,],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Stamp a task's `source_automation_id` (and status) directly. Used by
    /// scheduler tests to drive the open-task-limit gate without the
    /// `boss task create --automation` path (Maint task 6).
    #[cfg(test)]
    pub fn stamp_task_source_automation_for_test(
        &self,
        task_id: &str,
        automation_id: &str,
        status: &str,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks SET source_automation_id = ?2, status = ?3
              WHERE id = ?1 AND deleted_at IS NULL",
            params![task_id, automation_id, status],
        )?;
        Ok(())
    }

    /// Return the `source_automation_id` for `work_item_id`, or `None` if the
    /// task is not automation-produced (or the id is not a task at all).
    /// Used by the dispatcher to route automation-produced task executions to
    /// the automation pool. Returns `Ok(None)` rather than an error when the
    /// id is not found in `tasks` (e.g. it references a project or product).
    pub fn source_automation_id_for_work_item(&self, work_item_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT source_automation_id FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
            [work_item_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(|opt| opt.flatten())
        .map_err(Into::into)
    }

    /// Create a `ready` `automation_triage` work_execution bound to an
    /// automation (Maint task 6).
    ///
    /// A triage execution's `work_item_id` is the `automations.id`, not a
    /// task — so it cannot go through the task-centric `insert_execution`
    /// resolvers (which require the work_item to resolve to a product/task).
    /// We insert the row directly with the automation's already-resolved
    /// repo. Downstream: the dispatcher routes it to the automations pool on
    /// `kind`, the runner renders the triage preamble, and the outcome
    /// detector finalises the matching `automation_runs` row on Stop. The row
    /// starts `ready` so the coordinator's normal drain picks it up (and the
    /// existing `dispatch_not_before` / `pre_start_failure_count` machinery
    /// retries it transparently on a transient pre-start failure).
    pub fn create_automation_triage_execution(
        &self,
        automation_id: &str,
        repo_remote_url: &str,
    ) -> Result<WorkExecution> {
        let conn = self.connect()?;
        let id = next_id("exec");
        let now = now_string();
        let branch_naming_json = serde_json::to_string(&boss_protocol::BranchNaming::default()).unwrap_or_default();
        // Column list mirrors `insert_execution`; every column it omits has a
        // schema DEFAULT (pre_start_failure_count=0, dispatch_not_before=NULL,
        // transient_failure_count=0, host_id='local', …).
        conn.execute(
            "INSERT INTO work_executions (
                id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at, prefer_is_soft, pr_url, worker_branch_prefix,
                allow_dirty, branch_naming
             ) VALUES (?1, ?2, ?3, 'ready', ?4, NULL, NULL, NULL, NULL, 0, NULL, ?5, NULL, NULL, 0, NULL, NULL, 0, ?6)",
            params![
                id,
                automation_id,
                boss_protocol::EXECUTION_KIND_AUTOMATION_TRIAGE,
                repo_remote_url,
                now,
                branch_naming_json,
            ],
        )?;
        query_execution(&conn, &id)?.with_context(|| format!("missing automation triage execution after insert: {id}"))
    }

    /// Fetch the `automation_runs` row whose triage `work_execution` is
    /// `triage_execution_id`, if one exists. Used by the outcome detector on
    /// Stop to map a finished triage execution back to the occurrence it
    /// fired for. Newest occurrence first as a tie-break (a retried execution
    /// id is unique, so at most one row normally matches).
    pub fn automation_run_for_triage_execution(
        &self,
        triage_execution_id: &str,
    ) -> Result<Option<boss_protocol::AutomationRun>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, automation_id, scheduled_for, started_at, finished_at,
                    triage_execution_id, outcome, produced_task_id, detail
               FROM automation_runs
              WHERE triage_execution_id = ?1
              ORDER BY scheduled_for DESC, started_at DESC
              LIMIT 1",
            [triage_execution_id],
            map_automation_run,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Finalise the `automation_runs` row for a finished triage execution
    /// (Maint task 6 outcome detection). Sets the terminal `outcome`,
    /// `produced_task_id`, `finished_at`, and (when `Some`) `detail`, and
    /// mirrors the outcome onto `automations.last_outcome`.
    ///
    /// Deliberately does NOT touch `next_due_at`: the scheduler already
    /// advanced the schedule past this occurrence when it fired the triage.
    /// Returns `false` when no run matches the execution id (the scheduler
    /// never recorded it — e.g. a manual fire that failed before recording).
    pub fn finalize_automation_triage_run(
        &self,
        triage_execution_id: &str,
        outcome: &str,
        produced_task_id: Option<&str>,
        detail: Option<&str>,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT id, automation_id FROM automation_runs
                  WHERE triage_execution_id = ?1
                  ORDER BY scheduled_for DESC, started_at DESC LIMIT 1",
                [triage_execution_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((run_id, automation_id)) = row else {
            return Ok(false);
        };
        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        tx.execute(
            "UPDATE automation_runs
                SET outcome = ?2,
                    produced_task_id = ?3,
                    detail = COALESCE(?4, detail),
                    finished_at = ?5
              WHERE id = ?1",
            params![run_id, outcome, produced_task_id, detail, now_epoch.to_string()],
        )?;
        tx.execute(
            "UPDATE automations SET last_outcome = ?2 WHERE id = ?1",
            params![automation_id, outcome],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Stamp a human-readable reason onto the `automation_runs` row for a
    /// triage execution that has been deferred (e.g. pool exhausted) but not
    /// yet finalised. Only writes when the existing `detail` is NULL or empty
    /// so that a later `finalize_automation_triage_run` call carrying a more
    /// specific reason always wins.
    ///
    /// Returns `true` when a row was updated, `false` when no matching row
    /// exists yet (the scheduler records the row at fire time, but there is a
    /// brief window before that write completes).
    pub fn update_automation_run_detail_for_triage_execution(
        &self,
        triage_execution_id: &str,
        detail: &str,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let rows_changed = conn.execute(
            "UPDATE automation_runs
                SET detail = ?2
              WHERE triage_execution_id = ?1
                AND (detail IS NULL OR detail = '')",
            params![triage_execution_id, detail],
        )?;
        Ok(rows_changed > 0)
    }

    /// Mark a triage execution's `automation_runs` row as `pool_throttled` —
    /// the triage execution is queued in `ready` status waiting for an
    /// automation pool slot. Also updates `automations.last_outcome` so the
    /// sidebar reflects the correct non-failure state.
    ///
    /// Only transitions from `failed_will_retry` (the pessimistic initial
    /// state the scheduler writes) so it is idempotent: a second call while
    /// still throttled is a no-op. Returns `true` when a row was updated.
    pub fn update_automation_run_for_pool_throttle(&self, triage_execution_id: &str, detail: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows_changed = tx.execute(
            "UPDATE automation_runs
                SET outcome = 'pool_throttled', detail = ?2
              WHERE triage_execution_id = ?1
                AND outcome = 'failed_will_retry'",
            params![triage_execution_id, detail],
        )?;
        if rows_changed > 0 {
            tx.execute(
                "UPDATE automations
                    SET last_outcome = 'pool_throttled'
                  WHERE id = (SELECT automation_id FROM automation_runs
                               WHERE triage_execution_id = ?1 LIMIT 1)",
                params![triage_execution_id],
            )?;
        }
        tx.commit()?;
        Ok(rows_changed > 0)
    }

    /// Re-arm a triage occurrence for immediate retry after its execution's
    /// pane spawn was rejected `SlotBusy` (an engine/app slot-occupancy
    /// desync, not a genuine triage failure — see
    /// `ExecutionCoordinator::run_execution`'s pane-spawn-failure branch).
    ///
    /// Re-points the `automation_runs` row's `triage_execution_id` at the
    /// freshly created retry execution and resets `outcome` back to the
    /// pessimistic `failed_will_retry` default the scheduler itself writes
    /// at fire time, so the retry is indistinguishable from a fresh
    /// dispatch to both the Automations tab and the Stop-based outcome
    /// detector. Matched on the OLD `triage_execution_id` (not the
    /// occurrence) so a concurrent legitimate finalisation can't be
    /// clobbered. Returns `true` when a row was updated.
    pub fn requeue_automation_run_after_transient_spawn_failure(
        &self,
        old_triage_execution_id: &str,
        new_triage_execution_id: &str,
        detail: &str,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows_changed = tx.execute(
            "UPDATE automation_runs
                SET triage_execution_id = ?2,
                    outcome = ?3,
                    detail = ?4
              WHERE triage_execution_id = ?1",
            params![
                old_triage_execution_id,
                new_triage_execution_id,
                boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY,
                detail,
            ],
        )?;
        if rows_changed > 0 {
            tx.execute(
                "UPDATE automations
                    SET last_outcome = ?2
                  WHERE id = (SELECT automation_id FROM automation_runs
                               WHERE triage_execution_id = ?1 LIMIT 1)",
                params![
                    new_triage_execution_id,
                    boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY
                ],
            )?;
        }
        tx.commit()?;
        Ok(rows_changed > 0)
    }

    /// Mark a triage execution's `automation_runs` row as `triage_running` —
    /// a pool slot was claimed and the triage agent is now active. Also
    /// updates `automations.last_outcome`. Transitions from `pool_throttled`
    /// (if the run was previously queued) or `failed_will_retry` (if it was
    /// dispatched immediately). Returns `true` when a row was updated.
    pub fn mark_automation_run_triage_started(&self, triage_execution_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows_changed = tx.execute(
            "UPDATE automation_runs
                SET outcome = 'triage_running'
              WHERE triage_execution_id = ?1
                AND outcome IN ('failed_will_retry', 'pool_throttled')",
            params![triage_execution_id],
        )?;
        if rows_changed > 0 {
            tx.execute(
                "UPDATE automations
                    SET last_outcome = 'triage_running'
                  WHERE id = (SELECT automation_id FROM automation_runs
                               WHERE triage_execution_id = ?1 LIMIT 1)",
                params![triage_execution_id],
            )?;
        }
        tx.commit()?;
        Ok(rows_changed > 0)
    }

    /// Create the single maintenance task produced by an automation's triage
    /// phase (`boss task create --automation`). Maint task 6.
    ///
    /// Runs in one immediate transaction:
    /// 1. **Open-task-cap re-check** — the backstop against fan-out. The
    ///    scheduler already gated at fire time, but a misbehaving triage
    ///    agent could call this repeatedly within one run; re-checking the
    ///    cap transactionally guarantees at most `open_task_limit` open
    ///    produced tasks regardless of agent behaviour.
    /// 2. **Pre-file dedup gate** (investigation
    ///    `automation-duplicate-work-2026-07-14.md` §4 Layer 1) — when
    ///    `target_files` is non-empty and is a subset of (or equal to) the
    ///    declared target files of an open automation-sourced task in this
    ///    product, with sufficient name/description token overlap, refuse
    ///    the create instead of dispatching a near-certain duplicate worker.
    ///    High precision only: an undeclared candidate, or one that merely
    ///    shares a file without the name/description signal, always passes
    ///    through. On a gate hit: file a `followup` attention item linking
    ///    the suppressed candidate to the blocking task, record a
    ///    standalone `suppressed_duplicate` `automation_runs` row carrying
    ///    the blocking task's id, and return an error — the triage agent is
    ///    expected to end its run with `automation: skip — duplicate of
    ///    <blocking task>` instead of retrying.
    /// 3. Insert a product-level chore (`kind='chore'`, `project_id=NULL`)
    ///    inheriting the automation's repo override, `autostart=true` so
    ///    phase 2 starts automatically.
    /// 4. Stamp `source_automation_id` for provenance, backlog exclusion,
    ///    pool routing, and the open-task-limit denominator, and record its
    ///    declared `target_files`/`target_symbols` in `task_targets` (both
    ///    for this gate's own future comparisons and for layer 2's
    ///    `merge_poller` overlap detector).
    ///
    /// Returns an error (surfaced to the agent) when the cap is already met
    /// or the dedup gate fires, so the marker the agent then emits can be
    /// reconciled by the detector.
    pub fn create_automation_task(
        &self,
        automation_id: &str,
        name: &str,
        description: Option<&str>,
        target_files: &[String],
        target_symbols: &[String],
    ) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let automation = query_automation(&tx, automation_id).require("automation", automation_id)?;

        let open_siblings = query_open_sibling_fingerprints(&tx, automation_id)?;
        let open = open_siblings.len() as i64;
        if open >= automation.open_task_limit {
            anyhow::bail!(
                "automation {automation_id} is at its open-task limit \
                 ({open}/{}); refusing to create another task (fan-out backstop)",
                automation.open_task_limit
            );
        }

        // Dedup gate. The open-task cap above only counts rows; it is happy
        // to hold N copies of one finding, which is exactly what filled the
        // audited duplicate clusters. This compares what the candidate is
        // *about* against its open siblings.
        if let Some((sibling, matched)) = find_duplicate_sibling(&open_siblings, name, description) {
            record_dedup_suppression(&tx, automation_id, sibling, name, &matched)?;
            tx.commit()?;
            tracing::warn!(
                automation_id,
                surviving_task_id = %sibling.id,
                surviving_short_id = sibling.short_id,
                attempted_name = name,
                matched_on = matched.kind.as_str(),
                match_key = %matched.key,
                "automation dedup gate suppressed a duplicate task",
            );
            return Err(anyhow::Error::new(AutomationDuplicateTaskError {
                existing_id: sibling.id.clone(),
                existing_short_id: sibling.short_id,
                existing_name: sibling.name.clone(),
                matched_on: matched.kind.as_str(),
                match_key: matched.key,
            }));
        }

        let candidate_files: HashSet<String> = target_files
            .iter()
            .map(|f| normalize_target_file(f))
            .filter(|f| !f.is_empty())
            .collect();
        if let Some(blocker) = find_duplicate_gate_blocker(
            &tx,
            &automation.product_id,
            name,
            description.unwrap_or(""),
            &candidate_files,
        )? {
            let blocker_label =
                boss_protocol::short_id_label(blocker.short_id).unwrap_or_else(|| blocker.task_id.clone());
            create_attention_in_tx(
                &tx,
                CreateAttentionInput::builder()
                    .kind("followup")
                    .association_task_id(blocker.task_id.clone())
                    .source_task_id(blocker.task_id.clone())
                    .source_kind("automation_dedup_gate")
                    .proposed_name(format!("Possible duplicate of {blocker_label}: {name}"))
                    .proposed_description(format!(
                        "Automation {automation_id} tried to create a task named {name:?} declaring \
                         target file(s) {candidate_files:?}, which are a subset of (or equal to) \
                         {blocker_label}'s ({blocker_name:?}) already-declared targets, with matching \
                         name/description overlap. The create-path dedup gate suppressed it \
                         (investigation automation-duplicate-work-2026-07-14.md §4 Layer 1) rather than \
                         dispatch a likely-duplicate worker. Review {blocker_label}: if this really is \
                         distinct work, re-file it with force_duplicate semantics.",
                        blocker_name = blocker.name,
                    ))
                    .proposed_work_kind("task")
                    .rationale(format!("pre-file dedup gate: suppressed_duplicate of {blocker_label}"))
                    .build(),
            )?;

            let run_id = next_id("autorun");
            let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
            tx.execute(
                "INSERT INTO automation_runs
                     (id, automation_id, scheduled_for, started_at, finished_at,
                      triage_execution_id, outcome, produced_task_id, detail)
                 VALUES (?1, ?2, ?3, ?3, ?3, NULL, ?4, NULL, ?5)",
                params![
                    run_id,
                    automation_id,
                    now_epoch.to_string(),
                    boss_protocol::AUTOMATION_OUTCOME_SUPPRESSED_DUPLICATE,
                    format!(
                        "duplicate-suspect of {blocker_label} ({}): candidate {name:?} declares a subset \
                         of its target files with matching name/description overlap",
                        blocker.task_id,
                    ),
                ],
            )?;
            tx.commit()?;
            anyhow::bail!(
                "duplicate-suspect of {blocker_label}: candidate {name:?} declares target file(s) \
                 {candidate_files:?}, a subset of (or equal to) {blocker_label}'s already-declared \
                 targets, with matching name/description overlap; refusing to create another task \
                 (pre-file dedup gate). An attention item was filed on {blocker_label} — end this run \
                 with `automation: skip — duplicate of {blocker_label}` instead of retrying.",
            );
        }

        // `force_duplicate` bypasses the 60-second same-name guard, which
        // is the wrong tool here in both directions: a cron re-fire lands
        // long after the window closes, and a legitimately recurring
        // instruction can produce a same-named task days apart. The gates
        // above are what actually protect this path.
        let mut task = insert_chore_in_tx(
            &tx,
            CreateChoreInput::builder()
                .product_id(automation.product_id.clone())
                .name(name)
                .maybe_description(description.map(str::to_owned))
                .created_via(boss_protocol::CREATED_VIA_ENGINE_AUTO)
                .maybe_repo_remote_url(automation.repo_remote_url.clone())
                .force_duplicate(true)
                .build(),
        )?;
        tx.execute(
            "UPDATE tasks SET source_automation_id = ?2 WHERE id = ?1",
            params![task.id, automation_id],
        )?;
        insert_task_targets_in_tx(&tx, &task.id, target_files, target_symbols)?;
        tx.commit()?;
        task.source_automation_id = Some(automation_id.to_owned());
        Ok(task)
    }
}

/// How far back [`WorkDb::list_automation_sibling_tasks`] reaches for
/// already-resolved siblings.
///
/// Long enough to span several fires of a ~12h automation, so a finding
/// that was filed, merged, and re-derived on the next fire is still
/// visible to the triage agent. Short enough that a finding which has
/// genuinely recurred weeks later is not silently discouraged — this list
/// is advice to the agent, not a gate, and stale advice is worse than
/// none.
const SIBLING_RESOLVED_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;

/// Cap on the sibling list handed to the triage agent. An automation with
/// a long, churny history would otherwise push its whole backlog into the
/// preamble and bury the standing instruction.
const SIBLING_LIST_LIMIT: i64 = 20;

/// An open task belonging to one automation, carrying just enough to
/// fingerprint it and to name it back to the triage agent.
struct OpenSibling {
    id: String,
    short_id: i64,
    name: String,
    fingerprint: boss_engine_automation_dedup::TaskFingerprint,
}

/// The statuses that make a produced task "still open" — a finding
/// already being worked, reviewed, or waiting. Kept identical to the
/// open-task-cap query so the cap and the dedup gate can never disagree
/// about which siblings exist.
const OPEN_SIBLING_STATUSES: &str = "'todo', 'ready', 'active', 'in_review', 'blocked'";

/// Load and fingerprint every open task produced by `automation_id`.
///
/// Fingerprinting happens here, inside the same immediate transaction as
/// the subsequent insert, so a second triage agent racing this one cannot
/// slip a duplicate in between the check and the write.
fn query_open_sibling_fingerprints(conn: &Connection, automation_id: &str) -> Result<Vec<OpenSibling>> {
    let sql = format!(
        "SELECT id, short_id, name, description
           FROM tasks
          WHERE source_automation_id = ?1
            AND status IN ({OPEN_SIBLING_STATUSES})
            AND deleted_at IS NULL
          ORDER BY created_at ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([automation_id], |row| {
        let id: String = row.get(0)?;
        let short_id: Option<i64> = row.get(1)?;
        let name: String = row.get(2)?;
        let description: Option<String> = row.get(3)?;
        Ok((id, short_id.unwrap_or(0), name, description))
    })?;

    let mut siblings = Vec::new();
    for row in rows {
        let (id, short_id, name, description) = row?;
        let fingerprint = boss_engine_automation_dedup::fingerprint(&name, description.as_deref());
        siblings.push(OpenSibling {
            id,
            short_id,
            name,
            fingerprint,
        });
    }
    Ok(siblings)
}

/// First open sibling that already tracks this finding, if any.
///
/// Siblings arrive oldest-first, so the *original* filing is the one that
/// survives. That matters for the operator: the surviving row is the one
/// that already has the history, and possibly a PR.
fn find_duplicate_sibling<'a>(
    siblings: &'a [OpenSibling],
    name: &str,
    description: Option<&str>,
) -> Option<(&'a OpenSibling, boss_engine_automation_dedup::DuplicateMatch)> {
    let candidate = boss_engine_automation_dedup::fingerprint(name, description);
    siblings
        .iter()
        .find_map(|sibling| candidate.duplicate_of(&sibling.fingerprint).map(|m| (sibling, m)))
}

/// Append the `duplicate_suppressed` trace row. Written inside the
/// caller's transaction, which then commits *despite* returning an error
/// to the agent — the whole point is that the suppression outlives the
/// refused insert.
fn record_dedup_suppression(
    conn: &Connection,
    automation_id: &str,
    sibling: &OpenSibling,
    attempted_name: &str,
    matched: &boss_engine_automation_dedup::DuplicateMatch,
) -> Result<()> {
    conn.execute(
        "INSERT INTO automation_dedup_suppressions
             (id, automation_id, surviving_task_id, attempted_name, matched_on, match_key, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            next_id("adsup"),
            automation_id,
            sibling.id,
            attempted_name,
            matched.kind.as_str(),
            matched.key,
            now_string(),
        ],
    )?;
    Ok(())
}

/// Collapse consecutive `runs` (newest-first, as returned by the DB query)
/// that share the same `outcome` and `produced_task_id` into a single
/// entry, incrementing `repeat_count`. The kept entry is always the
/// newest of the run (input order is newest-first, so the first row of a
/// matching streak is the one retained).
fn collapse_automation_run_retries(runs: Vec<boss_protocol::AutomationRun>) -> Vec<boss_protocol::AutomationRun> {
    let mut collapsed: Vec<boss_protocol::AutomationRun> = Vec::with_capacity(runs.len());
    for run in runs {
        if let Some(last) = collapsed.last_mut()
            && last.outcome == run.outcome
            && last.produced_task_id == run.produced_task_id
        {
            last.repeat_count += 1;
            continue;
        }
        collapsed.push(run);
    }
    collapsed
}

#[cfg(test)]
mod retry_collapse_tests {
    use super::*;

    fn run(outcome: &str, produced_task_id: Option<&str>) -> boss_protocol::AutomationRun {
        boss_protocol::AutomationRun::builder()
            .id(format!("run_{outcome}_{produced_task_id:?}"))
            .automation_id("auto_1")
            .scheduled_for("1700000000")
            .started_at("1700000001")
            .outcome(outcome)
            .maybe_produced_task_id(produced_task_id.map(str::to_owned))
            .build()
    }

    #[test]
    fn collapses_consecutive_same_outcome_runs() {
        let runs = vec![
            run("failed_will_retry", None),
            run("failed_will_retry", None),
            run("failed_will_retry", None),
        ];
        let collapsed = collapse_automation_run_retries(runs);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].repeat_count, 3);
        assert_eq!(collapsed[0].outcome, "failed_will_retry");
    }

    #[test]
    fn does_not_collapse_across_a_different_outcome() {
        let runs = vec![
            run("failed_will_retry", None),
            run("failed_will_retry", None),
            run("produced_task", Some("t1")),
            run("failed_will_retry", None),
        ];
        let collapsed = collapse_automation_run_retries(runs);
        assert_eq!(collapsed.len(), 3);
        assert_eq!(collapsed[0].repeat_count, 2);
        assert_eq!(collapsed[1].repeat_count, 1);
        assert_eq!(collapsed[1].produced_task_id.as_deref(), Some("t1"));
        assert_eq!(collapsed[2].repeat_count, 1);
    }

    #[test]
    fn does_not_collapse_distinct_produced_tasks_with_same_outcome() {
        let runs = vec![run("produced_task", Some("t1")), run("produced_task", Some("t2"))];
        let collapsed = collapse_automation_run_retries(runs);
        assert_eq!(collapsed.len(), 2, "distinct produced tasks must never merge");
        assert!(collapsed.iter().all(|r| r.repeat_count == 1));
    }

    #[test]
    fn single_run_has_repeat_count_one() {
        let collapsed = collapse_automation_run_retries(vec![run("skipped", None)]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].repeat_count, 1);
    }
}
