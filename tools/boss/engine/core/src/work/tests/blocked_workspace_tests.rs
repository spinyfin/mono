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
        db.record_worker_idle_abandonment(&id, "worker stopped").unwrap();
        let prior = db.get_execution(&id).unwrap();
        assert!(prior.cube_workspace_id.is_none());
        assert!(prior.cube_lease_id.is_none());
        assert!(prior.workspace_path.is_none());
        assert_eq!(
            prior.preferred_workspace_id,
            if outcome == Some("blocked") { workspace } else { None }
        );
        assert!(db.record_worker_idle_abandonment(&id, "duplicate").unwrap().is_none());
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
    db.record_worker_idle_abandonment(&id, "decision needed").unwrap();
    assert!(!db.reconcile_active_dispatch(|_| false).unwrap().contains(&task));
    assert_eq!(db.latest_execution_for_work_item(&task).unwrap().unwrap().id, id);
    let next = db
        .request_execution(RequestExecutionInput::builder().work_item_id(task).build())
        .unwrap();
    assert_ne!(next.id, id);
    assert!(next.preferred_workspace_id.is_some());
    assert!(next.allow_dirty && next.prefer_is_soft);
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
    db.record_worker_idle_abandonment(&id, "decision needed").unwrap();
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
    db.record_worker_idle_abandonment(&id, "decision needed").unwrap();
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
