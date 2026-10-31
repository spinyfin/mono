//! A delivered revision reviews the same PR as its chain root. Reuse the
//! root's resolved review presentation rather than polling or classifying a
//! second, independently stale set of PR facts on the revision row.

use super::{Connection, Task, TaskKind, TaskStatus};
use std::collections::{HashMap, HashSet};

/// Present revisions that delivered their implementation as Review cards.
/// With `held` (`Some`), an `active` revision in that set (its latest
/// implementation execution completed, so it is held pending the automated
/// review) is shown as `in_review`; the stored status is untouched, so the
/// hold keeps gating verdict advancement, dependents and the review recovery
/// sweeps. Every `in_review` revision then takes the chain root's review
/// presentation when the root sits in the Review column: review/CI/ready
/// fields plus its blocked status, reason and detail. Under a root outside
/// Review, a stored `in_review` revision still receives the root's PR
/// identity and CI facts, but keeps its own status and section.
pub(super) fn attach(tasks: &mut [Task], chores: &[Task], held: Option<&HashSet<String>>) {
    let no_held = HashSet::new();
    let held = held.unwrap_or(&no_held);
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
            // The root is blocked or otherwise out of Review for a reason
            // unrelated to the PR's review. A stored in_review revision
            // stays in the Review lane, so it still needs the PR identity
            // and CI facts; a held revision is not shown as in review.
            if task.status == TaskStatus::InReview {
                copy_pr_facts(task, root);
            }
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
}

/// Active revisions whose latest `revision_implementation` execution
/// completed: the implementation is delivered and nothing is restarting it.
fn completed_revision_ids(conn: &Connection) -> rusqlite::Result<HashSet<String>> {
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

/// The held-revision set, read once per projection pass. A failed read
/// degrades to "nothing held" (held revisions then count as in revision)
/// rather than dropping the pass's other revision flags.
pub(super) fn held_revision_ids(conn: &Connection) -> HashSet<String> {
    completed_revision_ids(conn).unwrap_or_else(|err| {
        tracing::warn!(?err, "failed to classify held revisions; treating none as held");
        HashSet::new()
    })
}

/// PR identity and polled facts, without the status, section or queue state
/// that would move a card between lanes.
fn copy_pr_facts(dst: &mut Task, src: &Task) {
    dst.pr_url = src.pr_url.clone();
    dst.ci_required_state = src.ci_required_state.clone();
    dst.ci_required_detail = src.ci_required_detail.clone();
    dst.review_required_state = src.review_required_state.clone();
    dst.review_required_detail = src.review_required_detail.clone();
    dst.pr_mergeable_state = src.pr_mergeable_state.clone();
    dst.pr_state_polled_at = src.pr_state_polled_at.clone();
    dst.pr_updated_at = src.pr_updated_at.clone();
}

pub(super) fn copy_review_fields(dst: &mut Task, src: &Task) {
    copy_pr_facts(dst, src);
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
