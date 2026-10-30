//! A delivered revision reviews the same PR as its chain root. Reuse the
//! root's resolved review presentation rather than polling or classifying a
//! second, independently stale set of PR facts on the revision row.

use super::{Connection, Result, Task, TaskKind, TaskStatus};
use std::collections::{HashMap, HashSet};

/// Present revisions that delivered their implementation as Review cards.
/// With `project_held`, an `active` revision whose latest implementation
/// execution completed (it is held pending the automated review) is shown as
/// `in_review`; the stored status is untouched, so the hold keeps gating
/// verdict advancement, dependents and the review recovery sweeps. Every
/// `in_review` revision then takes the chain root's review presentation:
/// review/CI/ready fields plus its blocked status, reason and detail.
pub(super) fn attach(conn: &Connection, tasks: &mut [Task], chores: &[Task], project_held: bool) -> Result<()> {
    let is_candidate = |t: &Task| {
        t.kind == TaskKind::Revision
            && (t.status == TaskStatus::InReview || (project_held && t.status == TaskStatus::Active))
    };
    if !tasks.iter().any(is_candidate) {
        return Ok(());
    }
    let held = if project_held
        && tasks
            .iter()
            .any(|t| t.kind == TaskKind::Revision && t.status == TaskStatus::Active)
    {
        completed_revision_ids(conn)?
    } else {
        HashSet::new()
    };
    let by_id: HashMap<&str, &Task> = tasks.iter().chain(chores).map(|t| (t.id.as_str(), t)).collect();
    let mut targets: Vec<(usize, String)> = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        let shown = task.kind == TaskKind::Revision
            && (task.status == TaskStatus::InReview || (task.status == TaskStatus::Active && held.contains(&task.id)));
        if !shown {
            continue;
        }
        let mut ancestor = task.parent_task_id.as_deref();
        for _ in 0..super::MAX_CHAIN_DEPTH {
            let Some(root) = ancestor.and_then(|id| by_id.get(id)) else {
                break;
            };
            if root.kind != TaskKind::Revision {
                targets.push((index, root.id.clone()));
                break;
            }
            ancestor = root.parent_task_id.as_deref();
        }
    }
    let roots: HashMap<String, Task> = targets
        .iter()
        .filter_map(|(_, id)| by_id.get(id.as_str()).map(|t| (id.clone(), (*t).clone())))
        .collect();
    for (index, root_id) in targets {
        let task = &mut tasks[index];
        let root = &roots[&root_id];
        let position = boss_engine_board_gesture::BoardRow {
            status: root.status.clone(),
            autostart: root.autostart,
            blocked_reason: root.blocked_reason.clone(),
            merge_queue_state: root.merge_queue_state.clone(),
        }
        .position();
        if position.column != boss_protocol::BoardColumn::Review {
            continue;
        }
        // The owner decides the PR's review state, including blocked review
        // cards. When that state agrees, retain the shared badge resolver's
        // card-specific findings/self-link wording.
        if task.ai_review_state != root.ai_review_state
            || task.ai_review_stage != root.ai_review_stage
            || task.ai_review_badge.is_none()
        {
            task.ai_review_state = root.ai_review_state.clone();
            task.ai_review_badge = root.ai_review_badge.clone();
            task.ai_review_findings_revision_id = root.ai_review_findings_revision_id.clone();
        }
        task.ai_reviewing = root.ai_reviewing;
        task.ai_review_stage = root.ai_review_stage.clone();
        copy_review_fields(task, root);
        // Display-only: lifecycle reads retain the durable status.
        task.status = if root.status == TaskStatus::Blocked {
            TaskStatus::Blocked
        } else {
            TaskStatus::InReview
        };
        task.blocked_reason = root.blocked_reason.clone();
        task.blocked_detail = root.blocked_detail.clone();
    }
    Ok(())
}

/// Active revisions whose latest `revision_implementation` execution
/// completed: the implementation is delivered and nothing is restarting it.
pub(super) fn completed_revision_ids(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT t.id FROM tasks t
         WHERE t.kind = 'revision' AND t.status = 'active' AND t.deleted_at IS NULL
           AND (SELECT e.status FROM work_executions e
                WHERE e.work_item_id = t.id AND e.kind = 'revision_implementation'
                ORDER BY e.created_at DESC, e.id DESC LIMIT 1) = 'completed'",
    )?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ids)
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
