//! Workspace recovery and lease fallback on resume: cube in-place recovery,
//! recovery-patch replay, stale-lease reclaim, and fallback exhaustion.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

/// `BOSS_RECOVERY_DIR` is process-global, so the recovery tests
/// serialise on this lock rather than racing each other's tempdirs.
fn recovery_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Seed a chore with a dead predecessor execution (orphaned, so
/// `get_prior_orphaned_execution` finds it) plus a live resume execution
/// carrying `allow_dirty`. Returns `(dead_execution_id, resume_execution)`.
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

/// A git repo with one committed file, so `git apply --3way` has a blob
/// to three-way against.
fn init_recovery_workspace(path: &std::path::Path) -> bool {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !run(&["init", "--initial-branch=main"]) {
        return false;
    }
    let _ = run(&["config", "user.email", "t@example.com"]);
    let _ = run(&["config", "user.name", "T"]);
    std::fs::write(path.join("hello.txt"), "original\n").unwrap();
    run(&["add", "."]) && run(&["commit", "-m", "seed"])
}

fn lease_for(workspace_path: &std::path::Path, dirty_verified: Option<bool>) -> CubeWorkspaceLease {
    CubeWorkspaceLease {
        lease_id: "lease-resume".to_owned(),
        workspace_id: "mono-agent-003".to_owned(),
        workspace_path: workspace_path.to_path_buf(),
        dirty_verified,
    }
}

const RECOVERY_PATCH: &str = "diff --git a/hello.txt b/hello.txt\n\
                                  index 0000000..1111111 100644\n\
                                  --- a/hello.txt\n\
                                  +++ b/hello.txt\n\
                                  @@ -1 +1 @@\n\
                                  -original\n\
                                  +recovered work\n";

/// Cube recovered the tree in place. The patch must NOT be applied on top
/// — the hunks are already there, and a second application either
/// duplicates them or conflicts. The worker is still told it is resuming.
// The env guard is a std Mutex held across awaits. clippy flags that
// shape because it can block an executor thread, but here it is the
// point: `BOSS_RECOVERY_DIR` is process-global, each `#[tokio::test]`
// gets its own current-thread runtime, and serialising these tests on
// one thread is exactly the intended behaviour.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn recovery_prefers_cube_in_place_and_does_not_replay_the_patch() {
    let _guard = recovery_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The "recovered in place" content cube handed back.
    std::fs::write(ws.join("hello.txt"), "recovered work\n").unwrap();
    let recovery_dir = dir.path().join("recovery");
    std::fs::create_dir_all(&recovery_dir).unwrap();
    unsafe { std::env::set_var(crate::recovery_backup::RECOVERY_DIR_ENV, &recovery_dir) };

    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, resume) = seed_resume_pair(&db);
    let patch = recovery_dir.join(format!("{dead_id}.patch"));
    std::fs::write(&patch, RECOVERY_PATCH).unwrap();

    let coordinator = recovery_coordinator(db);
    coordinator
        .reconcile_workspace_recovery(&resume, "worker-1", &lease_for(&ws, Some(true)))
        .await;

    let report = crate::recovery_apply::RecoveryReport::read_for(&ws, &resume.id)
        .expect("an in-place recovery must still be reported to the worker");
    assert_eq!(report.source, crate::recovery_apply::RecoverySource::CubeInPlace);
    assert_eq!(report.from_execution_id, dead_id);
    assert!(report.applied.is_none(), "nothing was replayed");
    assert!(report.patch_error.is_none());
    // The file is untouched — the patch would have failed to apply here
    // anyway (its pre-image is `original`), so a silent apply attempt
    // would have surfaced as an error report.
    assert_eq!(
        std::fs::read_to_string(ws.join("hello.txt")).unwrap(),
        "recovered work\n"
    );
    // P4: consumed, so a later restart does not replay it.
    assert!(!patch.exists());
    assert!(recovery_dir.join(format!("{dead_id}.patch.applied")).exists());

    unsafe { std::env::remove_var(crate::recovery_backup::RECOVERY_DIR_ENV) };
}

/// Cube could NOT recover (`dirty_verified: false` — the tree had already
/// been reset). Now, and only now, the patch is replayed.
// The env guard is a std Mutex held across awaits. clippy flags that
// shape because it can block an executor thread, but here it is the
// point: `BOSS_RECOVERY_DIR` is process-global, each `#[tokio::test]`
// gets its own current-thread runtime, and serialising these tests on
// one thread is exactly the intended behaviour.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn recovery_falls_back_to_the_patch_when_cube_recovered_nothing() {
    let _guard = recovery_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    if !init_recovery_workspace(&ws) {
        eprintln!("skipping: git unavailable in sandbox");
        return;
    }
    let recovery_dir = dir.path().join("recovery");
    std::fs::create_dir_all(&recovery_dir).unwrap();
    unsafe { std::env::set_var(crate::recovery_backup::RECOVERY_DIR_ENV, &recovery_dir) };

    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, resume) = seed_resume_pair(&db);
    let patch = recovery_dir.join(format!("{dead_id}.patch"));
    std::fs::write(&patch, RECOVERY_PATCH).unwrap();

    let coordinator = recovery_coordinator(db);
    coordinator
        .reconcile_workspace_recovery(&resume, "worker-1", &lease_for(&ws, Some(false)))
        .await;

    assert_eq!(
        std::fs::read_to_string(ws.join("hello.txt")).unwrap(),
        "recovered work\n",
        "the patch must actually restore the work into the workspace",
    );
    let report = crate::recovery_apply::RecoveryReport::read_for(&ws, &resume.id).expect("report");
    assert_eq!(report.source, crate::recovery_apply::RecoverySource::Patch);
    let applied = report.applied.expect("a patch recovery must report what it restored");
    assert_eq!(applied.paths, ["hello.txt"]);
    assert_eq!((applied.insertions, applied.deletions), (1, 1));
    assert!(
        !patch.exists(),
        "a consumed patch must not be replayed on a later restart"
    );

    unsafe { std::env::remove_var(crate::recovery_backup::RECOVERY_DIR_ENV) };
}

/// A patch that does not apply must be loud: the worker is told recovery
/// FAILED, and the patch is deliberately NOT consumed because it is the
/// only remaining copy of that work.
// The env guard is a std Mutex held across awaits. clippy flags that
// shape because it can block an executor thread, but here it is the
// point: `BOSS_RECOVERY_DIR` is process-global, each `#[tokio::test]`
// gets its own current-thread runtime, and serialising these tests on
// one thread is exactly the intended behaviour.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn a_failed_patch_apply_is_reported_and_the_patch_is_kept() {
    let _guard = recovery_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    if !init_recovery_workspace(&ws) {
        eprintln!("skipping: git unavailable in sandbox");
        return;
    }
    let recovery_dir = dir.path().join("recovery");
    std::fs::create_dir_all(&recovery_dir).unwrap();
    unsafe { std::env::set_var(crate::recovery_backup::RECOVERY_DIR_ENV, &recovery_dir) };

    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, resume) = seed_resume_pair(&db);
    let patch = recovery_dir.join(format!("{dead_id}.patch"));
    // Pre-image that exists nowhere, and a blob id git cannot resolve, so
    // neither the direct apply nor the 3-way fallback can succeed.
    std::fs::write(
        &patch,
        "diff --git a/absent.txt b/absent.txt\n\
             index deadbee..cafebab 100644\n\
             --- a/absent.txt\n\
             +++ b/absent.txt\n\
             @@ -1 +1 @@\n\
             -never here\n\
             +replacement\n",
    )
    .unwrap();

    let coordinator = recovery_coordinator(db);
    coordinator
        .reconcile_workspace_recovery(&resume, "worker-1", &lease_for(&ws, Some(false)))
        .await;

    let report = crate::recovery_apply::RecoveryReport::read_for(&ws, &resume.id)
        .expect("a failed recovery must still be reported — silence would let the worker assume success");
    assert!(report.applied.is_none());
    let err = report.patch_error.expect("the failure must be carried to the worker");
    assert!(err.contains("git apply --3way"), "error should name the command: {err}");
    assert!(
        patch.exists(),
        "the only copy of the work must survive a failed apply for manual salvage",
    );
    assert!(!recovery_dir.join(format!("{dead_id}.patch.applied")).exists());
    let _ = dead_id;

    unsafe { std::env::remove_var(crate::recovery_backup::RECOVERY_DIR_ENV) };
}

/// A patch of nothing but Boss's own hook spool restores nothing, and
/// must not be reported to the worker as a recovery.
// The env guard is a std Mutex held across awaits. clippy flags that
// shape because it can block an executor thread, but here it is the
// point: `BOSS_RECOVERY_DIR` is process-global, each `#[tokio::test]`
// gets its own current-thread runtime, and serialising these tests on
// one thread is exactly the intended behaviour.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn a_bookkeeping_only_patch_is_not_reported_as_a_recovery() {
    let _guard = recovery_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let recovery_dir = dir.path().join("recovery");
    std::fs::create_dir_all(&recovery_dir).unwrap();
    unsafe { std::env::set_var(crate::recovery_backup::RECOVERY_DIR_ENV, &recovery_dir) };

    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, resume) = seed_resume_pair(&db);
    let patch = recovery_dir.join(format!("{dead_id}.patch"));
    std::fs::write(
        &patch,
        "diff --git a/.boss/events-pending.jsonl b/.boss/events-pending.jsonl\n\
             --- a/.boss/events-pending.jsonl\n\
             +++ b/.boss/events-pending.jsonl\n\
             @@ -0,0 +1 @@\n\
             +{\"event\":\"Stop\"}\n",
    )
    .unwrap();

    let coordinator = recovery_coordinator(db);
    coordinator
        .reconcile_workspace_recovery(&resume, "worker-1", &lease_for(&ws, Some(false)))
        .await;

    assert!(
        crate::recovery_apply::RecoveryReport::read_for(&ws, &resume.id).is_none(),
        "a bookkeeping-only patch restores nothing and must not claim a recovery",
    );
    assert!(!patch.exists(), "the spent patch is still retired");

    unsafe { std::env::remove_var(crate::recovery_backup::RECOVERY_DIR_ENV) };
}

/// A normal (non-resume) dispatch must not touch the recovery machinery
/// at all — no marker, no consumed patch.
// The env guard is a std Mutex held across awaits. clippy flags that
// shape because it can block an executor thread, but here it is the
// point: `BOSS_RECOVERY_DIR` is process-global, each `#[tokio::test]`
// gets its own current-thread runtime, and serialising these tests on
// one thread is exactly the intended behaviour.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn recovery_is_a_no_op_for_a_non_resume_dispatch() {
    let _guard = recovery_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let recovery_dir = dir.path().join("recovery");
    std::fs::create_dir_all(&recovery_dir).unwrap();
    unsafe { std::env::set_var(crate::recovery_backup::RECOVERY_DIR_ENV, &recovery_dir) };

    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (dead_id, _resume) = seed_resume_pair(&db);
    let patch = recovery_dir.join(format!("{dead_id}.patch"));
    std::fs::write(&patch, RECOVERY_PATCH).unwrap();

    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Fresh work");
    db.reconcile_product_executions(&product.id).unwrap();
    let fresh = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    assert!(!fresh.allow_dirty, "a fresh dispatch is not a resume");

    let coordinator = recovery_coordinator(db);
    coordinator
        .reconcile_workspace_recovery(&fresh, "worker-1", &lease_for(&ws, None))
        .await;

    assert!(crate::recovery_apply::RecoveryReport::read_for(&ws, &fresh.id).is_none());
    assert!(patch.exists(), "an unrelated execution's patch must be left alone");

    unsafe { std::env::remove_var(crate::recovery_backup::RECOVERY_DIR_ENV) };
}

/// Issue #962 -- the UI-crash resume reclaims a stale lease.
///
/// A prior worker (the dead execution) was leased into
/// `mono-agent-003` and then orphaned by the startup reaper, which
/// preserved its `cube_lease_id` / `cube_workspace_id`. Cube still
/// reports that workspace `leased` to the dead `lease-dead`. When the
/// hard-prefer resume dispatches, the coordinator must force-release
/// the dead lease first so the `--prefer` re-lease can succeed and
/// recover the in-flight checkout -- instead of failing the resume
/// and stranding the local work.
#[tokio::test]
async fn hard_prefer_resume_reclaims_stale_lease_then_leases() {
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

    // Resume execution: hard prefer back onto mono-agent-003.
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

    // The dead lease was force-released exactly once.
    let releases = cube.force_release_calls.lock().await;
    assert_eq!(releases.len(), 1, "stale lease must be reclaimed once");
    assert_eq!(releases[0].0, "lease-dead");
    drop(releases);

    // The prefer lease was then issued for the same workspace.
    let calls = cube.lease_calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2.as_deref(), Some("mono-agent-003"));
}

/// Safety: a hard-prefer resume must NOT force-release a lease that
/// cube reports holding a workspace the engine has no terminal
/// record for (e.g. a genuinely live worker in another slot). The
/// reclaim probe runs but finds nothing eligible, so the lease
/// attempt proceeds without any force-release.
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
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;

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

/// T981 regression — the coordinator's mid-spawn cancel handling.
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
