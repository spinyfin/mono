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
async fn blocked_recovery_missing_bookmark_fails_loudly() {
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
        let error = coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&prior.id));
        assert!(error.to_string().contains("expected engine-created recovery pointer"));
    }
}

#[tokio::test]
async fn missing_predecessor_bookmark_blocks_dispatch_with_detail() {
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
    let error = coordinator
        .schedule_execution(&next, &worker, DispatchAdmission::Queued)
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&prior.id));
    assert_eq!(db.get_execution(&next.id).unwrap().status, ExecutionStatus::Failed);
    let WorkItem::Chore(failed) = db.get_work_item(&next.work_item_id).unwrap() else {
        panic!("expected chore")
    };
    assert!(
        failed
            .blocked_detail
            .as_deref()
            .unwrap_or_default()
            .contains("recovery pointer")
    );
    assert!(db.execution_bookmark_optional(&next.id).unwrap().is_none());
    assert!(!repo.replacement.join("revision.txt").exists());
    assert!(cube.goto_calls.lock().await.is_empty());
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
        super::recovery::configure_recovery_origin(&repo.repo);
        JjRepo::run(&repo.repo, &["bookmark", "set", "pr/99", "-r", "main"]);
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
            cube.goto_calls.lock().await.len() == 1,
            "resolve the bound PR once before recovering unpushed work"
        );
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("revision.txt")).unwrap(),
            "unpushed revision"
        );
        assert_eq!(db.bookmark_recovery(&next.id).unwrap(), Some((prior.id, true)));
        let report = db
            .execution_restore_report(&next.id)
            .unwrap()
            .expect("restore report recorded");
        assert!(report.pointer.contains("pr/99"), "{}", report.pointer);
        assert!(report.conflicts.is_empty());
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

struct ChainHarness {
    cube: Arc<FakeCubeClient>,
    coordinator: Arc<ExecutionCoordinator>,
    next: WorkExecution,
    repo: boss_engine_test_git::jj::JjRepo,
    _dir: tempfile::TempDir,
}

/// A revision chain whose root task owns the bound PR, with the replacement
/// execution created through `request_resume_execution` (which never stamps
/// `pr_url`). `chain_root_pr` / `origin` toggle the failure shapes.
async fn chain_harness(chain_root_pr: bool, origin: bool, conflict: bool) -> ChainHarness {
    use boss_engine_recovery::execution_bookmark::{LocalJj, create};
    use boss_engine_test_git::jj::JjRepo;
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path.clone()).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let parent = create_test_chore_manual(&db, product.id, "Chain root");
    if chain_root_pr {
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/spinyfin/mono/pull/99' WHERE id = ?1",
                [&parent.id],
            )
            .unwrap();
    }
    let revision = if chain_root_pr {
        db.create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Revise")
                .build(),
            &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
        )
        .unwrap()
        .id
    } else {
        parent.id.clone()
    };
    let prior = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&revision)
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
    db.mark_execution_orphaned(&prior.id, "engine crash").unwrap();
    let next = db.request_resume_execution(&prior.id, 1, 0, "test").unwrap();
    assert!(next.pr_url.is_none(), "resume executions carry no pr_url");

    let repo = JjRepo::new(dir.path());
    if origin {
        super::recovery::configure_recovery_origin(&repo.repo);
    } else {
        JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    }
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/99", "-r", "main"]);
    let record = create(&LocalJj, &repo.worker, &prior.id, "local").await.unwrap();
    db.record_execution_bookmark(&record).unwrap();
    let (file, text) = if conflict {
        ("base.txt", "worker change\n")
    } else {
        ("revision.txt", "unpushed revision")
    };
    std::fs::write(repo.worker.join(file), text).unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    if conflict {
        JjRepo::run(&repo.repo, &["new", "main", "-m", "Conflicting main"]);
        std::fs::write(repo.repo.join("base.txt"), "main change\n").unwrap();
        JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
        JjRepo::run(&repo.repo, &["git", "export"]);
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
    ChainHarness {
        cube,
        coordinator,
        next,
        repo,
        _dir: dir,
    }
}

impl ChainHarness {
    async fn dispatch(&self) -> Result<()> {
        let worker = self
            .coordinator
            .pool_for_execution(&self.next)
            .claim_worker(&self.next.id, None)
            .await
            .unwrap();
        self.coordinator
            .schedule_execution(&self.next, &worker, DispatchAdmission::Queued)
            .await
            .map(|_| ())
    }
}

#[tokio::test]
async fn resumed_revision_without_pr_url_recovers_using_the_chain_root_pr() {
    let h = chain_harness(true, true, false).await;
    h.dispatch().await.unwrap();
    assert_eq!(h.cube.goto_calls.lock().await.len(), 1);
    assert_eq!(
        std::fs::read_to_string(h.repo.replacement.join("revision.txt")).unwrap(),
        "unpushed revision"
    );
    let report = h
        .coordinator
        .work_db
        .execution_restore_report(&h.next.id)
        .unwrap()
        .expect("report");
    assert!(report.pointer.contains("pr/99"), "{}", report.pointer);
    assert!(report.conflicts.is_empty());
}

#[tokio::test]
async fn conflicting_revision_restore_reaches_the_database() {
    let h = chain_harness(true, true, true).await;
    h.dispatch().await.unwrap();
    let report = h
        .coordinator
        .work_db
        .execution_restore_report(&h.next.id)
        .unwrap()
        .expect("report");
    assert!(report.conflicts.contains("base.txt"), "{}", report.conflicts);
    assert!(report.instructions().contains("FIRST TASK"));
}

#[tokio::test]
async fn rewritten_pr_dispatch_preserves_successor_work_through_second_recovery() {
    use boss_engine_recovery::execution_bookmark::{LocalJj, diff};
    use boss_engine_test_git::jj::JjRepo;
    let mut h = chain_harness(true, true, false).await;
    let db = h.coordinator.work_db.clone();
    let prior = db.recovery_predecessor(&h.next).unwrap().unwrap();
    let record = db.execution_bookmark(&prior.id).unwrap();
    JjRepo::run(&h.repo.repo, &["new", "main", "-m", "Old published PR"]);
    std::fs::write(h.repo.repo.join("stale-pr"), "old published").unwrap();
    JjRepo::run(&h.repo.repo, &["bookmark", "set", "pr/99", &record.base(), "-r", "@"]);
    JjRepo::run(&h.repo.worker, &["rebase", "-r", "@", "-d", "pr/99"]);
    JjRepo::run(&h.repo.repo, &["new", "main", "-m", "Rewritten PR"]);
    std::fs::write(h.repo.repo.join("rewritten-pr"), "force pushed").unwrap();
    JjRepo::run(
        &h.repo.repo,
        &["bookmark", "set", "pr/99", "-r", "@", "--allow-backwards"],
    );

    h.dispatch().await.unwrap();
    let successor = db.execution_bookmark(&h.next.id).unwrap();
    assert!(diff(&LocalJj, &successor).await.unwrap().contains("unpushed revision"));
    assert!(!h.repo.replacement.join("stale-pr").exists());
    std::fs::write(h.repo.replacement.join("successor-fix"), "successor work").unwrap();
    JjRepo::run(&h.repo.replacement, &["status"]);

    db.mark_execution_orphaned(&h.next.id, "second crash").unwrap();
    h.next = db.request_resume_execution(&h.next.id, 1, 0, "test").unwrap();
    *h.cube.next_workspace_id.lock().await = Some("replacement".into());
    // A fresh coordinator models the restart and gives dispatch a fresh pool.
    h.coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        h.cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    h.dispatch().await.unwrap();
    let patch = diff(&LocalJj, &db.execution_bookmark(&h.next.id).unwrap())
        .await
        .unwrap();
    assert!(patch.contains("unpushed revision") && patch.contains("successor work"));
    assert_eq!(
        db.bookmark_recovery(&h.next.id).unwrap(),
        Some((successor.execution_id, true))
    );
    assert!(h.repo.replacement.join("rewritten-pr").exists());
    assert!(!h.repo.replacement.join("stale-pr").exists());
}

#[tokio::test]
async fn revision_without_any_bound_pr_blocks_the_work_item() {
    let h = chain_harness(false, true, false).await;
    let error = h.dispatch().await.unwrap_err();
    assert!(error.to_string().contains("bound PR"), "{error:#}");
    assert!(h.cube.goto_calls.lock().await.is_empty());
    let (WorkItem::Chore(item) | WorkItem::Task(item)) =
        h.coordinator.work_db.get_work_item(&h.next.work_item_id).unwrap()
    else {
        panic!("expected chore")
    };
    assert_eq!(item.status, TaskStatus::Blocked);
    assert!(!item.autostart);
}

#[tokio::test]
async fn transient_fetch_failure_keeps_the_item_retryable() {
    // No origin remote: `jj git fetch` fails while the pointer is intact.
    let h = chain_harness(true, false, false).await;
    let autostart_before = match h.coordinator.work_db.get_work_item(&h.next.work_item_id).unwrap() {
        WorkItem::Chore(item) | WorkItem::Task(item) => item.autostart,
        _ => panic!("expected task"),
    };
    h.dispatch().await.unwrap_err();
    let (WorkItem::Chore(item) | WorkItem::Task(item)) =
        h.coordinator.work_db.get_work_item(&h.next.work_item_id).unwrap()
    else {
        panic!("expected chore")
    };
    assert_ne!(item.status, TaskStatus::Blocked, "transient failure must not block");
    assert_eq!(item.autostart, autostart_before, "autostart must not be cleared");
    assert_ne!(
        h.coordinator.work_db.get_execution(&h.next.id).unwrap().status,
        ExecutionStatus::Failed
    );
}
