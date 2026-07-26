//! The `pr_review` pool and the operator/breaker/automation pause gates.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

/// A `pr_review` execution must route to the review pool; a normal
/// chore execution must continue to route to the main pool. Review is
/// checked before automation so the reviewer of an automation-produced
/// task still lands in the review pool.
#[tokio::test]
async fn pr_review_execution_routes_to_review_pool() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    );
    coord.set_review_pool(WorkerPool::new_review(1));
    coord.set_automation_pool(WorkerPool::new_automation(1));

    let review_exec = WorkExecution::builder()
        .id("exec-review")
        .work_item_id("task-under-review")
        .created_at("1")
        .kind(ExecutionKind::PrReview)
        .repo_remote_url("git@github.com:spinyfin/mono.git")
        .status(ExecutionStatus::Ready)
        .build();
    assert!(coord.execution_targets_review_pool(&review_exec));

    // pool_for_execution must hand back the review pool — claiming from
    // it yields a `review-` worker id.
    let wid = coord
        .pool_for_execution(&review_exec)
        .claim_worker("exec-review", None)
        .await
        .unwrap();
    assert!(
        wid.starts_with(REVIEW_WORKER_ID_PREFIX),
        "pr_review must route to the review pool, got {wid:?}"
    );

    // A normal chore execution must NOT target the review pool.
    let chore_exec = WorkExecution::builder()
        .id("exec-chore")
        .work_item_id("regular-task")
        .created_at("1")
        .kind(ExecutionKind::ChoreImplementation)
        .repo_remote_url("git@github.com:spinyfin/mono.git")
        .status(ExecutionStatus::Ready)
        .build();
    assert!(!coord.execution_targets_review_pool(&chore_exec));
    let wid2 = coord
        .pool_for_execution(&chore_exec)
        .claim_worker("exec-chore", None)
        .await
        .unwrap();
    assert!(
        wid2.starts_with("worker-"),
        "chore must route to the main pool, got {wid2:?}"
    );
}

/// Releasing a `review-` worker id must free a slot in the review pool
/// (not the main or automation pool). This is the release-routing-by-
/// prefix guarantee `release_worker_and_kick` relies on.
#[tokio::test]
async fn review_prefix_worker_id_releases_to_review_pool() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(2),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    );
    coord.set_review_pool(WorkerPool::new_review(2));
    coord.set_automation_pool(WorkerPool::new_automation(2));
    let coordinator = Arc::new(coord);

    let wid = coordinator
        .review_worker_pool()
        .claim_worker("exec-r", None)
        .await
        .unwrap();
    assert!(wid.starts_with(REVIEW_WORKER_ID_PREFIX));
    assert_eq!(coordinator.review_worker_pool().idle_count().await, 1);

    // Release routes by prefix → the review-pool slot is freed.
    coordinator.release_worker_and_kick(&wid, None).await;
    assert_eq!(
        coordinator.review_worker_pool().idle_count().await,
        2,
        "release must free the review-pool slot"
    );
    // The other pools must be untouched.
    assert_eq!(coordinator.worker_pool().idle_count().await, 2);
    assert_eq!(coordinator.automation_worker_pool().idle_count().await, 2);
}

/// Review pool exhaustion must not block main-pool dispatch. When the
/// review pool is full, a regular chore continues to be dispatched on
/// the main pool and the deferred `pr_review` stays `ready`.
#[tokio::test]
async fn review_pool_exhaustion_does_not_block_main_pool() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    // One regular chore — must still dispatch even when review is full.
    create_test_chore(&db, product.id.clone(), "Regular chore");
    db.reconcile_product_executions(&product.id).unwrap();

    // Insert a ready `pr_review` execution. It never reaches the
    // schedule path in this test — the review pool is pre-occupied, so
    // the claim fails first — so a synthetic work_item_id is fine.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_executions
                   (id, work_item_id, kind, status, repo_remote_url, priority, created_at)
                 VALUES (?1, ?2, ?3, 'ready', ?4, 0, '1')",
            rusqlite::params![
                "exec-review-1",
                "task-under-review",
                EXECUTION_KIND_PR_REVIEW,
                "git@github.com:spinyfin/mono.git"
            ],
        )
        .unwrap();
    }

    let cube = Arc::new(FakeCubeClient::default());
    // Main pool: 1 slot; review pool: 1 slot.
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord);

    // Pre-occupy the review pool's only slot so the pr_review can't claim.
    let occupied = coordinator.review_worker_pool().claim_worker("occupied", None).await;
    assert!(occupied.is_some(), "review pool slot must be claimable");

    coordinator.kick();

    // Wait for the main chore to run.
    for _ in 0..200 {
        let execs = db.list_executions(None).unwrap();
        if execs
            .iter()
            .any(|e| e.status == ExecutionStatus::Running && e.kind != ExecutionKind::PrReview)
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let execs = db.list_executions(None).unwrap();
    let main_running = execs
        .iter()
        .filter(|e| e.status == ExecutionStatus::Running && e.kind != ExecutionKind::PrReview)
        .count();
    assert_eq!(
        main_running, 1,
        "the regular chore must run even when the review pool is full"
    );
    // The pr_review must stay ready — review pool was full.
    let review_ready = execs
        .iter()
        .filter(|e| e.kind == ExecutionKind::PrReview && e.status == ExecutionStatus::Ready)
        .count();
    assert_eq!(
        review_ready, 1,
        "the pr_review must be deferred while the review pool is full"
    );
}

// ── Reviewer workspace positioning tests ──────────────────────────────────

/// When a `pr_review` execution has a non-empty `pr_url` on its task,
/// `schedule_execution` must call `cube workspace goto` after the lease to
/// position the workspace on the PR head, and must NOT call `create_change`.
#[tokio::test]
async fn pr_review_with_pr_url_positions_via_goto_not_create_change() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/42";
    let (_, chore_id) = make_pr_review_fixture(&db, Some(pr_url));

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord);

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("review pool slot available");

    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(result.is_ok(), "schedule_execution must succeed: {result:?}");

    // goto_workspace must have been called with pr=42.
    let goto_calls = cube.goto_calls.lock().await;
    assert_eq!(goto_calls.len(), 1, "goto_workspace must be called exactly once");
    assert_eq!(goto_calls[0].1, 42, "goto_workspace must receive pr=42 for PR #42");
    drop(goto_calls);

    // lease_workspace must NOT have received resume_pr (it no longer exists).
    let lease_calls = cube.lease_calls.lock().await;
    assert_eq!(lease_calls.len(), 1, "lease_workspace must be called exactly once");
    drop(lease_calls);

    // create_change must NOT have been called — positioning happened via goto.
    assert!(
        cube.create_calls.lock().await.is_empty(),
        "create_change must not be called for the reviewer positioning path"
    );
}

/// When the `cube workspace lease` call fails for a `pr_review` execution,
/// `schedule_execution` must record a `cube_workspace_lease_failed` start
/// failure and must not release a workspace (none was acquired).
#[tokio::test]
async fn pr_review_lease_failure_records_start_failure() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/7";
    let (_, chore_id) = make_pr_review_fixture(&db, Some(pr_url));

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_lease: true,
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner::default());

    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_review_pool(WorkerPool::new_review(1));
    // Disable retries so the pre-start failure is terminal immediately.
    let coordinator = Arc::new(coord.with_pre_start_retry_delays(Vec::new()));

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("review pool slot available");

    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(result.is_err(), "schedule_execution must fail when the lease fails");

    // No workspace was ever leased so there is nothing to release.
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "no release should occur when the lease itself failed"
    );

    // A cube_workspace_lease_failed attention item must exist.
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(
        items.iter().any(|i| i.kind == "cube_workspace_lease_failed"),
        "expected a cube_workspace_lease_failed attention item, got {:?}",
        items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
    );
}

/// When `cube workspace goto` fails for a `pr_review` execution, dispatch must
/// fail loudly with a `cube_workspace_positioning_failed` attention item.
#[tokio::test]
async fn pr_review_goto_failure_records_positioning_failed() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/7";
    let (_, chore_id) = make_pr_review_fixture(&db, Some(pr_url));

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_goto: true,
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner::default());

    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord.with_pre_start_retry_delays(Vec::new()));

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("review pool slot available");

    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(result.is_err(), "schedule_execution must fail when goto fails");

    // The workspace was leased and must be released after goto failure.
    assert_eq!(
        cube.release_calls.lock().await.len(),
        1,
        "workspace must be released after goto failure"
    );

    // A cube_workspace_positioning_failed attention item must exist.
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(
        items.iter().any(|i| i.kind == "cube_workspace_positioning_failed"),
        "expected a cube_workspace_positioning_failed attention item, got {:?}",
        items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
    );
}

/// When a `pr_review` execution has no `pr_url` on its task, the normal
/// `create_change` path must be used.
#[tokio::test]
async fn pr_review_without_pr_url_uses_create_change_path() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    // No pr_url on the chore.
    let (_, chore_id) = make_pr_review_fixture(&db, None);

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord);

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("review pool slot available");

    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(
        result.is_ok(),
        "schedule_execution must succeed on the create_change path: {result:?}"
    );

    // goto_workspace must NOT have been called — no pr_url means no PR positioning.
    assert!(
        cube.goto_calls.lock().await.is_empty(),
        "goto_workspace must not be called when pr_url is absent"
    );

    // create_change must have been called once.
    assert_eq!(
        cube.create_calls.lock().await.len(),
        1,
        "create_change must be called when pr_url is absent"
    );
}

// ── Dispatch-pause review exemption tests ──────────────────────────────────

/// An operator-originated pause (`bossctl dispatch pause`, the human
/// toggle) must NOT hold `pr_review` executions: a review is the
/// lifecycle of a change already in flight, not new work, so it keeps
/// dispatching through `drain_ready_queue` while paused.
#[tokio::test]
async fn operator_pause_exempts_ready_pr_review_execution() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (_, chore_id) = make_pr_review_fixture(&db, None);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord);

    coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Operator);
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

    assert_eq!(
        runner.calls.lock().await.len(),
        1,
        "the review execution must dispatch despite the operator pause"
    );
}

/// The same operator pause must still hold a main-pool (non-review)
/// execution — only review rows are exempt. It stays `ready` until an
/// explicit resume kicks the scheduler.
#[tokio::test]
async fn operator_pause_holds_main_pool_row_until_resume() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Operator);
    coordinator.kick();

    // No positive event to wait on — the assertion is that nothing
    // changes while paused.
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runner.calls.lock().await.len(),
        0,
        "a main-pool row must be held, not dispatched, during an operator pause"
    );
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Ready,
        "the held execution must remain `ready` while paused"
    );

    // Resume mirrors `handle_set_dispatch_paused`: flip the flag, then
    // kick so the held row drains immediately.
    coordinator.set_dispatch_paused(false, 0, DispatchPauseOrigin::Operator);
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;
}

/// A breaker-originated pause (the spawn-capability circuit breaker —
/// see `spawn_health.rs`) must hold `pr_review` executions too: the
/// app's spawn path itself is broken, so exempting reviews would just
/// burn another spawn attempt against the same dead path.
#[tokio::test]
async fn breaker_pause_holds_pr_review_execution_too() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (_, chore_id) = make_pr_review_fixture(&db, None);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord);

    coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Breaker);
    coordinator.kick();

    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runner.calls.lock().await.len(),
        0,
        "a breaker-tripped pause must hold review executions, not exempt them"
    );
    assert_eq!(
        db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Ready,
        "the held review execution must remain `ready` while breaker-paused"
    );
}

/// Rows held by an operator pause must drain exactly once on resume,
/// even if the scheduler was kicked multiple times while paused (e.g.
/// by unrelated work being created) — no double-dispatch of the same
/// held row.
#[tokio::test]
async fn resume_kick_drains_held_row_exactly_once() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Operator);
    // Multiple kicks while paused must not cause multiple dispatches once resumed.
    coordinator.kick();
    coordinator.kick();
    coordinator.kick();
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runner.calls.lock().await.len(),
        0,
        "must stay held across repeated kicks"
    );

    coordinator.set_dispatch_paused(false, 0, DispatchPauseOrigin::Operator);
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;

    // Give any (incorrect) duplicate dispatch a window to land before asserting.
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runner.calls.lock().await.len(),
        1,
        "the held row must be dispatched exactly once after resume"
    );
}

// ── Automation-pause tests ──────────────────────────────────────────────

/// An automation-pause must hold an automation-pool row — it stays
/// `ready` until an explicit resume kicks the scheduler — mirroring
/// `operator_pause_holds_main_pool_row_until_resume` for the
/// independent `automation_paused` flag.
#[tokio::test]
async fn automation_pause_holds_automation_pool_row_until_resume() {
    use crate::work::CreateAutomationInput;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    let automation = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Test automation".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "UTC".to_owned(),
            },
            standing_instruction: "do maintenance".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();
    let auto_chore = create_test_chore(&db, product.id.clone(), "Automation chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, auto_chore.id],
        )
        .unwrap();
    }
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_id = db.list_executions(Some(&auto_chore.id)).unwrap()[0].id.clone();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);

    coordinator.set_automation_paused(true, 0);
    coordinator.kick();

    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runner.calls.lock().await.len(),
        0,
        "an automation-pool row must be held, not dispatched, while automation is paused"
    );
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Ready,
        "the held execution must remain `ready` while automation-paused"
    );

    coordinator.set_automation_paused(false, 0);
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;
}

/// An automation-pause must NOT hold main-pool or review-pool rows —
/// only rows targeting the automation pool are held. Demonstrates the
/// independence from `dispatch_paused`: dispatch keeps running normally
/// while only automation is paused.
#[tokio::test]
async fn automation_pause_does_not_hold_main_pool_row() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    coordinator.set_automation_paused(true, 0);
    coordinator.kick();

    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;
    assert_eq!(
        runner.calls.lock().await.len(),
        1,
        "a main-pool row must dispatch normally while only automation is paused"
    );
    assert!(
        !coordinator.is_dispatch_paused(),
        "automation pause must not flip the independent dispatch-pause flag"
    );
}
