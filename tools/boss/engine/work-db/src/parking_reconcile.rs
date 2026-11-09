//! The database half of the un-park reconciliation sequence.
//!
//! Section 1.5 of
//! `tools/boss/docs/designs/task-persistence-parking-and-autonomous-progression.md`
//! splits un-park into steps that are only database writes (1 and 2, run inside
//! the un-park transaction) and steps that call GitHub or the dispatcher (3 to
//! 7, a background job in `engine/core/src/unpark_reconcile.rs`). This module
//! owns the first half and the narrow seams the second half reads and writes:
//!
//! - **Step 2, clear stale halt stamps.** A `churn_guard` or dispatch-failure
//!   stamp (`dispatch_failed_reason` / `_error` / `_at`) that predates the park
//!   says nothing about now, so un-parking clears it in the same transaction
//!   that clears the park. Holds are deliberately left alone, because they
//!   record a decision a human has not yet made, or evidence a progression
//!   agent has not yet read: `blocked: awaiting_operator_answer` (a question is
//!   already in front of the human), `blocked: worker_failed` and a
//!   `deliberate_park` stamp. A `worker_failed` row the owner refused (ledger
//!   basis `operator_no`) is a recorded decision, a [`KeptHold::OperatorRefusal`];
//!   every other producer's `worker_failed` is [`KeptHold::FailureEvidence`],
//!   retained until progression clears it. Step 5 never redispatches either
//!   (`task_accepts_execution` refuses both blocked reasons), and step 7 flags
//!   them for progression. A stamp written *after* the park (an in-flight run
//!   that finished failing while the row was parked) is newer evidence and is
//!   kept too.
//! - **Step 5, per-item execution reconcile.** [`WorkDb::reconcile_item_execution`]
//!   runs exactly the code the board's ordinary invalidation reconcile runs, for
//!   one item (or the one project chain it belongs to). A park cancels the
//!   item's never-started executions, and the ordinary reconcile never mints
//!   over a cancelled latest execution (an explicit cancel is a decision), so
//!   [`cancelled_by_park`] tells the reconcile which cancelled row was the
//!   park's doing, recorded by execution id in the parking transaction, and is as good as absent
//!   once the item is un-parked, and is minted afresh "exactly as for a fresh
//!   row". An `active` row is left to the orphan sweep, which has the churn
//!   guard.
//! - **Step 7, the progression grace stamp.** `tasks.progression_grace_waived_at`
//!   marks an item whose stuck-age grace period the progression sweep must skip
//!   once. Nothing reads this stamp yet: it is meant for the progression
//!   sweep described in Part 2 of the design, and is inert until that sweep
//!   consumes it.
//! - **The churn floor.** [`WorkDb::last_unparked_at`] is the time of the item's
//!   last un-park, read from the task status ledger. The orphan sweep's churn
//!   guard counts only executions created at or after it, so evidence from
//!   before the owner decided to try again does not trip the guard on the first
//!   re-dispatch.

use super::*;
use crate::executions_runs::reconcile_tasks;
use boss_protocol::TaskKind;

pub(super) const MIGRATION_ID: &str = "20261107_1730_progression_grace_waived_at";

/// Ledger basis for the write that clears a stale halt stamp at un-park.
pub const BASIS_UNPARK_CLEARED_HALT_STAMP: &str = "unpark_cleared_halt_stamp";

/// What un-park did about the row's `dispatch_failed_*` halt stamp (step 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaltStamp {
    /// The row carried no halt stamp.
    Absent,
    /// A stamp from before the park was cleared.
    Cleared { reason: String },
    /// The stamp was kept: it is a `deliberate_park` hold, or it was written
    /// after the park.
    Kept { reason: String, why: &'static str },
}

/// A blocked state step 2 preserved across the un-park, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeptHold {
    /// `blocked: awaiting_operator_answer`: a question is already in front of
    /// the human, so nothing about the un-park answers it.
    OperatorQuestion,
    /// `blocked: worker_failed` whose ledger basis is `operator_no`: the owner
    /// refused the work, a recorded decision that stays a human hold.
    OperatorRefusal,
    /// `blocked: worker_failed` from any other producer (a declared blocker, the
    /// nudge breaker, a PR-binding guard, delivery without a PR). Not a human
    /// decision: the failure evidence is retained until progression clears it.
    FailureEvidence { basis: Option<String> },
}

impl KeptHold {
    /// The `blocked_reason` the hold is recorded under.
    pub fn blocked_reason(&self) -> &'static str {
        match self {
            KeptHold::OperatorQuestion => "awaiting_operator_answer",
            KeptHold::OperatorRefusal | KeptHold::FailureEvidence { .. } => "worker_failed",
        }
    }

    /// True when only a human can resolve the hold.
    pub fn is_human_hold(&self) -> bool {
        !matches!(self, KeptHold::FailureEvidence { .. })
    }
}

/// One row the un-park released, with what step 2 did to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnparkedRow {
    pub id: String,
    pub product_id: String,
    /// Set for a revision: the chain root it follows.
    pub parent_task_id: Option<String>,
    pub halt_stamp: HaltStamp,
    /// The blocked state step 2 preserved (`awaiting_operator_answer` or
    /// `worker_failed`), if the row has one.
    pub kept_hold: Option<KeptHold>,
}

impl UnparkedRow {
    pub fn is_revision(&self) -> bool {
        self.parent_task_id.is_some()
    }
}

/// Every row an un-park released. Empty for a park, and for an un-park of an
/// item that was not parked (the call is idempotent).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnparkReport {
    /// Set when the un-park was of a project.
    pub project_id: Option<String>,
    /// Released rows, parents before their revisions.
    pub rows: Vec<UnparkedRow>,
}

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    if !crate::schema_init::table_has_column(conn, "tasks", "progression_grace_waived_at")? {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN progression_grace_waived_at TEXT")?;
    }
    Ok(())
}

/// Step 2 for one row about to be un-parked: clear its stale halt stamp and
/// describe the outcome. Reads `task` as it was *before* the park columns are
/// cleared, because the stamp is judged against `task.parked_at`.
pub(crate) fn release_row(tx: &Connection, task: &Task, actor: &str) -> Result<UnparkedRow> {
    let halt_stamp = match task.dispatch_failed_reason.as_deref() {
        None => HaltStamp::Absent,
        Some(reason) if reason == DELIBERATE_PARK_DISPATCH_FAILED_REASON => HaltStamp::Kept {
            reason: reason.to_owned(),
            why: "a worker-declared blocker awaiting a decision",
        },
        Some(reason) if stamped_after_park(task) => HaltStamp::Kept {
            reason: reason.to_owned(),
            why: "written after the park, so it is current evidence",
        },
        Some(reason) => {
            crate::task_status_audit::execute(
                tx,
                BASIS_UNPARK_CLEARED_HALT_STAMP,
                actor,
                "UPDATE tasks
                 SET dispatch_failed_reason = NULL, dispatch_failed_error = NULL, dispatch_failed_at = NULL
                 WHERE id = ?1 AND dispatch_failed_reason = ?2",
                params![task.id, reason],
            )?;
            HaltStamp::Cleared {
                reason: reason.to_owned(),
            }
        }
    };
    let kept_hold = match task.blocked_reason.as_deref() {
        Some("awaiting_operator_answer") => Some(KeptHold::OperatorQuestion),
        Some("worker_failed") => Some(
            match crate::stuck_report::latest_basis(tx, &task.id, "blocked_reason")? {
                Some(basis) if basis == WorkerFailureBasis::OperatorNo.as_str() => KeptHold::OperatorRefusal,
                basis => KeptHold::FailureEvidence { basis },
            },
        ),
        _ => None,
    };
    Ok(UnparkedRow {
        id: task.id.clone(),
        product_id: task.product_id.clone(),
        parent_task_id: (task.kind == TaskKind::Revision)
            .then(|| task.parent_task_id.clone())
            .flatten(),
        halt_stamp,
        kept_hold,
    })
}

/// True only for never-started executions cancelled in a parking transaction.
/// Explicit cancellations remain decisions about the work, regardless of time.
pub(crate) fn cancelled_by_park(conn: &Connection, execution: &WorkExecution) -> Result<bool> {
    if execution.status != ExecutionStatus::Cancelled || execution.started_at.is_some() {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM parking_cancelled_executions WHERE execution_id = ?1)
         AND EXISTS(SELECT 1 FROM tasks WHERE id = ?2 AND deleted_at IS NULL
                    AND status IN ('todo', 'blocked') AND parked_at IS NULL)",
        params![execution.id, execution.work_item_id],
        |row| row.get(0),
    )?)
}

pub(super) fn migrate_cancel_provenance(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS parking_cancelled_executions (
            execution_id TEXT PRIMARY KEY REFERENCES work_executions(id) ON DELETE CASCADE
        )",
    )?;
    Ok(())
}

/// True when the row's halt stamp is strictly newer than its park, i.e. it was
/// written while the row was parked. A stamp with no readable time, or a park
/// with none, is treated as stale: there is no evidence it is current.
fn stamped_after_park(task: &Task) -> bool {
    let stamped = task.dispatch_failed_at.as_deref().and_then(|at| at.parse::<i64>().ok());
    let parked = task.parked_at.as_deref().and_then(|at| at.parse::<i64>().ok());
    matches!((stamped, parked), (Some(stamped), Some(parked)) if stamped > parked)
}

/// The open revisions an un-park of `parent_id` releases with it: those that
/// inherited the parent's own (direct) stamp.
pub(crate) fn directly_parked_revisions(tx: &Connection, parent_id: &str) -> Result<Vec<Task>> {
    let ids: Vec<String> = tx
        .prepare(
            "SELECT id FROM tasks
             WHERE parent_task_id = ?1 AND kind = 'revision' AND parked_via = ?2
               AND deleted_at IS NULL AND status NOT IN ('done', 'archived')
             ORDER BY created_at, id",
        )?
        .query_map(params![parent_id, boss_protocol::PARKED_VIA_DIRECT], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.iter()
        .map(|id| query_task(tx, id).require("task", id))
        .collect::<Result<_>>()
}

/// Every task a project's park cascaded onto, in creation order.
pub(crate) fn project_cascaded_tasks(tx: &Connection, via: &str) -> Result<Vec<Task>> {
    let ids: Vec<String> = tx
        .prepare("SELECT id FROM tasks WHERE parked_via = ?1 AND deleted_at IS NULL ORDER BY created_at, id")?
        .query_map([via], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.iter()
        .map(|id| query_task(tx, id).require("task", id))
        .collect::<Result<_>>()
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Re-run the execution reconcile for one item, as the board's ordinary
    /// invalidation would: a `ready` or `waiting_dependency` row is minted (or a
    /// stale one promoted) exactly as for a fresh row, and the guards that refuse
    /// an automatic mint (parked, deferred, human-driven, a worker-failed or
    /// operator-question hold, an unmet autostart) still apply. A project task
    /// or design task reconciles its whole project chain, because "first
    /// incomplete is `ready`" is a property of the chain.
    pub fn reconcile_item_execution(&self, work_item_id: &str) -> Result<ExecutionReconcileResult> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let task = query_task(&tx, work_item_id).require("task", work_item_id)?;
        let tasks = match (&task.kind, task.project_id.as_deref()) {
            (TaskKind::ProjectTask | TaskKind::Design, Some(project_id)) => {
                crate::query_ensure::list_tasks_for_product(&tx, &task.product_id)?
                    .into_iter()
                    .filter(|candidate| candidate.project_id.as_deref() == Some(project_id))
                    .collect()
            }
            _ => vec![task],
        };
        let mut result = ExecutionReconcileResult::default();
        let mut pending = PendingEvents::new();
        reconcile_tasks(&tx, &mut pending, &mut result, tasks)?;
        commit_and_publish(tx, pending, self.event_bus())?;
        Ok(result)
    }

    /// Step 7: mark `work_item_id` so the progression sweep skips its stuck-age
    /// grace period once. Idempotent (a second call refreshes the time). Returns
    /// whether an open, live row was stamped. Touches nothing else, `updated_at`
    /// included: it is bookkeeping for a consumer, not a state change.
    pub fn stamp_progression_grace_waived(&self, work_item_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE tasks SET progression_grace_waived_at = ?2
             WHERE id = ?1 AND deleted_at IS NULL AND parked_at IS NULL AND status NOT IN ('done', 'archived')",
            params![work_item_id, now_string()],
        )?;
        Ok(updated > 0)
    }

    /// When the grace waiver was last stamped on `work_item_id` (epoch seconds),
    /// if it has been and has not been consumed.
    pub fn progression_grace_waived_at(&self, work_item_id: &str) -> Result<Option<i64>> {
        let conn = self.connect()?;
        let stamp: Option<String> = conn
            .query_row(
                "SELECT progression_grace_waived_at FROM tasks WHERE id = ?1",
                [work_item_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(stamp.and_then(|at| at.parse().ok()))
    }

    /// When `work_item_id` was last un-parked (epoch seconds), read from the task
    /// status ledger; `None` if it never was. Both a direct un-park and a
    /// project's cascade write the row this reads. The orphan sweep's churn guard
    /// uses it as a floor, so executions that ended before the owner decided to
    /// try again are not evidence against the first attempt after it.
    pub fn last_unparked_at(&self, work_item_id: &str) -> Result<Option<i64>> {
        let conn = self.connect()?;
        match conn.query_row(
            "SELECT MAX(CAST(changed_at AS INTEGER)) FROM task_status_audit
             WHERE work_item_id = ?1 AND property = 'parked_at' AND new_value IS NULL",
            [work_item_id],
            |row| row.get::<_, Option<i64>>(0),
        ) {
            Ok(at) => Ok(at),
            Err(err) if crate::parking::sqlite_missing_table(&err, "task_status_audit") => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// The open revisions of chain root `root_id` (oldest first): the rows an
    /// un-park of the root re-enters into per-PR chain serialization.
    pub fn open_revision_ids(&self, root_id: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        crate::parking::open_revision_ids(&conn, root_id)
    }
}
