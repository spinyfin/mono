use super::helpers::*;
use crate::work::{FakePrStateChecker, PrOpenState};
use boss_engine_recovery::execution_bookmark::LocalJj;
use boss_engine_recovery_fixtures::create;
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn fresh_dispatch_preserves_and_reports_work_without_legacy_bookmark_failures() {
    use crate::workspace_lease_release::{DbPreservation, Preservation, TerminalCause};
    use boss_engine_recovery::execution_bookmark::{create_from, preserved_name};
    for has_work in [false, true] {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id, "Preserve workspace work");
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let repo = JjRepo::new(dir.path());
        super::recovery::configure_recovery_origin(&repo.repo);
        JjRepo::run(&repo.repo, &["git", "fetch", "--remote", "origin", "--branch", "main"]);
        let record = create_from(&LocalJj, &repo.worker, &execution.id, "local", None, None)
            .await
            .unwrap();
        db.record_execution_bookmark(&record).unwrap();
        db.start_execution_run(
            &execution.id,
            "worker",
            "mono",
            "lease",
            "worker",
            repo.worker.to_str().unwrap(),
        )
        .unwrap();
        let execution = db.get_execution(&execution.id).unwrap();
        if has_work {
            std::fs::write(repo.worker.join("first.txt"), "unpublished first edit").unwrap();
            JjRepo::run(&repo.worker, &["new", "-m", "Second edit"]);
            std::fs::write(repo.worker.join("second.txt"), "unpublished second edit").unwrap();
        }
        assert!(db.list_attention_items(&execution.id).unwrap().is_empty());
        DbPreservation { db: &db, adapter: None }
            .preserve(&execution, TerminalCause::ProvenCompleted)
            .await
            .unwrap();
        assert_eq!(db.preserved_tip(&execution.id).unwrap().is_some(), has_work);
        db.mark_execution_orphaned(&execution.id, "worker terminated").unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET finished_at = CAST(unixepoch('now') - 1000 AS TEXT) WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();
        let coordinator = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner::default()),
        );
        for _ in 0..2 {
            crate::abandoned_execution_bookmarks::run_one_pass(&db, &coordinator)
                .await
                .unwrap();
        }
        let notes = db.list_attention_items(&execution.id).unwrap();
        assert_eq!(notes.len(), usize::from(has_work));
        if has_work {
            assert_eq!(notes[0].kind, crate::abandoned_execution_bookmarks::RECOVERABLE_WORK);
            assert!(notes[0].body_markdown.contains(&preserved_name(&execution.id)));
            for file in ["first.txt", "second.txt"] {
                assert!(
                    JjRepo::run(
                        &repo.repo,
                        &[
                            "file",
                            "show",
                            "-r",
                            &preserved_name(&execution.id),
                            &format!("root:{file}")
                        ]
                    )
                    .contains("unpublished")
                );
            }
            // A successor publishes the preserved work while the teardown count
            // stays frozen on the row; the next scan must resolve, not re-raise.
            let preserved = preserved_name(&execution.id);
            // jj refuses to push undescribed commits; describing keeps change ids.
            JjRepo::run(
                &repo.repo,
                &[
                    "describe",
                    "-m",
                    "published work",
                    &format!("trunk()..bookmarks(exact:\"{preserved}\")"),
                ],
            );
            JjRepo::run(&repo.repo, &["bookmark", "create", "published", "-r", &preserved]);
            JjRepo::run(
                &repo.repo,
                &["git", "push", "--remote", "origin", "-b", "published", "--allow-new"],
            );
            assert!(db.preserved_tip(&execution.id).unwrap().unwrap().count > 0);
            crate::abandoned_execution_bookmarks::run_one_pass(&db, &coordinator)
                .await
                .unwrap();
            let notes = db.list_attention_items(&execution.id).unwrap();
            assert!(
                notes.iter().all(
                    |note| note.kind != crate::abandoned_execution_bookmarks::RECOVERABLE_WORK
                        || note.status == "resolved"
                ),
                "{notes:?}"
            );
        }
    }
}

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
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "a recovery failure never keeps the workspace reserved: nothing ran in it"
    );
    assert_eq!(db.get_execution(&id).unwrap().cube_lease_id.as_deref(), None);
    let attention = db.list_attention_items(&id).unwrap();
    assert!(
        attention
            .iter()
            .all(|a| a.kind != crate::workspace_lease_release::LEASE_UNRELEASED_ATTENTION_KIND)
    );
    assert!(
        attention
            .iter()
            .any(|a| a.kind == crate::execution_bookmark_recovery::CREATE_FAILED
                && a.body_markdown.contains("jj bookmark create failed"))
    );
}

#[tokio::test]
async fn dispatch_snapshots_the_pr_head_before_the_recovery_pointer_is_created() {
    for revision in [false, true] {
        bound_pr_snapshot_dispatch(revision, false).await;
    }
}

#[tokio::test]
async fn failed_pr_head_snapshot_retries_without_creating_an_ancestry_floor() {
    bound_pr_snapshot_dispatch(true, true).await;
}

struct BoundPrFixture {
    dir: tempfile::TempDir,
    db: Arc<WorkDb>,
    id: String,
    head: String,
}

async fn bound_pr_fixture(revision: bool) -> BoundPrFixture {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path.clone()).unwrap());
    seed_local_claude_driver(&db);
    let (_, chore_id) = make_pr_review_fixture(&db, Some("https://github.com/foo/bar/pull/99"));
    let (item, kind) = if revision {
        let task = db
            .create_revision(
                crate::work::CreateRevisionInput::builder()
                    .parent_task_id(chore_id)
                    .description("Review revision")
                    .autostart(false)
                    .created_via("test")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        assert!(task.pr_url.is_none());
        (task.id, ExecutionKind::RevisionImplementation)
    } else {
        (chore_id, ExecutionKind::ChoreImplementation)
    };
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE tasks SET autostart = 1 WHERE id = ?1", [&item])
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item)
                .kind(kind)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let id = execution.id;
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET repo_remote_url = 'https://github.com/foo/bar.git' WHERE id = ?1",
            [&id],
        )
        .unwrap();
    assert!(db.get_execution(&id).unwrap().pr_head_before.is_none());
    assert!(db.get_execution(&id).unwrap().pr_url.is_none());
    let repo = JjRepo::new(dir.path());
    JjRepo::run(
        &repo.worker,
        &["config", "set", "--repo", "revset-aliases.'trunk()'", "root()"],
    );
    // The PR head is an empty, undescribed commit; ancestry discovery skips it.
    let head = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/99", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "pr/99", "-m", "PR checkout"]);
    let ancestry = create(&LocalJj, &repo.worker, "exec_ancestry_probe", "local")
        .await
        .unwrap();
    assert_ne!(ancestry.base_commit.as_deref(), Some(head.as_str()));
    BoundPrFixture { dir, db, id, head }
}

async fn bound_pr_snapshot_dispatch(revision: bool, fail_first: bool) {
    let BoundPrFixture { dir, db, id, head, .. } = bound_pr_fixture(revision).await;
    let cube = Arc::new(
        FakeCubeClient {
            workspace_root: Some(dir.path().to_path_buf()),
            real_bookmarks: true,
            position_real_workspace: true,
            ..Default::default()
        }
        .with_next_workspace_id("worker"),
    );
    let verifier = crate::completion::tests::StubBranchVerifier::ok("pr/99");
    verifier
        .set_head_oid(if fail_first {
            Err("transient PR head fetch failure".into())
        } else {
            Ok(head.clone())
        })
        .await;
    let handler = crate::completion::integration_handler(db.clone(), "https://github.com/foo/bar/pull/99")
        .with_branch_verifier(verifier.clone());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..Default::default()
    });
    let sink = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let mut coordinator = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
        .with_pre_start_retry_delays(vec![Duration::from_secs(1)])
        .with_dispatch_events(sink.clone());
    coordinator.set_execution_started_hook(Arc::new(handler));
    let coordinator = Arc::new(coordinator);
    coordinator.kick();
    if fail_first {
        let events = wait_for_stage(&sink, &id, "pre_start_failure").await;
        let failure = events.iter().find(|e| e.stage == "pre_start_failure").unwrap();
        assert_eq!(failure.details["will_retry"], true);
        assert!(
            failure
                .error_message
                .as_deref()
                .unwrap()
                .contains("transient PR head fetch failure")
        );
        assert!(db.execution_bookmark_optional(&id).unwrap().is_none());
        assert!(db.get_execution(&id).unwrap().pr_head_before.is_none());
        assert!(cube.bookmark_calls.lock().await.is_empty());
        assert!(runner.calls.lock().await.is_empty());
        verifier.set_head_oid(Ok(head.clone())).await;
    }
    wait_for_execution_status(&db, &id, ExecutionStatus::Running).await;
    assert_eq!(
        db.get_execution(&id).unwrap().pr_head_before.as_deref(),
        Some(head.as_str())
    );
    assert_eq!(
        db.execution_bookmark(&id).unwrap().base_commit.as_deref(),
        Some(head.as_str())
    );
}

async fn wait_for_stage(
    sink: &crate::dispatch_events::RecordingDispatchEventSink,
    execution_id: &str,
    stage: &str,
) -> Vec<crate::dispatch_events::DispatchEvent> {
    for _ in 0..500 {
        let events = sink.events_for(execution_id).await;
        if events.iter().any(|event| event.stage == stage) {
            return events;
        }
        sleep(Duration::from_millis(20)).await;
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
        .record_start_failure_for_test(
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
        .recover_failed_dispatch_for_test(&execution, "worker-1", &error)
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
                super::recovery::preserve_work(&db, &prior.id, &repo.worker).await;
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
                assert!(
                    attentions[0]
                        .body_markdown
                        .contains(&boss_engine_recovery::execution_bookmark::preserved_name(&prior.id))
                );
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

/// A run that was never preserved has nothing for the abandoned-work sweep to
/// report, and not having a recorded store is not a recovery failure.
#[tokio::test]
async fn an_execution_without_a_preserved_tip_is_not_reported_or_failed_by_the_sweep() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path.clone()).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "Never preserved");
    let prior = db
        .create_execution(
            CreateExecutionInput::builder()
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
    let reported = crate::abandoned_execution_bookmarks::run_one_pass(&db, &coordinator)
        .await
        .unwrap();
    assert_eq!(reported, 0);
    assert!(db.list_attention_items(&prior.id).unwrap().is_empty());
}

/// The sweep that forgets the preserved references an execution no longer needs.
mod preserved_cleanup_sweep {
    use super::super::helpers::*;
    use crate::preserved_cleanup_sweep::{Backoff, CleanupReport, run_one_pass};
    use crate::work::CreateAttentionItemInput;
    use boss_engine_recovery::execution_bookmark::{LocalJj, preserved_archive_name, preserved_name};
    use boss_engine_recovery_fixtures::create;
    use boss_engine_storage_values::attention_kinds::{
        PRESERVED_WORK_ARCHIVED_ATTENTION_KIND, PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND,
    };
    use boss_engine_test_git::jj::JjRepo;

    const DAY: i64 = 24 * 60 * 60;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: Arc<WorkDb>,
        repo: JjRepo,
        coordinator: ExecutionCoordinator,
        backoff: Backoff,
    }

    fn fixture() -> Fixture {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let repo = JjRepo::new(dir.path());
        super::super::recovery::configure_recovery_origin(&repo.repo);
        JjRepo::run(&repo.repo, &["git", "fetch", "--remote", "origin", "--branch", "main"]);
        let coordinator = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner::default()),
        );
        Fixture {
            _dir: dir,
            db,
            repo,
            coordinator,
            backoff: Backoff::default(),
        }
    }

    impl Fixture {
        fn sql(&self, sql: &str, params: impl rusqlite::Params) {
            self.db.connect().unwrap().execute(sql, params).unwrap();
        }

        fn execution(&self, work_item_id: &str) -> String {
            let execution = self
                .db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(work_item_id)
                        .kind(ExecutionKind::ChoreImplementation)
                        .status(ExecutionStatus::Ready)
                        .build(),
                )
                .unwrap();
            self.db
                .start_execution_run(&execution.id, "worker", "mono", "lease", "workspace", "/workspace")
                .unwrap();
            execution.id
        }

        /// A finished execution whose unpublished edit teardown preserved under a
        /// real `preserved/<execution>` bookmark in the shared store.
        async fn preserved(&self, work_item_id: &str, file: &str) -> String {
            let id = self.execution(work_item_id);
            let record = create(&LocalJj, &self.repo.worker, &id, "local").await.unwrap();
            self.db.record_execution_bookmark(&record).unwrap();
            std::fs::write(self.repo.worker.join(file), "unpublished").unwrap();
            JjRepo::run(&self.repo.worker, &["describe", "-m", file]);
            super::super::recovery::preserve_work(&self.db, &id, &self.repo.worker).await;
            JjRepo::run(&self.repo.worker, &["new", "main"]);
            assert!(self.db.preserved_tip(&id).unwrap().is_some());
            self.db.mark_execution_orphaned(&id, "worker terminated").unwrap();
            id
        }

        fn chore(&self, name: &str) -> String {
            let product = create_test_product(&self.db);
            create_test_chore_manual(&self.db, product.id, name).id
        }

        fn close_chain(&self, chore_id: &str, closed_secs_ago: i64) {
            self.sql(
                "UPDATE tasks SET status = 'done', completed_at = CAST(unixepoch('now') - ?2 AS TEXT) WHERE id = ?1",
                rusqlite::params![chore_id, closed_secs_ago],
            );
        }

        fn exists(&self, name: &str) -> bool {
            !JjRepo::run(
                &self.repo.repo,
                &["bookmark", "list", "-T", "name ++ \"\\n\"", &format!("exact:{name}")],
            )
            .trim()
            .is_empty()
        }

        async fn pass(&mut self) -> CleanupReport {
            run_one_pass(&self.db, &self.coordinator, &mut self.backoff)
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn a_published_successor_releases_its_predecessors_tip_in_one_pass() {
        let mut f = fixture();
        let chore = f.chore("Restaged then published");
        let prior = f.preserved(&chore, "prior.txt").await;
        let successor = f.execution(&chore);
        let record = create(&LocalJj, &f.repo.worker, &successor, "local").await.unwrap();
        f.db.record_execution_bookmark(&record).unwrap();
        f.db.record_bookmark_recovery(&successor, &prior, true).unwrap();
        f.db.create_attention_item(CreateAttentionItemInput {
            execution_id: Some(prior.clone()),
            work_item_id: None,
            kind: crate::abandoned_execution_bookmarks::RECOVERABLE_WORK.to_owned(),
            status: None,
            title: "Unpublished execution work is recoverable".into(),
            body_markdown: "body".into(),
            resolved_at: None,
        })
        .unwrap();

        assert_eq!(
            f.pass().await,
            CleanupReport::default(),
            "the successor has not published"
        );
        assert!(f.exists(&preserved_name(&prior)));

        f.sql(
            "UPDATE work_executions SET pr_head_before = ?3, pr_head_after = ?2 WHERE id = ?1",
            rusqlite::params![successor, "b".repeat(40), "a".repeat(40)],
        );
        let report = f.pass().await;
        assert_eq!(report.superseded, 1, "{report:?}");
        assert_eq!(report.references_forgotten, 1);
        assert!(!f.exists(&preserved_name(&prior)));
        assert!(f.db.preserved_tip(&prior).unwrap().is_none());
        assert!(
            f.db.list_attention_items(&prior)
                .unwrap()
                .iter()
                .all(|item| item.status == "resolved"),
            "the recoverable-work item described a tip that is gone"
        );
        assert_eq!(
            f.pass().await,
            CleanupReport::default(),
            "a second pass has nothing to do"
        );
    }

    #[tokio::test]
    async fn superseded_archives_are_cleaned_after_the_chain_closes() {
        let mut f = fixture();
        let chore = f.chore("Archived variants");
        let prior = f.preserved(&chore, "prior.txt").await;
        let archive = preserved_archive_name(&prior, "aaaa");
        JjRepo::run(
            &f.repo.repo,
            &["bookmark", "create", &archive, "-r", &preserved_name(&prior)],
        );
        f.db.upsert_execution_attention(&prior, PRESERVED_WORK_ARCHIVED_ATTENTION_KIND, "Archived", "body")
            .unwrap();
        let successor = f.execution(&chore);
        let record = create(&LocalJj, &f.repo.worker, &successor, "local").await.unwrap();
        f.db.record_execution_bookmark(&record).unwrap();
        f.db.record_bookmark_recovery(&successor, &prior, true).unwrap();
        f.sql(
            "UPDATE work_executions SET pr_head_before = ?3, pr_head_after = ?2 WHERE id = ?1",
            rusqlite::params![successor, "b".repeat(40), "a".repeat(40)],
        );
        assert_eq!(f.pass().await.superseded, 1);
        assert!(!f.exists(&preserved_name(&prior)));
        assert!(
            f.exists(&archive),
            "unselected variants wait for a human or the retention period"
        );
        f.close_chain(&chore, 30 * DAY);
        assert_eq!(f.pass().await.closed_chain, 1);
        assert!(!f.exists(&archive));
        assert_eq!(f.pass().await, CleanupReport::default());
    }

    #[tokio::test]
    async fn a_chain_closed_past_the_retention_period_forgets_its_tip_and_archives() {
        let mut f = fixture();
        let open = f.chore("Still open");
        let closed = f.chore("Closed long ago");
        let recent = f.chore("Closed yesterday");
        let kept = f.preserved(&open, "open.txt").await;
        let old = f.preserved(&closed, "old.txt").await;
        let fresh = f.preserved(&recent, "recent.txt").await;
        let archive = preserved_archive_name(&old, "aaaa");
        JjRepo::run(
            &f.repo.repo,
            &["bookmark", "create", &archive, "-r", &preserved_name(&old)],
        );
        f.db.upsert_execution_attention(&old, PRESERVED_WORK_ARCHIVED_ATTENTION_KIND, "Archived", "body")
            .unwrap();
        f.close_chain(&closed, 30 * DAY);
        f.close_chain(&recent, DAY);

        let report = f.pass().await;
        assert_eq!(report.closed_chain, 1, "{report:?}");
        assert_eq!(report.references_forgotten, 2);
        assert!(!f.exists(&preserved_name(&old)) && !f.exists(&archive));
        assert!(f.db.preserved_tip(&old).unwrap().is_none());
        assert_eq!(
            f.db.list_attention_items(&old).unwrap()[0].status,
            "resolved",
            "the archived-variants item described references that are gone"
        );
        for survivor in [&kept, &fresh] {
            assert!(f.exists(&preserved_name(survivor)), "{survivor}");
            assert!(f.db.preserved_tip(survivor).unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn an_open_inspection_request_keeps_the_archives_when_the_tip_goes() {
        let mut f = fixture();
        let closed = f.chore("Closed with unsafe work set aside");
        let id = f.preserved(&closed, "old.txt").await;
        let archive = format!(
            "{}unsafe/change/commit",
            boss_engine_recovery::execution_bookmark::preserved_archive_prefix(&id)
        );
        JjRepo::run(
            &f.repo.repo,
            &["bookmark", "create", &archive, "-r", &preserved_name(&id)],
        );
        f.db.upsert_execution_attention(&id, PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND, "Inspect", "body")
            .unwrap();
        f.close_chain(&closed, 30 * DAY);
        assert_eq!(f.pass().await.closed_chain, 1);
        assert!(!f.exists(&preserved_name(&id)));
        assert!(f.exists(&archive), "work a human was asked to inspect is not deleted");
        assert_eq!(f.db.list_attention_items(&id).unwrap()[0].status, "open");
        assert_eq!(f.pass().await, CleanupReport::default());
        f.sql(
            "UPDATE work_attention_items SET status = 'resolved' WHERE execution_id = ?1",
            [&id],
        );
        assert_eq!(f.pass().await.closed_chain, 1);
        assert!(!f.exists(&archive));
        assert_eq!(f.pass().await, CleanupReport::default());
    }

    #[tokio::test]
    async fn an_unreachable_store_keeps_the_tip_and_is_not_retried_every_pass() {
        let mut f = fixture();
        let closed = f.chore("Store on a host that is gone");
        let id = f.preserved(&closed, "old.txt").await;
        f.sql(
            "UPDATE execution_bookmarks SET host_id = 'gone' WHERE execution_id = ?1",
            [&id],
        );
        f.close_chain(&closed, 30 * DAY);
        let first = f.pass().await;
        assert_eq!((first.failed, first.closed_chain), (1, 0), "{first:?}");
        assert!(
            f.db.preserved_tip(&id).unwrap().is_some(),
            "a failure keeps the reference"
        );
        assert!(f.exists(&preserved_name(&id)));
        let second = f.pass().await;
        assert_eq!((second.failed, second.deferred), (0, 1), "{second:?}");
    }

    #[tokio::test]
    async fn a_failing_prefix_longer_than_the_retry_window_cannot_starve_a_healthy_store() {
        use crate::preserved_cleanup_sweep::{RETRY_AFTER, run_one_pass_with};
        use std::time::{Duration, Instant};
        const BATCH: usize = 2;
        const INTERVAL: Duration = Duration::from_secs(5 * 60);
        // More failing records than one retry window's worth of batches.
        let failing_count = BATCH * (RETRY_AFTER.as_secs() / INTERVAL.as_secs()) as usize + BATCH;
        let mut f = fixture();
        let chore = f.chore("Many unreachable stores");
        let template = f.preserved(&chore, "template.txt").await;
        let record = f.db.preservation_source(&template).unwrap().unwrap();
        let tip = f.db.preserved_tip(&template).unwrap().unwrap();
        for _ in 0..failing_count {
            let id = f.execution(&chore);
            let mut record = record.clone();
            record.execution_id = id.clone();
            record.host_id = "gone".into();
            f.db.record_execution_bookmark(&record).unwrap();
            f.db.record_preserved_tip(&id, &tip).unwrap();
            f.db.mark_execution_orphaned(&id, "terminated").unwrap();
            f.sql("UPDATE work_executions SET finished_at = '1' WHERE id = ?1", [&id]);
        }
        let healthy = f.preserved(&chore, "healthy.txt").await;
        f.close_chain(&chore, 30 * DAY);
        let start = Instant::now();
        let mut passes = 0;
        while f.exists(&preserved_name(&healthy)) {
            assert!(passes < 4 * failing_count, "the healthy store was never reached");
            let now = start + INTERVAL * passes as u32;
            run_one_pass_with(&f.db, &f.coordinator, &mut f.backoff, now, BATCH)
                .await
                .unwrap();
            passes += 1;
        }
        assert!(f.db.preserved_tip(&healthy).unwrap().is_none());
    }

    #[tokio::test]
    async fn a_closed_chain_keeps_work_with_an_open_not_restaged_or_abandoned_item() {
        use boss_engine_storage_values::attention_kinds::{
            ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND, PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND,
        };
        let mut f = fixture();
        for kind in [
            PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND,
            ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND,
        ] {
            let chore = f.chore("Unrecovered work");
            let id = f.preserved(&chore, kind).await;
            f.db.upsert_execution_attention(&id, kind, "Unrecovered", "body")
                .unwrap();
            f.close_chain(&chore, 30 * DAY);
            assert_eq!(f.pass().await, CleanupReport::default());
            assert!(f.exists(&preserved_name(&id)));
            assert_eq!(f.db.list_attention_items(&id).unwrap()[0].status, "open");
            f.sql(
                "UPDATE work_attention_items SET status = 'resolved' WHERE execution_id = ?1",
                [&id],
            );
            assert_eq!(f.pass().await.closed_chain, 1);
            assert!(!f.exists(&preserved_name(&id)));
        }
    }
    #[tokio::test]
    async fn a_repository_that_no_longer_exists_has_nothing_to_forget() {
        let mut f = fixture();
        let closed = f.chore("Repository deleted");
        let id = f.preserved(&closed, "old.txt").await;
        f.sql(
            "UPDATE execution_bookmarks SET repo_path = '/nonexistent/boss-cleanup-test' WHERE execution_id = ?1",
            [&id],
        );
        f.close_chain(&closed, 30 * DAY);
        let report = f.pass().await;
        assert_eq!(
            (report.closed_chain, report.failed, report.references_forgotten),
            (1, 0, 0),
            "{report:?}"
        );
        assert!(f.db.preserved_tip(&id).unwrap().is_none());
    }
}
