//! Board membership travels with the live snapshot so spawn, stop, adoption
//! and slot release all update the board without a second database refresh.

use std::collections::BTreeMap;

use boss_protocol::{DoingSection, ExecutionKind, FrontendEvent, LiveWorkerState};

use super::ServerState;
use crate::work::WorkDb;

impl ServerState {
    pub(super) fn live_worker_board_snapshot(&self) -> FrontendEvent {
        let states = self.live_worker_states_snapshot();
        let doing_sections = classify_doing_sections(&self.work_db, &states);
        FrontendEvent::WorkerLiveStatesList { states, doing_sections }
    }
}

/// Every Doing card has exactly one section. A nonterminal slot is live even
/// while spawning, idle between turns, held, or awaiting input. Execution/run
/// status alone is not liveness: bookkeeping can finish before a worker exits.
/// Live implementation wins over simultaneous review. All remaining cards
/// are Queued, regardless of execution status or dispatch wait reason (pause,
/// capacity, dependency, deadline, retry, between runs, or an operator question).
/// The sparse map's absent-key default is part of the wire contract.
pub(super) fn classify_doing_sections(db: &WorkDb, states: &[LiveWorkerState]) -> BTreeMap<String, DoingSection> {
    let mut sections = BTreeMap::new();
    for state in states.iter().filter(|state| !state.activity.is_terminal()) {
        // Adopted review-guide workers may have no binding: their execution
        // targets a source comparison, not a task. Reuse the durable owner
        // resolution used for attention/audit instead of guessing in Swift.
        let execution = if state.work_item_id.is_none() || state.kind.is_none() {
            db.get_execution(&state.run_id).ok()
        } else {
            None
        };
        let Some(item_id) = state
            .work_item_id
            .as_deref()
            .or_else(|| execution.as_ref().map(|e| e.work_item_id.as_str()))
        else {
            continue;
        };
        let kind = state
            .kind
            .as_deref()
            .and_then(|kind| kind.parse().ok())
            .or_else(|| execution.as_ref().map(|e| e.kind.clone()));
        let section = section_for_live_kind(kind.as_ref());
        let owner = db.owning_work_item_id(item_id);
        sections
            .entry(owner)
            .and_modify(|current| {
                if section == DoingSection::Coding {
                    *current = section;
                }
            })
            .or_insert(section);
    }
    sections
}

fn section_for_live_kind(kind: Option<&ExecutionKind>) -> DoingSection {
    match kind {
        Some(ExecutionKind::PrReview | ExecutionKind::PrReviewGuide) => DoingSection::Reviewing,
        // Coding includes all non-review work on an item, including design,
        // investigation, automation triage and answering. An untyped live
        // worker also stays visibly active rather than falsely claiming a wait.
        Some(
            ExecutionKind::TaskImplementation
            | ExecutionKind::ChoreImplementation
            | ExecutionKind::RevisionImplementation
            | ExecutionKind::InvestigationImplementation
            | ExecutionKind::CiRemediation
            | ExecutionKind::ConflictResolution
            | ExecutionKind::ProductDesign
            | ExecutionKind::ProjectDesign
            | ExecutionKind::AutomationTriage
            | ExecutionKind::AnswerAgent,
        )
        | None => DoingSection::Coding,
    }
}

#[cfg(test)]
#[path = "doing_sections_tests.rs"]
mod tests;
