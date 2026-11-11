//! Positioning refusals stop dispatch until an explicit launch acknowledges repair.
use super::helpers::*;

fn task(db: &WorkDb, id: &str) -> boss_protocol::Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        _ => panic!("expected task"),
    }
}

#[tokio::test]
async fn deterministic_positioning_refusal_holds_once_and_manual_launch_recovers() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let (product_id, root) = make_pr_review_fixture(&db, Some("https://github.com/spinyfin/mono/pull/99"));
    let item = create_test_chore_manual(&db, product_id.clone(), "Revise PR").id;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?2 WHERE id = ?1",
            rusqlite::params![item, root],
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url("https://github.com/spinyfin/mono/pull/99")
                .build(),
        )
        .unwrap();
    let cube = Arc::new(FakeCubeClient::default());
    cube.positioning_refused.store(true, Ordering::SeqCst);
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..Default::default()
            }),
        )
        .with_dispatch_events(recording.clone()),
    );
    let worker = coordinator
        .pool_for_execution_for_test(&execution)
        .claim_worker(&execution.id, None)
        .await
        .unwrap();
    assert!(
        coordinator
            .schedule_execution_for_test(&execution, &worker, DispatchAdmission::Queued)
            .await
            .is_err()
    );
    let failed = db.get_execution(&execution.id).unwrap();
    assert_eq!(failed.status, ExecutionStatus::Failed);
    assert_eq!(failed.pre_start_failure_count, 1);
    assert!(failed.dispatch_not_before.is_none());
    assert_eq!(cube.goto_calls.lock().await.len(), 1);
    let blocked = task(&db, &item);
    assert_eq!(blocked.status, TaskStatus::Blocked);
    assert_eq!(blocked.blocked_reason.as_deref(), Some("workspace_positioning_refused"));
    assert!(
        blocked
            .blocked_detail
            .as_deref()
            .unwrap()
            .contains("abc123 onto def456")
    );
    let events = recording.events_for(&execution.id).await;
    let failure = events.iter().find(|e| e.stage == "pre_start_failure").unwrap();
    assert_eq!(failure.details["will_retry"], false);
    assert_eq!(failure.details["decision"], "blocked");
    assert_eq!(failure.details["reason"], "workspace_positioning_refused");
    for _ in 0..3 {
        db.reconcile_product_executions(&product_id).unwrap();
        db.reconcile_active_dispatch(|_| false).unwrap();
    }
    assert_eq!(db.list_executions(Some(&item)).unwrap().len(), 1);
    assert!(db.list_dispatch_failed_recovery_candidates(0).unwrap().is_empty());
    let attention = db.list_attention_items_for_work_item(&item).unwrap();
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0].kind, "workspace_positioning_refused");
    assert!(attention[0].body_markdown.contains("abc123 onto def456"));
    assert!(attention[0].body_markdown.contains("Inspect the commits"));
    let report = db.stuck_report(0, i64::MAX / 2, &|_| false).unwrap();
    let row = report.rows.iter().find(|r| r.work_item_id == item).unwrap();
    assert!(row.classification.contains("workspace_positioning_refused"));
    assert_eq!(row.wait_reason.as_deref(), Some("workspace_positioning_refused"));
    assert!(row.wait_label.as_deref().unwrap().contains("abc123 onto def456"));
    let runtime = db.get_task_runtime(&item).unwrap();
    assert_eq!(
        runtime.dispatch_wait_reason.as_deref(),
        Some("workspace_positioning_refused")
    );
    assert!(
        runtime
            .dispatch_wait_label
            .as_deref()
            .unwrap()
            .contains("abc123 onto def456")
    );

    cube.positioning_refused.store(false, Ordering::SeqCst);
    let fresh = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(item.clone())
                .explicit_start(true)
                .build(),
        )
        .unwrap();
    assert_ne!(fresh.id, execution.id);
    assert!(task(&db, &item).blocked_reason.is_none());
    assert!(
        db.list_attention_items_for_work_item(&item)
            .unwrap()
            .iter()
            .all(|a| a.status != "open")
    );
    let repaired = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..Default::default()
        }),
    ));
    let worker = repaired
        .pool_for_execution_for_test(&fresh)
        .claim_worker(&fresh.id, None)
        .await
        .unwrap();
    repaired
        .schedule_execution_for_test(&fresh, &worker, DispatchAdmission::Queued)
        .await
        .unwrap();
    assert_eq!(db.get_execution(&fresh.id).unwrap().status, ExecutionStatus::Running);
}

#[tokio::test]
async fn transient_positioning_failure_keeps_pre_start_backoff() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let (_, item) = make_pr_review_fixture(&db, None);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url("https://github.com/spinyfin/mono/pull/99")
                .build(),
        )
        .unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_first_n_goto: 1,
        ..Default::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_dispatch_events(recording.clone()),
    );
    let worker = coordinator
        .pool_for_execution_for_test(&execution)
        .claim_worker(&execution.id, None)
        .await
        .unwrap();
    assert!(
        coordinator
            .schedule_execution_for_test(&execution, &worker, DispatchAdmission::Queued)
            .await
            .is_err()
    );
    let retry = db.get_execution(&execution.id).unwrap();
    assert_eq!(retry.status, ExecutionStatus::Ready);
    assert_eq!(retry.pre_start_failure_count, 1);
    assert!(retry.dispatch_not_before.is_some());
    assert!(task(&db, &item).blocked_reason.is_none());
    assert!(db.list_attention_items_for_work_item(&item).unwrap().is_empty());
    let events = recording.events_for(&execution.id).await;
    let failure = events.iter().find(|e| e.stage == "pre_start_failure").unwrap();
    assert_eq!(failure.details["will_retry"], true);
    assert_eq!(failure.details["retry_delay_secs"], 5);
}
