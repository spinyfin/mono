//! Carry answered decisions to progression or restart an ordinary worker.

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
/// minted, and the resume id when the asking run was force-launched.
pub(crate) struct RestartAfterAnswer {
    pub minted: bool,
    pub force_resume_execution_id: Option<String>,
}

impl RestartAfterAnswer {
    pub(crate) fn none() -> Self {
        Self {
            minted: false,
            force_resume_execution_id: None,
        }
    }
}

/// Move the answered task back to Todo with `block` appended to its brief and
/// carry the answer to the next worker. Returns whether a ready execution was
/// minted for it to start now.
/// Progression questions instead restore the stuck state for the next progression run.
///
/// When the asking run was admitted by an operator force launch (see
/// `work_executions.force_launched`), `force_resume_execution_id` is the
/// minted resume so the caller can bypass an operator-originated pause.
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
    // Progression consumes its own answers; ordinary dispatch must not replace it.
    let asked_by_progression =
        query_execution(tx, &question.execution_id)?.is_some_and(|asking| asking.kind == ExecutionKind::Progression);
    if asked_by_progression {
        let has_pr = crate::dispatch_admission::primary_pr_awaits_review(tx, &task.id)?;
        let restore = |property: &str| -> Result<Option<String>> {
            Ok(tx
                .query_row(
                    "SELECT old_value FROM task_status_audit WHERE work_item_id = ?1
                 AND property = ?2 AND operation_id = (
                     SELECT operation_id FROM task_status_audit
                     WHERE work_item_id = ?1 AND property = 'blocked_reason'
                       AND new_value = 'awaiting_operator_answer' AND basis = 'progression_question'
                     ORDER BY id DESC LIMIT 1) ORDER BY id DESC LIMIT 1",
                    params![task.id, property],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten())
        };
        let status = if has_pr {
            "in_review".to_owned()
        } else {
            restore("status")?.unwrap_or_else(|| task.status.as_str().into())
        };
        let reason = if has_pr { None } else { restore("blocked_reason")? };
        let autostart = restore("autostart")?.unwrap_or_else(|| "0".into());
        crate::task_status_audit::execute(
            tx,
            &basis,
            actor,
            "UPDATE tasks SET description = ?2, status = ?5, blocked_reason = ?6,
             blocked_detail = NULL, dispatch_failed_reason = ?7, autostart = ?8,
             last_status_actor = ?4, updated_at = ?3 WHERE id = ?1
             AND deleted_at IS NULL AND status NOT IN ('done', 'archived')",
            params![
                task.id,
                description,
                now,
                actor,
                status,
                reason,
                restore("dispatch_failed_reason")?,
                autostart
            ],
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
        return Ok(RestartAfterAnswer::none());
    }
    super::super::prerequisite_question::continue_after_answer(pending, tx, task, question, suppress_start, attribution)
}
