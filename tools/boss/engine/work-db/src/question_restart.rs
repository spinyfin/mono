//! Restart the answered task and mint a resume execution.

use super::*;

/// Test shorthand for [`restart_after_answer_from`] answered by a human with no
/// socket provenance.
#[cfg(test)]
pub(crate) fn restart_after_answer(
    pending: &mut PendingEvents,
    tx: &rusqlite::Transaction<'_>,
    task: &Task,
    question: &OperatorQuestionView,
    block: &str,
    now: &str,
) -> Result<bool> {
    Ok(restart_after_answer_from(
        pending,
        tx,
        task,
        question,
        block,
        RestartOptions::new(now, false),
        crate::task_status_audit::WriteAttribution {
            actor: boss_protocol::LAST_STATUS_ACTOR_HUMAN,
            provenance: None,
        },
    )?
    .minted)
}

/// Result of [`restart_after_answer_from`]: whether a ready execution was
/// minted, and the work item to launch through the pause when the asking
/// run was force-launched.
pub(crate) struct RestartAfterAnswer {
    pub minted: bool,
    pub dispatch_work_item_id: Option<String>,
}

impl RestartAfterAnswer {
    pub(crate) fn none() -> Self {
        Self {
            minted: false,
            dispatch_work_item_id: None,
        }
    }
}

/// Move the answered task back to Todo with `block` appended to its brief and
/// carry the answer to the next worker. Returns whether a ready execution was
/// minted for it to start now.
///
/// When the asking run was admitted by an operator force launch (see
/// `work_executions.force_launched`), `dispatch_work_item_id` is the answered
/// task so the caller can launch it through `ExecutionCoordinator::launch_answered_work`.
///
/// A task still gated by prerequisites (a conflict revision queued behind
/// another writer on the same PR is the common case) cannot be started
/// explicitly: that request is refused, which would roll the whole answer back
/// and leave the question open. Such a task is parked as `blocked` /
/// `dependency` with a waiting execution, matching revision insert and
/// prerequisite approval, so the dependency cascade promotes it on the
/// event path when the gate clears.
///
/// `attribution` supplies the ledger actor/basis and `last_status_actor`.
pub(crate) fn restart_after_answer_from(
    pending: &mut PendingEvents,
    tx: &rusqlite::Transaction<'_>,
    task: &Task,
    question: &OperatorQuestionView,
    block: &str,
    options: RestartOptions<'_>,
    attribution: crate::task_status_audit::WriteAttribution<'_>,
) -> Result<RestartAfterAnswer> {
    let RestartOptions { now, suppress_start } = options;
    let description = format!("{}{block}", task.description);
    super::super::description_guard::validate_description_update(&task.description, &description, false)?;
    let actor = attribution.actor;
    let basis = attribution.basis("restart_after_answer");
    // An item with a bound PR that a progression run asked about goes back to
    // review with the answer on its brief. A new implementation of it would be
    // refused ("use a revision"), which would roll the whole answer back and leave
    // the question open for good.
    let asked_by_progression =
        query_execution(tx, &question.execution_id)?.is_some_and(|asking| asking.kind == ExecutionKind::Progression);
    if asked_by_progression && crate::dispatch_admission::primary_pr_awaits_review(tx, &task.id)? {
        crate::task_status_audit::execute(
            tx,
            &basis,
            actor,
            "UPDATE tasks SET description = ?2, status = 'in_review', blocked_reason = NULL,
             blocked_detail = NULL, autostart = 1, last_status_actor = ?4, updated_at = ?3 WHERE id = ?1",
            params![task.id, description, now, actor],
        )?;
        return Ok(RestartAfterAnswer::none());
    }
    crate::task_status_audit::execute(
        tx,
        &basis,
        actor,
        "UPDATE tasks SET description = ?2, status = 'todo', blocked_reason = NULL,
         blocked_detail = NULL, autostart = 1, last_status_actor = ?4, updated_at = ?3 WHERE id = ?1",
        params![task.id, description, now, actor],
    )?;
    if !parking::upkeep_eligible(tx, &task.id)? {
        return Ok(RestartAfterAnswer::none());
    }
    if !deps::gating_prereqs_for(tx, &task.id)?.is_empty() {
        maybe_engine_block_dependent(pending, tx, &task.id, now)?;
        let task = query_task(tx, &task.id).require("task", &task.id)?;
        if task.kind == TaskKind::Revision {
            reconcile_revision_execution(pending, tx, &mut ExecutionReconcileResult::default(), &task)?;
        } else {
            super::super::prerequisite_question::queue_gated_execution(tx, &task, question)?;
        }
        super::super::prerequisite_question::stamp_answered_waiting(tx, &task.id)?;
        let dispatch_work_item_id = super::super::prerequisite_question::inherit_force_launch_continuation(
            tx,
            &task.id,
            &question.execution_id,
            suppress_start,
        )?;
        return Ok(RestartAfterAnswer {
            minted: false,
            dispatch_work_item_id,
        });
    }
    super::super::prerequisite_question::continue_after_answer(pending, tx, task, question, suppress_start, attribution)
}
