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
        let (db, _prior, next) = blocked_pair(&dir.path().join("boss.db"));
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
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None)
            .await
            .unwrap();
        assert!(recovered.is_none());
    }
}

/// Records `execution_id`'s pointers in `db`, then deletes both heads so only
/// the baseline survives. Returns the repo holding them.
async fn record_then_delete_heads(
    db: &WorkDb,
    dir: &std::path::Path,
    execution_id: &str,
) -> boss_engine_test_git::jj::JjRepo {
    use boss_engine_recovery::execution_bookmark::{LocalJj, create};
    use boss_engine_test_git::jj::JjRepo;
    let repo = JjRepo::new(dir);
    let record = create(&LocalJj, &repo.worker, execution_id, "local").await.unwrap();
    db.record_execution_bookmark(&record).unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "delete", &record.head(), &record.publication()],
    );
    repo
}

fn recovery_coordinator_for(db: Arc<WorkDb>) -> Arc<ExecutionCoordinator> {
    Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    ))
}

#[tokio::test]
async fn self_retry_with_deleted_own_heads_drops_the_stale_row_and_continues() {
    let dir = tempdir().unwrap();
    let (db, _prior, next) = blocked_pair(&dir.path().join("boss.db"));
    let repo = record_then_delete_heads(&db, dir.path(), &next.id).await;
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient {
            real_bookmarks: true,
            ..FakeCubeClient::default()
        }),
        Arc::new(FakeExecutionRunner::default()),
    ));
    let lease = CubeWorkspaceLease {
        lease_id: "lease-new".into(),
        workspace_id: "workspace-old".into(),
        workspace_path: repo.worker.clone(),
        dirty_verified: None,
    };
    let recovered = coordinator
        .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None)
        .await
        .unwrap();
    assert!(recovered.is_none());
    assert!(
        db.execution_bookmark_optional(&next.id).unwrap().is_none(),
        "a stale row would make dispatch skip creating fresh pointers"
    );
    // Dispatch now creates fresh pointers for the same execution id; the
    // surviving baseline must not make that fail.
    let fresh = coordinator
        .host_adapter
        .create_execution_bookmark(&repo.worker, &next.id, None, None)
        .await
        .expect("fresh creation must succeed after the orphaned baseline is discarded");
    boss_engine_recovery::execution_bookmark::diff(&boss_engine_recovery::execution_bookmark::LocalJj, &fresh)
        .await
        .expect("fresh bookmarks must validate");
}

#[tokio::test]
async fn non_implementation_with_deleted_predecessor_heads_continues_without_recovery() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let (db, prior, next) = blocked_pair(&path);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE work_executions SET kind = 'pr_review'", [])
        .unwrap();
    let next = db.get_execution(&next.id).unwrap();
    assert_eq!(next.kind, ExecutionKind::PrReview);
    let repo = record_then_delete_heads(&db, dir.path(), &prior.id).await;
    let coordinator = recovery_coordinator_for(db.clone());
    let lease = CubeWorkspaceLease {
        lease_id: "lease-new".into(),
        workspace_id: "workspace-old".into(),
        workspace_path: repo.worker.clone(),
        dirty_verified: None,
    };
    let recovered = coordinator
        .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None)
        .await
        .unwrap();
    assert!(recovered.is_none());
    assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
}

#[tokio::test]
async fn missing_predecessor_bookmark_dispatches_into_a_clean_workspace() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let (db, _prior, next) = blocked_pair(&path);
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
    assert_eq!(db.get_execution(&next.id).unwrap().status, ExecutionStatus::Running);
    assert!(db.execution_bookmark_optional(&next.id).unwrap().is_some());
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

pub(super) struct ChainHarness {
    pub(super) cube: Arc<FakeCubeClient>,
    pub(super) coordinator: Arc<ExecutionCoordinator>,
    pub(super) next: WorkExecution,
    pub(super) repo: boss_engine_test_git::jj::JjRepo,
    pub(super) _dir: tempfile::TempDir,
}

/// A revision chain whose root task owns the bound PR, with the replacement
/// execution created through `request_resume_execution` (which never stamps
/// `pr_url`). `chain_root_pr` / `origin` toggle the failure shapes.
pub(super) async fn chain_harness(chain_root_pr: bool, origin: bool, conflict: bool) -> ChainHarness {
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

/// Model a lost SSH reply after the transplant committed, before its baseline
/// could be returned to the coordinator and persisted.
struct InterruptedTransplant;

#[async_trait::async_trait]
impl boss_engine_recovery::execution_bookmark::Jj for InterruptedTransplant {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        use boss_engine_recovery::execution_bookmark::LocalJj;
        let result = LocalJj.run(repo, args).await?;
        if args.first() == Some(&"duplicate") && args[1].contains("boss-base/") {
            return Err(anyhow!("injected lost reply after transplant"));
        }
        Ok(result)
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        use boss_engine_recovery::execution_bookmark::LocalJj;
        LocalJj.shared_repo(workspace).await
    }
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

    let prior_patch = diff(&LocalJj, &record).await.unwrap();
    let error = boss_engine_recovery::execution_bookmark::restore_rebased(
        &InterruptedTransplant,
        &record,
        &h.repo.replacement,
        Some("pr/99"),
        "main",
        "origin",
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("injected lost reply"));
    assert_eq!(diff(&LocalJj, &record).await.unwrap(), prior_patch);

    // Retry through the full coordinator, including predecessor validation.
    h.dispatch().await.unwrap();
    let (WorkItem::Task(item) | WorkItem::Chore(item)) = db.get_work_item(&h.next.work_item_id).unwrap() else {
        panic!("expected task or chore");
    };
    assert_ne!(item.status, TaskStatus::Blocked);
    let successor = db.execution_bookmark(&h.next.id).unwrap();
    assert!(diff(&LocalJj, &successor).await.unwrap().contains("unpushed revision"));
    assert!(!h.repo.replacement.join("stale-pr").exists());
    std::fs::write(h.repo.replacement.join("successor-fix"), "successor work").unwrap();
    JjRepo::run(&h.repo.replacement, &["status"]);

    assert_eq!(diff(&LocalJj, &record).await.unwrap(), prior_patch);
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

#[tokio::test]
async fn orphaned_predecessor_with_no_head_bookmarks_dispatches_cleanly() {
    use boss_engine_test_git::jj::JjRepo;
    let h = chain_harness(true, true, false).await;
    let db = &h.coordinator.work_db;
    let prior = db.recovery_predecessor(&h.next).unwrap().unwrap();
    let record = db.execution_bookmark(&prior.id).unwrap();
    JjRepo::run(
        &h.repo.repo,
        &["bookmark", "delete", &record.head(), &record.publication()],
    );
    h.dispatch().await.unwrap();
    let (WorkItem::Task(item) | WorkItem::Chore(item)) = db.get_work_item(&h.next.work_item_id).unwrap() else {
        panic!("expected implementation item")
    };
    assert_ne!(item.status, TaskStatus::Blocked);
    assert!(db.get_execution(&h.next.id).unwrap().started_at.is_some());
    assert!(db.bookmark_recovery(&h.next.id).unwrap().is_none());
    assert!(db.execution_restore_report(&h.next.id).unwrap().is_none());
    assert!(!h.repo.replacement.join("revision.txt").exists());
    let successor = db.execution_bookmark(&h.next.id).unwrap();
    assert!(
        boss_engine_recovery::execution_bookmark::diff(&boss_engine_recovery::execution_bookmark::LocalJj, &successor)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(h.cube.goto_calls.lock().await.len(), 1);
}

#[tokio::test]
async fn revision_start_retry_keeps_its_own_pointers_on_the_staged_history() {
    use boss_engine_recovery::execution_bookmark::{LocalJj, diff};
    use boss_engine_test_git::jj::JjRepo;
    for fresh in [false, true] {
        let mut h = chain_harness(true, true, false).await;
        let db = h.coordinator.work_db.clone();
        let conn = rusqlite::Connection::open(h._dir.path().join("boss.db")).unwrap();
        if fresh {
            conn.execute("DELETE FROM execution_bookmarks WHERE execution_id != ?1", [&h.next.id])
                .unwrap();
        }
        conn.execute_batch("CREATE TRIGGER fail_run_start BEFORE INSERT ON work_runs WHEN NEW.status = 'active' BEGIN SELECT RAISE(FAIL, 'injected run start failure'); END;").unwrap();
        assert!(
            h.dispatch()
                .await
                .unwrap_err()
                .to_string()
                .contains("injected run start failure")
        );
        let record = db.execution_bookmark(&h.next.id).unwrap();
        let patch = diff(&LocalJj, &record).await.unwrap();
        conn.execute_batch("DROP TRIGGER fail_run_start; UPDATE work_executions SET status = 'ready', dispatch_not_before = NULL WHERE started_at IS NULL;").unwrap();
        // A later retry sees a changed main and PR head, but must not restage its own refs.
        JjRepo::run(&h.repo.repo, &["new", "main", "-m", "Main advanced before retry"]);
        std::fs::write(h.repo.repo.join("later"), "later main").unwrap();
        JjRepo::run(&h.repo.repo, &["bookmark", "set", "main", "pr/99", "-r", "@"]);
        JjRepo::run(&h.repo.repo, &["git", "export"]);
        *h.cube.next_workspace_id.lock().await = Some("replacement".into());
        h.coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            h.cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        h.next = db.get_execution(&h.next.id).unwrap();
        h.dispatch().await.unwrap();
        assert_eq!(h.cube.goto_calls.lock().await.len(), 1);
        assert_eq!(diff(&LocalJj, &record).await.unwrap(), patch);
        for pointer in [record.base(), record.head(), record.publication()] {
            assert!(
                !JjRepo::run(
                    &h.repo.replacement,
                    &[
                        "log",
                        "-r",
                        &format!("bookmarks(exact:{pointer}) & ::@"),
                        "--no-graph",
                        "-T",
                        "commit_id"
                    ]
                )
                .is_empty()
            );
        }
        JjRepo::run(
            &h.repo.replacement,
            &["bookmark", "set", &record.head(), &record.publication(), "-r", "@"],
        );
        assert_eq!(diff(&LocalJj, &record).await.unwrap(), patch);
        if fresh {
            assert!(db.execution_restore_report(&h.next.id).unwrap().is_none());
        }
    }
}

#[tokio::test]
async fn non_implementation_pointer_failure_keeps_pre_start_retries() {
    let dir = tempdir().unwrap();
    let (db, _, mut next) = blocked_pair(&dir.path().join("boss.db"));
    next.kind = ExecutionKind::PrReview;
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![Duration::from_secs(60)]),
    );
    coordinator
        .record_start_failure(
            coordinator.clone(),
            &next,
            "worker",
            None,
            (crate::execution_bookmark_recovery::RECOVERY_FAILED, "Recovery failed"),
            &boss_engine_recovery::execution_bookmark::pointer_integrity_error("deleted refs"),
        )
        .unwrap();
    let after = db.get_execution(&next.id).unwrap();
    assert_ne!(after.status, ExecutionStatus::Failed);
    assert_eq!(after.pre_start_failure_count, 1);
}

#[tokio::test]
async fn revision_recovery_falls_back_to_main_when_the_pr_base_is_unavailable() {
    let h = chain_harness(true, true, false).await;
    h.cube.fail_pr_base.store(true, std::sync::atomic::Ordering::SeqCst);
    h.dispatch().await.unwrap();
    let report = h
        .coordinator
        .work_db
        .execution_restore_report(&h.next.id)
        .unwrap()
        .expect("report");
    assert!(report.pointer.contains("pr/99"), "{}", report.pointer);
    assert!(report.pr_bound);
    assert!(
        report.base_fallback.as_deref().unwrap().contains("`main`"),
        "{report:?}"
    );
    assert!(
        report.instructions().contains("could not be used"),
        "{}",
        report.instructions()
    );
    assert_eq!(
        std::fs::read_to_string(h.repo.replacement.join("revision.txt")).unwrap(),
        "unpushed revision"
    );
}

#[tokio::test]
async fn revision_recovery_discloses_a_missing_pr_base_branch() {
    let h = chain_harness(true, true, false).await;
    // The PR base resolves, but no such remote bookmark exists.
    *h.cube.pr_base.lock().await = Some("gone-parent".into());
    h.dispatch().await.unwrap();
    let report = h
        .coordinator
        .work_db
        .execution_restore_report(&h.next.id)
        .unwrap()
        .expect("report");
    let note = report.base_fallback.clone().expect("fallback is recorded");
    assert!(note.contains("gone-parent") && note.contains("`main`"), "{note}");
    let prompt = report.instructions();
    assert!(
        prompt.contains("gone-parent") && prompt.contains("parent's commits may now appear"),
        "{prompt}"
    );
    assert!(!prompt.contains("restaged onto its current base branch"), "{prompt}");
    assert_eq!(
        std::fs::read_to_string(h.repo.replacement.join("revision.txt")).unwrap(),
        "unpushed revision"
    );
}

#[tokio::test]
async fn revision_recovery_selects_the_repo_by_id_not_by_stored_url() {
    let mut h = chain_harness(true, true, false).await;
    for spelling in ["spinyfin/mono", "mono", "git@github.com:spinyfin/mono.git"] {
        h.next.repo_remote_url = spelling.to_owned();
        let lease = CubeWorkspaceLease {
            lease_id: "lease-spelling".into(),
            workspace_id: "replacement".into(),
            workspace_path: h.repo.replacement.clone(),
            dirty_verified: Some(true),
        };
        let recovered = h
            .coordinator
            .recover_execution_bookmark(&h.next, &lease, &h.coordinator.host_adapter, "mono", Some(99))
            .await
            .unwrap();
        assert!(recovered.is_some(), "{spelling}");
    }
}

#[tokio::test]
async fn pr_review_self_retry_with_a_record_still_positions_via_goto() {
    use boss_engine_recovery::execution_bookmark::{LocalJj, create};
    let mut h = chain_harness(true, true, false).await;
    let db = h.coordinator.work_db.clone();
    rusqlite::Connection::open(h._dir.path().join("boss.db"))
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'pr_review' WHERE id = ?1",
            [&h.next.id],
        )
        .unwrap();
    rusqlite::Connection::open(h._dir.path().join("boss.db"))
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = 'https://github.com/spinyfin/mono/pull/99' WHERE id = ?1",
            [&h.next.work_item_id],
        )
        .unwrap();
    h.next = db.get_execution(&h.next.id).unwrap();
    assert_eq!(h.next.kind, ExecutionKind::PrReview);
    let own = create(&LocalJj, &h.repo.replacement, &h.next.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&own).unwrap();
    h.dispatch().await.unwrap();
    assert_eq!(h.cube.goto_calls.lock().await.len(), 1, "must run cube workspace goto");
}
