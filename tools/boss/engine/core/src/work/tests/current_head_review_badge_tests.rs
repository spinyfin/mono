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
fn current_head_review_badge_never_projects_a_self_referencing_findings_revision() {
    let db = WorkDb::open(temp_db_path("current-head-review-self-link")).unwrap();
    let product = make_revision_product(&db, "current-head-self-link");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/8006");
    let revision = insert_revision_row(&db, &product, &root);
    let sibling = insert_revision_row(&db, &product, &root);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review' WHERE id IN (?1, ?2)",
            rusqlite::params![revision, sibling],
        )
        .unwrap();
    for (owner, head) in [(&root, "root-head"), (&revision, "revision-head")] {
        observe(&db, &root, Some(head), "success", "mergeable");
        let findings = verdict(&db, owner, head, "completed_with_findings");
        db.set_review_verdict_revision_task_id(&findings, &revision).unwrap();
        let tree = db.get_work_tree(&product).unwrap();
        for id in [&root, &revision, &sibling] {
            let task = tree
                .tasks
                .iter()
                .chain(tree.chores.iter())
                .find(|task| &task.id == id)
                .unwrap();
            assert_eq!(task.ai_review_state.as_deref(), Some("reviewed_with_findings"));
            assert_eq!(
                task.ai_review_findings_revision_id.as_deref(),
                if id == &revision { None } else { Some(revision.as_str()) }
            );
        }
    }
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

fn poll(db: &WorkDb, root: &str, sha: &str, ci: &str) {
    db.update_task_pr_poll_state(
        root,
        PrPollStateInput {
            ci_required_state: ci,
            review_required_state: "approved",
            pr_mergeable_state: "mergeable",
            pr_head_sha: Some(sha),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn current_head_review_badge_ci_success_for_old_head_is_not_all_clear() {
    let db = WorkDb::open(temp_db_path("current-head-review-ci-head")).unwrap();
    let product = make_revision_product(&db, "current-head-ci-head");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/8004");
    poll(&db, &root, "a", "success");
    // A partial `boss pr status --refresh` observation moves the head only.
    db.set_pr_status_observation(&root, "mergeable", Some("CLEAN"), Some("b"), "2026-01-01T00:00:00Z")
        .unwrap();
    verdict(&db, &root, "b", "completed_clean");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_clean_pending")
    );
    poll(&db, &root, "b", "success");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_all_clear")
    );
}

#[test]
fn current_head_review_badge_done_card_without_matching_verdict_shows_no_badge() {
    let db = WorkDb::open(temp_db_path("current-head-review-done")).unwrap();
    let product = make_revision_product(&db, "current-head-done");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/8005");
    observe(&db, &root, None, "success", "mergeable");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&root])
        .unwrap();
    assert_eq!(card(&db, &product, &root).ai_review_state, None);
}

/// Record the revision's single execution as completed. `head_after` is what
/// the engine stores as `pr_head_after`: `None` when the worker made no commit.
fn complete_revision_execution(db: &WorkDb, revision: &str, head_after: Option<&str>) {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed', finished_at = '2026-01-01T00:00:00Z',
                    pr_head_after = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, head_after],
        )
        .unwrap();
}

fn set_status(db: &WorkDb, id: &str, status: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = ?2 WHERE id = ?1",
            rusqlite::params![id, status],
        )
        .unwrap();
}

fn findings_with_revision(db: &WorkDb, product: &str, root: &str, sha: &str) -> String {
    let revision = insert_revision_row(db, product, root);
    let findings = verdict(db, root, sha, "completed_with_findings");
    db.set_review_verdict_revision_task_id(&findings, &revision).unwrap();
    revision
}

#[test]
fn current_head_review_badge_findings_revision_delivered_without_head_change_is_not_orange() {
    let db = WorkDb::open(temp_db_path("current-head-review-no-commit-fix")).unwrap();
    let product = make_revision_product(&db, "current-head-no-commit-fix");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/8006");
    observe(&db, &root, Some("h1"), "success", "mergeable");
    let revision = findings_with_revision(&db, &product, &root, "h1");

    // Revision still running: findings remain unresolved.
    set_status(&db, &revision, "active");
    let running = card(&db, &product, &root);
    assert_eq!(running.ai_review_state.as_deref(), Some("reviewed_with_findings"));
    assert_eq!(
        running.ai_review_findings_revision_id.as_deref(),
        Some(revision.as_str())
    );

    // Delivered, but no execution has completed yet: no evidence of resolution.
    set_status(&db, &revision, "in_review");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_with_findings")
    );

    // Delivered with no commit (PR body edit / justified no-change).
    complete_revision_execution(&db, &revision, None);
    let delivered = card(&db, &product, &root);
    assert_eq!(delivered.ai_review_state.as_deref(), Some("not_reviewed"));
    assert!(delivered.ai_review_findings_revision_id.is_none());

    // A newer verdict for the same head wins over the resolution.
    verdict(&db, &root, "h1", "completed_with_findings");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_with_findings")
    );
    verdict(&db, &root, "h1", "completed_clean");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_all_clear")
    );
}

#[test]
fn current_head_review_badge_findings_revision_with_new_commit_keeps_head_rules() {
    let db = WorkDb::open(temp_db_path("current-head-review-commit-fix")).unwrap();
    let product = make_revision_product(&db, "current-head-commit-fix");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/8007");
    observe(&db, &root, Some("h1"), "success", "mergeable");
    let revision = findings_with_revision(&db, &product, &root, "h1");
    set_status(&db, &revision, "in_review");
    // The revision pushed a new head the poller has not observed yet.
    complete_revision_execution(&db, &revision, Some("h2"));
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_with_findings")
    );

    // Once the new head is observed, it is simply not yet reviewed.
    observe(&db, &root, Some("h2"), "success", "mergeable");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("not_reviewed")
    );
    verdict(&db, &root, "h2", "completed_clean");
    assert_eq!(
        card(&db, &product, &root).ai_review_state.as_deref(),
        Some("reviewed_all_clear")
    );
}
