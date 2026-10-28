use super::*;

#[test]
fn completed_revision_is_not_redispatched_while_new_head_review_is_running() {
    // PR #143: implementation completed at 04:04:14Z, review started at
    // 04:04:21Z, then another implementation started at 04:06:24Z.
    // Cover reviews attached to either the PR owner or the revision itself.
    for review_on_root in [true, false] {
        let db = WorkDb::open_in_memory().unwrap();
        let product = make_revision_product(&db, "review-running");
        let root = make_in_review_chore(&db, &product, "https://github.com/brianduff/appoint/pull/143");
        let revision = insert_revision_row(&db, &product, &root);
        db.reconcile_product_executions(&product).unwrap();
        let original = executions_for(&db, &revision)[0].0.clone();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&original],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'active', autostart = 1 WHERE id = ?1",
            [&revision],
        )
        .unwrap();
        drop(conn);
        let review = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(if review_on_root { root } else { revision.clone() })
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET created_at = '9999999999' WHERE id = ?1",
                [&review.id],
            )
            .unwrap();
        for _ in 0..3 {
            db.reconcile_product_executions(&product).unwrap();
            assert!(db.reconcile_active_dispatch(|id| id == review.id).unwrap().is_empty());
            assert!(db.rescan_active_dispatch().unwrap().is_empty());
            assert!(
                !db.list_orphan_active_candidates(0).unwrap().contains(&revision),
                "the orphan sweep must not offer a completed revision for redispatch"
            );
            let implementations: Vec<_> = db
                .list_executions(Some(&revision))
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == ExecutionKind::RevisionImplementation)
                .collect();
            assert_eq!(implementations.len(), 1);
            assert_eq!(implementations[0].id, original);
            assert_eq!(implementations[0].status, ExecutionStatus::Completed);
            assert_eq!(db.get_execution(&review.id).unwrap().status, ExecutionStatus::Running);
        }
    }
}

#[test]
fn upgrade_rearms_legacy_pr_anchor_failure_once_without_releasing_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("work.db");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = make_revision_product(&db, "upgrade");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/42");
    let revision = insert_revision_row(&db, &product, &root);
    db.reconcile_product_executions(&product).unwrap();
    let before = executions_for(&db, &revision);
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
        [&before[0].0],
    )
    .unwrap();
    conn.execute("UPDATE tasks SET status = 'blocked', autostart = 0, blocked_reason = 'execution_recovery_failed', blocked_detail = 'PR head bookmark pr/42 must resolve to exactly one change; it is conflicted' WHERE id = ?1", [&revision]).unwrap();
    conn.execute_batch(
        "DROP TABLE schema_migrations; INSERT OR REPLACE INTO metadata VALUES ('schema_version', '58');",
    )
    .unwrap();
    drop(conn);
    drop(db);
    let db = WorkDb::open(path.clone()).unwrap();
    assert_eq!(task_status(&db, &revision), "todo");
    let after = executions_for(&db, &revision);
    assert_eq!(after.len(), 2);
    assert!(
        after
            .iter()
            .any(|(id, status)| id == &before[0].0 && status == "failed")
    );
    assert!(after.iter().any(|(_, status)| status == "waiting_dependency"));
    drop(db);
    let db = WorkDb::open(path.clone()).unwrap();
    assert_eq!(executions_for(&db, &revision).len(), 2);
}

#[test]
fn completed_revision_is_not_redispatched_while_completion_is_being_consumed() {
    let db = WorkDb::open(temp_db_path("revision-completed")).unwrap();
    let product = make_revision_product(&db, "completed");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/42");
    let revision = insert_revision_row(&db, &product, &root);
    db.reconcile_product_executions(&product).unwrap();
    let before = executions_for(&db, &revision);
    assert_eq!(before.len(), 1);
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
        [&before[0].0],
    )
    .unwrap();
    conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&revision])
        .unwrap();
    drop(conn);
    for _ in 0..3 {
        db.reconcile_product_executions(&product).unwrap();
        assert_eq!(
            executions_for(&db, &revision),
            vec![(before[0].0.clone(), "completed".into())]
        );
    }
    // A finished review is a later execution of another kind, not permission
    // to run the already-completed implementation again.
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = '9999999999' WHERE id = ?1",
            [&review.id],
        )
        .unwrap();
    for _ in 0..3 {
        db.reconcile_product_executions(&product).unwrap();
        assert_eq!(executions_for(&db, &revision).len(), 2);
    }
}

#[test]
fn unrecoverable_revision_raises_attention_on_the_pr_owner() {
    let db = WorkDb::open(temp_db_path("revision-recovery-attention")).unwrap();
    let product = make_revision_product(&db, "attention");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/42");
    let revision = insert_revision_row(&db, &product, &root);
    db.reconcile_product_executions(&product).unwrap();
    let execution = db.get_execution(&executions_for(&db, &revision)[0].0).unwrap();
    crate::execution_bookmark_recovery::report_chain_block(&db, &execution, "GitHub unavailable").unwrap();
    crate::execution_bookmark_recovery::report_chain_block(&db, &execution, "GitHub unavailable").unwrap();
    let attention = db
        .list_open_attention_items_of_kind(crate::execution_bookmark_recovery::CHAIN_BLOCKED)
        .unwrap();
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0].work_item_id.as_deref(), Some(root.as_str()));
    assert!(attention[0].body_markdown.contains("Later revisions"));
    assert!(attention[0].body_markdown.contains("GitHub unavailable"));
    let original_id = attention[0].id.clone();
    let next = insert_revision_row(&db, &product, &root);
    let next_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(next.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::WaitingDependency)
                .build(),
        )
        .unwrap();
    crate::execution_bookmark_recovery::report_chain_block(&db, &next_execution, "PR head deleted").unwrap();
    let attention = db
        .list_open_attention_items_of_kind(crate::execution_bookmark_recovery::CHAIN_BLOCKED)
        .unwrap();
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0].id, original_id);
    assert!(attention[0].body_markdown.contains(&next));
    assert!(attention[0].body_markdown.contains(&next_execution.id));
    assert!(attention[0].body_markdown.contains("PR head deleted"));
    assert!(!attention[0].body_markdown.contains(&execution.id));
    assert!(!attention[0].body_markdown.contains("GitHub unavailable"));
    assert_eq!(task_status(&db, &revision), "todo");
}
