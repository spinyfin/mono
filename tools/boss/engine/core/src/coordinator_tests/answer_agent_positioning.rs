//! `AnswerAgent` dispatch positioning: a successful `cube workspace goto
//! --pr` skips `create_change`, and a failed goto is non-fatal (fresh
//! `cube change create` fallback, lease kept, run stamped unpositioned).
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;
use crate::work::{CreateCommentInput, PublishReviewGuideOutcome, WorkItemPatch};
use boss_protocol::CommentAnchor;

/// Build an `AnswerAgent` execution bound to a comment on a published
/// review-guide version, whose owning task has an open PR — the only shape
/// [`pr_number_for_workspace_goto`] positions for.
fn make_answer_agent_fixture(db: &WorkDb, pr_url: &str) -> WorkExecution {
    let product = create_product(db);
    let root = create_active_chore(db, &product, "impl");
    db.update_work_item(
        &root,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr_url.to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let (series, comparison) = seed_review_guide_series(db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series, &comparison, "review-guide-v1")
        .unwrap();
    let PublishReviewGuideOutcome::Published(version) = db
        .publish_pr_review_guide_version(&attempt.id, "# Guide\n\nquote", "raw")
        .unwrap()
    else {
        panic!("expected published guide")
    };
    let comment = db
        .create_comment_with_guide_version(
            CreateCommentInput::builder()
                .artifact_kind("pr_review_guide")
                .artifact_id(series.clone())
                .anchor(CommentAnchor {
                    exact: "quote".into(),
                    ..Default::default()
                })
                .body("why retry?")
                .author("user:test")
                .doc_version("hash")
                .plain_text_projection_version(1)
                .build(),
            Some(&version.id),
        )
        .unwrap();
    let run = db
        .create_answer_agent_run(&comment.id, "pr_review_guide", &series, &version.id, 0)
        .unwrap();
    let execution = db
        .create_answer_agent_execution(&comment.id, "https://github.com/acme/widget")
        .unwrap();
    db.bind_answer_agent_run_execution(&run.id, &execution.id).unwrap();
    execution
}

/// A successful goto records the PR in `goto_calls`, skips `create_change`,
/// and stamps the run's `workspace_positioned` to `Some(true)`.
#[tokio::test]
async fn answer_agent_goto_success_positions_and_stamps_true() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);

    let pr_url = "https://github.com/acme/widget/pull/9";
    let execution = make_answer_agent_fixture(&db, pr_url);

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator
        .schedule_execution(&execution, &worker_id, DispatchAdmission::Queued)
        .await;
    assert!(result.is_ok(), "schedule_execution must succeed: {result:?}");

    let goto_calls = cube.goto_calls.lock().await;
    assert_eq!(goto_calls.len(), 1, "goto_workspace must be called exactly once");
    assert_eq!(goto_calls[0].1, 9, "goto_workspace must receive pr=9");
    drop(goto_calls);

    assert!(
        cube.create_calls.lock().await.is_empty(),
        "create_change must not be called when goto positioned the workspace"
    );

    let run = db
        .get_answer_agent_run_by_execution(&execution.id)
        .unwrap()
        .expect("answer_agent_runs row must exist for this execution");
    assert_eq!(run.workspace_positioned, Some(true));
}

/// A failed goto (e.g. the DB-derived `pr_lifecycle == Open` was stale) is
/// non-fatal for `AnswerAgent`: no `cube_workspace_positioning_failed`
/// dispatch event, `create_change` still runs, the lease is kept, and the
/// run is stamped `Some(false)`.
#[tokio::test]
async fn answer_agent_goto_failure_falls_back_without_failing_dispatch() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);

    let pr_url = "https://github.com/acme/widget/pull/9";
    let execution = make_answer_agent_fixture(&db, pr_url);

    let cube = Arc::new(FakeCubeClient {
        fail_goto: true,
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator
        .schedule_execution(&execution, &worker_id, DispatchAdmission::Queued)
        .await;
    assert!(
        result.is_ok(),
        "a failed goto must not fail AnswerAgent dispatch: {result:?}"
    );

    assert_eq!(
        cube.goto_calls.lock().await.len(),
        1,
        "goto_workspace must still have been attempted"
    );
    assert!(
        !cube.create_calls.lock().await.is_empty(),
        "create_change must run as the fresh-checkout fallback after a failed goto"
    );
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "the lease must be kept, not released, after a non-fatal AnswerAgent goto failure"
    );

    let run = db
        .get_answer_agent_run_by_execution(&execution.id)
        .unwrap()
        .expect("answer_agent_runs row must exist for this execution");
    assert_eq!(run.workspace_positioned, Some(false));
}
