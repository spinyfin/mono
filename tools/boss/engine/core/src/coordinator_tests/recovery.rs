//! Shared-store recovery and clean scratch leasing, including lease failures.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

/// Seed an orphaned predecessor and a resume of the same work item.
fn seed_resume_pair(db: &Arc<WorkDb>) -> (String, WorkExecution) {
    let product = create_test_product(db);
    let chore = create_test_chore_manual(db, product.id.clone(), "Recover me");
    db.reconcile_product_executions(&product.id).unwrap();
    db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let dead_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    db.start_execution_run(
        &dead_id,
        "agent-dead",
        "mono",
        "lease-dead",
        "mono-agent-003",
        "/tmp/mono-agent-003",
    )
    .unwrap();
    db.mark_execution_orphaned(&dead_id, "engine crash").unwrap();
    let resume = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .preferred_workspace_id("mono-agent-003")
                .allow_dirty(true)
                .build(),
        )
        .unwrap();
    (dead_id, resume)
}

fn recovery_coordinator(db: Arc<WorkDb>) -> Arc<ExecutionCoordinator> {
    Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    ))
}

#[tokio::test]
async fn merge_cancel_workspace_preference_is_ignored_without_dirty_reuse() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Continue review followup");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .preferred_workspace_id("mono-agent-003")
                .allow_dirty(true)
                .prefer_is_soft(true)
                .build(),
        )
        .unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_lease_when_prefer_set: true,
        next_workspace_id: Mutex::new(Some("mono-agent-004".to_owned())),
        ..FakeCubeClient::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    ));
    let repo = CubeRepoHandle {
        repo_id: "mono".to_owned(),
    };

    let lease = coordinator
        .lease_workspace_with_fallback(
            &execution,
            "worker-followup",
            &repo,
            "continue-review",
            &coordinator.host_adapter,
        )
        .await
        .expect("a merge-cancel preference is soft when its workspace is unavailable");

    assert_eq!(lease.workspace_id, "mono-agent-004");
    let calls = cube.lease_calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].2.is_none(), "recovery must not prefer the previous workspace");
    assert!(!calls[0].3, "the lease must start clean");
}

/// A revision resumes into clean scratch even when its prior workspace is held.
#[tokio::test]
async fn revision_resume_uses_fresh_scratch_without_workspace_pinning() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Resume revision");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .preferred_workspace_id("mono-agent-003")
                .allow_dirty(true)
                .prefer_is_soft(true)
                .build(),
        )
        .unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_lease_when_prefer_set: true,
        next_workspace_id: Mutex::new(Some("mono-agent-004".to_owned())),
        ..FakeCubeClient::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    ));
    let repo = CubeRepoHandle {
        repo_id: "mono".to_owned(),
    };

    let result = coordinator
        .lease_workspace_with_fallback(
            &execution,
            "worker-resume",
            &repo,
            "resume-revision",
            &coordinator.host_adapter,
        )
        .await;

    assert!(
        result.is_ok(),
        "a revision resume leases clean scratch and resolves recovery separately"
    );
    let calls = cube.lease_calls.lock().await;
    assert_eq!(calls.len(), 1, "one clean lease request should run");
}

async fn record_recovery_work(db: &WorkDb, id: &str, workspace: &std::path::Path) {
    use boss_engine_recovery::execution_bookmark::{LocalJj, create};
    let record = create(&LocalJj, workspace, id, "local").await.unwrap();
    db.record_execution_bookmark(&record).unwrap();
}

fn lease_for(workspace_path: &std::path::Path, dirty_verified: Option<bool>) -> CubeWorkspaceLease {
    CubeWorkspaceLease {
        lease_id: "lease-resume".into(),
        workspace_id: "replacement".into(),
        workspace_path: workspace_path.to_path_buf(),
        dirty_verified,
    }
}

#[tokio::test]
async fn recovery_uses_bookmark_without_replaying_a_legacy_patch() {
    use boss_engine_test_git::jj::JjRepo;
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, resume) = seed_resume_pair(&db);
    record_recovery_work(&db, &dead_id, &repo.worker).await;
    std::fs::write(repo.worker.join("hello.txt"), "recovered work\n").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let patch = dir.path().join(format!("{dead_id}.patch"));
    std::fs::write(&patch, "obsolete patch, must never replay").unwrap();
    let coordinator = recovery_coordinator(db);
    let restored = coordinator
        .recover_execution_bookmark(
            &resume,
            &lease_for(&repo.replacement, Some(true)),
            &coordinator.host_adapter,
        )
        .await
        .unwrap();
    assert_eq!(restored, Some((dead_id, true)));
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("hello.txt")).unwrap(),
        "recovered work\n"
    );
    assert!(
        patch.exists(),
        "legacy evidence is retained, never replayed or consumed"
    );
}

#[tokio::test]
async fn recovery_uses_shared_store_when_cube_recovered_nothing() {
    use boss_engine_test_git::jj::JjRepo;
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, resume) = seed_resume_pair(&db);
    record_recovery_work(&db, &dead_id, &repo.worker).await;
    std::fs::write(repo.worker.join("hello.txt"), "recovered work\n").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    JjRepo::run(&repo.worker, &["new", "root()", "-m", "Unrelated lease"]);
    std::fs::write(repo.worker.join("foreign.txt"), "foreign work").unwrap();
    let coordinator = recovery_coordinator(db);
    let restored = coordinator
        .recover_execution_bookmark(
            &resume,
            &lease_for(&repo.replacement, Some(false)),
            &coordinator.host_adapter,
        )
        .await
        .unwrap();
    assert_eq!(restored, Some((dead_id, true)));
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("hello.txt")).unwrap(),
        "recovered work\n"
    );
    assert!(!repo.replacement.join("foreign.txt").exists());
    assert_eq!(
        std::fs::read_to_string(repo.worker.join("foreign.txt")).unwrap(),
        "foreign work"
    );
}

#[tokio::test]
async fn a_failed_bookmark_recovery_is_loud_and_legacy_evidence_is_kept() {
    use boss_engine_test_git::jj::JjRepo;
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, resume) = seed_resume_pair(&db);
    record_recovery_work(&db, &dead_id, &repo.worker).await;
    JjRepo::run(&repo.repo, &["bookmark", "delete", &format!("boss-recovery/{dead_id}")]);
    let patch = dir.path().join(format!("{dead_id}.patch"));
    std::fs::write(&patch, "legacy evidence").unwrap();
    let coordinator = recovery_coordinator(db);
    let error = coordinator
        .recover_execution_bookmark(&resume, &lease_for(&repo.replacement, None), &coordinator.host_adapter)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exactly one"), "{error:#}");
    assert!(patch.exists());
    assert!(!repo.replacement.join("hello.txt").exists());
}

#[tokio::test]
async fn bookkeeping_only_work_is_not_reported_as_a_recovery() {
    use boss_engine_test_git::jj::JjRepo;
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, resume) = seed_resume_pair(&db);
    record_recovery_work(&db, &dead_id, &repo.worker).await;
    std::fs::create_dir(repo.worker.join(".boss")).unwrap();
    std::fs::write(repo.worker.join(".boss/events-pending.jsonl"), "bookkeeping").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let coordinator = recovery_coordinator(db);
    let restored = coordinator
        .recover_execution_bookmark(&resume, &lease_for(&repo.replacement, None), &coordinator.host_adapter)
        .await
        .unwrap();
    assert_eq!(restored, Some((dead_id, false)));
}

#[tokio::test]
async fn recovery_is_a_no_op_for_a_non_resume_dispatch() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, _) = seed_resume_pair(&db);
    let patch = dir.path().join(format!("{dead_id}.patch"));
    std::fs::write(&patch, "unrelated evidence").unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "Fresh work");
    let fresh = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id).build())
        .unwrap();
    let coordinator = recovery_coordinator(db);
    let restored = coordinator
        .recover_execution_bookmark(&fresh, &lease_for(dir.path(), None), &coordinator.host_adapter)
        .await
        .unwrap();
    assert!(restored.is_none());
    assert!(patch.exists());
}

/// A stale lease no longer needs to be reclaimed to recover the execution.
#[tokio::test]
async fn resume_does_not_reclaim_stale_lease_for_recovery() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Resume me");
    db.reconcile_product_executions(&product.id).unwrap();
    // autostart=false means reconcile won't auto-create an execution;
    // request one explicitly to seed the dead-predecessor record.
    db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();

    // Dead predecessor: started a run on mono-agent-003 with
    // lease-dead, then orphaned (lease columns preserved).
    let dead_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    db.start_execution_run(
        &dead_id,
        "agent-dead",
        "mono",
        "lease-dead",
        "mono-agent-003",
        "/tmp/mono-agent-003",
    )
    .unwrap();
    db.mark_execution_orphaned(&dead_id, "ui crash").unwrap();

    // Legacy workspace affinity must not influence the recovery lease.
    let resume = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .preferred_workspace_id("mono-agent-003")
                .build(),
        )
        .unwrap();

    // Cube reports mono-agent-003 still leased to the dead lease.
    let cube = Arc::new(FakeCubeClient::default().with_list_workspaces(vec![
            CubeWorkspaceStatus::builder()
                .workspace_id("mono-agent-003")
                .workspace_path(PathBuf::from("/tmp/mono-agent-003"))
                .state("leased")
                .lease_id("lease-dead")
                .holder("dead@host:1")
                .task("resume")
                .leased_at_epoch_s(1_700_000_000)
                .build(),
        ]));
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    ));
    let repo = CubeRepoHandle {
        repo_id: "mono".to_owned(),
    };

    let result = coordinator
        .lease_workspace_with_fallback(&resume, "worker-resume", &repo, "task", &coordinator.host_adapter)
        .await;
    assert!(result.is_ok(), "resume lease should succeed after reclaim");

    // Recovery leaves the previous lease alone.
    let releases = cube.force_release_calls.lock().await;
    assert!(releases.is_empty(), "recovery must not reclaim the old lease");
    drop(releases);

    // Request ordinary clean scratch without a workspace preference.
    let calls = cube.lease_calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].2.is_none(), "no workspace preference is sent");
}

/// A foreign lease is never queried or force-released for work recovery.
#[tokio::test]
async fn hard_prefer_resume_does_not_reclaim_unowned_lease() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Resume me");
    db.reconcile_product_executions(&product.id).unwrap();
    let resume = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .preferred_workspace_id("mono-agent-007")
                .build(),
        )
        .unwrap();

    // Cube reports the workspace leased to a lease the engine has no
    // terminal execution record for.
    let cube = Arc::new(FakeCubeClient::default().with_list_workspaces(vec![
            CubeWorkspaceStatus::builder()
                .workspace_id("mono-agent-007")
                .workspace_path(PathBuf::from("/tmp/mono-agent-007"))
                .state("leased")
                .lease_id("lease-unknown")
                .holder("someone@host:9")
                .task("other")
                .leased_at_epoch_s(1_700_000_000)
                .build(),
        ]));
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    ));
    let repo = CubeRepoHandle {
        repo_id: "mono".to_owned(),
    };

    let _ = coordinator
        .lease_workspace_with_fallback(&resume, "worker-resume", &repo, "task", &coordinator.host_adapter)
        .await;

    let releases = cube.force_release_calls.lock().await;
    assert!(releases.is_empty(), "must not reclaim a lease the engine doesn't own",);
}

/// When `preferred_workspace_id=null` and cube fails the first workspace
/// (e.g. because it has uncommitted work from a prior crashed lease),
/// the engine must retry with `any_free` policy and land on the second
/// workspace. This pins the fix for the 2026-05-12 dispatch failure
/// where a single bad workspace blocked dispatch despite 12+ free ones.
#[tokio::test]
async fn lease_falls_back_when_no_prefer_and_first_workspace_refused() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();

    // First lease call fails (simulating a workspace with uncommitted
    // work refusing the reset); second call succeeds on a different
    // workspace.
    let cube = Arc::new(FakeCubeClient {
        fail_first_n_leases: 1,
        ..FakeCubeClient::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_dispatch_events(recording.clone()),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;

    // Two cube lease invocations: first fails, second succeeds.
    let calls = cube.lease_calls.lock().await;
    assert_eq!(
        calls.len(),
        2,
        "engine must retry on any_free when no prefer set; got {:?}",
        calls
    );
    // Both calls have no --prefer (engine retries with same strategy).
    assert_eq!(calls[0].2, None);
    assert_eq!(calls[1].2, None);
    drop(calls);

    let events = recording.events_for(&execution_id).await;
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();

    // Timeline: attempted #1 → failed #1 → attempted #2 → leased.
    let attempt_events: Vec<&crate::dispatch_events::DispatchEvent> = events
        .iter()
        .filter(|e| e.stage == "cube_workspace_lease_attempted")
        .collect();
    assert_eq!(
        attempt_events.len(),
        2,
        "expected two lease_attempted events (initial + any_free retry); got stages {stages:?}"
    );
    assert_eq!(
        attempt_events[0]
            .details
            .get("fallback_policy")
            .and_then(|v| v.as_str()),
        Some("any_free"),
        "first attempt must carry any_free policy when no prefer set",
    );
    assert!(
        attempt_events[0]
            .details
            .get("prefer_workspace_id")
            .map(|v| v.is_null())
            .unwrap_or(false),
        "first attempt must have prefer_workspace_id=null; got {:?}",
        attempt_events[0].details,
    );
    assert_eq!(
        attempt_events[1]
            .details
            .get("fallback_policy")
            .and_then(|v| v.as_str()),
        Some("none"),
        "retry attempt has no further fallback",
    );

    let failed_events: Vec<&crate::dispatch_events::DispatchEvent> = events
        .iter()
        .filter(|e| e.stage == "cube_workspace_lease_failed")
        .collect();
    assert_eq!(
        failed_events.len(),
        1,
        "exactly one lease_failed event for the first attempt; got stages {stages:?}"
    );

    // Final state: a successful `cube_workspace_leased` event.
    let leased = events
        .iter()
        .find(|e| e.stage == "cube_workspace_leased")
        .expect("cube_workspace_leased event missing after any_free retry");
    assert_eq!(leased.outcome, "ok");

    // No attention item — the fallback succeeded.
    let attention_items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        attention_items.iter().all(|a| a.kind != "cube_workspace_lease_failed"),
        "any_free success must not raise a lease-failure attention item; got {attention_items:?}",
    );
}

/// When `preferred_workspace_id=null` and both lease attempts fail, the
/// execution must transition to `failed` with both
/// `cube_workspace_lease_failed` events visible — silent wait is not OK.
#[tokio::test]
async fn lease_fallback_failure_transitions_execution_to_failed() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_lease: true,
        ..FakeCubeClient::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    // No retries: go straight to permanent failure to keep the event
    // count assertions (2 attempts, 2 failures) unambiguous.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![])
        .with_dispatch_events(recording.clone()),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

    let events = recording.events_for(&execution_id).await;
    let attempt_count = events
        .iter()
        .filter(|e| e.stage == "cube_workspace_lease_attempted")
        .count();
    let failed_count = events
        .iter()
        .filter(|e| e.stage == "cube_workspace_lease_failed")
        .count();
    assert_eq!(
        attempt_count, 2,
        "expected initial + any_free retry attempt events; got {events:?}"
    );
    assert_eq!(
        failed_count, 2,
        "expected one lease_failed event per attempt; got {events:?}"
    );

    let attention_items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        attention_items.len(),
        1,
        "terminal lease failure must raise exactly one attention item",
    );
    assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
}

#[tokio::test]
async fn change_creation_failure_marks_execution_failed_and_releases_workspace() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_create: true,
        ..FakeCubeClient::default()
    });
    // No retries: go straight to permanent failure to keep the
    // release_calls assertion (exactly "lease-1") unambiguous.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![]),
    );
    coordinator.kick();
    wait_for_execution_status(
        db.as_ref(),
        &db.list_executions(Some(&chore.id)).unwrap()[0].id,
        ExecutionStatus::Failed,
    )
    .await;

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    assert_eq!(execution.status, ExecutionStatus::Failed);
    let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
    assert_eq!(run.status, "failed");
    assert_eq!(run.error_text.as_deref(), Some("cube change create failed"));
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert_eq!(coordinator.worker_pool().idle_count().await, 1);
}

/// Mid-spawn cancel regression — the coordinator's mid-spawn cancel handling.
/// When the runner reports `CancelledDuringSpawn` (it reaped the
/// just-spawned pane), the coordinator must release the cube lease
/// the cancel path deliberately left held, and must NOT drive the
/// row to `waiting_human` (the row is already terminal). This is the
/// downstream half of "the lease is not released until the process
/// exits": the in-flight run is the sole releaser for a mid-spawn
/// cancel.
#[tokio::test]
async fn cancelled_during_spawn_releases_lease_and_skips_completion() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Sort struct definitions");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        cancelled_during_spawn: true,
        work_db: Some(db.clone()),
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner)
            .with_pre_start_retry_delays(vec![]),
    );
    coordinator.kick();

    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    // The runner cancels the row inside the spawn; wait for that
    // terminal status to settle.
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Cancelled).await;

    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Cancelled,
        "the row stays cancelled — the coordinator must not move it to waiting_human",
    );
    // The deferred lease must have been released exactly once, and
    // the row's lease columns cleared (ownership claimed atomically).
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "the deferred cube lease must be released after the mid-spawn cancel",
    );
    assert!(
        execution.cube_lease_id.is_none(),
        "lease columns must be cleared once the deferred lease is released",
    );
    // The pool slot is returned so dispatch can proceed.
    assert_eq!(coordinator.worker_pool().idle_count().await, 1);
}

/// Engine restart between `run_started` and pane spawn: the execution is
/// already `running` with a lease, so `schedule_execution` cannot be
/// re-entered. `resume_pane_spawn_for_running_execution` must hand the
/// existing row back to the runner without re-leasing.
#[tokio::test]
async fn resume_pane_spawn_reenters_the_runner_for_a_running_leased_execution() {
    let (_dir, db) = open_db_arc();
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "stranded");
    db.reconcile_product_executions(&product.id).unwrap();
    db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let exec = db.list_executions(Some(&chore.id)).unwrap().into_iter().next().unwrap();
    let (exec, _run) = db
        .start_execution_run(
            &exec.id,
            "worker-1",
            "mono",
            "lease-stranded",
            "ws-stranded",
            "/tmp/ws-stranded",
        )
        .unwrap();

    let runner = Arc::new(FakeExecutionRunner {
        slot_id: Some(1),
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(2),
        Arc::new(FakeCubeClient::default()),
        runner.clone(),
    ));

    let err = coordinator
        .resume_pane_spawn_for_running_execution(&exec)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no engine-created recovery bookmark"));
    assert!(runner.calls.lock().await.is_empty());
    assert!(
        db.list_attention_items(&exec.id)
            .unwrap()
            .iter()
            .any(|a| a.kind == crate::execution_bookmark_recovery::RECOVERY_FAILED)
    );
    let _bookmark_store = crate::test_support::seed_empty_execution_bookmark(&db, &exec.id).await;
    coordinator
        .resume_pane_spawn_for_running_execution(&exec)
        .await
        .expect("resume must accept a running leased execution with its recorded bookmark");

    let mut saw_call = false;
    for _ in 0..100 {
        let calls = runner.calls.lock().await;
        if calls.iter().any(|(_, id, _, _)| id == &exec.id) {
            saw_call = true;
            break;
        }
        drop(calls);
        sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_call, "the runner must be invoked for the already-running execution");
    assert_eq!(db.get_execution(&exec.id).unwrap().status, ExecutionStatus::Running);
    assert_eq!(
        db.get_execution(&exec.id).unwrap().cube_lease_id.as_deref(),
        Some("lease-stranded"),
        "resume must keep the already-adopted lease"
    );
}
