//! The engine's one answer to "why is this work item waiting rather than
//! running?".
//!
//! Every surface that shows a wait reads it from here: the board's runtime
//! projection, `bossctl dispatch stuck`, `bossctl dispatch diagnose`, and the
//! scheduler heartbeat's unexplained-wait check. The classification is total:
//! a pending execution, a card the board queues with no execution at all, and
//! an `active` item with no run pending or live always get a reason from
//! `boss_engine_storage_values::dispatch_wait`. [`for_item`] is the entry
//! point that covers all three. When
//! no named condition holds the answer is
//! [`DISPATCH_WAIT_REASON_UNCLASSIFIED`], which is reported as an anomaly
//! rather than shown as a reason.
use super::*;
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AUTOSTART_OFF, DISPATCH_WAIT_REASON_AWAITING_DISPATCH,
    DISPATCH_WAIT_REASON_AWAITING_PARENT_PR, DISPATCH_WAIT_REASON_AWAITING_REDISPATCH,
    DISPATCH_WAIT_REASON_AWAITING_REVIEW, DISPATCH_WAIT_REASON_DEFERRED, DISPATCH_WAIT_REASON_DISPATCH_FAILED,
    DISPATCH_WAIT_REASON_HUMAN_DRIVEN, DISPATCH_WAIT_REASON_NOT_BEFORE, DISPATCH_WAIT_REASON_PARKED,
    DISPATCH_WAIT_REASON_PR_OPEN, DISPATCH_WAIT_REASON_PROJECT_ORDER, DISPATCH_WAIT_REASON_RECOVERY_ATTENTION,
    DISPATCH_WAIT_REASON_REPO_UNRESOLVED, DISPATCH_WAIT_REASON_RUN_ENDED, DISPATCH_WAIT_REASON_UNCLASSIFIED,
    DISPATCH_WAIT_REASON_WAITING_DEPENDENCY, UnexplainedWait, dispatch_wait_phrase, is_unlabelled_wait_code,
};
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AWAITING_MERGE, DISPATCH_WAIT_REASON_DELIBERATE_PARK, DISPATCH_WAIT_REASON_DISPATCH_PAUSED,
    DISPATCH_WAIT_REASON_REVIEW_BATCH, DISPATCH_WAIT_REASON_REVIEW_RESERVATION,
};
use boss_protocol::DispatchWaitBlocker;

#[derive(Default)]
pub(super) struct DispatchWait {
    pub reason: Option<String>,
    /// The phrase for `reason`; set whenever `reason` is.
    pub label: Option<String>,
    pub not_before: Option<String>,
    pub blocker: Option<DispatchWaitBlocker>,
    /// The projection consulted state the item's own execution rows do not
    /// version: another work item (dependency gating, project order, a
    /// chain-hold blocker) or the item's task row. Such a result is recomputed
    /// on every read and never retained by the board runtime cache.
    pub reads_other_items: bool,
}

impl DispatchWait {
    fn named(code: &str) -> Self {
        Self {
            reason: Some(code.to_owned()),
            label: Some(dispatch_wait_phrase(code)),
            ..Self::default()
        }
    }

    /// A wait behind another work item, named in the phrase when it has a
    /// short id.
    fn behind(code: &str, blocker: Option<DispatchWaitBlocker>) -> Self {
        let named = blocker.as_ref().and_then(blocker_short_label);
        let label = match (code, named) {
            (DISPATCH_WAIT_REASON_PROJECT_ORDER, Some(name)) => {
                format!("waiting for {name}, an earlier task in its project")
            }
            (_, Some(name)) => format!("waiting for {name}"),
            (_, None) => dispatch_wait_phrase(code),
        };
        Self {
            reason: Some(code.to_owned()),
            label: Some(label),
            blocker,
            ..Self::default()
        }
    }

    pub(super) fn is_unclassified(&self) -> bool {
        self.reason.as_deref() == Some(DISPATCH_WAIT_REASON_UNCLASSIFIED)
    }
}

/// `T<n>` / `P<n>` for a blocker that has a short id.
fn blocker_short_label(blocker: &DispatchWaitBlocker) -> Option<String> {
    let prefix = if blocker.kind == "project" { 'P' } else { 'T' };
    blocker.short_id.map(|n| format!("{prefix}{n}"))
}

/// Classify a pending execution. Only pending state is projected: an old hold
/// is never leaked onto a claimed, live or finished run, which is not waiting
/// on dispatch at all.
pub(super) fn for_execution(conn: &Connection, execution: Option<&WorkExecution>, now: i64) -> Result<DispatchWait> {
    let Some(execution) = execution else {
        return Ok(DispatchWait::default());
    };
    match execution.status {
        ExecutionStatus::WaitingDependency => for_gated(conn, execution),
        ExecutionStatus::Ready | ExecutionStatus::Queued => for_ready(conn, execution, now),
        ExecutionStatus::WaitingReview => review_wait(conn, &execution.work_item_id),
        ExecutionStatus::WaitingMerge => Ok(DispatchWait::named(DISPATCH_WAIT_REASON_AWAITING_MERGE)),
        _ => Ok(DispatchWait::default()),
    }
}

/// A `waiting_dependency` row is off the scheduler's walk, so a hold a drain
/// pass recorded while it was `ready` no longer says anything about it. It is
/// waiting behind an unmet prerequisite, behind its project's order, or (with
/// automatic start off) for an explicit start. Anything else is a row nothing
/// will promote.
fn for_gated(conn: &Connection, execution: &WorkExecution) -> Result<DispatchWait> {
    // Reuse dispatch's revision-aware satisfaction rules, including same-PR
    // sequencing. A missing prerequisite row still gates.
    let mut wait = if let Some(id) = deps::gating_prereqs_for(conn, &execution.work_item_id)?.first() {
        DispatchWait::behind(DISPATCH_WAIT_REASON_WAITING_DEPENDENCY, blocker_for(conn, id)?)
    } else {
        match query_task(conn, &execution.work_item_id)? {
            Some(task) => match project_order_head(conn, &task)? {
                Some(head) => DispatchWait::behind(DISPATCH_WAIT_REASON_PROJECT_ORDER, blocker_for(conn, &head)?),
                None if task.status == TaskStatus::Todo && !task.autostart => {
                    DispatchWait::named(DISPATCH_WAIT_REASON_AUTOSTART_OFF)
                }
                None => DispatchWait::named(DISPATCH_WAIT_REASON_UNCLASSIFIED),
            },
            None => DispatchWait::named(DISPATCH_WAIT_REASON_UNCLASSIFIED),
        }
    };
    wait.reads_other_items = true;
    Ok(wait)
}

/// A `ready` row waits out a dispatch deadline, sits behind a hold the
/// scheduler recorded on its last pass, or has not been reached by a pass yet.
fn for_ready(conn: &Connection, execution: &WorkExecution, now: i64) -> Result<DispatchWait> {
    if let Some(epoch) = execution
        .dispatch_not_before
        .as_deref()
        .and_then(|raw| raw.parse::<i64>().ok())
        .filter(|epoch| *epoch > now)
    {
        let mut wait = DispatchWait::named(DISPATCH_WAIT_REASON_NOT_BEFORE);
        wait.not_before = Some(epoch.to_string());
        return Ok(wait);
    }
    if let Some(reason) = execution
        .dispatch_wait_reason
        .as_ref()
        .filter(|reason| !reason.trim().is_empty())
    {
        let blocker_id: Option<String> = conn.query_row(
            "SELECT dispatch_wait_blocker_id FROM work_executions WHERE id = ?1",
            [&execution.id],
            |row| row.get(0),
        )?;
        match blocker_id {
            // A persisted chain hold is only current while its named writer/reviewer is live.
            // Do not carry its prose or link through the gap before the next scheduler pass.
            Some(id) if !blocker_is_live(conn, &id)? => {
                let mut wait = unheld(execution);
                wait.reads_other_items = true;
                return Ok(wait);
            }
            Some(id) => {
                let mut wait = recorded(reason);
                wait.blocker = blocker_for(conn, &id)?;
                wait.reads_other_items = true;
                return Ok(wait);
            }
            None => return Ok(recorded(reason)),
        }
    }
    Ok(unheld(execution))
}

/// A hold the scheduler recorded. A code nobody gave a phrase is not shown as
/// an identifier: it is the unclassified anomaly.
fn recorded(reason: &str) -> DispatchWait {
    if is_unlabelled_wait_code(reason) {
        return DispatchWait {
            reason: Some(DISPATCH_WAIT_REASON_UNCLASSIFIED.to_owned()),
            label: Some(dispatch_wait_phrase(reason)),
            ..DispatchWait::default()
        };
    }
    DispatchWait {
        reason: Some(reason.to_owned()),
        label: Some(dispatch_wait_phrase(reason)),
        ..DispatchWait::default()
    }
}

/// A pending row with no hold on it. `ready` means the scheduler's next pass
/// claims it or records what stops it. Nothing walks a `queued` row, so one
/// without a hold has no reason at all.
fn unheld(execution: &WorkExecution) -> DispatchWait {
    if execution.status == ExecutionStatus::Ready {
        DispatchWait::named(DISPATCH_WAIT_REASON_AWAITING_DISPATCH)
    } else {
        DispatchWait::named(DISPATCH_WAIT_REASON_UNCLASSIFIED)
    }
}

/// The earlier task of `task`'s project that the reconciler's ordinal chain
/// holds it behind, or `None` when the chain does not hold it.
///
/// Mirrors `reconcile_tasks`: the project's `project_task` / `design` rows
/// in `(ordinal, created_at, id)` order, where only the first one that still
/// accepts an execution is `ready`. A task that is already `active`, or that
/// does not accept an execution itself, is not held by the chain.
fn project_order_head(conn: &Connection, task: &Task) -> Result<Option<String>> {
    if !matches!(task.kind, TaskKind::ProjectTask | TaskKind::Design)
        || task.status == TaskStatus::Active
        || !task_accepts_execution(task)
    {
        return Ok(None);
    }
    let Some(project_id) = task.project_id.as_deref() else {
        return Ok(None);
    };
    // Rows that can never accept an execution are dropped in SQL; the rest go
    // through the reconciler's own predicate.
    let mut stmt = conn.prepare_cached(
        "SELECT id FROM tasks
         WHERE project_id = ?1 AND deleted_at IS NULL
           AND kind IN ('project_task', 'design')
           AND status NOT IN ('done', 'archived', 'in_review')
         ORDER BY ordinal IS NULL, ordinal ASC, created_at ASC, id ASC",
    )?;
    let ids = stmt
        .query_map([project_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        if id == task.id {
            return Ok(None);
        }
        if query_task(conn, &id)?.is_some_and(|earlier| task_accepts_execution(&earlier)) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// The one item-level classification: why `task` is waiting rather than
/// running, or the default (no reason) when it is not waiting on dispatch.
/// `latest` is the item's most recent execution.
///
/// A pending latest execution is classified from that row; otherwise the
/// item is classified from its own row ([`for_unscheduled`]). Every surface
/// that names a wait goes through here, so they cannot disagree.
pub(super) fn for_item(
    conn: &Connection,
    task: &Task,
    latest: Option<&WorkExecution>,
    now: i64,
) -> Result<DispatchWait> {
    if latest.is_some_and(|execution| {
        matches!(
            execution.status,
            ExecutionStatus::WaitingDependency
                | ExecutionStatus::Ready
                | ExecutionStatus::Queued
                | ExecutionStatus::WaitingReview
                | ExecutionStatus::WaitingMerge
        )
    }) {
        return for_execution(conn, latest, now);
    }
    Ok(for_unscheduled(conn, task, latest.map(|execution| &execution.status))?.unwrap_or_default())
}

/// Classify an item with nothing pending for it: a card the board lists as
/// queued (`todo` with automatic start on), or an `active` item with no run
/// pending or live. `latest` is the item's most recent execution. Returns
/// `None` when the item is in neither state.
///
/// For a queued card the named reasons are the reconciler's own refusals to
/// mint an execution, in its order (`reconcile_work_item_execution` /
/// `reconcile_revision_execution`). When none of them applies the reconciler
/// would have minted one, so its absence is unclassified.
pub(super) fn for_unscheduled(
    conn: &Connection,
    task: &Task,
    latest: Option<&ExecutionStatus>,
) -> Result<Option<DispatchWait>> {
    if task.deleted_at.is_none() {
        match latest {
            Some(ExecutionStatus::WaitingReview) => return review_wait(conn, &task.id).map(Some),
            Some(ExecutionStatus::WaitingMerge) => {
                return Ok(Some(DispatchWait::named(DISPATCH_WAIT_REASON_AWAITING_MERGE)));
            }
            _ => {}
        }
    }
    if task.deleted_at.is_some() || latest.is_some_and(|status| !status.is_terminal()) {
        return Ok(None);
    }
    match task.status {
        TaskStatus::Todo if task.autostart => for_queued_card(conn, task, latest).map(Some),
        TaskStatus::Active => for_idle_active(conn, task).map(Some),
        TaskStatus::InReview => review_wait(conn, &task.id).map(Some),
        _ => Ok(None),
    }
}

/// Holds shared by idle cards and review handling, including inherited holds.
fn item_hold(conn: &Connection, task: &Task) -> Result<Option<&'static str>> {
    Ok(if task.human_driven || work_item_is_human_driven(conn, &task.id)? {
        Some(DISPATCH_WAIT_REASON_HUMAN_DRIVEN)
    } else if task.is_parked() || !parking::upkeep_eligible(conn, &task.id)? {
        Some(DISPATCH_WAIT_REASON_PARKED)
    } else {
        None
    })
}

fn review_wait(conn: &Connection, id: &str) -> Result<DispatchWait> {
    use boss_engine_storage_values::dispatch_wait::{
        DISPATCH_WAIT_REASON_HUMAN_REVIEW, DISPATCH_WAIT_REASON_REVIEW_COMPLETE, DISPATCH_WAIT_REASON_REVISION_PENDING,
    };
    let Some(task) = query_task(conn, id)? else {
        return Ok(DispatchWait::named(DISPATCH_WAIT_REASON_UNCLASSIFIED));
    };
    if let Some(code) = item_hold(conn, &task)? {
        return Ok(DispatchWait::named(code));
    }
    let root = chain_root(conn, id)?;
    let human_review = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_attention_items
         WHERE work_item_id IN (?1, ?2) AND kind = 'pr_review_cycle_bound' AND status = 'open')",
        params![id, root],
        |row| row.get::<_, bool>(0),
    )?;
    let reviews =
        super::review_badge::fetch_current_heads(conn, &[id.to_owned()], super::review_badge::ReviewKey::Card)?;
    let review = reviews.get(id);
    let revision_pending = review
        .and_then(|review| review.findings_revision_id.as_deref())
        .map(|id| query_task(conn, id))
        .transpose()?
        .flatten()
        .is_some_and(|revision| {
            revision.deleted_at.is_none()
                && matches!(
                    revision.status,
                    TaskStatus::Todo | TaskStatus::Active | TaskStatus::Blocked
                )
        });
    let code = if human_review {
        DISPATCH_WAIT_REASON_HUMAN_REVIEW
    } else if super::dispatch::live_pre_merge_batch_holds(conn, id)? {
        DISPATCH_WAIT_REASON_REVIEW_BATCH
    } else if revision_pending {
        DISPATCH_WAIT_REASON_REVISION_PENDING
    } else if task.review_required_state.as_deref() == Some("changes_requested")
        || review.is_some_and(|review| review.state == AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS)
    {
        DISPATCH_WAIT_REASON_HUMAN_REVIEW
    } else if review.is_some_and(|review| {
        matches!(
            review.state,
            AI_REVIEW_STATE_REVIEWED_CLEAN_PENDING | AI_REVIEW_STATE_REVIEWED_ALL_CLEAR
        )
    }) {
        DISPATCH_WAIT_REASON_REVIEW_COMPLETE
    } else if !WorkDb::pre_merge_review_admission_candidates_in(conn, Some(id))?.is_empty() {
        DISPATCH_WAIT_REASON_REVIEW_RESERVATION
    } else if task.review_required_state.as_deref() == Some("approved") {
        DISPATCH_WAIT_REASON_AWAITING_MERGE
    } else if matches!(
        task.review_required_state.as_deref(),
        Some("required" | "review_required")
    ) {
        DISPATCH_WAIT_REASON_HUMAN_REVIEW
    } else if task.pr_url.as_deref().is_some_and(|url| !url.trim().is_empty())
        && matches!(task.review_required_state.as_deref(), None | Some("unknown"))
        && (task_kind_excluded_from_ai_review(&task.kind)
            || review.is_some_and(|review| review.state == AI_REVIEW_STATE_NOT_REVIEWED))
        && !conn.query_row(
            &format!(
                "{} SELECT EXISTS(SELECT 1 FROM family f JOIN tasks t ON t.id = f.id
                 WHERE t.deleted_at IS NULL AND (
                     (t.kind = 'revision' AND t.status IN ('todo', 'active', 'blocked'))
                     OR EXISTS(SELECT 1 FROM work_executions e WHERE e.work_item_id = t.id
                         AND e.kind = 'pr_review'
                         AND e.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned'))))",
                super::review_badge::review_family_cte("?1", super::review_badge::ReviewKey::Card)
            ),
            [id],
            |row| row.get::<_, bool>(0),
        )?
    {
        DISPATCH_WAIT_REASON_AWAITING_MERGE
    } else {
        DISPATCH_WAIT_REASON_UNCLASSIFIED
    };
    Ok(DispatchWait::named(code))
}

/// An `active` item with no run pending or live. The orphan sweep is what
/// starts a replacement run, so the sweep's own candidate predicate names the
/// ordinary state; the holds it leaves alone are named in turn. Anything
/// else is the anomaly [`DISPATCH_WAIT_REASON_UNCLASSIFIED`].
fn for_idle_active(conn: &Connection, task: &Task) -> Result<DispatchWait> {
    let code = if let Some(code) = item_hold(conn, task)? {
        code
    } else if task.dispatch_failed_reason.is_some() {
        DISPATCH_WAIT_REASON_DISPATCH_FAILED
    } else if conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_attention_items
         WHERE work_item_id = ?1 AND status = 'open' AND kind IN (?2, ?3))",
        params![
            task.id,
            ATTENTION_KIND_RECOVERY_PERMANENT,
            ATTENTION_KIND_RECOVERY_EXHAUSTED
        ],
        |row| row.get::<_, bool>(0),
    )? {
        DISPATCH_WAIT_REASON_RECOVERY_ATTENTION
    } else if super::dispatch_admission::work_item_is_deliberately_parked(conn, &task.id)? {
        DISPATCH_WAIT_REASON_DELIBERATE_PARK
    } else if !super::dispatch::orphan_active_candidates(conn, i64::MAX, Some(&task.id))?.is_empty() {
        if super::dispatch_admission::pause_holds_on(conn, &task.id)? {
            DISPATCH_WAIT_REASON_DISPATCH_PAUSED
        } else {
            DISPATCH_WAIT_REASON_AWAITING_REDISPATCH
        }
    } else if super::dispatch_admission::primary_pr_awaits_review(conn, &task.id)?
        || super::dispatch_admission::revision_implementation_completed(conn, &task.id)?
        || super::dispatch::live_pre_merge_batch_holds(conn, &task.id)?
        || conn.query_row(
            &format!("SELECT {}", super::dispatch::review_recovery_holds_sql("?1")),
            [&task.id],
            |row| row.get::<_, bool>(0),
        )?
    {
        // The sweep leaves these to review handling: a delivered pull request,
        // a completed revision, a live review batch or a review pass that
        // review recovery owns.
        DISPATCH_WAIT_REASON_AWAITING_REVIEW
    } else {
        DISPATCH_WAIT_REASON_UNCLASSIFIED
    };
    Ok(DispatchWait::named(code))
}

/// A `todo` card with automatic start on, nothing pending for it and no live
/// run.
fn for_queued_card(conn: &Connection, task: &Task, latest: Option<&ExecutionStatus>) -> Result<DispatchWait> {
    let code = if let Some(code) = item_hold(conn, task)? {
        code
    } else if task.deferred || work_item_is_deferred(conn, &task.id)? {
        DISPATCH_WAIT_REASON_DEFERRED
    } else if latest.is_some() {
        // The reconciler never mints a replacement after a finished run.
        DISPATCH_WAIT_REASON_RUN_ENDED
    } else if task.kind == TaskKind::Revision {
        match get_chain_root_task(conn, &task.id)? {
            Some(root) if root.pr_url.as_deref().is_none_or(|url| url.trim().is_empty()) => {
                DISPATCH_WAIT_REASON_AWAITING_PARENT_PR
            }
            _ => DISPATCH_WAIT_REASON_UNCLASSIFIED,
        }
    } else if task.pr_url.as_deref().is_some_and(|url| !url.trim().is_empty()) {
        DISPATCH_WAIT_REASON_PR_OPEN
    } else if resolve_repo_for_work_item(conn, &task.id)?.is_none() {
        DISPATCH_WAIT_REASON_REPO_UNRESOLVED
    } else {
        DISPATCH_WAIT_REASON_UNCLASSIFIED
    };
    Ok(DispatchWait::named(code))
}

/// Write [`for_unscheduled`]'s answer onto `runtime`, for callers that hold
/// the task row. Applied after the execution-keyed projection and after any
/// board runtime cache hit, because the answer comes from the task row, which
/// that cache does not version.
///
/// Inputs include the task and parent chain, executions, project/product,
/// recovery attention, deliberate parks, pause metadata and review batches.
/// The board journal invalidates these inputs, including cycle-root holds;
/// this annotation is recomputed after every execution-cache hit.
pub(super) fn annotate_unscheduled(conn: &Connection, task: &Task, runtime: &mut TaskRuntime) -> Result<()> {
    if let Some(wait) = for_unscheduled(conn, task, runtime.execution_status.as_ref())? {
        runtime.dispatch_wait_reason = wait.reason;
        runtime.dispatch_wait_label = wait.label;
    }
    Ok(())
}

/// Whether a recorded chain-hold blocker still holds the chain: running, waiting on a human,
/// or claimed (in-flight, cube setup not yet committed to `running`). The scheduler holds on
/// in-flight siblings too, so `query_live_execution_for_work_item` alone would drop that window.
fn blocker_is_live(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_executions
         WHERE work_item_id = ?1 AND status IN ('claimed', 'running', 'waiting_human'))",
        [work_item_id],
        |row| row.get(0),
    )?)
}

fn blocker_for(conn: &Connection, id: &str) -> Result<Option<DispatchWaitBlocker>> {
    if let Some(task) = query_task(conn, id)? {
        return Ok(Some(DispatchWaitBlocker {
            work_item_id: task.id,
            product_id: task.product_id,
            short_id: task.short_id,
            kind: "task".into(),
        }));
    }
    Ok(query_project(conn, id)?.map(|project| DispatchWaitBlocker {
        work_item_id: project.id,
        product_id: project.product_id,
        short_id: project.short_id,
        kind: "project".into(),
    }))
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Every wait the engine has not explained, for the scheduler heartbeat.
    ///
    /// Two kinds of row come back. An unclassified wait is one the classifier
    /// could not name: a pending execution with no hold, gate or order behind
    /// it, or a card queued on the board with no execution and no reason for
    /// the reconciler to have withheld one. An `awaiting_dispatch` wait is a
    /// `ready` row no scheduler pass has claimed or held yet, which is
    /// expected for a moment and unexplained if it lasts. The caller decides
    /// how long is too long.
    ///
    /// A `waiting_dependency` row only counts while its task is one the board
    /// shows as waiting (`active`, or `todo` with automatic start on).
    pub fn list_unexplained_dispatch_waits(&self) -> Result<Vec<UnexplainedWait>> {
        let conn = self.connect()?;
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let mut waits = Vec::new();

        let pending: Vec<String> = conn
            .prepare(
                "SELECT id FROM work_executions
                 WHERE status IN ('queued', 'ready', 'waiting_dependency', 'waiting_review', 'waiting_merge')
                 ORDER BY created_at ASC, id ASC",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in pending {
            let Some(execution) = query_execution(&conn, &id)? else {
                continue;
            };
            let wait = for_execution(&conn, Some(&execution), now)?;
            let awaiting = wait.reason.as_deref() == Some(DISPATCH_WAIT_REASON_AWAITING_DISPATCH);
            if !wait.is_unclassified() && !awaiting {
                continue;
            }
            let task = query_task(&conn, &execution.work_item_id)?;
            let board_waiting = task.as_ref().is_none_or(|task| {
                task.deleted_at.is_none()
                    && (task.status == TaskStatus::Active || (task.status == TaskStatus::Todo && task.autostart))
            });
            if execution.status == ExecutionStatus::WaitingDependency && !board_waiting {
                continue;
            }
            waits.push(UnexplainedWait {
                work_item_id: execution.work_item_id.clone(),
                reason: wait.reason.unwrap_or_default(),
                task_status: task.map(|task| task.status.as_str().to_owned()),
                execution: Some(execution),
            });
        }

        // Rows whose own columns already name the hold (deferred, human-driven,
        // parked) are skipped in SQL: this runs on every heartbeat and a
        // product can carry many of them. The classifier decides the rest.
        let unscheduled: Vec<String> = conn
            .prepare(
                "SELECT t.id FROM tasks t
                 WHERE t.status = 'todo' AND t.autostart = 1 AND t.deleted_at IS NULL
                   AND COALESCE(t.deferred, 0) = 0 AND COALESCE(t.human_driven, 0) = 0
                   AND t.parked_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM work_executions we
                       WHERE we.work_item_id = t.id
                         AND we.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned'))
                 ORDER BY t.id ASC",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in unscheduled {
            let Some(task) = query_task(&conn, &id)? else {
                continue;
            };
            let latest = query_latest_execution_for_work_item(&conn, &id)?.map(|execution| execution.status);
            if for_unscheduled(&conn, &task, latest.as_ref())?.is_some_and(|wait| wait.is_unclassified()) {
                waits.push(UnexplainedWait {
                    work_item_id: id,
                    execution: None,
                    reason: DISPATCH_WAIT_REASON_UNCLASSIFIED.to_owned(),
                    task_status: Some(task.status.as_str().to_owned()),
                });
            }
        }

        // Active items with no run pending or live: the orphan sweep starts a
        // replacement, so most are `awaiting_redispatch`; any the classifier
        // cannot name is reported.
        let idle_active: Vec<String> = conn
            .prepare(
                "SELECT t.id FROM tasks t
                 WHERE t.status IN ('active', 'in_review') AND t.deleted_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM work_executions we
                       WHERE we.work_item_id = t.id
                         AND we.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned'))
                 ORDER BY t.id ASC",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in idle_active {
            let Some(task) = query_task(&conn, &id)? else {
                continue;
            };
            let latest = query_latest_execution_for_work_item(&conn, &id)?.map(|execution| execution.status);
            if for_unscheduled(&conn, &task, latest.as_ref())?.is_some_and(|wait| wait.is_unclassified()) {
                waits.push(UnexplainedWait {
                    work_item_id: id,
                    execution: None,
                    reason: DISPATCH_WAIT_REASON_UNCLASSIFIED.to_owned(),
                    task_status: Some(task.status.as_str().to_owned()),
                });
            }
        }
        Ok(waits)
    }

    /// Reclassify a `ready` row that scheduler passes have left unexplained as
    /// [`DISPATCH_WAIT_REASON_UNCLASSIFIED`], so its card stops saying it is
    /// about to be dispatched. `observed_reason` is the recorded hold the
    /// caller classified from; the write is skipped (returning `false`) when
    /// the row has since been claimed or given a hold. The next pass that
    /// claims the row or records a real hold replaces this value.
    pub fn mark_dispatch_wait_unclassified(&self, execution_id: &str, observed_reason: Option<&str>) -> Result<bool> {
        let conn = self.connect()?;
        let changed = conn.execute(
            "UPDATE work_executions
             SET dispatch_wait_reason = ?2, dispatch_wait_since = ?3, dispatch_wait_blocker_id = NULL
             WHERE id = ?1 AND status = 'ready' AND dispatch_wait_reason IS ?4",
            params![
                execution_id,
                DISPATCH_WAIT_REASON_UNCLASSIFIED,
                now_string(),
                observed_reason
            ],
        )?;
        Ok(changed > 0)
    }
}
