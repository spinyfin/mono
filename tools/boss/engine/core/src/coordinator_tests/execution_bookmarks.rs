use super::helpers::*;
use boss_engine_recovery::execution_bookmark::{LocalJj, create};
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn bookmark_creation_failure_is_recorded_before_a_worker_can_start() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Bookmark failure");
    db.reconcile_product_executions(&product.id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_bookmark_create: true,
        ..Default::default()
    });
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(vec![]),
    );
    coordinator.kick();
    let id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    wait_for_execution_status(&db, &id, ExecutionStatus::Failed).await;
    assert!(
        runner.calls.lock().await.is_empty(),
        "a missing recovery ref must prevent spawn"
    );
    assert_eq!(cube.bookmark_calls.lock().await.as_slice(), [id.as_str()]);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    let attention = db.list_attention_items(&id).unwrap();
    assert!(
        attention
            .iter()
            .any(|a| a.kind == crate::execution_bookmark_recovery::CREATE_FAILED
                && a.body_markdown.contains("jj bookmark create failed"))
    );
}

async fn wait_for_stage(
    sink: &crate::dispatch_events::RecordingDispatchEventSink,
    execution_id: &str,
    stage: &str,
) -> Vec<crate::dispatch_events::DispatchEvent> {
    for _ in 0..200 {
        let events = sink.events_for(execution_id).await;
        if events.iter().any(|event| event.stage == stage) {
            return events;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "no `{stage}` event for {execution_id}; got {:?}",
        sink.events_for(execution_id).await
    );
}

/// The dispatch event stream, not just the tracing log, must record a
/// pre-start failure. Without it the timeline stops at `cube_change_created
/// ok`, `bossctl dispatch diagnose` shows no failure, and `stage_stalled`
/// fires on a stage that had already failed.
#[tokio::test]
async fn bookmark_creation_failure_reaches_the_dispatch_event_stream() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Bookmark failure");
    db.reconcile_product_executions(&product.id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_bookmark_create: true,
        ..Default::default()
    });
    let sink = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![])
        .with_dispatch_events(sink.clone()),
    );
    coordinator.kick();
    let id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    let events = wait_for_stage(&sink, &id, "pre_start_retries_exhausted").await;

    let failure = events
        .iter()
        .find(|event| event.stage == "pre_start_failure")
        .expect("pre_start_failure event");
    assert_eq!(failure.outcome, "error");
    assert!(
        failure
            .error_message
            .as_deref()
            .is_some_and(|error| error.contains("jj bookmark create failed")),
        "{failure:?}"
    );
    assert_eq!(
        failure.details["reason"],
        crate::execution_bookmark_recovery::CREATE_FAILED
    );
    assert_eq!(failure.details["will_retry"], false);

    let exhausted = events
        .iter()
        .find(|event| event.stage == "pre_start_retries_exhausted")
        .expect("pre_start_retries_exhausted event");
    assert_eq!(exhausted.outcome, "error");
    assert_eq!(exhausted.details["pre_start_failure_count"], 1);

    // The failure is recorded as the timeline's terminal event, so the
    // stalled-stage detector has nothing to flag — no detector special case.
    let timeline = crate::dispatch_reader::TimelineState::from_events(&events).unwrap();
    assert!(!timeline.is_live(), "failed pre-start stage must not stay live");
    let thresholds = crate::dispatch_reader::StageThresholds::new(Duration::from_secs(1));
    assert!(timeline.stall_to_emit(&id, u128::MAX / 2, &thresholds).is_none());
}

/// A failure that will be retried is recorded too, flagged `will_retry`
/// with its backoff, and is not followed by a terminal "exhausted" event.
#[tokio::test]
async fn retried_pre_start_failure_is_recorded_with_its_backoff() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Bookmark failure");
    db.reconcile_product_executions(&product.id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_bookmark_create: true,
        ..Default::default()
    });
    let sink = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![Duration::from_secs(3600)])
        .with_dispatch_events(sink.clone()),
    );
    coordinator.kick();
    let id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    let events = wait_for_stage(&sink, &id, "pre_start_failure").await;
    let failure = events.iter().find(|event| event.stage == "pre_start_failure").unwrap();
    assert_eq!(failure.outcome, "error");
    assert_eq!(failure.details["will_retry"], true);
    assert_eq!(failure.details["retry_delay_secs"], 3600);
    assert_eq!(failure.details["pre_start_failure_count"], 1);
    assert!(events.iter().all(|event| event.stage != "pre_start_retries_exhausted"));
}

#[tokio::test]
async fn bookkeeping_failure_is_terminal_and_recovery_does_not_record_it_twice() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path.clone()).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Bookkeeping failure");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap().remove(0);
    db.claim_execution_for_dispatch(&execution.id).unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_failure BEFORE UPDATE OF pre_start_failure_count ON work_executions
         BEGIN SELECT RAISE(ABORT, 'bookkeeping unavailable'); END;",
        )
        .unwrap();
    let sink = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_dispatch_events(sink.clone()),
    );
    let original = anyhow!("underlying bookmark error").context("bookmark creation failed");
    let error = coordinator
        .record_start_failure(
            coordinator.clone(),
            &execution,
            "worker-1",
            Some("mono"),
            ("execution_bookmark_create_failed", "Bookmark failed"),
            &original,
        )
        .await
        .unwrap_err();
    // Even if the database recovers, fallback must not count this attempt again.
    connection.execute_batch("DROP TRIGGER reject_failure").unwrap();
    coordinator
        .recover_failed_dispatch(&execution, "worker-1", &error)
        .await;
    let events = sink.events_for(&execution.id).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let failure = &events[0];
    assert_eq!(failure.stage, "pre_start_failure");
    assert_eq!(failure.outcome, "error");
    assert_eq!(failure.worker_id.as_deref(), Some("worker-1"));
    assert_eq!(failure.cube_repo_id.as_deref(), Some("mono"));
    assert_eq!(
        failure.error_message.as_deref(),
        Some("bookmark creation failed: underlying bookmark error")
    );
    assert_eq!(failure.details["reason"], "execution_bookmark_create_failed");
    assert_eq!(failure.details["will_retry"], false);
    assert!(
        failure.details["bookkeeping_error"]
            .as_str()
            .unwrap()
            .contains("bookkeeping unavailable")
    );
    assert!(failure.details.get("pre_start_failure_count").is_none());
    let after = db.get_execution(&execution.id).unwrap();
    assert_eq!(after.pre_start_failure_count, 0);
    assert_eq!(after.status, ExecutionStatus::Ready);
    let timeline = crate::dispatch_reader::TimelineState::from_events(&events).unwrap();
    assert!(!timeline.is_live());
    let thresholds = crate::dispatch_reader::StageThresholds::new(Duration::from_secs(1));
    assert!(
        timeline
            .stall_to_emit(&execution.id, u128::MAX / 2, &thresholds)
            .is_none()
    );
}

#[tokio::test]
async fn sweep_reports_real_unpushed_revisions_independent_of_original_workspace_and_ignores_empty_runs() {
    for original_state in ["released", "removed", "reused", "leased_elsewhere"] {
        for has_work in [false, true] {
            let dir = tempdir().unwrap();
            let path = dir.path().join("boss.db");
            let db = Arc::new(WorkDb::open(path.clone()).unwrap());
            let product = create_test_product(&db);
            let chore = create_test_chore_manual(&db, product.id, "Recover revision");
            let prior = db
                .create_execution(
                    CreateExecutionInput::builder()
                        .created_via("test")
                        .work_item_id(chore.id)
                        .kind(ExecutionKind::RevisionImplementation)
                        .status(ExecutionStatus::Ready)
                        .build(),
                )
                .unwrap();
            db.start_execution_run(
                &prior.id,
                "worker",
                "mono",
                "released-lease",
                "old-workspace",
                "/workspace/removed",
            )
            .unwrap();
            let repo = JjRepo::new(dir.path());
            let record = create(&LocalJj, &repo.worker, &prior.id, "local").await.unwrap();
            db.record_execution_bookmark(&record).unwrap();
            if has_work {
                std::fs::write(repo.worker.join("revision.txt"), "unpushed revision").unwrap();
                JjRepo::run(&repo.worker, &["describe", "-m", "Unpushed revision"]);
            }
            db.mark_execution_orphaned(&prior.id, "worker terminated").unwrap();
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute(
                    "UPDATE work_executions SET finished_at = CAST(unixepoch('now') - 1000 AS TEXT) WHERE id = ?1",
                    [&prior.id],
                )
                .unwrap();
            match original_state {
                "removed" => std::fs::remove_dir_all(&repo.worker).unwrap(),
                "reused" | "leased_elsewhere" => {
                    JjRepo::run(&repo.worker, &["new", "root()", "-m", "Unrelated lease"]);
                    std::fs::write(repo.worker.join("foreign.txt"), "foreign edits").unwrap();
                }
                _ => {}
            }
            let cube = Arc::new(FakeCubeClient::default());
            let coordinator = ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            );
            let reported = crate::abandoned_execution_bookmarks::run_one_pass(&db, &coordinator)
                .await
                .unwrap();
            assert_eq!(reported, usize::from(has_work));
            let attentions = db.list_attention_items(&prior.id).unwrap();
            assert_eq!(attentions.len(), usize::from(has_work));
            if has_work {
                assert_eq!(
                    attentions[0].kind,
                    crate::abandoned_execution_bookmarks::RECOVERABLE_WORK
                );
                assert!(attentions[0].body_markdown.contains(&record.head()));
                crate::abandoned_execution_bookmarks::run_one_pass(&db, &coordinator)
                    .await
                    .unwrap();
                assert_eq!(
                    db.list_attention_items(&prior.id).unwrap().len(),
                    1,
                    "repeated probes must not duplicate attention"
                );
            }
            assert!(cube.status_calls.lock().await.is_empty());
            assert!(cube.lease_calls.lock().await.is_empty());
            if matches!(original_state, "reused" | "leased_elsewhere") {
                assert_eq!(
                    std::fs::read_to_string(repo.worker.join("foreign.txt")).unwrap(),
                    "foreign edits"
                );
            }
        }
    }
}

/// The abandoned-bookmark sweep is the loudness path that still fails a
/// missing pointer: dispatch and resume must not, but the sweep still
/// raises `execution_bookmark_recovery_failed` attention.
#[tokio::test]
async fn missing_pointer_is_reported_instead_of_skipped_by_the_sweep() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path.clone()).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "Missing pointer");
    let prior = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(&prior.id, "worker", "mono", "released", "removed", "/workspace/removed")
        .unwrap();
    db.mark_execution_orphaned(&prior.id, "terminated").unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET finished_at = CAST(unixepoch('now') - 1000 AS TEXT) WHERE id = ?1",
            [&prior.id],
        )
        .unwrap();
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    );
    let failure = crate::abandoned_execution_bookmarks::run_one_pass(&db, &coordinator)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("no engine-created recovery bookmark"));
    let attentions = db.list_attention_items(&prior.id).unwrap();
    assert_eq!(attentions.len(), 1);
    assert_eq!(attentions[0].kind, crate::execution_bookmark_recovery::RECOVERY_FAILED);
    assert!(
        attentions[0]
            .body_markdown
            .contains("no engine-created recovery bookmark")
    );
}
