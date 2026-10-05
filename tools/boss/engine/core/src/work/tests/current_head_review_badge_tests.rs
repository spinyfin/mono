use super::*;

fn observe(db: &WorkDb, root: &str, sha: Option<&str>, ci: &str, mergeable: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2, ci_required_state = ?3, pr_mergeable_state = ?4 WHERE id = ?1",
            rusqlite::params![root, sha, ci, mergeable],
        )
        .unwrap();
}

fn verdict(db: &WorkDb, owner: &str, sha: &str, outcome: &'static str) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(owner)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
        [&execution.id],
    )
    .unwrap();
    WorkDb::insert_review_verdict_in_tx(
        &conn,
        &execution.id,
        owner,
        &crate::work::ReviewVerdictInput {
            head_sha: Some(sha.to_owned()),
            findings_count: if outcome == "completed_clean" { 0 } else { 2 },
            revision_warranted: outcome == "completed_with_findings",
            gate_outcome: outcome,
        },
    )
    .unwrap();
    execution.id
}

fn card(db: &WorkDb, product: &str, root: &str) -> Task {
    db.get_work_tree(product)
        .unwrap()
        .chores
        .into_iter()
        .find(|t| t.id == root)
        .unwrap()
}

#[test]
fn current_head_review_badge_findings_revision_clean_then_new_unreviewed_head() {
    let db = WorkDb::open(temp_db_path("current-head-review-transition")).unwrap();
    let product = make_revision_product(&db, "current-head-transition");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/8002");
    let revision = insert_revision_row(&db, &product, &root);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&revision])
        .unwrap();
    observe(&db, &root, Some("old"), "success", "mergeable");
    // Legacy verdict on a revision must not outrank the cycle root's later
    // clean batch verdict for the new head.
    let findings = verdict(&db, &revision, "old", "completed_with_findings");
    db.set_review_verdict_revision_task_id(&findings, &revision).unwrap();
    let old = card(&db, &product, &root);
    assert_eq!(old.ai_review_state.as_deref(), Some("reviewed_with_findings"));
    assert_eq!(old.ai_review_findings_revision_id.as_deref(), Some(revision.as_str()));

    observe(&db, &root, Some("fixed"), "success", "mergeable");
    for status in ["active", "in_review"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                rusqlite::params![root, status],
            )
            .unwrap();
        assert_eq!(
            card(&db, &product, &root).ai_review_state.as_deref(),
            Some("not_reviewed")
        );
    }
    verdict(&db, &root, "fixed", "completed_clean");
    let clean = card(&db, &product, &root);
    assert_eq!(clean.ai_review_state.as_deref(), Some("reviewed_all_clear"));
    assert!(clean.ai_review_findings_revision_id.is_none());

    // Returning to Doing preserves the current head's review evidence,
    // but must not imply readiness while implementation is in progress.
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&root])
        .unwrap();
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_clean_pending")
    );
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&root])
        .unwrap();

    // A late result for the superseded SHA must not displace this review.
    verdict(&db, &root, "old", "completed_with_findings");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_all_clear")
    );
    observe(&db, &root, Some("next"), "success", "mergeable");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("not_reviewed")
    );
    verdict(&db, &root, "next", "completed_with_findings");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_with_findings")
    );
    observe(&db, &root, None, "success", "mergeable");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("not_reviewed")
    );
}

#[test]
fn current_head_review_badge_clean_does_not_imply_ready_with_ci_conflicts_or_pending_revisions() {
    let db = WorkDb::open(temp_db_path("current-head-review-readiness")).unwrap();
    let product = make_revision_product(&db, "current-head-readiness");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/8003");
    verdict(&db, &root, "clean", "completed_clean");
    for (ci, mergeable) in [
        ("fail", "mergeable"),
        ("in_progress", "mergeable"),
        ("success", "conflicting"),
        ("success", "unknown"),
    ] {
        observe(&db, &root, Some("clean"), ci, mergeable);
        assert_eq!(
            card(&db, &product, &root).ai_review_state.as_deref(),
            Some("reviewed_clean_pending")
        );
    }
    observe(&db, &root, Some("clean"), "success", "mergeable");
    let revision = insert_revision_row(&db, &product, &root);
    for status in ["todo", "active", "blocked"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                rusqlite::params![revision, status],
            )
            .unwrap();
        assert_eq!(
            card(&db, &product, &root).ai_review_state.as_deref(),
            Some("reviewed_clean_pending")
        );
    }
    // in_review means the revision has already delivered. Its presence alone
    // cannot negate the clean verdict for exactly the head now on the PR.
    for status in ["in_review", "done"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                rusqlite::params![revision, status],
            )
            .unwrap();
        assert_eq!(
            card(&db, &product, &root).ai_review_state.as_deref(),
            Some("reviewed_all_clear")
        );
    }
}
