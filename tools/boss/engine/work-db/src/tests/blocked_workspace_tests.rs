use super::*;

fn stamp_outcome(db: &WorkDb, id: &str, outcome: Option<&str>) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = ?2 WHERE id = ?1",
            params![id, outcome],
        )
        .unwrap();
}

#[test]
fn abandonment_preserves_only_a_durable_blocked_declaration() {
    for outcome in [None, Some("delivered"), Some("no_changes_needed"), Some("blocked")] {
        let db = WorkDb::open(temp_db_path("blocked-preservation")).unwrap();
        let (_, task, id) = make_waiting_human_chore(&db, "park");
        let workspace = db.get_execution(&id).unwrap().cube_workspace_id;
        stamp_outcome(&db, &id, outcome);
        db.record_worker_failure(&id, "worker stopped").unwrap();
        let prior = db.get_execution(&id).unwrap();
        assert_eq!(prior.cube_workspace_id, workspace);
        assert_eq!(prior.cube_lease_id.as_deref(), Some("lease-1"));
        assert_eq!(prior.workspace_path.as_deref(), Some("/workspaces/ws-1"));
        assert!(db.claim_execution_workspace_release(&id, "lease-1").unwrap());
        db.finish_execution_workspace_release(&prior).unwrap();
        assert!(db.get_execution(&id).unwrap().cube_lease_id.is_none());
        assert_eq!(
            prior.preferred_workspace_id,
            if outcome == Some("blocked") { workspace } else { None }
        );
        assert!(db.record_worker_failure(&id, "duplicate").unwrap().is_none());
        let next = db
            .request_execution(RequestExecutionInput::builder().work_item_id(task).build())
            .unwrap();
        assert_eq!(next.preferred_workspace_id, prior.preferred_workspace_id);
        assert_eq!(next.allow_dirty, outcome == Some("blocked"));
        assert_eq!(next.prefer_is_soft, outcome == Some("blocked"));
    }
}

#[test]
fn active_reconcile_preserves_park_until_explicit_resume_carries_workspace() {
    let db = WorkDb::open(temp_db_path("blocked-active-reconcile")).unwrap();
    let (_, task, id) = make_waiting_human_chore(&db, "park");
    stamp_outcome(&db, &id, Some("blocked"));
    db.record_worker_failure(&id, "decision needed").unwrap();
    assert!(!db.reconcile_active_dispatch(|_| false).unwrap().contains(&task));
    assert_eq!(db.latest_execution_for_work_item(&task).unwrap().unwrap().id, id);
    let next = db
        .request_execution(RequestExecutionInput::builder().work_item_id(task).build())
        .unwrap();
    assert_ne!(next.id, id);
    assert!(next.preferred_workspace_id.is_some());
    assert!(next.allow_dirty && next.prefer_is_soft);
    let WorkItem::Chore(retried) = db.get_work_item(&next.work_item_id).unwrap() else {
        panic!("expected chore");
    };
    assert!(retried.blocked_reason.is_none());
    assert!(retried.blocked_detail.is_none());
}

#[test]
fn revision_reconcile_preserves_park_until_explicit_resume_prefers_its_own_workspace() {
    let db = WorkDb::open(temp_db_path("blocked-revision")).unwrap();
    let product = make_revision_product(&db, "blocked-revision");
    let pr = "https://github.com/spinyfin/mono/pull/2823";
    let parent = make_in_review_chore(&db, &product, pr);
    let revision = insert_revision_row(&db, &product, &parent);
    db.reconcile_product_executions(&product).unwrap();
    let id = executions_for(&db, &revision)[0].0.clone();
    db.start_execution_run(
        &id,
        "worker",
        "mono",
        "lease-revision",
        "workspace-revision",
        "/tmp/workspace-revision",
    )
    .unwrap();
    stamp_outcome(&db, &id, Some("blocked"));
    db.record_worker_failure(&id, "decision needed").unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET autostart = 1 WHERE id = ?1", [&revision])
        .unwrap();
    db.reconcile_product_executions(&product).unwrap();
    assert_eq!(db.latest_execution_for_work_item(&revision).unwrap().unwrap().id, id);
    let next = db
        .request_execution(RequestExecutionInput::builder().work_item_id(revision).build())
        .unwrap();
    assert_ne!(next.id, id);
    assert_eq!(next.preferred_workspace_id.as_deref(), Some("workspace-revision"));
    assert!(next.allow_dirty && next.prefer_is_soft);
    assert_eq!(next.pr_url.as_deref(), Some(pr));
    assert_eq!(db.recovery_predecessor(&next).unwrap().unwrap().id, id);
}

#[test]
fn recovery_does_not_search_past_a_newer_failed_attempt_or_override_explicit_pin() {
    let db = WorkDb::open(temp_db_path("blocked-latest-only")).unwrap();
    let (_, task, id) = make_waiting_human_chore(&db, "park");
    stamp_outcome(&db, &id, Some("blocked"));
    db.record_worker_failure(&id, "decision needed").unwrap();
    let next = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&task)
                .preferred_workspace_id("explicit-other")
                .build(),
        )
        .unwrap();
    assert_eq!(next.preferred_workspace_id.as_deref(), Some("explicit-other"));
    assert!(!next.allow_dirty && !next.prefer_is_soft);
    db.connect()
        .unwrap()
        .execute("UPDATE work_executions SET status = 'failed' WHERE id = ?1", [&next.id])
        .unwrap();
    let fresh = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&task).build())
        .unwrap();
    assert!(fresh.preferred_workspace_id.is_none());
    assert!(!fresh.allow_dirty);
}

/// A reviewer never writes to the PR branch, so a dead reviewer's recovery
/// record must never become the next reviewer's predecessor: inheriting it
/// would pin the host, reposition the review on the head the dead reviewer
/// looked at, and fail the review before start whenever that record's
/// pointers no longer match a rewritten PR history. An implementation kind
/// of the same item still inherits.
#[test]
fn read_only_reviews_never_inherit_a_recovery_predecessor() {
    let db = WorkDb::open(temp_db_path("read-only-review-predecessor")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "reviewed PR");
    for (kind, inherits) in [
        (ExecutionKind::PrReview, false),
        (ExecutionKind::ChoreImplementation, true),
    ] {
        let prior = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(kind.clone())
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        db.start_execution_run(
            &prior.id,
            "worker",
            "mono",
            "lease-old",
            "workspace-old",
            "/tmp/workspace-old",
        )
        .unwrap();
        db.record_execution_bookmark(&boss_engine_recovery::execution_bookmark::ExecutionBookmark {
            execution_id: prior.id.clone(),
            repo_path: "/tmp/shared".into(),
            host_id: "local".into(),
            base_commit: None,
        })
        .unwrap();
        db.record_worker_failure(&prior.id, "died before start").unwrap();
        let next = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(kind.clone())
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        assert_eq!(
            db.recovery_predecessor(&next).unwrap().map(|prior| prior.id),
            inherits.then(|| prior.id.clone()),
            "{kind:?}"
        );
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'cancelled', finished_at = '2' WHERE id = ?1",
                params![next.id],
            )
            .unwrap();
    }
}
