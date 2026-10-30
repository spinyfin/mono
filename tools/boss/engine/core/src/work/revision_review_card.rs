//! A delivered revision reviews the same PR as its chain root. Reuse the
//! root's resolved review presentation rather than polling or classifying a
//! second, independently stale set of PR facts on the revision row.

use super::{Connection, PendingEvents, Result, Task, TaskKind, TaskStatus};
use std::collections::HashMap;

pub(super) fn attach(tasks: &mut [Task], chores: &[Task]) {
    let rows: HashMap<_, _> = tasks.iter().chain(chores).map(|t| (t.id.clone(), t.clone())).collect();
    for task in tasks {
        if task.kind != TaskKind::Revision || task.status != TaskStatus::InReview {
            continue;
        }
        let mut ancestor = task.parent_task_id.as_ref();
        for _ in 0..super::MAX_CHAIN_DEPTH {
            let Some(root) = ancestor.and_then(|id| rows.get(id)) else {
                break;
            };
            if root.kind != TaskKind::Revision {
                // The owner decides the PR's review state, including blocked
                // review cards. When that state agrees, retain the shared
                // badge resolver's card-specific findings/self-link wording.
                if task.ai_review_state != root.ai_review_state || task.ai_review_stage != root.ai_review_stage {
                    task.ai_review_state = root.ai_review_state.clone();
                    task.ai_review_badge = root.ai_review_badge.clone();
                    task.ai_review_findings_revision_id = root.ai_review_findings_revision_id.clone();
                }
                task.ai_reviewing = root.ai_reviewing;
                task.ai_review_stage = root.ai_review_stage.clone();
                copy_review_fields(task, root);
                break;
            }
            ancestor = root.parent_task_id.as_ref();
        }
    }
}

/// Catch up revisions completed by an older engine that kept the card in
/// Doing during automated review. Call only after the latest implementation
/// is known to be completed; explicit restarts must remain in Doing.
pub(super) fn reconcile_completed(pending: &mut PendingEvents, conn: &Connection, id: &str) -> Result<()> {
    if super::WorkDb::advance_held_pending_review_task_to_in_review_in_tx(conn, id, &super::now_string())? {
        super::stage_task_board_change(pending, conn, id)?;
    }
    Ok(())
}

pub(super) fn copy_review_fields(dst: &mut Task, src: &Task) {
    dst.pr_url = src.pr_url.clone();
    dst.ci_required_state = src.ci_required_state.clone();
    dst.ci_required_detail = src.ci_required_detail.clone();
    dst.review_required_state = src.review_required_state.clone();
    dst.review_required_detail = src.review_required_detail.clone();
    dst.pr_mergeable_state = src.pr_mergeable_state.clone();
    dst.pr_state_polled_at = src.pr_state_polled_at.clone();
    dst.pr_updated_at = src.pr_updated_at.clone();
    dst.merge_queue_state = src.merge_queue_state.clone();
    dst.merge_queue_detail = src.merge_queue_detail.clone();
    dst.has_in_progress_revision = src.has_in_progress_revision;
    dst.in_revision_blocked_only = src.in_revision_blocked_only;
    dst.ready_for_review = src.ready_for_review;
    dst.review_section = src.review_section;
    dst.review_guide_lifecycle = src.review_guide_lifecycle.clone();
    dst.review_guide_readable_version_id = src.review_guide_readable_version_id.clone();
    dst.review_guide_selected_comparison_id = src.review_guide_selected_comparison_id.clone();
    dst.review_guide_stale_source = src.review_guide_stale_source;
    dst.review_guide_error = src.review_guide_error.clone();
    dst.review_guide_update_status = src.review_guide_update_status.clone();
}
