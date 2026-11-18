use super::helpers::*;
use super::recovery_restage::bookmark_targets;

pub(super) fn blocked_pair(path: &std::path::Path) -> (Arc<WorkDb>, WorkExecution, WorkExecution) {
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
    // Teardown returned the run's cube lease. Dispatch holds a successor while
    // its recovery predecessor still records one, so a finished predecessor
    // must not. Only the lease is cleared: the workspace columns stay, as
    // they do while a release is still finishing, for tests that read them.
    assert!(db.claim_execution_workspace_release(&prior.id, "lease-old").unwrap());
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
            .lease_workspace_with_fallback_for_test(&next, "worker", &repo, "task", &coordinator.host_adapter())
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
        .lease_workspace_with_fallback_for_test(&next, "worker", &repo, "task", &coordinator.host_adapter())
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
            .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", None, None)
            .await
            .unwrap();
        assert!(recovered.is_none());
    }
}

#[tokio::test]
async fn predecessor_record_without_heads_is_an_empty_run_not_a_recovery_failure() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = blocked_pair(&dir.path().join("boss.db"));
    let repo = record_without_bookmarks(&db, dir.path(), &prior.id).await;
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCubeClient {
                real_bookmarks: true,
                ..FakeCubeClient::default()
            }),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_dispatch_events(recording.clone()),
    );
    let lease = CubeWorkspaceLease {
        lease_id: "lease-new".into(),
        workspace_id: "workspace-old".into(),
        workspace_path: repo.worker.clone(),
        dirty_verified: None,
    };
    let recovered = coordinator
        .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", None, None)
        .await
        .unwrap();
    assert!(recovered.is_none());
    assert!(
        db.execution_bookmark_optional(&prior.id).unwrap().is_some(),
        "the predecessor's record is not retired"
    );
    let retired: Vec<_> = db
        .list_attention_items_for_work_item(&prior.work_item_id)
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == crate::execution_bookmark_recovery::RECOVERY_RETIRED)
        .collect();
    assert!(retired.is_empty(), "{retired:?}");
    let events = format!("{:?}", recording.events().await);
    assert!(!events.contains("missing_bookmark"), "{events}");
    assert!(!events.contains("pointers_retired"), "{events}");
}

/// Dispatch creates no `boss*` bookmark for an execution.
fn assert_no_execution_bookmarks(repo: &std::path::Path, execution_id: &str) {
    let listed = boss_engine_test_git::jj::JjRepo::run(
        repo,
        &[
            "bookmark",
            "list",
            "-T",
            "name ++ \"\\n\"",
            &format!("glob:boss*{execution_id}"),
        ],
    );
    assert!(listed.trim().is_empty(), "dispatch created bookmarks: {listed}");
}

/// Records `execution_id`'s starting commit in `db` and creates no bookmark, as
/// dispatch does. Returns the repo holding it.
async fn record_without_bookmarks(
    db: &WorkDb,
    dir: &std::path::Path,
    execution_id: &str,
) -> boss_engine_test_git::jj::JjRepo {
    use boss_engine_recovery::execution_bookmark::LocalJj;
    use boss_engine_recovery_fixtures::create;
    use boss_engine_test_git::jj::JjRepo;
    let repo = JjRepo::new(dir);
    let record = create(&LocalJj, &repo.worker, execution_id, "local").await.unwrap();
    db.record_execution_bookmark(&record).unwrap();
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
async fn self_retry_without_bookmarks_leaves_the_record_and_continues() {
    let dir = tempdir().unwrap();
    let (db, _prior, next) = blocked_pair(&dir.path().join("boss.db"));
    let repo = record_without_bookmarks(&db, dir.path(), &next.id).await;
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
        .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", None, None)
        .await
        .unwrap();
    assert!(recovered.is_none());
    assert!(
        db.execution_bookmark_optional(&next.id).unwrap().is_some(),
        "legacy records remain untouched when nothing was preserved"
    );
    // Empty initialization records the starting commit for the same
    // execution id even when a leftover baseline name is still present.
    let fresh = coordinator
        .host_adapter()
        .create_execution_bookmark(&repo.worker, &next.id, None, None)
        .await
        .expect("fresh creation must succeed after the orphaned baseline is discarded");
    assert_eq!(
        fresh.base_commit.as_deref(),
        Some(
            boss_engine_test_git::jj::JjRepo::run(
                &repo.repo,
                &["log", "--no-graph", "-r", "trunk()", "-T", "commit_id"]
            )
            .as_str()
        )
    );
    assert_no_execution_bookmarks(&repo.repo, &fresh.execution_id);
}

/// A row recorded by dispatch for a run that preserved nothing has a baseline
/// commit and no head. Redispatching it is ordinary, not a recovery failure.
#[tokio::test]
async fn redispatch_of_a_fresh_empty_execution_raises_no_recovery_note() {
    use boss_engine_recovery::execution_bookmark::{LocalJj, create_from};
    use boss_engine_test_git::jj::JjRepo;
    for self_retry in [true, false] {
        let dir = tempdir().unwrap();
        let (db, prior, next) = blocked_pair(&dir.path().join("boss.db"));
        let repo = JjRepo::new(dir.path());
        super::recovery::configure_recovery_origin(&repo.repo);
        JjRepo::run(&repo.repo, &["git", "fetch", "--remote", "origin", "--branch", "main"]);
        let source = if self_retry { &next } else { &prior };
        let record = create_from(&LocalJj, &repo.worker, &source.id, "local", None, None)
            .await
            .unwrap();
        db.record_execution_bookmark(&record).unwrap();
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
            .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", None, None)
            .await
            .unwrap();
        assert!(recovered.is_none(), "self_retry {self_retry}: {recovered:?}");
        for work_item in [&prior.work_item_id, &next.work_item_id] {
            assert!(
                db.list_attention_items_for_work_item(work_item)
                    .unwrap()
                    .iter()
                    .all(|item| item.kind != crate::execution_bookmark_recovery::RECOVERY_RETIRED),
                "self_retry {self_retry}: a fresh empty run must not write a recovery note"
            );
        }
    }
}

#[tokio::test]
async fn self_retry_without_bookmarks_and_stale_report_still_positions_on_the_bound_pr() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let (db, _prior, next) = blocked_pair(&path);
    seed_local_claude_driver(&db);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET pr_url = 'https://github.com/spinyfin/mono/pull/99' WHERE id = ?1",
            [&next.id],
        )
        .unwrap();
    let next = db.get_execution(&next.id).unwrap();
    use boss_engine_recovery::execution_bookmark::RestoreReport;
    use boss_engine_test_git::jj::JjRepo;
    let repo = record_without_bookmarks(&db, dir.path(), &next.id).await;
    let original = db.execution_bookmark(&next.id).unwrap();
    super::recovery::configure_recovery_origin(&repo.repo);
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/99", "-r", "main"]);
    db.record_execution_restore_report(
        &next.id,
        &RestoreReport::builder()
            .pointer("boss-recovery/exec_prior + pr/99")
            .commits("abc1234 Preserved work\n")
            .base_sha("0123456789abcdef")
            .conflicts("")
            .pr_bound(true)
            .build(),
    )
    .unwrap();
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
        .pool_for_execution_for_test(&next)
        .claim_worker(&next.id, None)
        .await
        .unwrap();
    coordinator
        .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
        .await
        .unwrap();
    assert_eq!(
        cube.goto_calls.lock().await.len(),
        1,
        "a stale restore report must not suppress positioning on the bound PR"
    );
    assert!(
        db.execution_restore_report(&next.id).unwrap().is_none(),
        "the stale report must be cleared"
    );
    let fresh = db.execution_bookmark(&next.id).unwrap();
    assert_eq!(fresh, original, "legacy metadata is not repaired during dispatch");
    assert_no_execution_bookmarks(&repo.repo, &fresh.execution_id);
}

#[tokio::test]
async fn read_only_review_leaves_predecessor_records_uninspected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let (db, prior, next) = blocked_pair(&path);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE work_executions SET kind = 'pr_review'", [])
        .unwrap();
    let next = db.get_execution(&next.id).unwrap();
    assert_eq!(next.kind, ExecutionKind::PrReview);
    let repo = record_without_bookmarks(&db, dir.path(), &prior.id).await;
    let coordinator = recovery_coordinator_for(db.clone());
    let lease = CubeWorkspaceLease {
        lease_id: "lease-new".into(),
        workspace_id: "workspace-old".into(),
        workspace_path: repo.worker.clone(),
        dirty_verified: None,
    };
    let recovered = coordinator
        .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", None, None)
        .await
        .unwrap();
    assert!(recovered.is_none());
    assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
    assert!(db.execution_bookmark_retired_reason(&prior.id).unwrap().is_none());
    assert!(
        db.list_attention_items_for_work_item(&prior.work_item_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        boss_engine_test_git::jj::JjRepo::run(
            &repo.repo,
            &["bookmark", "list", &format!("glob:preserved-archive/{}/*", prior.id)]
        )
        .is_empty()
    );
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
        .pool_for_execution_for_test(&next)
        .claim_worker(&next.id, None)
        .await
        .unwrap();
    coordinator
        .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
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
        use boss_engine_recovery::execution_bookmark::LocalJj;
        use boss_engine_recovery_fixtures::create;
        use boss_engine_test_git::jj::JjRepo;
        let repo = JjRepo::new(dir.path());
        super::recovery::configure_recovery_origin(&repo.repo);
        JjRepo::run(&repo.repo, &["bookmark", "set", "pr/99", "-r", "main"]);
        let record = if recovered {
            boss_engine_recovery::execution_bookmark::create_from(
                &LocalJj,
                &repo.worker,
                &prior.id,
                "local",
                None,
                None,
            )
            .await
            .unwrap()
        } else {
            create(&LocalJj, &repo.worker, &prior.id, "local").await.unwrap()
        };
        db.record_execution_bookmark(&record).unwrap();
        std::fs::write(repo.worker.join("revision.txt"), "unpushed revision").unwrap();
        JjRepo::run(&repo.worker, &["status"]);
        if recovered {
            // A worker names its publication branch itself before publishing.
            JjRepo::run(
                &repo.worker,
                &["bookmark", "set", &format!("boss/{}", prior.id), "-r", "@"],
            );
        }
        super::recovery::preserve_work(&db, &prior.id, &repo.worker).await;
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
            .pool_for_execution_for_test(&next)
            .claim_worker(&next.id, None)
            .await
            .unwrap();
        coordinator
            .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
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
        assert!(report.pointer.starts_with("preserved/"), "{}", report.pointer);
        assert!(report.conflicts.is_empty());
        assert_no_execution_bookmarks(&repo.repo, &next.id);
        assert!(JjRepo::run(&repo.replacement, &["diff", "--git", "--from", "main"]).contains("revision.txt"));
        assert!(cube.create_calls.lock().await.is_empty());
        assert!(cube.lease_calls.lock().await[0].1.starts_with(&format!("{} ", next.id)));
    }
}

/// Dispatch against a predecessor whose baseline bookmark is gone and whose
/// record predates the baseline commit. Neither is an integrity failure: the
/// floor is trunk and the unpublished revision work is restaged.
#[tokio::test]
async fn blocked_revision_dispatch_restages_a_record_without_a_baseline() {
    for pr_carries_work in [true, false] {
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
        use boss_engine_recovery::execution_bookmark::LocalJj;
        use boss_engine_recovery_fixtures::create;
        use boss_engine_test_git::jj::JjRepo;
        let repo = JjRepo::new(dir.path());
        super::recovery::configure_recovery_origin(&repo.repo);
        JjRepo::run(&repo.repo, &["bookmark", "set", "pr/99", "-r", "main"]);
        let record = create(&LocalJj, &repo.worker, &prior.id, "local").await.unwrap();
        db.record_execution_bookmark(&record).unwrap();
        std::fs::write(repo.worker.join("revision.txt"), "unpushed revision").unwrap();
        JjRepo::run(&repo.worker, &["describe", "-m", "Revision work"]);
        super::recovery::preserve_work(&db, &prior.id, &repo.worker).await;
        let work = JjRepo::run(
            &repo.repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &format!("bookmarks(exact:\"preserved/{}\")", prior.id),
                "-T",
                "commit_id",
            ],
        );
        // A record from before the baseline commit was recorded.
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE execution_bookmarks SET base_commit = NULL WHERE execution_id = ?1",
                [&prior.id],
            )
            .unwrap();
        if pr_carries_work {
            JjRepo::run(
                &repo.repo,
                &["bookmark", "set", "pr/99", "-r", &work, "--allow-backwards"],
            );
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
            .pool_for_execution_for_test(&next)
            .claim_worker(&next.id, None)
            .await
            .unwrap();
        coordinator
            .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
            .await
            .unwrap();
        assert!(
            !cube.goto_calls.lock().await.is_empty(),
            "the bound PR is resolved before reconciling"
        );
        let retired_attention: Vec<_> = db
            .list_attention_items_for_work_item(&prior.work_item_id)
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == crate::execution_bookmark_recovery::RECOVERY_RETIRED)
            .collect();
        // No baseline bookmark and no baseline commit is not an integrity
        // failure: the floor falls back to trunk and the unpublished revision
        // work is restaged, whether or not the PR bookmark happens to carry it.
        assert_eq!(
            db.bookmark_recovery(&next.id).unwrap(),
            Some((prior.id.clone(), true)),
            "pr_carries_work={pr_carries_work}"
        );
        assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
        assert!(retired_attention.is_empty(), "{retired_attention:?}");
        let report = db.execution_restore_report(&next.id).unwrap().expect("restore report");
        assert!(report.commits.contains("Revision work"), "{}", report.commits);
        assert!(
            db.execution_bookmark_optional(&next.id).unwrap().is_some(),
            "the new run has its own pointers"
        );
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
    use boss_engine_recovery::execution_bookmark::LocalJj;
    use boss_engine_recovery_fixtures::create;
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
    let next = db
        .request_resume_execution(&prior.id, 1, 0, "test")
        .unwrap()
        .expect("resumable predecessor");
    assert!(next.pr_url.is_none(), "resume executions carry no pr_url");

    let repo = JjRepo::new(dir.path());
    if origin {
        super::recovery::configure_recovery_origin(&repo.repo);
    } else {
        JjRepo::run(&repo.repo, &["bookmark", "set", "main", "pr/99", "-r", "@"]);
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
    super::recovery::preserve_work(&db, &prior.id, &repo.worker).await;
    // Preserving the work is followed by returning the orphaned run's lease;
    // dispatch holds the resume until that has happened.
    release_finished_execution_lease(&db, &prior.id);
    if conflict {
        JjRepo::run(&repo.repo, &["new", "main", "-m", "Conflicting main"]);
        std::fs::write(repo.repo.join("base.txt"), "main change\n").unwrap();
        JjRepo::run(&repo.repo, &["bookmark", "set", "main", "pr/99", "-r", "@"]);
        JjRepo::run(&repo.repo, &["git", "export"]);
    }
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        next_workspace_id: Mutex::new(Some("replacement".into())),
        real_bookmarks: true,
        position_real_workspace: true,
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
    pub(super) async fn dispatch(&self) -> Result<()> {
        let worker = self
            .coordinator
            .pool_for_execution_for_test(&self.next)
            .claim_worker(&self.next.id, None)
            .await
            .unwrap();
        self.coordinator
            .schedule_execution_for_test(&self.next, &worker, DispatchAdmission::Queued)
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
        .work_db()
        .execution_restore_report(&h.next.id)
        .unwrap()
        .expect("report");
    assert!(report.pointer.starts_with("preserved/"), "{}", report.pointer);
    assert!(report.conflicts.is_empty());
}

#[tokio::test]
async fn conflicting_revision_restore_reaches_the_database() {
    let h = chain_harness(true, true, true).await;
    h.dispatch().await.unwrap();
    let report = h
        .coordinator
        .work_db()
        .execution_restore_report(&h.next.id)
        .unwrap()
        .expect("report");
    assert!(!report.conflicts.is_empty(), "{}", report.conflicts);
    assert!(boss_engine_test_git::jj::JjRepo::run(&h.repo.replacement, &["resolve", "--list"]).contains("base.txt"));
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
        if args.first() == Some(&"rebase") {
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
    use boss_engine_test_git::jj::JjRepo;
    let mut h = chain_harness(true, true, false).await;
    let db = h.coordinator.work_db().clone();
    let prior = db.recovery_predecessor(&h.next).unwrap().unwrap();
    JjRepo::run(&h.repo.repo, &["new", "main", "-m", "Old published PR"]);
    std::fs::write(h.repo.repo.join("stale-pr"), "old published").unwrap();
    JjRepo::run(&h.repo.repo, &["bookmark", "set", "pr/99", "-r", "@"]);
    // Dispatch recorded this PR head as the execution's baseline commit.
    let old_pr = JjRepo::run(&h.repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    assert_eq!(
        db.connect()
            .unwrap()
            .execute(
                "UPDATE execution_bookmarks SET base_commit = ?2 WHERE execution_id = ?1 AND retired_at IS NULL",
                rusqlite::params![prior.id, old_pr],
            )
            .unwrap(),
        1
    );
    JjRepo::run(&h.repo.repo, &["git", "export"]);
    JjRepo::run(
        &h.repo.repo,
        &["git", "fetch", "--remote", "origin", "--branch", "pr/99"],
    );

    let record = db.execution_bookmark(&prior.id).unwrap();
    JjRepo::run(&h.repo.worker, &["rebase", "-r", "@", "-d", "pr/99"]);
    JjRepo::run(&h.repo.repo, &["new", "main", "-m", "Rewritten PR"]);
    std::fs::write(h.repo.repo.join("rewritten-pr"), "force pushed").unwrap();
    JjRepo::run(
        &h.repo.repo,
        &["bookmark", "set", "pr/99", "-r", "@", "--allow-backwards"],
    );

    super::recovery::preserve_work(&db, &prior.id, &h.repo.worker).await;
    let saved = db.preserved_tip(&prior.id).unwrap().unwrap();
    JjRepo::run(&h.repo.replacement, &["new", "pr/99", "-m", "Position retry"]);
    let error = boss_engine_recovery::execution_bookmark::restore_preserved(
        &InterruptedTransplant,
        &record,
        &h.repo.replacement,
        Some(&saved),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("injected lost reply"));
    assert!(
        JjRepo::run(
            &h.repo.repo,
            &[
                "file",
                "show",
                "-r",
                &boss_engine_recovery::execution_bookmark::preserved_name(&prior.id),
                "root:revision.txt"
            ]
        )
        .contains("unpushed revision")
    );

    // Retry through the full coordinator, including predecessor validation.
    h.dispatch().await.unwrap();
    let (WorkItem::Task(item) | WorkItem::Chore(item)) = db.get_work_item(&h.next.work_item_id).unwrap() else {
        panic!("expected task or chore");
    };
    assert_ne!(item.status, TaskStatus::Blocked);
    let successor = db.execution_bookmark(&h.next.id).unwrap();
    assert!(JjRepo::run(&h.repo.replacement, &["diff", "--git", "--from", "pr/99"]).contains("unpushed revision"));
    assert!(!h.repo.replacement.join("stale-pr").exists());
    std::fs::write(h.repo.replacement.join("successor-fix"), "successor work").unwrap();
    JjRepo::run(&h.repo.replacement, &["status"]);

    super::recovery::preserve_work(&db, &h.next.id, &h.repo.replacement).await;
    db.mark_execution_orphaned(&h.next.id, "second crash").unwrap();
    // The second crash's lease is returned once its work is preserved, as for the first.
    release_finished_execution_lease(&db, &h.next.id);
    h.next = db
        .request_resume_execution(&h.next.id, 1, 0, "test")
        .unwrap()
        .expect("resumable predecessor");
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
    let patch = JjRepo::run(&h.repo.replacement, &["diff", "--git", "--from", "pr/99"]);
    assert!(patch.contains("unpushed revision") && patch.contains("successor work"));
    assert_eq!(
        db.bookmark_recovery(&h.next.id).unwrap(),
        Some((successor.execution_id, true))
    );
    assert!(h.repo.replacement.join("rewritten-pr").exists());
    assert!(!h.repo.replacement.join("stale-pr").exists());
}

#[tokio::test]
async fn revision_without_any_bound_pr_restages_on_trunk() {
    let h = chain_harness(false, true, false).await;
    let db = h.coordinator.work_db();
    let prior = db.recovery_predecessor(&h.next).unwrap().unwrap();
    h.dispatch().await.unwrap();
    assert!(h.cube.goto_calls.lock().await.is_empty());
    assert_eq!(db.get_execution(&h.next.id).unwrap().status, ExecutionStatus::Running);
    assert!(db.preserved_tip(&prior.id).unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(h.repo.replacement.join("revision.txt")).unwrap(),
        "unpushed revision"
    );
    assert!(
        db.list_attention_items_for_work_item(&prior.work_item_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn positioning_transport_failure_retries_without_archiving() {
    use boss_engine_test_git::jj::JjRepo;
    let mut h = chain_harness(true, true, false).await;
    h.cube = Arc::new(FakeCubeClient {
        workspace_root: Some(h._dir.path().to_path_buf()),
        next_workspace_id: Mutex::new(Some("replacement".into())),
        real_bookmarks: true,
        position_real_workspace: true,
        fail_first_n_goto: 1,
        ..Default::default()
    });
    h.coordinator = Arc::new(ExecutionCoordinator::new(
        h.coordinator.work_db().clone(),
        WorkerPool::new(1),
        h.cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..Default::default()
        }),
    ));
    let db = &h.coordinator.work_db();
    let prior = db.recovery_predecessor(&h.next).unwrap().unwrap();
    let autostart = match db.get_work_item(&h.next.work_item_id).unwrap() {
        WorkItem::Task(item) | WorkItem::Chore(item) => item.autostart,
        _ => panic!("expected implementation item"),
    };
    let record = db.execution_bookmark(&prior.id).unwrap();
    let pointers = bookmark_targets(&record);
    let error = h.dispatch().await.unwrap_err();
    assert!(format!("{error:#}").contains("cube workspace goto failed"), "{error:#}");
    assert!(!boss_engine_recovery::execution_bookmark::is_pointer_integrity_error(
        &error
    ));
    let after = db.get_execution(&h.next.id).unwrap();
    assert_eq!(after.pre_start_failure_count, 1);
    assert!(after.dispatch_not_before.is_some());
    assert!(after.started_at.is_none());
    assert_ne!(after.status, ExecutionStatus::Failed);
    let (WorkItem::Task(item) | WorkItem::Chore(item)) = db.get_work_item(&h.next.work_item_id).unwrap() else {
        panic!("expected implementation item");
    };
    assert_ne!(item.status, TaskStatus::Blocked);
    assert_eq!(item.autostart, autostart);
    assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
    assert!(db.execution_bookmark_retired_reason(&prior.id).unwrap().is_none());
    assert_eq!(bookmark_targets(&record), pointers);
    assert!(
        JjRepo::run(
            &h.repo.repo,
            &["bookmark", "list", &format!("glob:preserved-archive/{}/*", prior.id)]
        )
        .is_empty()
    );
}

#[tokio::test]
async fn orphaned_predecessor_without_a_preserved_tip_dispatches_cleanly() {
    use boss_engine_test_git::jj::JjRepo;
    let h = chain_harness(true, true, false).await;
    let db = &h.coordinator.work_db();
    let prior = db.recovery_predecessor(&h.next).unwrap().unwrap();
    db.clear_preserved_tip(&prior.id).unwrap();
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
    assert!(successor.base_commit.is_some());
    assert_no_execution_bookmarks(&h.repo.repo, &successor.execution_id);
    assert!(JjRepo::run(&h.repo.replacement, &["diff", "--git", "--from", "pr/99"]).is_empty());
    assert_eq!(h.cube.goto_calls.lock().await.len(), 1);
}

#[tokio::test]
async fn revision_start_retry_keeps_its_own_pointers_on_the_staged_history() {
    use boss_engine_test_git::jj::JjRepo;
    for fresh in [false, true] {
        let mut h = chain_harness(true, true, false).await;
        let db = h.coordinator.work_db().clone();
        h.cube = Arc::new(FakeCubeClient {
            workspace_root: Some(h._dir.path().to_path_buf()),
            next_workspace_id: Mutex::new(Some("replacement".into())),
            position_real_workspace: true,
            real_bookmarks: true,
            ..FakeCubeClient::default()
        });
        h.coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            h.cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        let conn = rusqlite::Connection::open(h._dir.path().join("boss.db")).unwrap();
        if fresh {
            JjRepo::run(
                &h.repo.repo,
                &["git", "fetch", "--remote", "origin", "--branch", "main"],
            );
            conn.execute("UPDATE work_executions SET preserved_commit_id = NULL, preserved_change_id = NULL, preserved_commit_count = NULL WHERE id != ?1", [&h.next.id]).unwrap();
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
        let patch = if fresh {
            assert!(db.preserved_tip(&h.next.id).unwrap().is_none());
            let patch = JjRepo::run(
                &h.repo.replacement,
                &["diff", "--git", "--from", record.base_commit.as_deref().unwrap()],
            );
            assert!(patch.is_empty());
            patch
        } else {
            assert!(db.preserved_tip(&h.next.id).unwrap().is_some());
            JjRepo::run(&h.repo.replacement, &["diff", "--git", "--from", "pr/99"])
        };
        conn.execute_batch("DROP TRIGGER fail_run_start; UPDATE work_executions SET status = 'ready', dispatch_not_before = NULL WHERE started_at IS NULL;").unwrap();
        // A retry restores staged work when the first attempt had some, and
        // positions on the current PR when it did not.
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
        // Both attempts position the lease; only unpublished work is restaged.
        assert_eq!(h.cube.goto_calls.lock().await.len(), 2);
        if fresh {
            assert!(h.repo.replacement.join("later").exists());
            assert!(db.execution_restore_report(&h.next.id).unwrap().is_none());
            assert!(db.preserved_tip(&h.next.id).unwrap().is_none());
            // The first attempt's record had no recovery head: an ordinary empty
            // run is not a recovery failure and raises no attention item.
            let retired: Vec<_> = db
                .list_attention_items_for_work_item(&db.owning_work_item_id(&h.next.work_item_id))
                .unwrap()
                .into_iter()
                .filter(|item| item.kind == crate::execution_bookmark_recovery::RECOVERY_RETIRED)
                .collect();
            assert!(retired.is_empty(), "{retired:?}");
            continue;
        }
        let restored_patch = || JjRepo::run(&h.repo.replacement, &["diff", "--git", "--from", "pr/99"]);
        assert_eq!(restored_patch(), patch);
        assert!(h.repo.replacement.join("later").exists());
        assert_no_execution_bookmarks(&h.repo.repo, &record.execution_id);
        assert_eq!(restored_patch(), patch);
        // The retry restaged the recorded preserved tip, so it records a restore report.
        assert!(db.execution_restore_report(&h.next.id).unwrap().is_some());
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
        .record_start_failure_for_test(
            coordinator.clone(),
            &next,
            "worker",
            None,
            (crate::execution_bookmark_recovery::RECOVERY_FAILED, "Recovery failed"),
            &boss_engine_recovery::execution_bookmark::pointer_integrity_error("deleted refs"),
        )
        .await
        .unwrap();
    let after = db.get_execution(&next.id).unwrap();
    assert_ne!(after.status, ExecutionStatus::Failed);
    assert_eq!(after.pre_start_failure_count, 1);
}

#[tokio::test]
async fn revision_recovery_uses_pr_head_without_resolving_its_base() {
    let h = chain_harness(true, true, false).await;
    h.cube.fail_pr_base.store(true, std::sync::atomic::Ordering::SeqCst);
    h.dispatch().await.unwrap();
    let report = h
        .coordinator
        .work_db()
        .execution_restore_report(&h.next.id)
        .unwrap()
        .expect("report");
    assert!(report.pointer.starts_with("preserved/"), "{}", report.pointer);
    assert!(report.base_fallback.is_none());
    assert_eq!(
        report.base_sha,
        boss_engine_test_git::jj::JjRepo::run(&h.repo.repo, &["log", "-r", "pr/99", "--no-graph", "-T", "commit_id"])
    );
    assert_eq!(
        std::fs::read_to_string(h.repo.replacement.join("revision.txt")).unwrap(),
        "unpushed revision"
    );
}

#[tokio::test]
async fn revision_recovery_uses_pr_head_when_parent_branch_is_gone() {
    let h = chain_harness(true, true, false).await;
    // The PR base resolves, but no such remote bookmark exists.
    *h.cube.pr_base.lock().await = Some("gone-parent".into());
    h.dispatch().await.unwrap();
    let report = h
        .coordinator
        .work_db()
        .execution_restore_report(&h.next.id)
        .unwrap()
        .expect("report");
    assert!(report.base_fallback.is_none());
    assert_eq!(
        report.base_sha,
        boss_engine_test_git::jj::JjRepo::run(&h.repo.repo, &["log", "-r", "pr/99", "--no-graph", "-T", "commit_id"])
    );
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
            .recover_execution_bookmark_for_test(
                &h.next,
                &lease,
                &h.coordinator.host_adapter(),
                "mono",
                Some(99),
                Some(99),
            )
            .await
            .unwrap();
        assert!(recovered.is_some(), "{spelling}");
    }
}

#[tokio::test]
async fn pr_review_self_retry_with_a_record_still_positions_via_goto() {
    use boss_engine_recovery::execution_bookmark::LocalJj;
    use boss_engine_recovery_fixtures::create;
    let mut h = chain_harness(true, true, false).await;
    let db = h.coordinator.work_db().clone();
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

/// A pre-start lease whose recovery record cannot be used (its store is gone)
/// must not wedge every later dispatch of the same execution: preservation
/// fails without holding the lease, which is released, and a fresh workspace
/// is leased.
#[tokio::test]
async fn unpreservable_pre_start_lease_does_not_block_redispatch() {
    let dir = tempdir().unwrap();
    let (db, _prior, next) = blocked_pair(&dir.path().join("boss.db"));
    db.restore_execution_workspace(
        &next.id,
        "kept-lease",
        Some("workspace-kept"),
        Some("/tmp/workspace-kept-gone"),
    )
    .unwrap();
    db.record_execution_bookmark(&boss_engine_recovery::execution_bookmark::ExecutionBookmark {
        execution_id: next.id.clone(),
        repo_path: PathBuf::from("/tmp/boss-no-such-repo"),
        host_id: "local".to_owned(),
        base_commit: None,
        pr_head_before: None,
    })
    .unwrap();
    let cube = Arc::new(FakeCubeClient::default());
    *cube.list_workspaces_response.lock().await = vec![
        CubeWorkspaceStatus::builder()
            .workspace_id("workspace-kept")
            .workspace_path(PathBuf::from("/tmp/workspace-kept-gone"))
            .state("leased")
            .lease_id("kept-lease")
            .build(),
    ];
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    ));
    let repo = CubeRepoHandle { repo_id: "mono".into() };
    let next = db.get_execution(&next.id).unwrap();
    let lease = coordinator
        .lease_workspace_with_fallback_for_test(&next, "worker", &repo, "task", &coordinator.host_adapter())
        .await
        .expect("an unpreservable lease must not block leasing a fresh workspace");
    assert_ne!(lease.lease_id, "kept-lease");
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["kept-lease"],
        "the lease goes back to cube even though its workspace could not be preserved"
    );
    let after = db.get_execution(&next.id).unwrap();
    assert_ne!(after.cube_lease_id.as_deref(), Some("kept-lease"));
    assert!(
        db.list_attention_items(&next.id)
            .unwrap()
            .iter()
            .any(|item| item.kind == "preserved_work_needs_inspection" && item.status == "open"),
        "the unpreserved workspace is reported, not silently released"
    );
}
