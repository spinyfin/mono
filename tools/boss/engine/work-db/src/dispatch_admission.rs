//! Read-only dispatch-admission facts for a work item — the DB-layer half
//! of the shared "would this work item dispatch right now, and if not,
//! why not?" evaluator. See
//! `ExecutionCoordinator::evaluate_dispatch_admission`
//! (`coordinator/dispatch_admission.rs`) for the half that adds the pause
//! snapshot and interactive-concurrency-cap check this can't answer
//! without the live worker pool. See
//! `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`.

use super::*;

/// Everything [`WorkDb`] alone can answer about whether `work_item_id`
/// would currently dispatch, independent of the dispatch pause and the
/// interactive concurrency cap (both live only on `ExecutionCoordinator`).
/// Purely read-only — no row is created and no attention item is
/// resolved, unlike the mutating `request_execution_in_tx_with_live_check`
/// path this mirrors the eligibility checks of.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct DispatchAdmissionFacts {
    pub resolved_work_item_id: String,
    /// `Some(reason)` when the work item is ineligible for dispatch
    /// outright — unknown, not a dispatchable kind, deleted, terminal,
    /// parked, human-driven, or (once the status axis clears) no resolvable
    /// repo — mirrors the `bail!` conditions
    /// `request_execution_in_tx_with_live_check` enforces before ever
    /// creating a row. Deliberately does NOT include `in_review`: see
    /// `status_ineligibility`'s doc for why that stays eligible on both
    /// the ordinary and the forced path. Parking uses
    /// [`parking::unpark_first_refusal`] so this evaluator and the
    /// mutating request path cannot disagree.
    pub ineligible_reason: Option<String>,
    /// Work items gating this one, per [`WorkDb::gating_prereqs_for`].
    /// Always empty when `ineligible_reason` is `Some` (there is no point
    /// resolving dependencies for a row that can't dispatch anyway).
    #[builder(default)]
    pub unmet_dependencies: Vec<String>,
    /// `true` when the item is churn-guard parked under either
    /// representation: an open `churn_guard_parked` attention item
    /// (`pr_review_recovery`'s path) or `tasks.dispatch_failed_reason =
    /// 'churn_guard'` (`orphan_sweep`'s path) — see the query in
    /// [`dispatch_admission_facts`] for why both are checked. Informational
    /// only: an explicit dispatch request (forced or not) has always
    /// cleared this rather than being blocked by it — see
    /// `request_execution_in_tx_with_live_check`'s unconditional
    /// `resolve_attention_kind_in_tx` call.
    #[builder(default)]
    pub churn_guard_parked: bool,
    /// `true` when the task has `autostart = false` and is still `todo`.
    /// Informational only, for the same reason as `churn_guard_parked`:
    /// explicit dispatch (forced or not) has always bypassed this gate —
    /// see `task_accepts_execution`'s doc comment.
    #[builder(default)]
    pub autostart_disabled: bool,
    /// `true` when this row's latest execution ended in a *deliberate*
    /// park rather than a lost pane: the worker declared
    /// `run_done --outcome blocked`, or an open park attention item
    /// (`run_done_declared_blocked` / `nudge_breaker_tripped`) is still
    /// asking a human to adjudicate. The durable signal is
    /// `work_executions.run_done_outcome`; the attention item is the other
    /// representation (the nudge-breaker park never stamps the column).
    ///
    /// Blocking on every **automatic** mint path (`orphan_sweep`,
    /// `rescan_active_dispatch`, `reconcile_active_dispatch`,
    /// `reconcile_work_item_execution`, `reconcile_revision_execution`).
    /// Informational on this evaluator
    /// for the same reason as `churn_guard_parked`: an explicit
    /// `bossctl work start` / kanban drag-to-Doing is the un-park
    /// gesture and must not be refused.
    #[builder(default)]
    pub deliberate_parked: bool,
    /// `true` when a fresh execution for this work item would route to
    /// the interactive main pool (i.e. neither `pr_review` nor
    /// automation-sourced) — the only pool the concurrency cap governs.
    #[builder(default)]
    pub targets_main_pool: bool,
    /// Whether the task currently has critical priority.
    #[builder(default)]
    pub is_critical: bool,
    /// `true` when a fresh execution for this work item would route to
    /// the dedicated review pool (i.e. `pr_review`). Used by
    /// `ExecutionCoordinator::evaluate_dispatch_admission` to report no
    /// pause in effect for such rows, rather than a bypassable one.
    #[builder(default)]
    pub exempt_from_operator_pause: bool,
}

/// Attention kinds that mean "a human was asked to adjudicate this run,
/// do not auto-remint." Execution-scoped (`execution_id` set,
/// `work_item_id` NULL), matched against a specific execution id.
const DELIBERATE_PARK_ATTENTION_KINDS: &[&str] = &[
    boss_engine_storage_values::attention_kinds::RUN_DONE_BLOCKED_ATTENTION_KIND,
    boss_engine_storage_values::attention_kinds::NUDGE_BREAKER_ATTENTION_KIND,
];

/// Execution-scoped attention half of [`work_item_is_deliberately_parked`].
/// Always called with the latest execution id so superseded executions cannot latch the park.
fn has_open_execution_attention_of_kind_on(conn: &Connection, execution_id: &str, kinds: &[&str]) -> Result<bool> {
    if kinds.is_empty() {
        return Ok(false);
    }
    let placeholders = (2..2 + kinds.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT EXISTS(
             SELECT 1 FROM work_attention_items a
             WHERE a.execution_id = ?1
               AND a.status = 'open'
               AND a.kind IN ({placeholders})
         )"
    );
    let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(kinds.len() + 1);
    bound.push(&execution_id);
    for kind in kinds {
        bound.push(kind);
    }
    conn.query_row(&sql, bound.as_slice(), |row| row.get::<_, i64>(0))
        .map(|found| found != 0)
        .map_err(Into::into)
}

/// Compatibility guard for executions parked by older engines. New worker
/// failures use task status and blocked_reason, never this attention latch.
/// `true` when automatic mint must not create a replacement execution for
/// `work_item_id`. Keys on the *latest* execution's
/// `run_done_outcome = 'blocked'` (the durable signal — the column is
/// not cleared by `ClearedBy::WorkResumed`) and, additionally, on an open
/// park attention item filed against that same latest execution (the
/// nudge-breaker park never stamps the column). Both halves are scoped to
/// the latest execution so that a fresh replacement execution — minted by
/// an explicit `bossctl work start` — is never kept parked by a park item
/// belonging to the row it replaced. Explicit dispatch also resolves park
/// attention synchronously. Latest-execution scoping assumes automatic mint
/// paths honor the park before inserting a replacement; PR-review mint paths
/// instead require `in_review`, while a nudge-breaker park leaves work `active`.
/// Takes `&Connection` so transactional callers need not re-lock the DB.
pub fn work_item_is_deliberately_parked(conn: &Connection, work_item_id: &str) -> Result<bool> {
    // Compatibility only: new terminal failures move the task out of active
    // atomically. Do not give their declarations or attention items park semantics.
    let latest: Option<(String, Option<String>, String)> = conn
        .query_row(
            &format!(
                "SELECT id, run_done_outcome, status FROM work_executions
                 WHERE id = {}",
                latest_execution_id_sql("?1")
            ),
            params![work_item_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((latest_execution_id, latest_outcome, status)) = latest else {
        return Ok(false);
    };
    deliberate_park(&status, latest_outcome.as_deref(), || {
        has_open_execution_attention_of_kind_on(conn, &latest_execution_id, DELIBERATE_PARK_ATTENTION_KINDS)
    })
}

/// SQL scalar: the id of `item`'s latest execution, the row
/// [`work_item_is_deliberately_parked`] keys on.
fn latest_execution_id_sql(item: &str) -> String {
    format!(
        "(SELECT latest.id FROM work_executions latest
          WHERE latest.work_item_id = {item}
          ORDER BY latest.created_at DESC, latest.id DESC
          LIMIT 1)"
    )
}

/// The park decision over an item's latest execution. `park_attention` is
/// only consulted when the row's own columns do not settle it.
fn deliberate_park(
    status: &str,
    run_done_outcome: Option<&str>,
    park_attention: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    if status == "failed" {
        return Ok(false);
    }
    if run_done_outcome == Some(boss_protocol::RunDoneOutcome::Blocked.as_str()) {
        return Ok(true);
    }
    park_attention()
}

/// The members of `ids` for which [`work_item_is_deliberately_parked`] is
/// true, read with one statement.
pub(crate) fn deliberately_parked_among<S: AsRef<str>>(conn: &Connection, ids: &[S]) -> Result<HashSet<String>> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let kinds = DELIBERATE_PARK_ATTENTION_KINDS
        .iter()
        .map(|kind| format!("'{kind}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT item.value, e.status, e.run_done_outcome,
                EXISTS(SELECT 1 FROM work_attention_items a
                       WHERE a.execution_id = e.id AND a.status = 'open' AND a.kind IN ({kinds}))
         FROM json_each(?1) item
         JOIN work_executions e ON e.id = {latest}",
        latest = latest_execution_id_sql("item.value"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([id_array(ids)], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, bool>(3)?,
        ))
    })?;
    let mut parked = HashSet::new();
    for row in rows {
        let (id, status, outcome, park_attention) = row?;
        if deliberate_park(&status, outcome.as_deref(), || Ok(park_attention))? {
            parked.insert(id);
        }
    }
    Ok(parked)
}

/// Explicit dispatch acknowledges parks on prior executions in the same transaction.
pub(crate) fn resolve_deliberate_park_attention(conn: &Connection, work_item_id: &str) -> Result<()> {
    for kind in DELIBERATE_PARK_ATTENTION_KINDS {
        conn.execute(
            "UPDATE work_attention_items SET status = 'resolved', resolved_at = ?1
             WHERE status = 'open' AND kind = ?2 AND execution_id IN
                 (SELECT id FROM work_executions WHERE work_item_id = ?3)",
            params![super::now_string(), kind, work_item_id],
        )?;
    }
    Ok(())
}

/// A delivered primary PR belongs to review/merge handling, even while
/// deferred review admission leaves its owner in `active`.
/// Revisions intentionally resume their parent's PR and are not owners.
pub(crate) fn primary_pr_awaits_review(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(query_task(conn, work_item_id)?.is_some_and(|task| owns_delivered_pr(&task.kind, task.pr_url.as_deref())))
}

/// The row-level rule behind [`primary_pr_awaits_review`].
fn owns_delivered_pr(kind: &TaskKind, pr_url: Option<&str>) -> bool {
    *kind != TaskKind::Revision && pr_url.is_some_and(|url| !url.trim().is_empty())
}

/// SQL scalar: the status of `item`'s latest `revision_implementation`
/// execution when `item` is a revision, else NULL.
fn latest_revision_implementation_status_sql(item: &str) -> String {
    format!(
        "(SELECT e.status FROM work_executions e JOIN tasks t ON t.id = e.work_item_id
          WHERE e.work_item_id = {item} AND t.kind = 'revision' AND e.kind = 'revision_implementation'
          ORDER BY e.created_at DESC, e.id DESC LIMIT 1)"
    )
}

/// Completed revision work belongs to completion/review handling. A review
/// execution (including one on the chain root) must not hide that completion.
/// Explicit starts create a newer implementation and remain allowed.
pub(crate) fn revision_implementation_completed(conn: &Connection, work_item_id: &str) -> Result<bool> {
    let status: Option<String> = conn.query_row(
        &format!("SELECT {}", latest_revision_implementation_status_sql("?1")),
        [work_item_id],
        |row| row.get(0),
    )?;
    Ok(status.as_deref() == Some("completed"))
}

/// True when this item already belongs in review: it owns a delivered
/// primary PR, or it is a revision whose implementation has completed.
/// Unblock restores `in_review` rather than `todo` for these rows.
pub(crate) fn delivered_or_completed(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(primary_pr_awaits_review(conn, work_item_id)? || revision_implementation_completed(conn, work_item_id)?)
}

/// The members of `ids` for which [`primary_pr_awaits_review`] or
/// [`revision_implementation_completed`] is true, read with one statement.
pub(crate) fn delivered_or_completed_among<S: AsRef<str>>(conn: &Connection, ids: &[S]) -> Result<HashSet<String>> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let sql = format!(
        "SELECT item.id, item.kind, item.pr_url, {}
         FROM tasks item WHERE item.id IN (SELECT value FROM json_each(?1))",
        latest_revision_implementation_status_sql("item.id")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([id_array(ids)], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut held = HashSet::new();
    for row in rows {
        let (id, kind, pr_url, revision_status) = row?;
        let kind: TaskKind = kind
            .parse()
            .map_err(|_| anyhow::anyhow!("unknown task kind in DB row {id}: {kind}"))?;
        if owns_delivered_pr(&kind, pr_url.as_deref()) || revision_status.as_deref() == Some("completed") {
            held.insert(id);
        }
    }
    Ok(held)
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub fn dispatch_admission_facts(&self, work_item_id: &str) -> Result<DispatchAdmissionFacts> {
        let conn = self.connect()?;
        dispatch_admission_facts_on(&conn, work_item_id)
    }
}

/// Admission facts read using an existing connection.
pub(crate) fn dispatch_admission_facts_on(conn: &Connection, work_item_id: &str) -> Result<DispatchAdmissionFacts> {
    struct RawFacts {
        resolved_work_item_id: String,
        ineligible_reason: Option<String>,
        churn_guard_parked: bool,
        autostart_disabled: bool,
        deliberate_parked: bool,
        kind: Option<ExecutionKind>,
        is_critical: bool,
    }
    let raw = {
        let resolved_work_item_id =
            resolve_friendly_work_item_id(conn, work_item_id)?.unwrap_or_else(|| work_item_id.to_owned());

        if !resolved_work_item_id.starts_with("task_") {
            // Projects/products never carry an execution lifecycle.
            RawFacts {
                resolved_work_item_id: resolved_work_item_id.clone(),
                ineligible_reason: Some(format!("{resolved_work_item_id} is not a dispatchable work item")),
                churn_guard_parked: false,
                autostart_disabled: false,
                deliberate_parked: false,
                kind: None,
                is_critical: false,
            }
        } else {
            let row: Option<(String, Option<String>, i64, i64, String)> = conn
                    .query_row(
                        "SELECT status, archived_reason, autostart, deleted_at IS NOT NULL, priority FROM tasks WHERE id = ?1",
                        params![resolved_work_item_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                    )
                    .optional()?;
            let Some((status_str, archived_reason, autostart, deleted, priority)) = row else {
                return Ok(DispatchAdmissionFacts::builder()
                    .resolved_work_item_id(resolved_work_item_id.clone())
                    .ineligible_reason(format!("unknown work item: {resolved_work_item_id}"))
                    .build());
            };
            let status: TaskStatus = status_str.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            // Shares `status_ineligibility` with
            // `request_execution_in_tx_with_live_check` (see its doc)
            // so this read-only evaluator can never diverge from what
            // the mutating request path actually enforces on the
            // status axis.
            let ineligible_reason =
                if let Some(ineligibility) = status_ineligibility(&status, archived_reason.as_deref(), deleted != 0) {
                    Some(match ineligibility {
                        StatusIneligibility::Deleted => format!("{resolved_work_item_id} is deleted"),
                        StatusIneligibility::Terminal {
                            status,
                            archived_reason,
                        } => {
                            let reason_suffix = archived_reason.map(|r| format!(" — {r}")).unwrap_or_default();
                            format!(
                                "{resolved_work_item_id} is `{status}`{reason_suffix} — terminal work items \
                                 cannot be dispatched"
                            )
                        }
                    })
                } else if let Some(refusal) = parking::unpark_first_refusal(conn, &resolved_work_item_id)? {
                    Some(refusal.to_string())
                } else if primary_pr_awaits_review(conn, &resolved_work_item_id)?
                    && query_live_execution_for_work_item(conn, &resolved_work_item_id)?.is_none()
                    && query_latest_execution_for_work_item(conn, &resolved_work_item_id)?
                        .is_none_or(|execution| execution.status.is_terminal())
                {
                    Some("Work already has a bound PR; use a revision to change it".into())
                } else if work_item_is_human_driven(conn, &resolved_work_item_id)? {
                    Some(format!(
                        "{resolved_work_item_id} is human-driven — no agent worker will run"
                    ))
                } else {
                    // Mirrors the request path's unresolved-repo bail
                    // (`dispatch_helpers::request_execution_in_tx_with_live_check`)
                    // so this evaluator can't report `would_dispatch: true`
                    // for an item the mutating request would then reject —
                    // read-only, so unlike that path this never records the
                    // sticky repo-unresolved attention item itself.
                    match resolve_repo_for_work_item(conn, &resolved_work_item_id)? {
                        Some(_) => None,
                        None => {
                            let label = repo_unresolved_kind_label(conn, &resolved_work_item_id)?;
                            Some(repo_unresolved_attention_body(&resolved_work_item_id, label))
                        }
                    }
                };
            // Two representations, one per caller: `orphan_sweep` bounces
            // an `active` task straight to `dispatch_failed_reason =
            // 'churn_guard'` (see `WorkDb::bounce_churn_guard_parked_to_backlog`),
            // while `pr_review_recovery` still files the
            // `churn_guard_parked` attention item for an `in_review` task
            // (Backlog is not a meaningful bounce target for a task with
            // an open PR under review). Checking both keeps this
            // informational-only fact accurate for either source — see
            // `docs/designs/dispatch-halt-state-vs-attention-items.md`.
            let churn_guard_parked: i64 = conn.query_row(
                "SELECT
                        (SELECT COUNT(*) FROM tasks
                          WHERE id = ?1 AND dispatch_failed_reason = ?3)
                        + (SELECT COUNT(*) FROM work_attention_items
                            WHERE work_item_id = ?1 AND kind = ?2 AND status = 'open')",
                params![
                    resolved_work_item_id,
                    CHURN_GUARD_PARKED_ATTENTION_KIND,
                    CHURN_GUARD_DISPATCH_FAILED_REASON
                ],
                |row| row.get(0),
            )?;
            let autostart_disabled = autostart == 0 && status == TaskStatus::Todo;
            let deliberate_parked = work_item_is_deliberately_parked(conn, &resolved_work_item_id)?;
            // Mirrors the request path's own idempotency/re-dispatch
            // guard (`request_execution_in_tx_with_live_check`'s
            // "governing" execution): a non-terminal execution already
            // present for this work item — notably a `ready`
            // `pr_review` execution — is REUSED rather than a fresh one
            // being created, so its own kind (not the task's kind-
            // derived hypothetical fresh-execution kind) is what
            // actually governs pool routing and the operator-pause
            // review exemption. Falls back to the fresh-execution kind
            // when no non-terminal execution exists yet.
            let live = query_live_execution_for_work_item(conn, &resolved_work_item_id)?;
            let latest = query_latest_execution_for_work_item(conn, &resolved_work_item_id)?;
            let governing = live.or(latest).filter(|e| !e.status.is_terminal());
            let kind = match governing {
                Some(execution) => execution.kind,
                None => execution_kind_for_work_item(conn, &resolved_work_item_id)?,
            };
            RawFacts {
                resolved_work_item_id,
                ineligible_reason,
                churn_guard_parked: churn_guard_parked > 0,
                autostart_disabled,
                deliberate_parked,
                kind: Some(kind),
                is_critical: critical_priority::is_critical(&priority),
            }
        }
    };

    let unmet_dependencies = if raw.ineligible_reason.is_none() {
        deps::gating_prereqs_for(conn, &raw.resolved_work_item_id)?
    } else {
        Vec::new()
    };
    let (exempt_from_operator_pause, targets_main_pool) = pause_exemption_facts(raw.kind, || {
        matches!(
            super::automations::source_automation_id_on(conn, &raw.resolved_work_item_id),
            Ok(Some(_))
        )
    });
    Ok(DispatchAdmissionFacts::builder()
        .resolved_work_item_id(raw.resolved_work_item_id)
        .maybe_ineligible_reason(raw.ineligible_reason)
        .unmet_dependencies(unmet_dependencies)
        .churn_guard_parked(raw.churn_guard_parked)
        .autostart_disabled(raw.autostart_disabled)
        .deliberate_parked(raw.deliberate_parked)
        .targets_main_pool(targets_main_pool)
        .is_critical(raw.is_critical)
        .exempt_from_operator_pause(exempt_from_operator_pause)
        .build())
}

/// `(exempt_from_operator_pause, targets_main_pool)` for an item whose
/// governing execution kind is `kind`. The single decision both the per-item
/// admission facts and the set-based [`PersistedPause::held_among`] use, so
/// the two cannot drift. `from_automation` is only asked when it matters.
fn pause_exemption_facts(kind: Option<ExecutionKind>, from_automation: impl FnOnce() -> bool) -> (bool, bool) {
    let exempt = kind == Some(ExecutionKind::PrReview);
    let targets_main_pool = match kind {
        Some(kind) => kind != ExecutionKind::PrReview && kind != ExecutionKind::AutomationTriage && !from_automation(),
        None => false,
    };
    (exempt, targets_main_pool)
}

/// The persisted dispatch pause, read once for any number of items.
pub(crate) struct PersistedPause {
    /// An operator pause whose scope leaves reviews running: the only pause
    /// coordinator admission exempts any work from.
    overridable: bool,
}

impl PersistedPause {
    /// The pause in force, or `None` when dispatch is running.
    pub(crate) fn load(conn: &Connection) -> Result<Option<Self>> {
        use boss_engine_storage_values::dispatch_pause::*;
        let metadata = |key: &str| -> Result<Option<String>> {
            Ok(conn
                .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |r| r.get(0))
                .optional()?)
        };
        if metadata(METADATA_KEY_DISPATCH_PAUSED)?.as_deref() != Some("1") {
            return Ok(None);
        }
        let origin = metadata(METADATA_KEY_DISPATCH_PAUSE_ORIGIN)?;
        let scope = metadata(METADATA_KEY_DISPATCH_PAUSE_SCOPE)?;
        Ok(Some(Self {
            overridable: persisted_origin_is_operator(origin.as_deref())
                && !resolve_persisted_pause_scope(scope.as_deref(), origin.as_deref()).holds_reviews(),
        }))
    }

    /// Whether this pause holds `id`, with the same exemptions as coordinator
    /// admission. The per-item reference [`Self::held_among`] is tested against.
    #[cfg(test)]
    pub(crate) fn holds(&self, conn: &Connection, id: &str) -> Result<bool> {
        use boss_engine_storage_values::dispatch_pause::exempts_work;
        if !self.overridable {
            return Ok(true);
        }
        let facts = dispatch_admission_facts_on(conn, id)?;
        Ok(!exempts_work(
            self.overridable,
            facts.exempt_from_operator_pause,
            facts.targets_main_pool && facts.is_critical,
        ))
    }

    /// The ids of `tasks` this pause holds: [`Self::holds`] for each, from a
    /// fixed number of statements however many there are. Only an overridable
    /// pause exempts anything, so only then are the governing execution kinds
    /// and automation sources read, once for the whole set.
    pub(crate) fn held_among(&self, conn: &Connection, tasks: &[&Task]) -> Result<HashSet<String>> {
        use boss_engine_storage_values::dispatch_pause::exempts_work;
        if !self.overridable || tasks.is_empty() {
            return Ok(tasks.iter().map(|task| task.id.clone()).collect());
        }
        let ids: Vec<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
        let ids_json = super::query_ensure::id_array(&ids);

        // The governing execution of `dispatch_admission_facts_on`: the
        // newest live run, else the newest run if it is not terminal.
        let mut stmt = conn.prepare(
            "SELECT e.work_item_id, e.kind, e.status
             FROM json_each(?1) item
             JOIN work_executions e ON e.work_item_id = item.value
             WHERE e.kind <> 'progression'
             ORDER BY e.work_item_id, e.created_at DESC, e.id DESC",
        )?;
        let mut live: HashMap<String, ExecutionKind> = HashMap::new();
        let mut latest: HashMap<String, (ExecutionKind, ExecutionStatus)> = HashMap::new();
        let mut rows = stmt.query([&ids_json])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let kind_raw: String = row.get(1)?;
            let kind = kind_raw.parse::<ExecutionKind>().map_err(|e| anyhow::anyhow!("{e}"))?;
            let status_raw: String = row.get(2)?;
            let status = status_raw
                .parse::<ExecutionStatus>()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if status.is_live() {
                live.entry(id.clone()).or_insert(kind.clone());
            }
            latest.entry(id).or_insert((kind, status));
        }
        let mut stmt = conn.prepare(
            "SELECT t.id FROM json_each(?1) item
             JOIN tasks t ON t.id = item.value
             WHERE t.deleted_at IS NULL AND t.source_automation_id IS NOT NULL",
        )?;
        let from_automation: HashSet<String> = stmt
            .query_map([&ids_json], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;

        Ok(tasks
            .iter()
            .filter(|task| {
                let governing = live.get(&task.id).or_else(|| {
                    latest
                        .get(&task.id)
                        .filter(|(_, status)| !status.is_terminal())
                        .map(|(k, _)| k)
                });
                let kind = match governing {
                    Some(kind) => kind.clone(),
                    None => super::exec_status_helpers::execution_kind_for_task_kind(&task.kind),
                };
                let (review, targets_main) = pause_exemption_facts(Some(kind), || from_automation.contains(&task.id));
                !exempts_work(
                    self.overridable,
                    review,
                    targets_main && critical_priority::is_critical(&task.priority),
                )
            })
            .map(|task| task.id.clone())
            .collect())
    }
}

#[cfg(test)]
mod churn_guard_parked_fact_tests {
    use crate::test_support::*;

    /// `orphan_sweep`'s representation: `dispatch_failed_reason =
    /// 'churn_guard'` on the task row, no attention item.
    #[test]
    fn true_via_dispatch_failed_reason() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        db.bounce_churn_guard_parked_to_backlog(
            &work_item_id,
            "orphan_sweep",
            3,
            &[],
            "terminal executions",
            crate::ChurnTrip::Window,
        );

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(facts.churn_guard_parked);
    }

    /// `pr_review_recovery`'s representation: an open `churn_guard_parked`
    /// attention item, task status untouched.
    #[test]
    fn true_via_open_attention_item() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        db.file_churn_guard_parked_attention(
            &work_item_id,
            "pr_review_recovery",
            3,
            &[],
            "terminal pr_review executions",
        );

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(facts.churn_guard_parked);
    }

    /// Neither representation present: not reported as parked.
    #[test]
    fn false_when_neither_representation_present() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(!facts.churn_guard_parked);
    }
}

#[cfg(test)]
mod deliberate_parked_fact_tests {
    use crate::test_support::*;
    use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus, RunDoneOutcome};

    fn stamp_run_done_outcome(db: &crate::WorkDb, execution_id: &str, outcome: &str) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET run_done_outcome = ?2 WHERE id = ?1",
                rusqlite::params![execution_id, outcome],
            )
            .unwrap();
    }

    fn abandoned_execution(db: &crate::WorkDb, work_item_id: &str) -> String {
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Abandoned)
                .build(),
        )
        .unwrap()
        .id
    }

    /// Durable signal: the latest execution declared `blocked`.
    #[test]
    fn true_via_latest_run_done_outcome_blocked() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = abandoned_execution(&db, &work_item_id);
        stamp_run_done_outcome(&db, &execution_id, RunDoneOutcome::Blocked.as_str());

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(facts.deliberate_parked);
    }

    /// Nudge-breaker park never stamps `run_done_outcome`; the open
    /// attention item is the other representation of the same fact.
    #[test]
    fn true_via_open_park_attention_without_column() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = abandoned_execution(&db, &work_item_id);
        db.create_attention_item(boss_protocol::CreateAttentionItemInput {
            execution_id: Some(execution_id),
            work_item_id: None,
            kind: boss_engine_storage_values::attention_kinds::NUDGE_BREAKER_ATTENTION_KIND.to_owned(),
            status: None,
            title: "nudge breaker parked".to_owned(),
            body_markdown: "parked".to_owned(),
            resolved_at: None,
        })
        .unwrap();

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(facts.deliberate_parked);
    }

    /// An older blocked declaration must not park a later execution that
    /// did not declare blocked — operator start mints a new latest row.
    /// Production always files an open `run_done_declared_blocked`
    /// attention item alongside the column stamp, so this also pins that
    /// the attention half of the fact is scoped to the latest execution,
    /// not "any execution of this work item, ever": without that scoping
    /// the open item on `older` alone would keep `deliberate_parked` true
    /// forever, even though the column half already says otherwise.
    #[test]
    fn false_when_only_an_older_execution_declared_blocked() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let older = abandoned_execution(&db, &work_item_id);
        stamp_run_done_outcome(&db, &older, RunDoneOutcome::Blocked.as_str());
        db.create_attention_item(boss_protocol::CreateAttentionItemInput {
            execution_id: Some(older.clone()),
            work_item_id: None,
            kind: boss_engine_storage_values::attention_kinds::RUN_DONE_BLOCKED_ATTENTION_KIND.to_owned(),
            status: None,
            title: "Run ended: worker declared itself blocked".to_owned(),
            body_markdown: "blocked".to_owned(),
            resolved_at: None,
        })
        .unwrap();
        let _newer = abandoned_execution(&db, &work_item_id);

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(!facts.deliberate_parked);
    }

    #[test]
    fn false_when_latest_declared_delivered() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = abandoned_execution(&db, &work_item_id);
        stamp_run_done_outcome(&db, &execution_id, RunDoneOutcome::Delivered.as_str());

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(!facts.deliberate_parked);
    }

    #[test]
    fn false_when_neither_column_nor_open_attention() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let _execution_id = abandoned_execution(&db, &work_item_id);

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(!facts.deliberate_parked);
    }
}

#[cfg(test)]
mod owner_park_admission_tests {
    use crate::test_support::*;
    use boss_protocol::{CreateProjectInput, CreateTaskInput, LAST_STATUS_ACTOR_HUMAN};

    fn request_refusal(db: &crate::WorkDb, work_item_id: &str) -> String {
        db.request_execution(
            boss_protocol::RequestExecutionInput::builder()
                .work_item_id(work_item_id)
                .build(),
        )
        .unwrap_err()
        .to_string()
    }

    #[test]
    fn admission_agrees_with_request_execution_for_a_directly_parked_task() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_test_chore_manual(&db, &product_id, "parked start").id;
        db.park_work_item_as_actor(&work_item_id, true, Some("not now"), LAST_STATUS_ACTOR_HUMAN)
            .unwrap();

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        let request = request_refusal(&db, &work_item_id);
        let reason = facts.ineligible_reason.expect("parked row is ineligible");
        assert_eq!(reason, request);
        assert!(reason.contains("un-park it first"), "{reason}");
    }

    #[test]
    fn admission_agrees_with_request_execution_for_a_child_of_a_parked_project() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project_id = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(&product_id)
                    .name("Parked project")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap()
            .id;
        let child = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(&product_id)
                    .project_id(&project_id)
                    .name("child")
                    .autostart(false)
                    .build(),
            )
            .unwrap()
            .id;
        db.park_work_item_as_actor(&project_id, true, Some("focus elsewhere"), LAST_STATUS_ACTOR_HUMAN)
            .unwrap();

        let facts = db.dispatch_admission_facts(&child).unwrap();
        let request = request_refusal(&db, &child);
        let reason = facts.ineligible_reason.expect("project-parked child is ineligible");
        assert_eq!(reason, request);
        assert!(
            reason.contains(&format!("because project {project_id} is parked")),
            "{reason}"
        );
    }
}
