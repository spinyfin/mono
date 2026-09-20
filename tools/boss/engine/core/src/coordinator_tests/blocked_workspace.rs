use super::helpers::*;

fn blocked_pair(path: &std::path::Path) -> (Arc<WorkDb>, WorkExecution, WorkExecution) {
    let db = Arc::new(WorkDb::open(path.to_path_buf()).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "Blocked revision");
    let prior = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&chore.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &prior.id,
        "worker",
        "mono",
        "lease-old",
        "workspace-old",
        "/tmp/workspace-old",
    )
    .unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'blocked' WHERE id = ?1",
            [&prior.id],
        )
        .unwrap();
    db.record_worker_failure(&prior.id, "needs a decision").unwrap();
    let prior = db.get_execution(&prior.id).unwrap();
    let next = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .preferred_workspace_id("workspace-old")
                .allow_dirty(true)
                .prefer_is_soft(true)
                .build(),
        )
        .unwrap();
    (db, prior, next)
}

#[tokio::test]
async fn blocked_revision_leases_clean_scratch_regardless_of_prior_lease_identity() {
    for scenario in [
        "valid",
        "leased",
        "clean",
        "unknown-dirty",
        "foreign",
        "legacy",
        "missing-identity",
    ] {
        let dir = tempdir().unwrap();
        let (db, prior, next) = blocked_pair(&dir.path().join("boss.db"));
        let last_task = match scenario {
            "foreign" => Some("exec_foreign same name".to_owned()),
            "legacy" => Some("revision_implementation Blocked revision".to_owned()),
            "missing-identity" => None,
            _ => Some(format!("{} revision_implementation Blocked revision", prior.id)),
        };
        let cube = Arc::new(FakeCubeClient {
            fail_lease_when_prefer_set: scenario == "leased",
            dirty_verified: match scenario {
                "clean" => Some(false),
                "unknown-dirty" => None,
                _ => Some(true),
            },
            recovery_status: Some(
                CubeWorkspaceStatus::builder()
                    .workspace_id("workspace-old")
                    .workspace_path(PathBuf::from("/tmp/workspace-old"))
                    .state("leased")
                    .lease_id("lease-1")
                    .maybe_last_task(last_task)
                    .build(),
            ),
            ..FakeCubeClient::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
        let repo = CubeRepoHandle { repo_id: "mono".into() };
        let lease = coordinator
            .lease_workspace_with_fallback(&next, "worker", &repo, "task", &coordinator.host_adapter)
            .await
            .unwrap();
        let calls = cube.lease_calls.lock().await;
        assert!(
            calls[0].2.is_none(),
            "{scenario}: recovery must not request the old workspace"
        );
        assert!(!calls[0].3);
        assert_ne!(lease.workspace_id, "workspace-old", "{scenario}");
        assert_eq!(calls.len(), 1, "{scenario}");
        assert!(
            cube.status_calls.lock().await.is_empty(),
            "lease history is not recovery provenance"
        );
        assert!(cube.release_calls.lock().await.is_empty());
    }
}

#[tokio::test]
async fn blocked_revision_retry_ignores_old_workspace_markers() {
    // Legacy markers cannot establish shared-store provenance or pin a lease.
    use boss_engine_recovery::recovery_apply::{RecoveryReport, RecoverySource};
    let dir = tempdir().unwrap();
    let (db, prior, next) = blocked_pair(&dir.path().join("boss.db"));
    let workspace = dir.path().join("workspace-old");
    std::fs::create_dir_all(&workspace).unwrap();
    // Simulate cube's `last_task` after a deferral release already
    // overwrote it with this (replacement) execution's own label —
    // the exact corruption described above.
    let cube = Arc::new(FakeCubeClient {
        dirty_verified: Some(true),
        workspace_root: Some(dir.path().to_path_buf()),
        recovery_status: Some(
            CubeWorkspaceStatus::builder()
                .workspace_id("workspace-old")
                .workspace_path(workspace.clone())
                .state("leased")
                .lease_id("lease-1")
                .last_task(format!("{} revision_implementation Blocked revision", next.id))
                .build(),
        ),
        ..FakeCubeClient::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    ));
    // Even an execution-matching legacy marker must not affect scratch selection.
    RecoveryReport {
        for_execution_id: next.id.clone(),
        from_execution_id: prior.id.clone(),
        source: RecoverySource::BlockedInPlace,
        applied: None,
        patch_error: None,
    }
    .write(&workspace)
    .unwrap();
    let repo = CubeRepoHandle { repo_id: "mono".into() };
    let lease = coordinator
        .lease_workspace_with_fallback(&next, "worker", &repo, "task", &coordinator.host_adapter)
        .await
        .unwrap();
    assert_ne!(lease.workspace_id, "workspace-old");
    let calls = cube.lease_calls.lock().await;
    assert_eq!(calls.len(), 1, "must directly request clean scratch");
    assert!(cube.release_calls.lock().await.is_empty());
}

#[tokio::test]
async fn blocked_recovery_missing_bookmark_yields_none_and_dispatch_proceeds() {
    for dirty_verified in [None, Some(false), Some(true)] {
        let dir = tempdir().unwrap();
        let (db, prior, next) = blocked_pair(&dir.path().join("boss.db"));
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db,
                WorkerPool::new(1),
                Arc::new(FakeCubeClient::default()),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_dispatch_events(recording.clone()),
        );
        let lease = CubeWorkspaceLease {
            lease_id: "lease-new".into(),
            workspace_id: "workspace-old".into(),
            workspace_path: dir.path().to_path_buf(),
            dirty_verified,
        };
        let recovered = coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter)
            .await
            .unwrap();
        assert_eq!(recovered, None, "missing predecessor bookmark is nothing to recover");
        let events = recording.events_for(&next.id).await;
        let skipped = events
            .iter()
            .find(|e| e.stage == "workspace_recovery")
            .expect("missing bookmark must still emit a workspace_recovery event");
        assert_eq!(skipped.outcome, "skipped");
        assert_eq!(skipped.details["reason"], "missing_bookmark");
        assert_eq!(skipped.details["predecessor"], prior.id);
    }
}

#[tokio::test]
async fn missing_predecessor_bookmark_dispatches_into_a_clean_workspace() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let (db, prior, next) = blocked_pair(&path);
    seed_local_claude_driver(&db);
    use boss_engine_test_git::jj::JjRepo;
    let repo = JjRepo::new(dir.path());
    std::fs::write(repo.worker.join("revision.txt"), "unpushed revision").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        next_workspace_id: Mutex::new(Some("replacement".into())),
        real_bookmarks: true,
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner)
            .with_dispatch_events(recording.clone()),
    );
    let worker = coordinator
        .pool_for_execution(&next)
        .claim_worker(&next.id, None)
        .await
        .unwrap();
    coordinator
        .schedule_execution(&next, &worker, DispatchAdmission::Queued)
        .await
        .unwrap();
    assert_ne!(
        cube.lease_calls.lock().await[0].2.clone().unwrap_or_default(),
        "workspace-old",
        "missing bookmark must not pin the predecessor workspace"
    );
    assert!(cube.lease_calls.lock().await[0].2.is_none());
    assert!(!repo.replacement.join("revision.txt").exists());
    assert_eq!(db.bookmark_recovery(&next.id).unwrap(), None);
    assert_eq!(
        db.execution_bookmark(&next.id).unwrap().head(),
        format!("boss-recovery/{}", next.id)
    );
    let events = recording.events_for(&next.id).await;
    let skipped = events
        .iter()
        .find(|e| e.stage == "workspace_recovery")
        .expect("dispatch must record the skipped recovery");
    assert_eq!(skipped.outcome, "skipped");
    assert_eq!(skipped.details["predecessor"], prior.id);
}

#[tokio::test]
async fn blocked_revision_dispatch_restores_bookmark_with_or_without_original_workspace() {
    for recovered in [false, true] {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        let (db, prior, next) = blocked_pair(&path);
        seed_local_claude_driver(&db);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE work_executions SET pr_url = 'https://github.com/spinyfin/mono/pull/99' WHERE id = ?1",
                [&next.id],
            )
            .unwrap();
        let next = db.get_execution(&next.id).unwrap();
        use boss_engine_recovery::execution_bookmark::{LocalJj, create};
        use boss_engine_test_git::jj::JjRepo;
        let repo = JjRepo::new(dir.path());
        let record = create(&LocalJj, &repo.worker, &prior.id, "local").await.unwrap();
        db.record_execution_bookmark(&record).unwrap();
        std::fs::write(repo.worker.join("revision.txt"), "unpushed revision").unwrap();
        JjRepo::run(&repo.worker, &["status"]);
        if !recovered {
            std::fs::remove_dir_all(&repo.worker).unwrap();
        }
        let cube = Arc::new(FakeCubeClient {
            workspace_root: Some(dir.path().to_path_buf()),
            next_workspace_id: Mutex::new(Some("replacement".into())),
            real_bookmarks: true,
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
            runner,
        ));
        let worker = coordinator
            .pool_for_execution(&next)
            .claim_worker(&next.id, None)
            .await
            .unwrap();
        coordinator
            .schedule_execution(&next, &worker, DispatchAdmission::Queued)
            .await
            .unwrap();
        assert!(
            cube.goto_calls.lock().await.is_empty(),
            "PR positioning must not overwrite recovered work"
        );
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("revision.txt")).unwrap(),
            "unpushed revision"
        );
        assert_eq!(db.bookmark_recovery(&next.id).unwrap(), Some((prior.id, true)));
        assert_eq!(
            db.execution_bookmark(&next.id).unwrap().head(),
            format!("boss-recovery/{}", next.id)
        );
        assert!(
            boss_engine_recovery::execution_bookmark::diff(&LocalJj, &db.execution_bookmark(&next.id).unwrap())
                .await
                .unwrap()
                .contains("revision.txt")
        );
        assert!(cube.create_calls.lock().await.is_empty());
        assert!(cube.lease_calls.lock().await[0].1.starts_with(&format!("{} ", next.id)));
    }
}
