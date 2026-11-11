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
    // Cube's own message, with no JSON envelope around it.
    assert_eq!(
        blocked.blocked_detail.as_deref(),
        Some(
            "Workspace blocked: Cannot position abc123 onto def456: unpublished content. Inspect the commits and resolve before launching."
        )
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
    assert!(!attention[0].body_markdown.contains("error_kind"));
    assert!(!attention[0].body_markdown.contains("Cube command failed"));
    let report = db.stuck_report(0, i64::MAX / 2, &|_| false).unwrap();
    let row = report.rows.iter().find(|r| r.work_item_id == item).unwrap();
    assert!(row.classification.contains("workspace_positioning_refused"));
    assert_eq!(row.wait_reason.as_deref(), Some("workspace_positioning_refused"));
    assert!(!row.wait_label.as_deref().unwrap().contains("error_kind"));
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
            .starts_with("Workspace blocked: Cannot position abc123 onto def456")
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

fn refused_revision_fixture(db: &Arc<WorkDb>) -> (String, WorkExecution) {
    let (product_id, root) = make_pr_review_fixture(db, Some("https://github.com/spinyfin/mono/pull/99"));
    let item = create_test_chore_manual(db, product_id, "Revise PR").id;
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
                .status(ExecutionStatus::Claimed)
                .pr_url("https://github.com/spinyfin/mono/pull/99")
                .build(),
        )
        .unwrap();
    db.record_workspace_positioning_refusal(
        &execution.id,
        "worker",
        None,
        "Cube command failed: refused",
        "PR 99 is MERGED",
    )
    .unwrap();
    (item, execution)
}

fn recheck_coordinator(db: &Arc<WorkDb>) -> Arc<ExecutionCoordinator> {
    Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    ))
}

fn fixed_probe(accepts: bool) -> crate::positioning_refusal_recheck_sweep::PositioningProbe {
    Arc::new(move |_| Box::pin(async move { Ok(accepts) }))
}

#[tokio::test]
async fn recheck_sweep_restarts_a_held_item_on_its_own_once_the_refusal_clears() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let (item, _) = refused_revision_fixture(&db);
    let coordinator = recheck_coordinator(&db);
    assert_eq!(
        task(&db, &item).blocked_reason.as_deref(),
        Some("workspace_positioning_refused")
    );

    // While cube would still refuse, the probe is read-only: no execution, hold intact.
    let outcome = crate::positioning_refusal_recheck_sweep::run_one_pass(&db, &coordinator, &fixed_probe(false)).await;
    assert_eq!((outcome.held, outcome.cleared), (1, 0));
    assert_eq!(db.list_executions(Some(&item)).unwrap().len(), 1);
    assert_eq!(task(&db, &item).status, TaskStatus::Blocked);
    assert_eq!(
        db.list_attention_items_for_work_item(&item)
            .unwrap()
            .iter()
            .filter(|a| a.status == "open")
            .count(),
        1
    );

    // Once it accepts, the sweep clears hold and attention and mints the next execution
    // without an explicit launch.
    let outcome = crate::positioning_refusal_recheck_sweep::run_one_pass(&db, &coordinator, &fixed_probe(true)).await;
    assert_eq!((outcome.cleared, outcome.restarted), (1, 1));
    assert!(task(&db, &item).blocked_reason.is_none());
    assert_eq!(db.list_executions(Some(&item)).unwrap().len(), 2);
    assert!(
        db.list_attention_items_for_work_item(&item)
            .unwrap()
            .iter()
            .all(|a| a.status != "open")
    );
    assert!(db.list_positioning_holds().unwrap().is_empty());
}

#[tokio::test]
async fn review_phase_refusal_keeps_in_review_and_rejects_automatic_starts() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (product_id, _) = make_pr_review_fixture(&db, None);
    let item = create_test_chore_manual(&db, product_id, "Reviewed").id;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/spinyfin/mono/pull/5' WHERE id = ?1",
            rusqlite::params![item],
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Claimed)
                .pr_url("https://github.com/spinyfin/mono/pull/5")
                .build(),
        )
        .unwrap();
    db.record_workspace_positioning_refusal(
        &execution.id,
        "worker",
        None,
        "Cube command failed: refused",
        "PR 5 is MERGED",
    )
    .unwrap();

    assert_eq!(task(&db, &item).status, TaskStatus::InReview);
    assert!(task(&db, &item).blocked_reason.is_none());
    assert_review_phase_hold_surfaces(&db, &item, "PR 5 is MERGED");
    assert!(
        db.list_chores_pending_merge_check()
            .unwrap()
            .iter()
            .any(|p| p.work_item_id == item)
    );
    assert!(
        db.request_execution(RequestExecutionInput::builder().work_item_id(item.clone()).build())
            .is_err()
    );
    assert_eq!(db.list_executions(Some(&item)).unwrap().len(), 1);

    // The recheck lowers only the hold: the item stays in review.
    let coordinator = recheck_coordinator(&db);
    let outcome = crate::positioning_refusal_recheck_sweep::run_one_pass(&db, &coordinator, &fixed_probe(true)).await;
    assert_eq!((outcome.cleared, outcome.restarted), (1, 0));
    assert_eq!(task(&db, &item).status, TaskStatus::InReview);
    assert!(task(&db, &item).blocked_reason.is_none());
    assert!(db.list_positioning_holds().unwrap().is_empty());
    assert_review_phase_hold_cleared(&db, &item);
}

#[tokio::test]
async fn review_hold_probe_uses_the_task_pr_when_the_execution_has_none() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let (product_id, _) = make_pr_review_fixture(&db, None);
    let item = create_test_chore_manual(&db, product_id, "Reviewed without execution PR").id;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/spinyfin/mono/pull/5' WHERE id = ?1",
            rusqlite::params![item],
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Claimed)
                .build(),
        )
        .unwrap();
    assert!(execution.pr_url.is_none());
    db.record_workspace_positioning_refusal(
        &execution.id,
        "worker",
        None,
        "Cube command failed: refused",
        "PR 5 is MERGED",
    )
    .unwrap();

    let holds = db.list_positioning_holds().unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(
        holds[0].pr_url.as_deref(),
        Some("https://github.com/spinyfin/mono/pull/5")
    );
    assert_review_phase_hold_surfaces(&db, &item, "PR 5 is MERGED");

    let coordinator = recheck_coordinator(&db);
    let outcome = crate::positioning_refusal_recheck_sweep::run_one_pass(&db, &coordinator, &fixed_probe(true)).await;
    assert_eq!((outcome.cleared, outcome.restarted), (1, 0));
    assert_eq!(task(&db, &item).status, TaskStatus::InReview);
    assert!(db.list_positioning_holds().unwrap().is_empty());
    assert_review_phase_hold_cleared(&db, &item);
}

#[tokio::test]
async fn recheck_sweep_does_not_reopen_a_retired_spawning_attempt() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let (product_id, root) = make_pr_review_fixture(&db, Some("https://github.com/spinyfin/mono/pull/99"));
    let item = create_test_chore_manual(&db, product_id.clone(), "Fix CI").id;
    let now = boss_engine_utils::epoch_time::now_epoch_secs().to_string();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO conflict_resolutions
                 (id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch, status, created_at)
             VALUES ('crz_retired', ?1, ?2, 'https://github.com/spinyfin/mono/pull/99', 99, 'feat', 'main', 'failed', ?3)",
            rusqlite::params![product_id, root, now],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?2, created_via = 'merge-conflict:crz_retired' WHERE id = ?1",
            rusqlite::params![item, root],
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Claimed)
                .pr_url("https://github.com/spinyfin/mono/pull/99")
                .build(),
        )
        .unwrap();
    db.record_workspace_positioning_refusal(
        &execution.id,
        "worker",
        None,
        "Cube command failed: refused",
        "PR 99 is MERGED",
    )
    .unwrap();

    let coordinator = recheck_coordinator(&db);
    let outcome = crate::positioning_refusal_recheck_sweep::run_one_pass(&db, &coordinator, &fixed_probe(true)).await;
    assert_eq!(outcome.cleared, 1);
    let attempt_status: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT status FROM conflict_resolutions WHERE id = 'crz_retired'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attempt_status, "failed");
}

#[test]
fn refusal_without_a_pr_tells_the_operator_to_launch_manually() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (product_id, _) = make_pr_review_fixture(&db, None);
    let item = create_test_chore_manual(&db, product_id, "No PR").id;
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Claimed)
                .build(),
        )
        .unwrap();
    db.record_workspace_positioning_refusal(
        &execution.id,
        "worker",
        None,
        "Cube command failed: refused",
        "unpublished content",
    )
    .unwrap();
    let holds = db.list_positioning_holds().unwrap();
    assert_eq!(holds.len(), 1);
    assert!(holds[0].pr_url.is_none());
    let attention = db.list_attention_items_for_work_item(&item).unwrap();
    assert!(
        attention[0]
            .body_markdown
            .contains("No pull request is bound, so this hold cannot clear automatically"),
        "{}",
        attention[0].body_markdown
    );
    assert!(
        attention[0]
            .body_markdown
            .contains("launch the item manually once the reported state is resolved"),
        "{}",
        attention[0].body_markdown
    );
}

fn assert_review_phase_hold_surfaces(db: &WorkDb, item: &str, message: &str) {
    let runtime = db.get_task_runtime(item).unwrap();
    assert_eq!(
        runtime.dispatch_wait_reason.as_deref(),
        Some("workspace_positioning_refused")
    );
    let label = runtime.dispatch_wait_label.as_deref().unwrap();
    assert!(label.starts_with("Workspace blocked:"), "{label}");
    assert!(label.contains(message), "{label}");
    let report = db.stuck_report(0, i64::MAX / 2, &|_| false).unwrap();
    let row = report.rows.iter().find(|row| row.work_item_id == item).unwrap();
    assert!(
        row.classification.contains("workspace_positioning_refused"),
        "{}",
        row.classification
    );
    assert_eq!(row.wait_reason.as_deref(), Some("workspace_positioning_refused"));
    assert!(row.wait_label.as_deref().unwrap().contains(message));
}

fn assert_review_phase_hold_cleared(db: &WorkDb, item: &str) {
    let runtime = db.get_task_runtime(item).unwrap();
    assert_ne!(
        runtime.dispatch_wait_reason.as_deref(),
        Some("workspace_positioning_refused")
    );
    let report = db.stuck_report(0, i64::MAX / 2, &|_| false).unwrap();
    if let Some(row) = report.rows.iter().find(|row| row.work_item_id == item) {
        assert!(
            !row.classification.contains("workspace_positioning_refused"),
            "{}",
            row.classification
        );
        assert_ne!(row.wait_reason.as_deref(), Some("workspace_positioning_refused"));
    }
}
