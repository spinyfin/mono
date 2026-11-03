use super::helpers::*;
use boss_engine_recovery::execution_bookmark::{self as recovery, LocalJj};
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn dispatch_prefers_preserved_work_over_retired_legacy_pointers() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "pr/99", "-r", "@"]);
    JjRepo::run(&repo.repo, &["git", "export"]);
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "remote",
            "add",
            "origin",
            repo.repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    JjRepo::run(&repo.repo, &["git", "fetch"]);
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpushed fix").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker)
        .await
        .unwrap()
        .unwrap();
    db.record_preserved_tip(&prior.id, &tip).unwrap();
    db.retire_execution_bookmark(&prior.id, "legacy pointers retired")
        .unwrap();
    JjRepo::run(
        &repo.repo,
        &[
            "bookmark",
            "delete",
            &record.head(),
            &record.base(),
            &record.publication(),
        ],
    );
    let cube = Arc::new(FakeCubeClient {
        position_real_workspace: true,
        ..FakeCubeClient::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "next".into(),
        dirty_verified: None,
    };
    let recovered = coordinator
        .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", Some(99), Some(99))
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.has_work);
    assert_eq!(recovered.predecessor, prior.id);
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("fix")).unwrap(),
        "unpushed fix"
    );
    assert_eq!(
        JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "change_id"]),
        tip.change_id
    );
    assert!(db.execution_restore_report(&next.id).unwrap().is_some());
}

#[tokio::test]
async fn undecidable_preserved_bookmark_starts_fresh_and_raises_one_attention() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let (db, prior, mut next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "pr/99", "-r", "@"]);
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpushed fix").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker)
        .await
        .unwrap()
        .unwrap();
    db.record_preserved_tip(&prior.id, &tip).unwrap();
    let name = recovery::preserved_name(&prior.id);
    for title in ["First unrelated work", "Second unrelated work"] {
        JjRepo::run(&repo.repo, &["new", "main", "-m", title]);
        std::fs::write(repo.repo.join(title), title).unwrap();
        JjRepo::run(&repo.repo, &["status"]);
    }
    let op = JjRepo::run(&repo.repo, &["op", "log", "-n", "1", "--no-graph", "-T", "id"]);
    for rev in [
        "description(substring:\"First unrelated\")",
        "description(substring:\"Second unrelated\")",
    ] {
        JjRepo::run(
            &repo.repo,
            &["--at-op", &op, "bookmark", "set", &name, "-r", rev, "--allow-backwards"],
        );
    }
    let rev = format!("bookmarks(exact:\"{name}\")");
    let before = JjRepo::run(
        &repo.repo,
        &["log", "--no-graph", "-r", &rev, "-T", "commit_id ++ \"\\n\""],
    );
    assert_eq!(before.lines().count(), 2);
    let cube = Arc::new(FakeCubeClient {
        position_real_workspace: true,
        ..FakeCubeClient::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "next".into(),
        dirty_verified: None,
    };
    for _ in 0..2 {
        assert!(
            coordinator
                .recover_execution_bookmark_for_test(
                    &next,
                    &lease,
                    &coordinator.host_adapter(),
                    "mono",
                    Some(99),
                    Some(99)
                )
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(!repo.replacement.join("fix").exists());
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &["log", "--no-graph", "-r", &rev, "-T", "commit_id ++ \"\\n\""]
        ),
        before
    );
    assert_eq!(
        db.list_attention_items(&prior.id)
            .unwrap()
            .iter()
            .filter(|item| {
                item.kind == boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND
            })
            .count(),
        1
    );
    let calls = cube.goto_calls.lock().await.len();
    next.kind = ExecutionKind::PrReview;
    assert!(
        coordinator
            .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", Some(99), Some(99))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        cube.goto_calls.lock().await.len(),
        calls,
        "reviews never enter recovery"
    );
}

fn export_origin(repo: &JjRepo) {
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "pr/99", "-r", "@"]);
    JjRepo::run(&repo.repo, &["git", "export"]);
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "remote",
            "add",
            "origin",
            repo.repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    JjRepo::run(&repo.repo, &["git", "fetch"]);
}

fn coordinator_for(db: Arc<WorkDb>) -> (ExecutionCoordinator, Arc<FakeCubeClient>) {
    let cube = Arc::new(FakeCubeClient {
        position_real_workspace: true,
        ..FakeCubeClient::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db,
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    );
    (coordinator, cube)
}

#[tokio::test]
async fn self_retry_restages_preserved_work_when_legacy_pointers_are_retired() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let (db, execution, _) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    export_origin(&repo);
    let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("retry-work"), "self-retry unpublished").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker)
        .await
        .unwrap()
        .unwrap();
    db.record_preserved_tip(&execution.id, &tip).unwrap();
    db.retire_execution_bookmark(&execution.id, "legacy pointers retired")
        .unwrap();
    JjRepo::run(
        &repo.repo,
        &[
            "bookmark",
            "delete",
            &record.head(),
            &record.base(),
            &record.publication(),
        ],
    );
    let (coordinator, _) = coordinator_for(db.clone());
    let lease = CubeWorkspaceLease {
        workspace_id: "retry".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "retry".into(),
        dirty_verified: None,
    };
    let recovered = coordinator
        .recover_execution_bookmark_for_test(
            &execution,
            &lease,
            &coordinator.host_adapter(),
            "mono",
            Some(99),
            Some(99),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.has_work);
    assert_eq!(recovered.predecessor, execution.id);
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("retry-work")).unwrap(),
        "self-retry unpublished"
    );
    assert_eq!(
        JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "change_id"]),
        tip.change_id
    );
}

#[tokio::test]
async fn pre_start_teardown_then_retry_preserves_later_unpublished_edits() {
    use crate::workspace_lease_release::{
        DbBookmarkGate, EmptyOccupancy, LeaseReleaseOutcome, PaneStatus, ReleaseEvidence, TerminalCause, apply_release,
    };
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db_path = dir.path().join("boss.db");
    let (db, prior, execution) = super::blocked_workspace::blocked_pair(&db_path);
    export_origin(&repo);
    let prior_record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&prior_record).unwrap();
    std::fs::write(repo.worker.join("inherited"), "inherited unpublished").unwrap();
    let inherited = recovery::preserve(&LocalJj, &prior_record, &repo.worker)
        .await
        .unwrap()
        .unwrap();
    db.record_preserved_tip(&prior.id, &inherited).unwrap();
    let (coordinator, _) = coordinator_for(db.clone());
    let first_lease = CubeWorkspaceLease {
        workspace_id: "first".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "first".into(),
        dirty_verified: None,
    };
    assert!(
        coordinator
            .recover_execution_bookmark_for_test(
                &execution,
                &first_lease,
                &coordinator.host_adapter(),
                "mono",
                Some(99),
                Some(99)
            )
            .await
            .unwrap()
            .unwrap()
            .has_work
    );
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = ?2, cube_workspace_id = ?3, workspace_path = ?4 WHERE id = ?1",
            rusqlite::params![execution.id, "first", "first", repo.replacement.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let record = recovery::create(&LocalJj, &repo.replacement, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "first",
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate { db: &db, adapter: None },
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    let frozen = db.preserved_tip(&execution.id).unwrap().unwrap();
    assert_eq!(frozen.change_id, inherited.change_id);

    let retry = dir.path().join("retry");
    JjRepo::run(
        &repo.repo,
        &[
            "workspace",
            "add",
            retry.to_str().unwrap(),
            "--name",
            "retry",
            "-r",
            "main",
        ],
    );
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = ?2, cube_workspace_id = ?3, workspace_path = ?4, status = 'ready' WHERE id = ?1",
            rusqlite::params![execution.id, "retry", "retry", retry.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let retry_lease = CubeWorkspaceLease {
        workspace_id: "retry".into(),
        workspace_path: retry.clone(),
        lease_id: "retry".into(),
        dirty_verified: None,
    };
    assert!(
        coordinator
            .recover_execution_bookmark_for_test(
                &execution,
                &retry_lease,
                &coordinator.host_adapter(),
                "mono",
                Some(99),
                Some(99)
            )
            .await
            .unwrap()
            .unwrap()
            .has_work
    );
    JjRepo::run(&retry, &["new", "-m", "Later worker commit"]);
    std::fs::write(retry.join("later"), "later unpublished").unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "retry",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate { db: &db, adapter: None },
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed', finished_at = '1' WHERE id = ?1",
            rusqlite::params![execution.id],
        )
        .unwrap();
    let updated = db.preserved_tip(&execution.id).unwrap().unwrap();
    assert_ne!(updated.commit_id, frozen.commit_id);
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &recovery::preserved_name(&execution.id),
                "-T",
                "commit_id",
            ],
        ),
        updated.commit_id
    );
    assert_eq!(
        std::fs::read_to_string(retry.join("later")).unwrap(),
        "later unpublished"
    );

    let successor_path = dir.path().join("successor");
    JjRepo::run(
        &repo.repo,
        &[
            "workspace",
            "add",
            successor_path.to_str().unwrap(),
            "--name",
            "successor",
            "-r",
            "main",
        ],
    );
    let successor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(execution.work_item_id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let successor_lease = CubeWorkspaceLease {
        workspace_id: "successor".into(),
        workspace_path: successor_path.clone(),
        lease_id: "successor".into(),
        dirty_verified: None,
    };
    let recovered = coordinator
        .recover_execution_bookmark_for_test(
            &successor,
            &successor_lease,
            &coordinator.host_adapter(),
            "mono",
            Some(99),
            Some(99),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.has_work);
    assert_eq!(
        std::fs::read_to_string(successor_path.join("later")).unwrap(),
        "later unpublished"
    );
}

#[tokio::test]
async fn missing_preserved_bookmark_with_unresolvable_commit_raises_attention() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    export_origin(&repo);
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpushed fix").unwrap();
    let mut tip = recovery::preserve(&LocalJj, &record, &repo.worker)
        .await
        .unwrap()
        .unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "delete", &recovery::preserved_name(&prior.id)],
    );
    tip.commit_id = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into();
    db.record_preserved_tip(&prior.id, &tip).unwrap();
    let (coordinator, _) = coordinator_for(db.clone());
    let lease = CubeWorkspaceLease {
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "next".into(),
        dirty_verified: None,
    };
    assert!(
        coordinator
            .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", Some(99), Some(99))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.list_attention_items(&prior.id)
            .unwrap()
            .iter()
            .filter(|item| {
                item.kind == boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND
            })
            .count(),
        1
    );
}
