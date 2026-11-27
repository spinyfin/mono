//! Engine-side execution and creation-time validation of structured choice dispositions.
use super::*;
use crate::prerequisite_question::RestartOptions;
use boss_protocol::{ChoiceDisposition, OperatorAnswerType, OperatorQuestion, OperatorQuestionView};

/// Validate a decision card at creation time so a stored card can never offer
/// an option whose disposition is missing or cannot be executed by the engine.
pub(crate) fn validate_question_for_creation(
    asking: Option<&Task>,
    question: &OperatorQuestion,
) -> std::result::Result<(), String> {
    question.answer_type.validate_for_creation()?;
    let Some(task) = asking else {
        return Err("an operator question can only be filed for a task, chore, revision, or followup".into());
    };
    if let OperatorAnswerType::MultipleChoice { options, .. } = &question.answer_type
        && options
            .iter()
            .any(|opt| matches!(opt.disposition, ChoiceDisposition::Archive | ChoiceDisposition::Park))
        && (task.deleted_at.is_some() || task.status.is_terminal())
    {
        let state = if task.deleted_at.is_some() {
            "deleted".to_owned()
        } else {
            task.status.to_string()
        };
        return Err(format!(
            "cannot offer archive or park options on {} while it is {state}",
            task.id
        ));
    }
    Ok(())
}

/// Resolve the owner-executable disposition of `choice` from the card's
/// structured option metadata — never from button label text. Non-owner
/// answerers always return `None` so worker/coordinator answers resume instead
/// of archiving or parking.
pub(crate) fn owner_choice_disposition(
    actor: &str,
    question: &OperatorQuestionView,
    choice: &str,
) -> Option<ChoiceDisposition> {
    if actor != LAST_STATUS_ACTOR_HUMAN {
        return None;
    }
    question.answer_type.choice_disposition(choice)
}

/// Park the item as the owner and settle the answered question.
///
/// The answer is first carried exactly as an ordinary choice would carry
/// it (`restart_after_answer_from` with the explicit start suppressed): the
/// brief gains the recorded answer, autostart is restored, an item still
/// gated by prerequisites settles into the normal dependency block, and
/// the continuation over the answered execution is queued. The park then
/// cancels that never-started continuation as a park cancellation, so
/// nothing launches while the hold is in force and un-parking re-mints
/// exactly one.
pub(crate) fn park_answer_in_tx(
    tx: &rusqlite::Transaction<'_>,
    pending: &mut PendingEvents,
    task: &Task,
    question: &OperatorQuestionView,
    block: &str,
    now: &str,
    attribution: crate::task_status_audit::WriteAttribution<'_>,
) -> Result<Vec<String>> {
    anyhow::ensure!(
        attribution.actor == LAST_STATUS_ACTOR_HUMAN,
        "only the owner can choose to park an item"
    );
    crate::pr_flow::restart_after_answer_from(
        pending,
        tx,
        task,
        question,
        block,
        RestartOptions::new(now, true),
        crate::task_status_audit::WriteAttribution {
            actor: attribution.actor,
            provenance: Some(attribution.provenance.unwrap_or("operator_question_park")),
        },
    )?;
    // Revisions share their parent's parking boundary, as in the ladder.
    let root = get_chain_root_task(tx, &task.id)?.unwrap_or_else(|| task.clone());
    crate::parking::set_task_parked(
        tx,
        pending,
        &root.id,
        true,
        Some("the owner chose to park this item in an operator question"),
        attribution.actor,
        now,
    )?;
    let mut touched = vec![root.id];
    if !touched.contains(&task.id) {
        touched.push(task.id.clone());
    }
    Ok(touched)
}
