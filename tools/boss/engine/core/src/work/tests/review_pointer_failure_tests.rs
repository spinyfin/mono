//! Pointer failures settle batches without consuming review capacity or retrying.
use super::*;

#[test]
fn sibling_pre_start_failures_keep_guards_and_release_capacity_until_manual_restart() {
    let db = WorkDb::open(temp_db_path("sibling-pointer-failures")).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "blocked review");
    db.update_work_item(
        &root.id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some("https://github.com/example/repo/pull/42".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let ReviewBatchDispatch::Created { batch, executions } = db
        .create_pre_merge_review_batch(
            batch_input(root.id.clone(), "blocked-head", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    else {
        panic!("expected batch")
    };
    assert_eq!(executions.len(), 2);
    for execution in &executions {
        db.record_pre_start_failure(&execution.id, "worker", None, "pointer failure", &[])
            .unwrap();
        db.create_attention_item(
            CreateAttentionItemInput::builder()
                .execution_id(execution.id.clone())
                .kind(crate::execution_bookmark_recovery::RECOVERY_FAILED)
                .title("Recovery failed")
                .body_markdown("Preserved work is missing")
                .build(),
        )
        .unwrap();
    }
    // Force the exact equal-second race, independently of machine speed.
    db.connect()
        .unwrap()
        .execute("UPDATE work_runs SET started_at = '100'", [])
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_attention_items SET created_at = '100', last_raised_at = '100'",
            [],
        )
        .unwrap();
    let other_product = create_test_product(&db);
    let other_root = create_test_chore_manual(&db, other_product.id, "other product review");
    for index in 0..3 {
        db.create_review_batch(
            batch_input(
                other_root.id.clone(),
                &format!("other-head-{index}"),
                ReviewBatchPhase::PreMerge,
            ),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();
    }
    assert!(!db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap());
    for pass in 0..3 {
        db.reconcile_stale_attention_signals().unwrap();
        for execution in &executions {
            assert!(
                db.list_attention_items(&execution.id)
                    .unwrap()
                    .iter()
                    .any(|item| item.kind == crate::execution_bookmark_recovery::RECOVERY_FAILED
                        && item.status == "open"),
                "pass {pass}: a sibling bookkeeping run must not clear the guard"
            );
        }
        for candidate in db.list_dead_review_batch_member_candidates().unwrap() {
            db.retry_dead_review_batch_member(&candidate.execution_id).unwrap();
        }
        assert_eq!(db.list_executions(Some(&root.id)).unwrap().len(), 2);
    }
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Failed
    );
    assert!(
        db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap(),
        "the other product must regain the fourth reservation"
    );
    assert!(find_quorum_failed_attention(&db, &root.id).is_none());
    assert!(db.list_dead_review_batch_member_candidates().unwrap().is_empty());
    let needs_readmission = format!(
        "SELECT {}",
        crate::work::review_batches::pre_merge_review_needs_readmission_sql("?1")
    );
    let readmission = || {
        db.connect()
            .unwrap()
            .query_row(&needs_readmission, [&root.id], |row| row.get::<_, bool>(0))
            .unwrap()
    };
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = 'blocked-head' WHERE id = ?1",
            [&root.id],
        )
        .unwrap();
    assert!(!readmission(), "the deferred sweep must respect the open pointer guard");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_head_sha = 'new-head' WHERE id = ?1", [&root.id])
        .unwrap();
    assert!(readmission(), "a new head must remain eligible for normal admission");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = 'blocked-head' WHERE id = ?1",
            [&root.id],
        )
        .unwrap();

    assert!(
        matches!(
            db.create_pre_merge_review_batch(
                batch_input(root.id.clone(), "blocked-head", ReviewBatchPhase::PreMerge),
                "https://github.com/example/repo",
            )
            .unwrap(),
            ReviewBatchDispatch::ExistingBatch { .. }
        ),
        "automatic admission must not replace a pointer-blocked batch at the same head"
    );

    let replacement = db
        .request_pr_review(&root.id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    assert_eq!(db.list_executions(Some(&root.id)).unwrap().len(), 3);
    db.start_execution_run(&replacement.id, "worker", "host", "lease", "workspace", "/tmp/review")
        .unwrap();
    db.reconcile_stale_attention_signals().unwrap();
    assert!(readmission(), "genuine work resumption clears the admission guard");
    for execution in &executions {
        assert!(
            db.list_attention_items(&execution.id)
                .unwrap()
                .iter()
                .all(|item| item.kind != crate::execution_bookmark_recovery::RECOVERY_FAILED || item.status != "open")
        );
        assert!(matches!(
            db.retry_dead_review_batch_member(&execution.id).unwrap(),
            RetryDeadReviewBatchMember::NotRetried
        ));
    }
    assert!(db.list_dead_review_batch_member_candidates().unwrap().is_empty());
    assert_eq!(
        db.list_executions(Some(&root.id)).unwrap().len(),
        3,
        "manual restart must not resurrect the settled batch"
    );
}
