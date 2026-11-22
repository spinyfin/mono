//! Recovery and fallback must leave review ownership with its batch, even
//! between a member dying and its replacement being admitted.

use std::sync::Arc;

use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus, ReviewBatchPhase, ReviewBatchStatus};

use super::{PR_URL, batch_input, bind_open_pr, insert_revision_row, raw};
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::execution_publisher::NoopExecutionPublisher;
use crate::test_support::{create_test_chore_manual, create_test_product_with_repo, make_coordinator, open_db};
use crate::work::{FakePrStateChecker, PrOpenState, ReviewBatchDispatch, WorkDb};

const REPO: &str = "https://github.com/example/repo";

#[tokio::test]
async fn legacy_recovery_respects_live_ownership_but_not_completed_history() {
    for (status, generation, refires) in [
        ("collecting", 1, false),
        ("completed", 1, true),
        ("failed", crate::work::MAX_AUTOMATIC_PRE_MERGE_BATCH_GENERATIONS, false),
        ("failed", 1, false),
    ] {
        let (_dir, db) = open_db();
        let db = Arc::new(db);
        let product = create_test_product_with_repo(&db, "legacy recovery", Some(REPO));
        let root = create_test_chore_manual(&db, product.id, "Review target");
        bind_open_pr(&db, &root.id);
        let (batch, _) = db
            .create_review_batch(
                batch_input(root.id.clone(), "old-head", ReviewBatchPhase::PreMerge),
                &[crate::work::ReviewBatchMemberCreateInput::builder()
                    .attempt(1)
                    .provider_effort("medium")
                    .requested_driver("claude")
                    .resolved_model("test-model")
                    .role(boss_protocol::ReviewBatchMemberRole::ClaudeReviewer)
                    .status(boss_protocol::ReviewBatchMemberStatus::Pending)
                    .build()],
            )
            .unwrap();
        raw(&db)
            .execute(
                "UPDATE pr_review_batches SET status = ?2, generation = ?3 WHERE id = ?1",
                rusqlite::params![batch.id, status, generation],
            )
            .unwrap();
        db.create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&root.id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Failed)
                .build(),
        )
        .unwrap();
        let outcome = recover(&db).await;
        assert_eq!(outcome.refired, usize::from(refires));
        assert_eq!(outcome.error_skipped, 0);
        assert_eq!(outcome.churn_skipped, 0);
        assert_eq!(
            db.list_executions(Some(&root.id)).unwrap().len(),
            1 + usize::from(refires)
        );
    }
}

async fn recover(db: &Arc<WorkDb>) -> crate::pr_review_recovery::PrReviewRecoveryOutcome {
    crate::pr_review_recovery::run_one_pass(
        db,
        make_coordinator(db.clone(), 0),
        &RecordingDispatchEventSink::new(),
        Arc::new(FakePrStateChecker::always(PrOpenState::Open)),
    )
    .await
}

#[tokio::test]
async fn live_batch_prevents_fallback_minting_a_memberless_reviewer() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let product = create_test_product_with_repo(&db, "batch fallback", Some(REPO));
    let root = create_test_chore_manual(&db, product.id.clone(), "Review target");
    bind_open_pr(&db, &root.id);
    let revision = insert_revision_row(&db, &product.id, &root.id);
    bind_open_pr(&db, &revision);
    let ReviewBatchDispatch::Created { batch, executions } = db
        .create_pre_merge_review_batch(batch_input(root.id.clone(), "head", ReviewBatchPhase::PreMerge), REPO)
        .unwrap()
    else {
        panic!("expected new batch");
    };
    for execution in &executions {
        raw(&db)
            .execute(
                "UPDATE work_executions SET status = 'running' WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();
        assert!(
            db.request_resume_execution(&execution.id, 1, 0, "transient API failure")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get_execution(&execution.id).unwrap().status,
            ExecutionStatus::Orphaned
        );
    }
    // Every leaf is terminal but the batch still owns their retries, so the
    // fallback must not enqueue a memberless legacy reviewer.
    for task_id in [&root.id, &revision] {
        for (fanout, review_result_written) in [(true, false), (false, false), (true, true), (false, true)] {
            raw(&db)
                .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [task_id])
                .unwrap();
            let mut outcome = crate::merge_poller::SweepOutcome::default();
            crate::merge_poller::sweep_stalled_reviewer(
                &db,
                &NoopExecutionPublisher,
                crate::merge_poller::StalledReview {
                    task_id,
                    product_id: &product.id,
                    pr_url: PR_URL,
                    review_result_written,
                    skip_unbatched_review_holds: fanout,
                },
                &FakePrStateChecker::always(PrOpenState::Open),
                &mut outcome,
            )
            .await;
            assert_eq!(outcome.reviewer_fallback_review_refired, 0);
            assert_eq!(outcome.reviewer_fallback_advanced, 0);
            let task = db.get_work_item(task_id).unwrap();
            let (boss_protocol::WorkItem::Task(task) | boss_protocol::WorkItem::Chore(task)) = task else {
                panic!("expected task");
            };
            assert_eq!(task.status, boss_protocol::TaskStatus::Active);
        }
        // Also exercise the atomic guard: a batch may have been admitted
        // after a sweep checked ownership but before it requests a review.
        let error = db
            .request_pr_review(task_id, &FakePrStateChecker::always(PrOpenState::Open))
            .unwrap_err();
        assert!(error.to_string().contains("live pre_merge review batch"), "{error}");
        assert!(db.create_pr_review_execution_dedup(task_id, REPO).is_err());
    }
    assert_eq!(db.list_executions(Some(&root.id)).unwrap().len(), executions.len());
    assert!(db.list_executions(Some(&revision)).unwrap().is_empty());
    assert!(db.list_attention_items_for_work_item(&root.id).unwrap().is_empty());

    let outcome = recover(&db).await;
    assert_eq!(outcome.refired, executions.len());
    for execution in db.list_executions(Some(&root.id)).unwrap() {
        let member = db.review_batch_member_for_execution(&execution.id).unwrap().unwrap();
        assert_eq!(member.batch_id, batch.id);
        if execution.status == ExecutionStatus::Ready {
            assert_eq!(member.attempt, 2);
        }
    }
}

#[tokio::test]
async fn memberless_failed_review_does_not_preempt_stale_batch_readmission() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let product = create_test_product_with_repo(&db, "batch readmission", Some(REPO));
    let root = create_test_chore_manual(&db, product.id, "Review target");
    bind_open_pr(&db, &root.id);
    let ReviewBatchDispatch::Created { batch, executions } = db
        .create_pre_merge_review_batch(batch_input(root.id.clone(), "head", ReviewBatchPhase::PreMerge), REPO)
        .unwrap()
    else {
        panic!("expected new batch");
    };
    for execution in &executions {
        db.mark_execution_redundant(&execution.id).unwrap();
    }
    raw(&db)
        .execute(
            "UPDATE pr_review_batch_members SET attempt = 2 WHERE batch_id = ?1",
            [&batch.id],
        )
        .unwrap();
    raw(&db)
        .execute(
            "UPDATE pr_review_batches SET updated_at = '1' WHERE id = ?1",
            [&batch.id],
        )
        .unwrap();
    // A failed memberless review exists for the cycle root; it must not preempt batch readmission.
    let legacy = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&root.id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Failed)
                .build(),
        )
        .unwrap();
    assert!(
        db.list_dead_pr_review_candidates()
            .unwrap()
            .iter()
            .any(|c| c.execution_id == legacy.id)
    );
    assert_eq!(db.reap_inert_review_batches(600).unwrap(), vec![batch.id.clone()]);
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Failed
    );

    let before = db.list_executions(Some(&root.id)).unwrap().len();
    let outcome = recover(&db).await;
    assert_eq!(outcome.refired, 0);
    assert_eq!(outcome.churn_skipped, 0);
    assert_eq!(db.list_executions(Some(&root.id)).unwrap().len(), before);
    let ReviewBatchDispatch::Created {
        batch: next,
        executions,
    } = db
        .create_pre_merge_review_batch(batch_input(root.id.clone(), "head", ReviewBatchPhase::PreMerge), REPO)
        .unwrap()
    else {
        panic!("stale batch must permit automatic readmission");
    };
    assert_eq!(next.generation, 2);
    assert_ne!(next.id, batch.id);
    for execution in executions {
        assert_eq!(execution.status, ExecutionStatus::Ready);
        assert_eq!(
            db.review_batch_member_for_execution(&execution.id)
                .unwrap()
                .unwrap()
                .batch_id,
            next.id
        );
    }
}
