//! A merge request remains live while the engine resolves its conflict.
use super::*;

pub const RESOLVING_MERGE_CONFLICT: &str = "resolving_conflicts";

pub(crate) fn start(conn: &Connection, id: &str, pr_url: &str, base: &str) -> Result<bool> {
    let detail = serde_json::json!({
        "source": "conflict_resolution",
        "status_label": format!("Resolving conflicts with {base}…"),
        "section_order": 500000,
    })
    .to_string();
    Ok(conn.execute(
        "UPDATE tasks SET merge_queue_state = 'resolving_conflicts', merge_queue_detail = ?3
         WHERE id = ?1 AND pr_url = ?2 AND deleted_at IS NULL
           AND (status = 'in_review' OR (status = 'blocked' AND blocked_reason = 'merge_conflict'))",
        params![id, pr_url, detail],
    )? > 0)
}

pub(crate) fn stop(conn: &Connection, id: &str, reason: &str) -> Result<bool> {
    let detail = serde_json::json!({
        "source": "conflict_resolution",
        "status_label": format!("Merge paused: {reason}"),
    })
    .to_string();
    let changed = conn.execute(
        "UPDATE tasks SET merge_queue_state = NULL, merge_queue_detail = ?3, blocked_detail = ?2
         WHERE id = ?1 AND merge_queue_state = 'resolving_conflicts'",
        params![id, reason, detail],
    )? > 0;
    if changed {
        conn.execute(
            "UPDATE github_merge_intents SET status = 'conflict_resolution_stopped'
             WHERE work_item_id = ?1 AND status = 'active'",
            [id],
        )?;
    }
    Ok(changed)
}

/// A different PR binding or an explicit move out of review cancels the request.
pub(crate) fn cancel(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE tasks SET merge_queue_state = NULL, merge_queue_detail = NULL
         WHERE id = ?1 AND CASE WHEN json_valid(merge_queue_detail)
             THEN json_extract(merge_queue_detail, '$.source') = 'conflict_resolution' ELSE 0 END",
        [id],
    )?;
    if changed > 0 {
        conn.execute(
            "UPDATE github_merge_intents SET status = 'cancelled' WHERE work_item_id = ?1 AND status = 'active'",
            [id],
        )?;
    }
    Ok(())
}

/// Keep resolver bookkeeping blocked internally while reporting the ongoing
/// merge to board clients. Only this engine-owned request can override a
/// conflict block; human moves and other blocking reasons remain authoritative.
pub(crate) fn project(tasks: &mut [Task], chores: &mut [Task]) {
    for task in tasks.iter_mut().chain(chores.iter_mut()) {
        if task.merge_queue_state.as_deref() == Some(RESOLVING_MERGE_CONFLICT)
            && task.status == TaskStatus::Blocked
            && task.blocked_reason.as_deref() == Some("merge_conflict")
        {
            task.status = TaskStatus::InReview;
            task.blocked_reason = None;
            task.blocked_detail = None;
        }
    }
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Record the user's merge request before releasing a deferred conflict.
    pub fn start_merge_conflict_resolution(&self, id: &str, pr_url: &str, base: &str) -> Result<bool> {
        let conn = self.connect()?;
        start(&conn, id, pr_url, base)
    }

    pub fn stop_merge_conflict_resolution(&self, id: &str, reason: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = stop(&tx, id, reason)?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn merge_conflict_resolution_requested(&self, id: &str, pr_url: &str) -> Result<bool> {
        let conn = self.connect()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND pr_url = ?2
             AND deleted_at IS NULL AND merge_queue_state = 'resolving_conflicts'
             AND (status = 'in_review' OR (status = 'blocked' AND blocked_reason = 'merge_conflict')))",
            params![id, pr_url],
            |row| row.get(0),
        )?)
    }

    /// Replace the old, pre-rebase head with the head GitHub accepted. The
    /// transaction never exposes a gap in the card's Merging state.
    pub fn finish_merge_conflict_resolution(&self, id: &str, pr_url: &str, head: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND pr_url = ?2 AND status = 'in_review'
             AND deleted_at IS NULL AND merge_queue_state = 'resolving_conflicts')",
            params![id, pr_url],
            |row| row.get(0),
        )?;
        if !current {
            return Ok(());
        }
        tx.execute(
            "UPDATE github_merge_intents SET status = 'head_advanced' WHERE work_item_id = ?1 AND status = 'active'",
            [id],
        )?;
        tx.execute(
            "INSERT INTO github_merge_intents (id, work_item_id, pr_url, head_sha, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5)",
            params![next_id("gmi"), id, pr_url, head, now_string()],
        )?;
        tx.execute(
            "UPDATE tasks SET merge_queue_state = 'queued', merge_queue_detail = '{\"section_order\":500000}', blocked_detail = NULL
             WHERE id = ?1 AND pr_url = ?2 AND merge_queue_state = 'resolving_conflicts'",
            params![id, pr_url],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_engine_board_gesture::BoardRow;
    use boss_protocol::{BoardGroup, ReviewSection};

    const PR: &str = "https://github.com/example/repo/pull/42";

    fn fixture(requested: bool) -> (WorkDb, String, String, String) {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = crate::test_support::create_test_product_named(&db, "Merging");
        let task = crate::test_support::create_test_chore_manual(&db, product.id.clone(), "Resolve and merge");
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(PR.into()),
                ..Default::default()
            },
        )
        .unwrap();
        if requested {
            db.record_github_merge_intent(
                GithubMergeIntentInsertInput::builder()
                    .work_item_id(&task.id)
                    .pr_url(PR)
                    .head_sha("old-head")
                    .build(),
                "{}",
            )
            .unwrap();
        }
        db.mark_chore_blocked_merge_conflict(&task.id, PR).unwrap();
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(&product.id)
                    .work_item_id(&task.id)
                    .pr_url(PR)
                    .pr_number(42)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("base")
                    .head_sha_before("old-head")
                    .build(),
            )
            .unwrap()
            .unwrap();
        (db, product.id, task.id, attempt.id)
    }

    fn card(db: &WorkDb, product: &str, id: &str) -> Task {
        db.get_work_tree(product)
            .unwrap()
            .chores
            .into_iter()
            .find(|task| task.id == id)
            .unwrap()
    }

    #[test]
    fn requested_merge_stays_visible_through_resolution_and_polling() {
        let (db, product, id, attempt) = fixture(true);
        for state in [None, Some("auto_merge_enabled")] {
            db.update_task_pr_poll_state(
                &id,
                PrPollStateInput {
                    caller: "merge_conflict_test",
                    ci_required_state: "success",
                    review_required_state: "approved",
                    ci_required_detail: None,
                    review_required_detail: None,
                    merge_queue_state: state,
                    merge_queue_detail: None,
                    preserve_merge_queue_state: false,
                    pr_mergeable_state: "conflicting",
                    pr_merge_state_status: None,
                    pr_head_sha: Some("old-head"),
                    pr_updated_at: None,
                },
            )
            .unwrap();
            let task = card(&db, &product, &id);
            assert_eq!(task.status, TaskStatus::InReview);
            assert_eq!(task.review_section, None);
            let WorkItem::Chore(single) = db.get_work_item(&id).unwrap() else {
                panic!("expected chore")
            };
            assert_eq!(single.status, task.status);
            assert_eq!(single.blocked_reason, task.blocked_reason);
            assert!(
                task.merge_queue_detail
                    .as_deref()
                    .unwrap()
                    .contains("Resolving conflicts with main")
            );
            assert_eq!(
                BoardRow {
                    status: task.status,
                    autostart: task.autostart,
                    blocked_reason: task.blocked_reason,
                    merge_queue_state: task.merge_queue_state
                }
                .position()
                .group,
                Some(BoardGroup::Merging)
            );
            // Resolver bookkeeping remains truthful; only board presentation is projected.
            assert_eq!(db.task_blocked_reason(&id).unwrap().as_deref(), Some("merge_conflict"));
        }
        db.clear_chore_blocked_merge_conflict(&id, PR).unwrap();
        db.mark_conflict_resolution_succeeded_at_rung(&attempt, Some("new-head"), 1)
            .unwrap();
        assert_eq!(
            card(&db, &product, &id).merge_queue_state.as_deref(),
            Some(RESOLVING_MERGE_CONFLICT)
        );
        db.finish_merge_conflict_resolution(&id, PR, "new-head").unwrap();
        assert_eq!(card(&db, &product, &id).merge_queue_state.as_deref(), Some("queued"));
        assert_eq!(
            db.get_active_github_merge_intent(&id, PR).unwrap().unwrap().head_sha,
            "new-head"
        );
    }

    #[test]
    fn failures_and_revision_handoffs_leave_merging_with_a_reason() {
        for outcome in ["failed", "abandoned", "revision"] {
            let (db, product, id, attempt) = fixture(true);
            match outcome {
                "failed" => {
                    db.mark_conflict_resolution_failed(&attempt, "Rebase failed").unwrap();
                }
                "abandoned" => {
                    db.mark_conflict_resolution_abandoned(&attempt, "Resolution stopped")
                        .unwrap();
                }
                _ => {
                    db.set_conflict_resolution_revision_task_id(&attempt, "revision")
                        .unwrap();
                }
            }
            let task = card(&db, &product, &id);
            assert_eq!(task.merge_queue_state, None, "{outcome}");
            assert_eq!(task.review_section, Some(ReviewSection::Blocked));
            assert!(task.blocked_detail.is_some());
            assert!(db.get_active_github_merge_intent(&id, PR).unwrap().is_none());
            db.update_task_pr_poll_state(
                &id,
                PrPollStateInput {
                    caller: "merge_conflict_test",
                    merge_queue_state: Some("auto_merge_enabled"),
                    ci_required_state: "success",
                    pr_mergeable_state: "mergeable",
                    ..Default::default()
                },
            )
            .unwrap();
            let polled = card(&db, &product, &id);
            assert_eq!(polled.merge_queue_state, None);
            assert_eq!(
                polled.merge_queue_detail, task.merge_queue_detail,
                "the failure explanation must survive polling"
            );
        }
    }

    #[test]
    fn background_resolution_does_not_become_a_merge_request() {
        let (db, product, id, _) = fixture(false);
        let task = card(&db, &product, &id);
        assert_eq!(task.status, TaskStatus::Blocked);
        assert_eq!(task.merge_queue_state, None);
        assert!(!db.merge_conflict_resolution_requested(&id, PR).unwrap());
    }

    #[test]
    fn human_moves_and_pr_rebinding_override_the_merge_request() {
        let (db, product, id, _) = fixture(true);
        db.update_work_item(
            &id,
            WorkItemPatch {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(card(&db, &product, &id).status, TaskStatus::Active);
        assert!(!db.merge_conflict_resolution_requested(&id, PR).unwrap());
        db.update_work_item(
            &id,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !db.merge_conflict_resolution_requested(&id, PR).unwrap(),
            "returning to review cannot resurrect a cancelled merge"
        );
        db.start_merge_conflict_resolution(&id, PR, "main").unwrap();
        let replacement = "https://github.com/example/repo/pull/43";
        db.update_work_item(
            &id,
            WorkItemPatch {
                pr_url: Some(replacement.into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!db.merge_conflict_resolution_requested(&id, replacement).unwrap());
        assert_eq!(card(&db, &product, &id).merge_queue_state, None);
    }

    #[test]
    fn changing_the_blocked_reason_cancels_the_merge_continuation() {
        let (db, product, id, _) = fixture(true);
        db.update_work_item(
            &id,
            WorkItemPatch {
                blocked_reason: Some("dependency".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(card(&db, &product, &id).merge_queue_state, None);
        assert!(!db.merge_conflict_resolution_requested(&id, PR).unwrap());
    }
}
