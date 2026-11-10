use super::helpers::*;
use crate::conflict_watch::on_conflict_detected;
use crate::merge_poller::{OpenPrStatus, PrLifecycleProbe, PrLifecycleState, PrReviewState};
use crate::work::{PendingMergeCheck, PrOpenState, StaticPrStateChecker};

#[tokio::test]
async fn scheduling_cube_refusal_survives_watcher_probes_and_recovers() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let repo = "git@github.com:foo/bar.git";
    let product = create_test_product_with_repo(&db, "Conflicts", Some(repo));
    let parent = create_test_chore_manual(&db, product.id.clone(), "Conflicting PR");
    let pr = "https://github.com/foo/bar/pull/42";
    db.update_work_item(
        &parent.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let candidate = PendingMergeCheck {
        work_item_id: parent.id.clone(),
        product_id: product.id.clone(),
        pr_url: pr.into(),
    };
    let probe = PrLifecycleProbe::builder()
        .url(pr)
        .state(PrLifecycleState::Open(OpenPrStatus::conflict_only()))
        .head_ref_oid("head")
        .base_ref_oid("base")
        .head_ref_name("feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build();
    let checker = StaticPrStateChecker(PrOpenState::Open);
    let publisher = RecordingPublisher::default();
    // Worker fallback is still allowed when mechanical resolution is disabled.
    assert!(on_conflict_detected(&db, &publisher, None, &checker, &candidate, &probe).await);
    let attempt = db
        .latest_conflict_resolution_for_work_item(&parent.id)
        .unwrap()
        .unwrap();
    let revision = attempt.revision_task_id.clone().unwrap();
    // The revision's creation-time execution is superseded by the one below.
    for queued in db.list_executions(Some(&revision)).unwrap() {
        if !queued.status.is_terminal() {
            db.cancel_execution(&queued.id).unwrap();
        }
    }
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&revision)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(repo)
                .pr_url(pr)
                .build(),
        )
        .unwrap();
    let diagnostic = "cube cannot move pr/42: cube will not pick a side with unique content; inspect jj log; jj bookmark set pr/42 -r head --allow-backwards";
    let cube = Arc::new(FakeCubeClient {
        goto_cube_error: Some(diagnostic.into()),
        ..Default::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![Duration::from_secs(60); 3]),
    );
    let worker = coordinator.worker_pool().claim_worker(&exec.id, None).await.unwrap();
    let result = coordinator
        .schedule_execution_for_test(&exec, &worker, DispatchAdmission::Queued)
        .await;
    assert!(result.unwrap_err().to_string().contains("unique content"));
    let failed = db.get_execution(&exec.id).unwrap();
    assert_eq!(failed.status, ExecutionStatus::Failed);
    assert_eq!(failed.pre_start_failure_count, 1, "no identical scheduling retries");
    for _ in 0..8 {
        on_conflict_detected(&db, &publisher, Some(cube.as_ref()), &checker, &candidate, &probe).await;
    }
    let stopped = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert!(crate::work::conflict_workspace_failed(&stopped));
    assert_eq!(stopped.revision_task_id.as_deref(), Some(revision.as_str()));
    let WorkItem::Task(retained) = db.get_work_item(&revision).unwrap() else {
        panic!()
    };
    assert_eq!(retained.status, TaskStatus::Todo);
    assert_eq!(
        retained.dispatch_failed_reason.as_deref(),
        Some("cube_workspace_positioning_failed")
    );
    assert!(retained.dispatch_failed_error.unwrap().contains(diagnostic));
    assert!(retained.deleted_at.is_none());
    let WorkItem::Chore(blocked) = db.get_work_item(&parent.id).unwrap() else {
        panic!()
    };
    assert!(blocked.blocked_detail.unwrap().contains(diagnostic));
    let items = db.list_attention_items_for_work_item(&parent.id).unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].body_markdown.contains(diagnostic));
    assert_eq!(
        db.list_conflict_resolutions(None, &[], Some(&parent.id), None)
            .unwrap()
            .len(),
        1
    );

    let repaired = Arc::new(FakeCubeClient::default());
    assert!(on_conflict_detected(&db, &publisher, Some(repaired.as_ref()), &checker, &candidate, &probe).await);
    assert!(on_conflict_detected(&db, &publisher, None, &checker, &candidate, &probe).await);
    let resumed = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    let next_revision = resumed.revision_task_id.unwrap();
    assert_ne!(next_revision, revision);
    let next = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(next_revision)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(repo)
                .pr_url(pr)
                .build(),
        )
        .unwrap();
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        repaired,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..Default::default()
        }),
    ));
    let worker = coordinator.worker_pool().claim_worker(&next.id, None).await.unwrap();
    coordinator
        .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
        .await
        .unwrap();
    assert_eq!(db.get_execution(&next.id).unwrap().status, ExecutionStatus::Running);
    assert_eq!(
        db.list_attention_items_for_work_item(&parent.id).unwrap()[0].status,
        "resolved"
    );
}

const PR_URL: &str = "https://github.com/foo/bar/pull/42";
const REPO: &str = "git@github.com:foo/bar.git";

/// A parent PR with a conflict attempt, its revision, and a ready execution
/// for that revision, with the parent optionally held by a foreign blocker.
async fn conflict_revision_fixture(db: &Arc<WorkDb>, blocker: Option<&str>) -> (String, String, String, String) {
    seed_local_claude_driver(db);
    let product = create_test_product_with_repo(db, "Conflicts", Some(REPO));
    let parent = create_test_chore_manual(db, product.id.clone(), "Conflicting PR");
    db.update_work_item(
        &parent.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(PR_URL.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let candidate = PendingMergeCheck {
        work_item_id: parent.id.clone(),
        product_id: product.id.clone(),
        pr_url: PR_URL.into(),
    };
    let probe = PrLifecycleProbe::builder()
        .url(PR_URL)
        .state(PrLifecycleState::Open(OpenPrStatus::conflict_only()))
        .head_ref_oid("head")
        .base_ref_oid("base")
        .head_ref_name("feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build();
    let checker = StaticPrStateChecker(PrOpenState::Open);
    on_conflict_detected(db, &RecordingPublisher::default(), None, &checker, &candidate, &probe).await;
    let attempt = db
        .latest_conflict_resolution_for_work_item(&parent.id)
        .unwrap()
        .unwrap();
    let revision = attempt.revision_task_id.clone().unwrap();
    for queued in db.list_executions(Some(&revision)).unwrap() {
        if !queued.status.is_terminal() {
            db.cancel_execution(&queued.id).unwrap();
        }
    }
    if let Some(reason) = blocker {
        db.update_work_item(
            &parent.id,
            WorkItemPatch {
                status: Some("blocked".into()),
                blocked_reason: Some(reason.into()),
                ..Default::default()
            },
        )
        .unwrap();
    }
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&revision)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(REPO)
                .pr_url(PR_URL)
                .build(),
        )
        .unwrap();
    (parent.id, attempt.id, revision, exec.id)
}

#[tokio::test]
async fn scheduling_refusal_under_a_foreign_blocker_keeps_the_blocker_and_shows_the_stop() {
    for blocker in ["dependency", "review_feedback"] {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let (parent, attempt, _revision, exec_id) = conflict_revision_fixture(&db, Some(blocker)).await;
        let exec = db.get_execution(&exec_id).unwrap();
        let diagnostic = "cube will not pick a side with unique content; --allow-backwards";
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                Arc::new(FakeCubeClient {
                    goto_cube_error: Some(diagnostic.into()),
                    ..Default::default()
                }),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![Duration::from_secs(60); 3]),
        );
        let worker = coordinator.worker_pool().claim_worker(&exec.id, None).await.unwrap();
        assert!(
            coordinator
                .schedule_execution_for_test(&exec, &worker, DispatchAdmission::Queued)
                .await
                .is_err()
        );
        assert!(crate::work::conflict_workspace_failed(
            &db.get_conflict_resolution(&attempt).unwrap().unwrap()
        ));
        let WorkItem::Chore(task) = db.get_work_item(&parent).unwrap() else {
            panic!()
        };
        assert_eq!(
            task.blocked_reason.as_deref(),
            Some(blocker),
            "the foreign blocker is kept"
        );
        // The card renders the open conflict-stop attention for these kinds.
        let items = db.list_attention_items_for_work_item(&parent).unwrap();
        let stop: Vec<_> = items
            .iter()
            .filter(|item| {
                item.kind == crate::work::CONFLICT_REMEDIATION_STOPPED_ATTENTION_KIND && item.status == "open"
            })
            .collect();
        assert_eq!(stop.len(), 1, "{blocker}: {items:?}");
        assert!(stop[0].body_markdown.contains(diagnostic));
    }
}

#[tokio::test]
async fn lease_stage_cube_error_retries_and_fails_the_attempt_only_when_retries_are_exhausted() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (parent, attempt, _revision, exec_id) = conflict_revision_fixture(&db, None).await;
    let exec = db.get_execution(&exec_id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_lease_with_cube_cli_error: true,
        ..Default::default()
    });
    let build = |delays: Vec<Duration>| {
        Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(delays),
        )
    };
    // One retry is available: the first lease failure is not terminal.
    let coordinator = build(vec![Duration::from_secs(60)]);
    let worker = coordinator.worker_pool().claim_worker(&exec.id, None).await.unwrap();
    let _ = coordinator
        .schedule_execution_for_test(&exec, &worker, DispatchAdmission::Queued)
        .await;
    let retried = db.get_execution(&exec.id).unwrap();
    assert_ne!(
        retried.status,
        ExecutionStatus::Failed,
        "the first lease failure is retried"
    );
    let row = db.get_conflict_resolution(&attempt).unwrap().unwrap();
    assert!(
        !crate::work::conflict_workspace_failed(&row),
        "attempt is not failed: {row:?}"
    );
    assert!(db.list_attention_items_for_work_item(&parent).unwrap().is_empty());

    // The retry budget is spent: the same failure now fails the attempt.
    let coordinator = build(Vec::new());
    let worker = coordinator.worker_pool().claim_worker(&exec.id, None).await.unwrap();
    let _ = coordinator
        .schedule_execution_for_test(&retried, &worker, DispatchAdmission::Queued)
        .await;
    assert_eq!(db.get_execution(&exec.id).unwrap().status, ExecutionStatus::Failed);
    let row = db.get_conflict_resolution(&attempt).unwrap().unwrap();
    assert!(crate::work::conflict_workspace_failed(&row), "{row:?}");
}
