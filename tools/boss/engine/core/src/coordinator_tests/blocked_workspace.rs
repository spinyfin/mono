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
    db.record_worker_idle_abandonment(&prior.id, "needs a decision")
        .unwrap();
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
async fn blocked_revision_reclaims_only_verified_prior_identity_and_falls_back_softly() {
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
        assert_eq!(calls[0].2.as_deref(), Some("workspace-old"), "{scenario}");
        assert!(calls[0].3);
        if scenario == "valid" {
            assert_eq!(lease.workspace_id, "workspace-old");
            assert_eq!(calls.len(), 1);
            assert!(cube.release_calls.lock().await.is_empty());
        } else {
            assert_ne!(lease.workspace_id, "workspace-old", "{scenario}");
            assert_eq!(calls.len(), 2, "{scenario}");
            assert!(calls[1].2.is_none());
            assert!(!calls[1].3);
            if scenario != "leased" {
                assert!(calls[1].4.contains(&"workspace-old".to_owned()));
                assert_eq!(cube.release_calls.lock().await.len(), 1);
            }
        }
    }
}

#[tokio::test]
async fn blocked_revision_retry_after_deferral_release_trusts_its_own_recovery_marker() {
    // A post-lease deferral (e.g. the chain-sibling guard in
    // `schedule_execution`) releases the lease it just took to hand the
    // workspace back. `release_workspace` sets `last_task =
    // COALESCE(task, last_task)`, and `task` was stamped at lease time with
    // THIS execution's own id (`execution_task_summary`), not `prior.id` —
    // so that release silently overwrites the `prior.id` marker
    // `verify_blocked_workspace`'s `last_task` check depends on, even
    // though the workspace itself never changed. On a retry, cube's
    // `last_task` no longer proves the identity — but the on-disk
    // `RecoveryReport` this same execution wrote on its first successful
    // verification does, and must be trusted instead.
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
    // This execution already verified this exact workspace on an earlier
    // dispatch attempt and recorded it in the on-disk marker.
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
    assert_eq!(lease.workspace_id, "workspace-old");
    let calls = cube.lease_calls.lock().await;
    assert_eq!(calls.len(), 1, "must not fall back to a fresh workspace");
    assert!(cube.release_calls.lock().await.is_empty());
}

#[tokio::test]
async fn blocked_recovery_report_distinguishes_fresh_checkout_without_patch_replay() {
    use boss_engine_recovery::recovery_apply::{RecoveryReport, RecoverySource};
    for recovered in [false, true] {
        let dir = tempdir().unwrap();
        let (db, prior, next) = blocked_pair(&dir.path().join("boss.db"));
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(1),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner::default()),
        ));
        let lease = CubeWorkspaceLease {
            lease_id: "lease-new".into(),
            workspace_id: if recovered { "workspace-old" } else { "fresh" }.into(),
            workspace_path: dir.path().to_path_buf(),
            dirty_verified: recovered.then_some(true),
        };
        coordinator.reconcile_workspace_recovery(&next, "worker", &lease).await;
        let report = RecoveryReport::read_for(dir.path(), &next.id).unwrap();
        assert_eq!(report.from_execution_id, prior.id);
        assert_eq!(
            report.source,
            if recovered {
                RecoverySource::BlockedInPlace
            } else {
                RecoverySource::BlockedFresh
            }
        );
        assert!(report.applied.is_none());
    }
}

#[tokio::test]
async fn blocked_revision_dispatch_keeps_verified_checkout_and_positions_fresh_fallback() {
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
        let workspace = dir.path().join("workspace-old");
        std::fs::create_dir_all(&workspace).unwrap();
        let cube = Arc::new(FakeCubeClient {
            fail_lease_when_prefer_set: !recovered,
            dirty_verified: Some(true),
            workspace_root: Some(dir.path().to_path_buf()),
            recovery_status: Some(
                CubeWorkspaceStatus::builder()
                    .workspace_id("workspace-old")
                    .workspace_path(workspace)
                    .state("leased")
                    .lease_id("lease-1")
                    .last_task(format!("{} revision_implementation Blocked revision", prior.id))
                    .build(),
            ),
            ..FakeCubeClient::default()
        });
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(db, WorkerPool::new(1), cube.clone(), runner));
        let worker = coordinator
            .pool_for_execution(&next)
            .claim_worker(&next.id, None)
            .await
            .unwrap();
        coordinator
            .schedule_execution(&next, &worker, DispatchAdmission::Queued)
            .await
            .unwrap();
        assert_eq!(cube.goto_calls.lock().await.len(), usize::from(!recovered));
        assert!(cube.create_calls.lock().await.is_empty());
        assert!(cube.lease_calls.lock().await[0].1.starts_with(&format!("{} ", next.id)));
    }
}
