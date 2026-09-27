//! Dispatch-level coverage for a `PostMergeReviewer` batch member: the seam
//! where a `cube workspace goto --pr <n>` positioning attempt would hard-fail
//! for every post-merge review, since the batch only exists because its PR
//! already merged (see `goto_workspace_revision` on `HostAdapter`/`CubeClient`
//! and the `post_merge_target_sha` special-case in `spawn_attempt`).
//!
//! The lower-level batch/member/quorum mechanics are covered in
//! `work/tests/review_batches_tests.rs`; this module exercises only the
//! dispatch path that those tests cannot reach.

use super::helpers::*;

use boss_protocol::{ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile};

use crate::work::ReviewBatchCreateInput;

fn deep_classification() -> ReviewClassification {
    ReviewClassification::builder()
        .changed_files(vec!["tools/boss/engine/core/src/lib.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Deep)
        .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
        .build()
}

/// Create a post-merge review batch (and its sole `PostMergeReviewer`
/// execution) via the same `create_post_merge_review_batch` path the merge
/// poller uses in production, returning the dispatchable execution.
fn make_post_merge_review_execution(db: &WorkDb, merge_sha: &str) -> WorkExecution {
    let product = create_test_product_named(db, "PostMergeProduct");
    let cycle_root = create_test_chore_manual(db, product.id, "post-merge review target");

    let input = ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root.id)
        .base_sha("base-sha")
        .classification(deep_classification())
        .phase(ReviewBatchPhase::PostMerge)
        .pr_number(2909)
        .pr_url("https://github.com/spinyfin/mono/pull/2909")
        .target_sha(merge_sha)
        .merge_sha(merge_sha)
        .build();

    match db
        .create_post_merge_review_batch(input, "git@github.com:spinyfin/mono.git")
        .unwrap()
    {
        crate::work::ReviewBatchDispatch::Created { executions, .. } => {
            assert_eq!(
                executions.len(),
                1,
                "a post-merge batch has exactly one member execution"
            );
            executions.into_iter().next().unwrap()
        }
        other => panic!("expected a newly-created post-merge review batch, got {other:?}"),
    }
}

/// A `PrReview` execution that is the sole member of a `PostMerge` batch
/// must position via `cube workspace goto --revision <merge_sha>`, and must
/// NOT go through the PR-head `--pr <n>` path — `cube workspace goto --pr`
/// hard-errors on a MERGED PR, and the batch's PR is merged by construction.
#[tokio::test]
async fn post_merge_review_batch_member_positions_via_goto_revision_not_pr() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);

    let merge_sha = "merge-sha-abc123";
    let execution = make_post_merge_review_execution(&db, merge_sha);
    assert_eq!(execution.kind, ExecutionKind::PrReview);

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord);

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("review pool slot available");

    let result = coordinator
        .schedule_execution(&execution, &worker_id, DispatchAdmission::Queued)
        .await;
    assert!(result.is_ok(), "schedule_execution must succeed: {result:?}");

    // Positioned via `--revision`, on the merge commit — never `--pr`.
    let goto_revision_calls = cube.goto_revision_calls.lock().await;
    assert_eq!(
        goto_revision_calls.len(),
        1,
        "goto_workspace_revision must be called exactly once for a post-merge review"
    );
    assert_eq!(
        goto_revision_calls[0].1, merge_sha,
        "goto_workspace_revision must receive the batch's merge commit SHA"
    );
    drop(goto_revision_calls);

    let goto_calls = cube.goto_calls.lock().await;
    assert!(
        goto_calls.is_empty(),
        "goto_workspace (the PR-head path) must never be called for a post-merge review: {goto_calls:?}"
    );
    drop(goto_calls);

    // Positioning already happened via goto_workspace_revision — no fresh
    // jj change should be created on top.
    let create_calls = cube.create_calls.lock().await;
    assert!(
        create_calls.is_empty(),
        "create_change must not be called when goto_workspace_revision positions the workspace"
    );
}

#[tokio::test]
async fn review_guide_positions_at_frozen_comparison_head() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product_named(&db, "GuideProduct");
    let root = create_test_chore_manual(&db, product.id, "guide target");
    let head = "b".repeat(40);
    let packet = crate::test_support::review_guide_source_packet(&"a".repeat(40), &head);
    let crate::work::PrSourceCapturePersistOutcome::Stored(capture) = db
        .persist_pr_review_guide_source_capture(&root.id, 1, crate::work::PrSourceCaptureTrigger::Creation, &packet)
        .unwrap()
    else {
        panic!("expected capture")
    };
    let attempt = db
        .create_pr_review_guide_attempt(
            &capture.series_id,
            &capture.comparison_id,
            boss_review_guide::PROMPT_VERSION,
        )
        .unwrap();
    let execution = db
        .create_pr_review_guide_execution(&capture.comparison_id, "acme/widget")
        .unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let mut coord = ExecutionCoordinator::new(db, WorkerPool::new(1), cube.clone(), runner);
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord);
    let worker = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .unwrap();
    coordinator
        .schedule_execution(&execution, &worker, DispatchAdmission::Queued)
        .await
        .unwrap();
    let calls = cube.goto_revision_calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, head);
    assert!(cube.goto_calls.lock().await.is_empty());
    assert!(cube.create_calls.lock().await.is_empty());
}
