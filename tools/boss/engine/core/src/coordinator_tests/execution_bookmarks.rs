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
