//! Resolves the human label of an execution — the work item it ran for, that
//! item's PR, and what the run was for — from durable rows.
//!
//! Backs `ResolveExecutionLabels`, which the Agents view uses for runs the
//! engine has already released. The live-worker state that carried the same
//! fields while the run was alive is dropped at release, so the label is
//! rebuilt from the execution → work item → PR binding instead of being
//! remembered by the frontend. The fields are derived by the same helper the
//! live state uses ([`work_item_header_fields`]), so a pane reads identically
//! before and after the run finishes.

use boss_protocol::{ExecutionKind, ExecutionLabelStatus, ExecutionWorkLabel};

use crate::live_worker_metadata::work_item_header_fields;
use crate::runner::work_item_name;
use crate::work::{WorkDb, WorkExecution};

/// Upper bound on ids resolved per request. The Agents view asks for the
/// finished panes on screen, never more than a few dozen.
pub const MAX_EXECUTION_LABELS_PER_REQUEST: usize = 256;

/// One label per requested id, in request order. A deleted work item or an
/// unknown execution is a label with that status, never an error.
pub fn resolve_execution_labels(work_db: &WorkDb, execution_ids: &[String]) -> Vec<ExecutionWorkLabel> {
    execution_ids
        .iter()
        .take(MAX_EXECUTION_LABELS_PER_REQUEST)
        .map(|id| match work_db.get_execution(id) {
            Ok(execution) => label_for_execution(work_db, &execution),
            Err(error) => {
                tracing::warn!(execution_id = %id, %error, "execution label: execution row not found");
                ExecutionWorkLabel::builder()
                    .execution_id(id.clone())
                    .status(ExecutionLabelStatus::ExecutionUnknown)
                    .build()
            }
        })
        .collect()
}

/// The label of an execution whose row is already in hand.
pub fn label_for_execution(work_db: &WorkDb, execution: &WorkExecution) -> ExecutionWorkLabel {
    let purpose = purpose(&execution.kind).map(str::to_owned);
    // A review-guide run points `work_item_id` at a comparison; the work item
    // it explains is the comparison series' root task.
    let owning_id = work_db.owning_work_item_id(&execution.work_item_id);
    let work_item = match work_db.get_work_item(&owning_id) {
        Ok(item) => item,
        Err(error) => {
            return unbound_label(execution, purpose, &owning_id, &error);
        }
    };
    let (product_id, work_item_short_id, pr_url) = work_item_header_fields(Some(&work_item), execution);
    ExecutionWorkLabel::builder()
        .execution_id(execution.id.clone())
        .status(ExecutionLabelStatus::Resolved)
        .kind(execution.kind.as_str())
        .work_item_id(owning_id)
        .work_item_title(work_item_name(&work_item))
        .maybe_product_id(product_id)
        .maybe_pr_url(pr_url)
        .maybe_purpose(purpose)
        .maybe_work_item_short_id(work_item_short_id)
        .build()
}

/// No work-item row backs the execution. Kinds that are never bound to a work
/// item are not "deleted"; everything else lost its item.
fn unbound_label(
    execution: &WorkExecution,
    purpose: Option<String>,
    owning_id: &str,
    error: &anyhow::Error,
) -> ExecutionWorkLabel {
    let status = if matches!(
        execution.kind,
        ExecutionKind::AnswerAgent | ExecutionKind::AutomationTriage
    ) {
        ExecutionLabelStatus::NoWorkItem
    } else {
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %owning_id,
            kind = %execution.kind,
            %error,
            "execution label: work item no longer resolves"
        );
        ExecutionLabelStatus::WorkItemDeleted
    };
    ExecutionWorkLabel::builder()
        .execution_id(execution.id.clone())
        .status(status)
        .kind(execution.kind.as_str())
        .maybe_pr_url(execution.pr_url.clone().filter(|url| !url.trim().is_empty()))
        .maybe_purpose(purpose)
        .build()
}

/// What a run was for, for the kinds where the work item's title alone would
/// not say. `None` for plain implementation runs.
fn purpose(kind: &ExecutionKind) -> Option<&'static str> {
    match kind {
        ExecutionKind::AnswerAgent => Some("Answer"),
        ExecutionKind::AutomationTriage => Some("Automation triage"),
        ExecutionKind::CiRemediation => Some("CI fix"),
        ExecutionKind::ConflictResolution => Some("Conflict resolution"),
        ExecutionKind::InvestigationImplementation => Some("Investigation"),
        ExecutionKind::PrReview => Some("Review"),
        ExecutionKind::PrReviewGuide => Some("Review guide"),
        ExecutionKind::ProductDesign | ExecutionKind::ProjectDesign => Some("Design"),
        ExecutionKind::Progression => Some("Progression"),
        ExecutionKind::RevisionImplementation => Some("Revision"),
        ExecutionKind::ChoreImplementation | ExecutionKind::TaskImplementation => None,
    }
}

#[cfg(test)]
#[path = "execution_label_tests.rs"]
mod tests;
