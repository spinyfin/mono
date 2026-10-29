//! Lease-pending occupancy: the interactive cap counts live workers only,
//! concurrent cube leases are bounded per repo, a timed-out lease backs
//! off instead of immediately re-claiming, and wait text never calls
//! lease-pending rows "working".
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;
use std::sync::atomic::Ordering;

use boss_protocol::ADMISSION_BLOCKER_INTERACTIVE_CONCURRENCY_CAP;

async fn wait_until_in_flight_leases(cube: &FakeCubeClient, n: usize) {
    for _ in 0..200 {
        if cube.in_flight_leases.load(Ordering::SeqCst) >= n {
            return;
        }
        tokio::time::timeout(Duration::from_millis(10), cube.lease_entered.notified())
            .await
            .ok();
    }
    panic!(
        "never reached {n} in-flight leases (in_flight={}, peak={})",
        cube.in_flight_leases.load(Ordering::SeqCst),
        cube.peak_in_flight_leases.load(Ordering::SeqCst)
    );
}

#[test]
fn interactive_cap_wait_reason_never_calls_lease_pending_working() {
    assert_eq!(
        interactive_cap_wait_reason(2, 0),
        "Waiting for an available worker (2 working)"
    );
    assert_eq!(
        interactive_cap_wait_reason(2, 9),
        "Waiting for an available worker (2 working, 9 waiting for a workspace)"
    );
}

/// Filling the pool with lease-pending claims (no workspace yet) must not
/// hold a ready row when the live-worker count is still below the cap.
#[tokio::test]
async fn lease_pending_does_not_block_admission_when_working_below_cap() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Ready while others lease");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(MAX_CONCURRENT_INTERACTIVE_WORKERS),
        cube.clone(),
        runner.clone(),
    ));
    const LEASE_PENDING: usize = 5;
    for i in 0..LEASE_PENDING {
        coordinator
            .worker_pool()
            .claim_worker(&format!("exec-lease-pending-{i}"), None)
            .await
            .expect("idle slot for a lease-pending claim");
    }
    let occupancy = coordinator.worker_pool().occupancy().await;
    assert_eq!(occupancy.working, 0);
    assert_eq!(occupancy.lease_pending, LEASE_PENDING);

    let admission = coordinator
        .evaluate_dispatch_admission(&chore.id)
        .await
        .expect("admission preview");
    assert!(
        admission.would_dispatch,
        "lease-pending claims must not raise the interactive-cap blocker, got: {:?}",
        admission.blockers
    );

    coordinator.drain_ready_queue().await;
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

    let occupancy = coordinator.worker_pool().occupancy().await;
    assert_eq!(occupancy.working, 1, "the ready row must become a live worker");
    assert_eq!(occupancy.lease_pending, LEASE_PENDING);
    assert_eq!(cube.lease_calls.lock().await.len(), 1);
}

/// Four ready rows against one origin, with hung leases: at most two
/// attempts are in flight, and the extras stay ready waiting for a workspace.
#[tokio::test]
async fn per_repo_lease_attempt_bound_holds_ready_rows() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chores: Vec<_> = (0..4)
        .map(|i| create_test_chore(&db, product.id.clone(), format!("Lease storm {i}")))
        .collect();
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_ids: Vec<String> = chores
        .iter()
        .map(|chore| db.list_executions(Some(&chore.id)).unwrap().pop().unwrap().id)
        .collect();

    let cube = Arc::new(FakeCubeClient {
        hang_lease: true,
        ..FakeCubeClient::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(MAX_CONCURRENT_INTERACTIVE_WORKERS),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        )
        .with_max_concurrent_leases_per_repo(MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO)
        .with_dispatch_events(recording.clone()),
    );

    coordinator.drain_ready_queue().await;
    wait_until_in_flight_leases(&cube, MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO).await;

    assert!(
        cube.peak_in_flight_leases.load(Ordering::SeqCst) <= MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO,
        "peak in-flight leases exceeded the per-repo bound: {}",
        cube.peak_in_flight_leases.load(Ordering::SeqCst)
    );
    let occupancy = coordinator.worker_pool().occupancy().await;
    assert_eq!(occupancy.working, 0);
    assert_eq!(occupancy.lease_pending, MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO);

    let mut claimed = 0;
    let mut waiting_for_workspace = 0;
    for id in &execution_ids {
        let row = db.get_execution(id).unwrap();
        match row.status {
            ExecutionStatus::Claimed => claimed += 1,
            ExecutionStatus::Ready => {
                assert_eq!(row.dispatch_wait_reason.as_deref(), Some(WAITING_FOR_WORKSPACE_REASON));
                waiting_for_workspace += 1;
            }
            other => panic!("unexpected status {other} for {id}"),
        }
    }
    assert_eq!(claimed, MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO);
    assert_eq!(waiting_for_workspace, 2);

    let mut bound_skips = 0;
    for id in &execution_ids {
        bound_skips += recording
            .events_for(id)
            .await
            .into_iter()
            .filter(|e| {
                e.stage == crate::dispatch_events::Stage::WorkerClaimed.as_str()
                    && e.outcome == crate::dispatch_events::Outcome::Skipped.as_str()
                    && e.details.get("reason").and_then(|v| v.as_str()) == Some("repo_lease_bound")
            })
            .count();
    }
    assert_eq!(bound_skips, 2, "the two extras must record the per-repo bound");
}

/// A timed-out cube lease must not immediately re-attempt: the row goes
/// back to ready with a future `dispatch_not_before`, and the slot is idle.
#[tokio::test]
async fn lease_timeout_backs_off_and_releases_the_slot() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Hung lease");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_id = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap().id;

    let cube = Arc::new(FakeCubeClient {
        hang_lease: true,
        ..FakeCubeClient::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(2),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![Duration::from_secs(5), Duration::from_secs(15)])
        .with_cube_lease_timeout(Duration::from_millis(80)),
    );

    coordinator.drain_ready_queue().await;
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Ready).await;

    let after = db.get_execution(&execution_id).unwrap();
    assert_eq!(after.status, ExecutionStatus::Ready);
    assert_eq!(after.pre_start_failure_count, 1);
    let not_before: i64 = after
        .dispatch_not_before
        .as_deref()
        .expect("timeout must stamp dispatch_not_before")
        .parse()
        .expect("dispatch_not_before is an epoch second");
    let now = boss_engine_utils::epoch_time::now_epoch_secs() as i64;
    assert!(
        not_before >= now + 4,
        "backoff must be at least 5s, got not_before={not_before} now={now}"
    );
    let idle = coordinator.worker_pool().occupancy().await;
    assert_eq!(idle.working, 0, "the slot must be free while the row is backing off");
    assert_eq!(
        idle.lease_pending, 0,
        "the slot must be free while the row is backing off"
    );
    assert_eq!(cube.lease_calls.lock().await.len(), 1);

    coordinator.drain_ready_queue().await;
    sleep(Duration::from_millis(50)).await;
    assert_eq!(
        cube.lease_calls.lock().await.len(),
        1,
        "a timed-out lease must not re-attempt inside the backoff window"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
}

/// When the live-worker cap is full and other slots are only acquiring a
/// workspace, the held row's wait text names both counts.
#[tokio::test]
async fn cap_hold_text_names_working_and_lease_pending() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Held by mixed occupancy");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();

    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(MAX_CONCURRENT_INTERACTIVE_WORKERS + 4),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        )
        .with_pre_start_retry_delays(Vec::new())
        .with_dispatch_events(recording.clone()),
    );
    for i in 0..MAX_CONCURRENT_INTERACTIVE_WORKERS {
        coordinator
            .worker_pool()
            .claim_working(&format!("exec-working-{i}"), None)
            .await
            .expect("idle slot below the cap");
    }
    const LEASE_PENDING: usize = 2;
    for i in 0..LEASE_PENDING {
        coordinator
            .worker_pool()
            .claim_worker(&format!("exec-lease-pending-{i}"), None)
            .await
            .expect("idle slot for a lease-pending claim");
    }

    coordinator.drain_ready_queue().await;

    let held = db.get_execution(&execution.id).unwrap();
    assert_eq!(held.status, ExecutionStatus::Ready);
    let expected = interactive_cap_wait_reason(MAX_CONCURRENT_INTERACTIVE_WORKERS, LEASE_PENDING);
    assert_eq!(held.dispatch_wait_reason.as_deref(), Some(expected.as_str()));

    let events = recording.events_for(&execution.id).await;
    let cap_event = events
        .iter()
        .find(|e| {
            e.stage == crate::dispatch_events::Stage::WorkerClaimed.as_str()
                && e.outcome == crate::dispatch_events::Outcome::Skipped.as_str()
        })
        .expect("a WorkerClaimed/Skipped event for the cap-held row");
    assert_eq!(
        cap_event.details.get("reason").and_then(|v| v.as_str()),
        Some("interactive_concurrency_cap")
    );
    assert_eq!(
        cap_event.details.get("live_workers").and_then(|v| v.as_u64()),
        Some(MAX_CONCURRENT_INTERACTIVE_WORKERS as u64)
    );
    assert_eq!(
        cap_event.details.get("lease_pending").and_then(|v| v.as_u64()),
        Some(LEASE_PENDING as u64)
    );

    let admission = coordinator
        .evaluate_dispatch_admission(&chore.id)
        .await
        .expect("admission preview");
    assert!(!admission.would_dispatch);
    let cap_blocker = admission
        .blockers
        .iter()
        .find(|b| b.code == ADMISSION_BLOCKER_INTERACTIVE_CONCURRENCY_CAP)
        .expect("interactive-cap blocker");
    assert!(
        cap_blocker.message.contains("waiting for a workspace"),
        "admission text must name lease-pending rows, got: {}",
        cap_blocker.message
    );
}
