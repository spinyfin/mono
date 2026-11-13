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
use super::unscheduled_wait::{self, Candidate, ClassifyCost};
use super::*;
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AUTOSTART_OFF, DISPATCH_WAIT_REASON_AWAITING_DISPATCH, DISPATCH_WAIT_REASON_AWAITING_MERGE,
    DISPATCH_WAIT_REASON_FAILURE_BACKOFF, DISPATCH_WAIT_REASON_HOST_LEASE_PARK, DISPATCH_WAIT_REASON_NOT_BEFORE,
    DISPATCH_WAIT_REASON_PROJECT_ORDER, DISPATCH_WAIT_REASON_UNCLASSIFIED, DISPATCH_WAIT_REASON_WAITING_DEPENDENCY,
    UnexplainedWait, dispatch_wait_phrase, failure_backoff_wait_label, is_unlabelled_wait_code,
};
use boss_protocol::{DispatchFailureAttempt, DispatchWaitBlocker};

#[derive(Default, bon::Builder)]
#[builder(on(String, into))]
pub(super) struct DispatchWait {
    pub reason: Option<String>,
    /// The phrase for `reason`; set whenever `reason` is.
    pub label: Option<String>,
    pub not_before: Option<String>,
    pub blocker: Option<DispatchWaitBlocker>,
    /// Failed attempts that produced a [`DISPATCH_WAIT_REASON_FAILURE_BACKOFF`]
    /// wait. Empty for every other reason.
    pub failures: Vec<DispatchFailureAttempt>,
    /// The projection consulted state the item's own execution rows do not
    /// version: another work item (dependency gating, project order, a
    /// chain-hold blocker) or the item's task row. Such a result is recomputed
    /// on every read and never retained by the board runtime cache.
    pub reads_other_items: bool,
}

impl DispatchWait {
    pub(super) fn named(code: &str) -> Self {
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

/// Who names the wait of a run parked waiting for review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewNaming {
    /// The execution projection, by classifying the run's item now.
    Inline,
    /// The caller, which classifies every item with nothing pending as a set
    /// afterwards ([`annotate_unscheduled_many`]). That pass names a parked
    /// run from the same item row, so naming it here as well would only
    /// repeat the work once per card.
    Deferred,
}

/// Classify a pending execution. Only pending state is projected: an old hold
/// is never leaked onto a claimed, live or finished run, which is not waiting
/// on dispatch at all.
pub(super) fn for_execution(conn: &Connection, execution: Option<&WorkExecution>, now: i64) -> Result<DispatchWait> {
    for_execution_naming(conn, execution, now, ReviewNaming::Inline)
}

/// [`for_execution`] with the naming of a review-parked run chosen by the
/// caller.
pub(super) fn for_execution_naming(
    conn: &Connection,
    execution: Option<&WorkExecution>,
    now: i64,
    review: ReviewNaming,
) -> Result<DispatchWait> {
    let Some(execution) = execution else {
        return Ok(DispatchWait::default());
    };
    match execution.status {
        ExecutionStatus::WaitingDependency => for_gated(conn, execution),
        ExecutionStatus::Ready | ExecutionStatus::Queued => for_ready(conn, execution, now),
        ExecutionStatus::WaitingReview if review == ReviewNaming::Deferred => Ok(DispatchWait::default()),
        ExecutionStatus::WaitingReview => review_wait(conn, &execution.work_item_id),
        // The set-based pass that names review cards also says when a merge
        // wait hides a deferred conflict; a single-item read asks here.
        ExecutionStatus::WaitingMerge if review == ReviewNaming::Deferred => {
            Ok(DispatchWait::named(DISPATCH_WAIT_REASON_AWAITING_MERGE))
        }
        ExecutionStatus::WaitingMerge => unscheduled_wait::merge_wait(conn, &execution.work_item_id),
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
        let mut wait = not_before_wait(conn, execution)?;
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

/// Classify the current deadline by its writer.
fn not_before_wait(conn: &Connection, execution: &WorkExecution) -> Result<DispatchWait> {
    let source: Option<String> = conn.query_row(
        "SELECT dispatch_not_before_source FROM work_executions WHERE id = ?1",
        [&execution.id],
        |row| row.get(0),
    )?;
    if source.as_deref() == Some(DISPATCH_WAIT_REASON_FAILURE_BACKOFF) {
        return Ok(DispatchWait::builder()
            .reason(DISPATCH_WAIT_REASON_FAILURE_BACKOFF)
            .label(failure_backoff_wait_label(
                execution.pre_start_failure_count.max(execution.transient_failure_count) + 1,
            ))
            .failures(collect_dispatch_failures(conn, &execution.work_item_id, &execution.id)?)
            // Failure details have no runtime epoch triggers.
            .reads_other_items(true)
            .build());
    }
    Ok(DispatchWait::named(
        if source.as_deref() == Some(DISPATCH_WAIT_REASON_HOST_LEASE_PARK) {
            DISPATCH_WAIT_REASON_HOST_LEASE_PARK
        } else {
            DISPATCH_WAIT_REASON_NOT_BEFORE
        },
    ))
}

fn nonempty_text(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

/// One entry per failed start or failed execution in the current retry sequence.
fn collect_dispatch_failures(
    conn: &Connection,
    work_item_id: &str,
    current_execution_id: &str,
) -> Result<Vec<DispatchFailureAttempt>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.last_error, e.finished_at, e.created_at, r.error_text, r.finished_at
         FROM work_executions e
         LEFT JOIN work_runs r ON r.id = (
             SELECT id FROM work_runs WHERE execution_id = e.id ORDER BY rowid DESC LIMIT 1)
         WHERE e.work_item_id = ?1
           AND e.kind = (SELECT kind FROM work_executions WHERE id = ?2)
           AND e.rowid <= (SELECT rowid FROM work_executions WHERE id = ?2)
           AND e.rowid > COALESCE((SELECT MAX(done.rowid) FROM work_executions done
               WHERE done.work_item_id = ?1 AND done.kind = e.kind
                 AND done.rowid < (SELECT rowid FROM work_executions WHERE id = ?2)
                 AND done.status NOT IN ('failed', 'orphaned') AND done.finished_at IS NOT NULL), 0)
           AND (e.status IN ('failed', 'orphaned') OR e.id = ?2)
         ORDER BY e.rowid",
    )?;
    let rows = stmt
        .query_map(params![work_item_id, current_execution_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut failures = Vec::new();
    for (id, last_error, finished, created, run_error, run_finished) in rows {
        let mut starts = conn.prepare_cached(
            "SELECT error_text, happened_at FROM dispatch_start_failures WHERE execution_id = ?1 ORDER BY rowid",
        )?;
        let recorded = starts
            .query_map([&id], |row| {
                Ok(DispatchFailureAttempt {
                    error_text: row.get(0)?,
                    happened_at: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        failures.extend(recorded);
        if id == current_execution_id {
            continue;
        }
        let attention: Option<(String, String)> = conn
            .query_row(
                "SELECT COALESCE(NULLIF(TRIM(body_markdown), ''), title), created_at
             FROM work_attention_items WHERE execution_id = ?1 AND kind IN ('hung_worker', 'stale_worker')
             ORDER BY rowid DESC LIMIT 1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let error = nonempty_text(run_error)
            .or_else(|| nonempty_text(last_error))
            .or_else(|| {
                attention
                    .as_ref()
                    .and_then(|(body, _)| nonempty_text(Some(body.clone())))
            });
        if let Some(error_text) = error {
            failures.push(DispatchFailureAttempt {
                error_text,
                happened_at: finished
                    .or(run_finished)
                    .or_else(|| attention.map(|(_, time)| time))
                    .unwrap_or(created),
            });
        }
    }
    Ok(failures)
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
/// queued (`todo` with automatic start on), an `active` item with no run
/// pending or live, or an item in review. `latest` is the status of the
/// item's most recent execution. Returns `None` when the item is in none of
/// those states.
///
/// This is [`unscheduled_wait::classify`] for one item; a caller holding many
/// items passes them all to [`annotate_unscheduled_many`] instead, which
/// costs the same statements as one.
pub(super) fn for_unscheduled(
    conn: &Connection,
    task: &Task,
    latest: Option<&ExecutionStatus>,
) -> Result<Option<DispatchWait>> {
    let (mut waits, _) = unscheduled_wait::classify(conn, &[Candidate { task, latest }], None)?;
    Ok(waits.remove(&task.id))
}

/// The wait of a run parked waiting for review, named from its item's row.
fn review_wait(conn: &Connection, id: &str) -> Result<DispatchWait> {
    match query_task(conn, id)? {
        Some(task) => unscheduled_wait::review_wait(conn, &task),
        None => Ok(DispatchWait::named(DISPATCH_WAIT_REASON_UNCLASSIFIED)),
    }
}

/// Write [`for_unscheduled`]'s answer onto `runtime`, for a caller that holds
/// one task row.
pub(super) fn annotate_unscheduled(conn: &Connection, task: &Task, runtime: &mut TaskRuntime) -> Result<()> {
    annotate_unscheduled_many(conn, [(task, runtime)], None).map(|_| ())
}

/// Write [`for_unscheduled`]'s answer onto every card's runtime with a number
/// of statements that does not depend on how many cards there are. Applied
/// after the execution-keyed projection and after any board runtime cache
/// hit, because the answer comes from the task row, which that cache does not
/// version.
///
/// Inputs include the task and parent chain, executions, project/product,
/// recovery attention, deliberate parks, pause metadata and review batches.
/// The board journal invalidates these inputs, including cycle-root holds;
/// this annotation is recomputed after every execution-cache hit.
///
/// `counter`, when given, attributes statements to the returned branch costs.
pub(super) fn annotate_unscheduled_many<'a>(
    conn: &Connection,
    cards: impl IntoIterator<Item = (&'a Task, &'a mut TaskRuntime)>,
    counter: Option<&super::db_diagnostics::StatementCounter<'_>>,
) -> Result<ClassifyCost> {
    let mut cards: Vec<(&Task, &mut TaskRuntime)> = cards.into_iter().collect();
    let (mut waits, cost) = {
        let candidates: Vec<Candidate<'_>> = cards
            .iter()
            .map(|(task, runtime)| Candidate {
                task,
                latest: runtime.execution_status.as_ref(),
            })
            .collect();
        unscheduled_wait::classify(conn, &candidates, counter)?
    };
    for (task, runtime) in &mut cards {
        if let Some(wait) = waits.remove(&task.id) {
            runtime.dispatch_wait_reason = wait.reason;
            runtime.dispatch_wait_label = wait.label;
        }
    }
    Ok(cost)
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

/// What one [`WorkDb::list_unexplained_dispatch_waits`] pass read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UnexplainedWaitCost {
    /// Pending executions projected one by one.
    pub pending_executions: usize,
    /// Queued, idle `active` and in-review items listed for classification.
    pub unscheduled_items: usize,
    /// The set-based classification of those items, by branch.
    pub classified: ClassifyCost,
    /// Every statement the pass ran.
    pub statements: u64,
    pub millis: f64,
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
    ///
    /// The read holds the engine's one database connection, so what it costs
    /// is what every other writer waits behind on each beat. Items with
    /// nothing pending are classified as sets: the statements this runs do
    /// not grow with the number of queued, idle or in-review items, and only
    /// a pending execution still costs its own projection. Each pass logs
    /// its cost at `debug`.
    pub fn list_unexplained_dispatch_waits(&self) -> Result<Vec<UnexplainedWait>> {
        let (waits, cost) = self.list_unexplained_dispatch_waits_measured()?;
        tracing::debug!(
            duration_ms = cost.millis,
            statements = cost.statements,
            pending_executions = cost.pending_executions,
            unscheduled_items = cost.unscheduled_items,
            classify_statements = cost.classified.statements(),
            queued = cost.classified.queued.cards,
            idle_active = cost.classified.idle_active.cards,
            review = cost.classified.review.cards,
            unexplained = waits.len(),
            "unexplained dispatch waits: pass complete",
        );
        Ok(waits)
    }

    /// [`Self::list_unexplained_dispatch_waits`] with what the pass cost.
    pub(crate) fn list_unexplained_dispatch_waits_measured(
        &self,
    ) -> Result<(Vec<UnexplainedWait>, UnexplainedWaitCost)> {
        let conn = self.connect()?;
        let started = std::time::Instant::now();
        let counter = super::db_diagnostics::StatementCounter::install(&conn);
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let mut waits = Vec::new();

        let pending: Vec<WorkExecution> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM work_executions we
                 WHERE we.status IN ('queued', 'ready', 'waiting_dependency', 'waiting_review', 'waiting_merge')
                 ORDER BY we.created_at ASC, we.id ASC",
                super::executions_runs::EXECUTION_COLUMNS
            ))?;
            let rows = stmt.query_map([], map_execution)?;
            collect_rows(rows)?
        };
        // Runs parked for review are named from their items' rows, together.
        let mut parked: Vec<&str> = pending
            .iter()
            .filter(|execution| execution.status == ExecutionStatus::WaitingReview)
            .map(|execution| execution.work_item_id.as_str())
            .collect();
        parked.sort_unstable();
        parked.dedup();
        let parked_tasks = query_tasks(&conn, &parked)?;
        let parked_waits = unscheduled_wait::review_waits(&conn, &parked_tasks.iter().collect::<Vec<_>>())?;
        let pending_count = pending.len();
        for execution in pending {
            let reason = if execution.status == ExecutionStatus::WaitingReview {
                // A run whose item no longer exists has nothing to name it.
                parked_waits
                    .get(&execution.work_item_id)
                    .is_none_or(DispatchWait::is_unclassified)
                    .then(|| DISPATCH_WAIT_REASON_UNCLASSIFIED.to_owned())
            } else {
                let wait = for_execution(&conn, Some(&execution), now)?;
                let awaiting = wait.reason.as_deref() == Some(DISPATCH_WAIT_REASON_AWAITING_DISPATCH);
                (wait.is_unclassified() || awaiting).then(|| wait.reason.unwrap_or_default())
            };
            let Some(reason) = reason else {
                continue;
            };
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
                reason,
                task_status: task.map(|task| task.status.as_str().to_owned()),
                execution: Some(execution),
            });
        }

        // Every item with nothing pending, with its row and the status of its
        // latest run, in one read: queued cards first, then `active` and
        // in-review items, each by id. Queued rows whose own columns already
        // name the hold (deferred, human-driven, parked) are skipped in SQL:
        // this runs on every heartbeat and a product can carry many of them.
        // Most idle `active` items are `awaiting_redispatch`, because the
        // orphan sweep starts a replacement. The classifier decides them all
        // together; any it cannot name is reported.
        let unscheduled: Vec<(Task, Option<ExecutionStatus>)> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT {columns},
                        (SELECT e.status FROM work_executions e
                         WHERE e.work_item_id = tasks.id AND e.kind <> 'progression'
                         ORDER BY e.created_at DESC, e.id DESC
                         LIMIT 1)
                 FROM tasks
                 WHERE deleted_at IS NULL
                   AND ((status = 'todo' AND autostart = 1
                         AND COALESCE(deferred, 0) = 0 AND COALESCE(human_driven, 0) = 0
                         AND parked_at IS NULL)
                        OR status IN ('active', 'in_review'))
                   AND NOT EXISTS (
                       SELECT 1 FROM work_executions we
                       WHERE we.work_item_id = tasks.id
                         AND we.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned'))
                 ORDER BY status = 'todo' DESC, id ASC",
                columns = super::query_ensure::TASK_COLUMNS
            ))?;
            let rows = stmt.query_map([], |row| {
                let task = map_task_with_parent_provenance_and_archived_reason(row)?;
                let column = row.as_ref().column_count() - 1;
                let latest: Option<String> = row.get(column)?;
                let latest = latest
                    .map(|raw| parse_text_column::<ExecutionStatus>(column, &raw))
                    .transpose()?;
                Ok((task, latest))
            })?;
            collect_rows(rows)?
        };
        let candidates: Vec<Candidate<'_>> = unscheduled
            .iter()
            .map(|(task, latest)| Candidate {
                task,
                latest: latest.as_ref(),
            })
            .collect();
        let (classified, cost) = unscheduled_wait::classify(&conn, &candidates, Some(&counter))?;
        for (task, _) in &unscheduled {
            if classified.get(&task.id).is_some_and(DispatchWait::is_unclassified) {
                waits.push(UnexplainedWait {
                    work_item_id: task.id.clone(),
                    execution: None,
                    reason: DISPATCH_WAIT_REASON_UNCLASSIFIED.to_owned(),
                    task_status: Some(task.status.as_str().to_owned()),
                });
            }
        }
        let cost = UnexplainedWaitCost {
            pending_executions: pending_count,
            unscheduled_items: unscheduled.len(),
            classified: cost,
            statements: counter.total(),
            millis: started.elapsed().as_secs_f64() * 1000.0,
        };
        Ok((waits, cost))
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
