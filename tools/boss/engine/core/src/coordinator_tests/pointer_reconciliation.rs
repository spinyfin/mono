use super::helpers::*;
use boss_engine_recovery::execution_bookmark::{self as recovery, LocalJj};
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn task_and_chore_recovery_keeps_verified_pr_files_and_persists_baseline() {
    for kind in [ExecutionKind::TaskImplementation, ExecutionKind::ChoreImplementation] {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        let (db, prior, mut next) = super::blocked_workspace::blocked_pair(&path);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE work_executions SET kind = ?1 WHERE id IN (?2, ?3)",
                rusqlite::params![kind.as_str(), prior.id, next.id],
            )
            .unwrap();
        next.kind = kind;
        let repo = JjRepo::new(dir.path());
        super::recovery::configure_recovery_origin(&repo.repo);
        let mut record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
            .await
            .unwrap();
        record.base_commit = None;
        db.record_execution_bookmark(&record).unwrap();
        std::fs::write(repo.worker.join("pr-only.txt"), "published implementation").unwrap();
        JjRepo::run(&repo.worker, &["describe", "-m", "Published implementation"]);
        JjRepo::run(&repo.worker, &["bookmark", "set", "pr/99", "-r", "@"]);
        JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
        let cube = Arc::new(FakeCubeClient {
            workspace_root: Some(dir.path().to_path_buf()),
            real_bookmarks: true,
            ..Default::default()
        });
        let coordinator = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        );
        let lease = CubeWorkspaceLease {
            dirty_verified: Some(false),
            lease_id: "lease".into(),
            workspace_id: "replacement".into(),
            workspace_path: repo.replacement.clone(),
        };
        let recovered = coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None, Some(99))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            recovered.reconciliation.unwrap().anchor,
            recovery::Anchor::PrHead { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("pr-only.txt")).unwrap(),
            "published implementation"
        );
        let reloaded = db.execution_bookmark(&prior.id).unwrap();
        assert!(reloaded.base_commit.is_some());
        // Retry after pointers were reconciled but before the new run starts.
        // No reconciliation is needed now; the recorded PR still owns positioning.
        coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None, Some(99))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("pr-only.txt")).unwrap(),
            "published implementation"
        );
        JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
        assert!(recovery::diff(&LocalJj, &reloaded).await.unwrap().is_empty());
        assert!(
            db.record_execution_bookmark(&reloaded).is_err(),
            "live duplicates must fail"
        );
        db.retire_execution_bookmark(&prior.id, "test retirement").unwrap();
        db.record_execution_bookmark(&reloaded).unwrap();
    }
}

#[tokio::test]
async fn checkout_of_predecessor_publication_cannot_hide_unpublished_work() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("published.txt"), "published").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Published"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/99", "-r", "@"]);
    JjRepo::run(&repo.repo, &["new", &record.base(), "-m", "Unpublished publication"]);
    std::fs::write(repo.repo.join("unpublished.txt"), "unique work").unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", &record.publication(), "-r", "@", "--allow-backwards"],
    );
    let before = recovery::snapshot_pointers(&LocalJj, &record).await.unwrap();
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        real_bookmarks: true,
        goto_reset_bookmark: Some(record.publication()),
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(db, WorkerPool::new(1), cube, Arc::new(FakeExecutionRunner::default()));
    let lease = CubeWorkspaceLease {
        dirty_verified: Some(false),
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
    };
    let error = coordinator
        .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", Some(99), Some(99))
        .await
        .unwrap_err();
    assert_eq!(
        recovery::pointer_integrity_kind(&error),
        Some(recovery::PointerIntegrityKind::NotContained)
    );
    assert_eq!(recovery::snapshot_pointers(&LocalJj, &record).await.unwrap(), before);
}

#[tokio::test]
async fn closed_pr_does_not_prevent_retiring_a_missing_baseline() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    let mut record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    record.base_commit = None;
    db.record_execution_bookmark(&record).unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        real_bookmarks: true,
        closed_pr: true,
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        dirty_verified: Some(false),
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
    };
    assert!(
        coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None, Some(99))
            .await
            .unwrap()
            .is_none()
    );
    assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_none());
}

#[tokio::test]
async fn rewrite_checkpoint_survives_baseline_loss_and_record_reload() {
    let dir = tempdir().unwrap();
    let (db, prior, _) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "main", "-m", "PR implementation"]);
    std::fs::write(repo.worker.join("pr.txt"), "implementation").unwrap();
    JjRepo::run(&repo.worker, &["new", "-m", "Revision"]);
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("revision.txt"), "revision").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "New upstream"]);
    std::fs::write(repo.repo.join("upstream.txt"), "upstream").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "new-upstream", "-r", "@"]);
    JjRepo::run(&repo.worker, &["rebase", "-b", &record.head(), "-d", "new-upstream"]);
    db.checkpoint_rewritten_baselines(&prior.work_item_id).await.unwrap();
    let reloaded = db.execution_bookmark(&prior.id).unwrap();
    assert_ne!(reloaded.base_commit, record.base_commit);
    JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
    let patch = recovery::diff(&LocalJj, &reloaded).await.unwrap();
    assert!(patch.contains("revision.txt"));
    assert!(!patch.contains("pr.txt"));
}
