//! Engine-side meanings of the known, item-local disposition choices.
use super::*;
use crate::prerequisite_question::RestartOptions;
use boss_protocol::OperatorQuestionView;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Archive,
    Park,
}

/// Deliberately match complete labels that name the item being disposed of,
/// never substrings, arbitrary prose, or a bare verb: "Archive the old logs",
/// "Archive" and "Park" on a question about something other than the item
/// remain instructions for the worker.
pub(crate) fn known_choice(choice: &str) -> Option<Disposition> {
    let lower = choice.trim().to_ascii_lowercase();
    for noun in boss_protocol::ITEM_DISPOSITION_NOUNS {
        if lower == format!("archive this {noun}") {
            return Some(Disposition::Archive);
        }
        if lower == format!("park this {noun}") {
            return Some(Disposition::Park);
        }
    }
    None
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
