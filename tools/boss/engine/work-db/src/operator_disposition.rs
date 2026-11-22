//! Engine-side meanings of the known, item-local disposition choices.
use super::*;

pub(crate) enum Disposition {
    Archive,
    Park,
}

/// Deliberately match complete labels, never substrings or arbitrary prose:
/// "Archive the old logs" and "Pick a PR" remain instructions for the worker.
/// These aliases also cover questions persisted before disposition handling.
pub(crate) fn known_choice(choice: &str) -> Option<Disposition> {
    match choice.trim().to_ascii_lowercase().as_str() {
        "archive"
        | "archive it"
        | "archive this"
        | "archive this item"
        | "archive this chore"
        | "archive this task"
        | "archive this revision"
        | "archive this followup" => Some(Disposition::Archive),
        "park" | "park it" | "park this" | "park this item" | "park this chore" | "park this task"
        | "park this revision" | "park this followup" => Some(Disposition::Park),
        _ => None,
    }
}

impl WorkDb {
    pub(crate) fn park_answer_in_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        pending: &mut PendingEvents,
        task: &Task,
        now: &str,
        attribution: crate::task_status_audit::WriteAttribution<'_>,
    ) -> Result<Vec<String>> {
        anyhow::ensure!(
            attribution.actor == LAST_STATUS_ACTOR_HUMAN,
            "only the owner can choose to park an item"
        );
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
        // The question is answered. Leave a resumable item for unpark, with no
        // ready execution minted while the owner's hold is in force.
        self.update_task_in_tx(
            tx,
            pending,
            &task.id,
            WorkItemPatch::builder().status("todo").build(),
            crate::task_status_audit::WriteAttribution {
                actor: attribution.actor,
                provenance: Some(attribution.provenance.unwrap_or("operator_question_park")),
            },
        )?;
        Ok(vec![root.id])
    }
}
