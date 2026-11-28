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
    let offers_item_action = matches!(
        &question.answer_type,
        OperatorAnswerType::MultipleChoice { options, .. }
            if options
                .iter()
                .any(|opt| matches!(opt.disposition, ChoiceDisposition::Archive | ChoiceDisposition::Park))
    );
    if !offers_item_action {
        return Ok(());
    }
    // A run whose work item is not a task (a project or product design run)
    // has no item to archive or park; every other kind of question is fine.
    let Some(task) = asking else {
        return Err("archive and park options can only be offered on a task, chore, revision, or followup".into());
    };
    if task.deleted_at.is_some() || task.status.is_terminal() {
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
    park_chain_root_in_tx(tx, pending, task, attribution.actor, now)
}

/// Park the item's chain root as the owner and return every work item the
/// park touched. Revisions share their parent's parking boundary, as in the
/// ladder. Shared by the answer handler and the attention path so the two
/// cannot drift in who may park or in the recorded reason.
pub(crate) fn park_chain_root_in_tx(
    tx: &rusqlite::Transaction<'_>,
    pending: &mut PendingEvents,
    task: &Task,
    actor: &str,
    now: &str,
) -> Result<Vec<String>> {
    anyhow::ensure!(
        actor == LAST_STATUS_ACTOR_HUMAN,
        "only the owner can choose to park an item"
    );
    let root = get_chain_root_task(tx, &task.id)?.unwrap_or_else(|| task.clone());
    crate::parking::set_task_parked(
        tx,
        pending,
        &root.id,
        true,
        Some("the owner chose to park this item in an operator question"),
        actor,
        now,
    )?;
    let mut touched = vec![root.id];
    if !touched.contains(&task.id) {
        touched.push(task.id.clone());
    }
    Ok(touched)
}

pub(super) const MIGRATION_ID: &str = "20261128_0230_operator_question_dispositions";

/// Give every open multiple-choice card stored before dispositions existed
/// structured `options`, so answering it executes the action its button names.
///
/// This is the one place label wording is interpreted: a one-time repair of
/// rows written when the engine matched labels. Labels that carried an engine
/// effect before (archive/park an item, the progression ladder's three
/// buttons, the merge-conflict mismatch's two buttons) map to that
/// effect's disposition; every other label becomes `resume`. Rows that already
/// carry `options`, and answered or withdrawn rows, are left untouched.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, question_json,
                EXISTS(SELECT 1 FROM progression_ladder l WHERE l.question_id = operator_questions.id),
                conflict_resolution_id IS NOT NULL
         FROM operator_questions WHERE status = 'open'",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (id, json, is_ladder, is_mismatch) in rows {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&json) else {
            continue;
        };
        let Some(answer_type) = value.get_mut("answer_type").and_then(|v| v.as_object_mut()) else {
            continue;
        };
        if answer_type.get("kind").and_then(|v| v.as_str()) != Some("multiple_choice")
            || answer_type
                .get("options")
                .and_then(|v| v.as_array())
                .is_some_and(|options| !options.is_empty())
        {
            continue;
        }
        let Some(choices) = answer_type.get("choices").and_then(|v| v.as_array()) else {
            continue;
        };
        let options: Vec<serde_json::Value> = choices
            .iter()
            .filter_map(|choice| choice.as_str())
            .map(|label| {
                serde_json::json!({
                    "disposition": legacy_label_disposition(label, is_ladder, is_mismatch).as_str(),
                    "label": label,
                })
            })
            .collect();
        if options.len() != choices.len() {
            continue;
        }
        answer_type.insert("options".to_owned(), serde_json::Value::Array(options));
        conn.execute(
            "UPDATE operator_questions SET question_json = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(&value)?],
        )?;
    }
    Ok(())
}

/// The effect a legacy label had, for [`migrate`] only.
fn legacy_label_disposition(label: &str, is_ladder: bool, is_mismatch: bool) -> ChoiceDisposition {
    if is_ladder {
        match label {
            crate::progression_ladder::PARK_IT => return ChoiceDisposition::Park,
            crate::progression_ladder::ARCHIVE_IT => return ChoiceDisposition::Archive,
            _ => return ChoiceDisposition::Resume,
        }
    }
    if is_mismatch {
        match label {
            boss_protocol::CONFLICT_MISMATCH_CHOICE_REWRITE => return ChoiceDisposition::RewriteOntoBase,
            boss_protocol::CONFLICT_MISMATCH_CHOICE_FORCE_MERGE => return ChoiceDisposition::ForceMerge,
            _ => return ChoiceDisposition::Resume,
        }
    }
    match label.trim().to_ascii_lowercase().as_str() {
        "archive this item"
        | "archive this chore"
        | "archive this task"
        | "archive this revision"
        | "archive this followup" => ChoiceDisposition::Archive,
        "park this item" | "park this chore" | "park this task" | "park this revision" | "park this followup" => {
            ChoiceDisposition::Park
        }
        _ => ChoiceDisposition::Resume,
    }
}
