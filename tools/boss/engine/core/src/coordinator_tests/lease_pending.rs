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

fn seed_ready_chores(db: &WorkDb, n: usize, label: &str) -> Vec<String> {
    let product = create_test_product(db);
    let chores: Vec<_> = (0..n)
        .map(|i| create_test_chore(db, product.id.clone(), format!("{label} {i}")))
        .collect();
    db.reconcile_product_executions(&product.id).unwrap();
    chores
        .iter()
        .map(|chore| db.list_executions(Some(&chore.id)).unwrap().pop().unwrap().id)
        .collect()
}

async fn wait_until_lease_calls(cube: &FakeCubeClient, n: usize) {
    for _ in 0..300 {
        if cube.lease_calls.lock().await.len() >= n {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("never reached {n} lease calls");
}

/// A permit freed by a finished lease re-drains a held row on its own: the
/// second row dispatches without anything but the release kicking the
/// scheduler, and the bound is never exceeded.
#[tokio::test]
async fn released_lease_permit_redrains_held_rows_without_an_external_kick() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution_ids = seed_ready_chores(&db, 2, "Permit handoff");

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let cube = Arc::new(FakeCubeClient {
        lease_gate: Some(gate.clone()),
        ..FakeCubeClient::default()
    });
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
        .with_max_concurrent_leases_per_repo(1),
    );

    coordinator.drain_ready_queue().await;
    wait_until_in_flight_leases(&cube, 1).await;
    let held = execution_ids
        .iter()
        .filter(|id| db.get_execution(id).unwrap().status == ExecutionStatus::Ready)
        .count();
    assert_eq!(held, 1, "the second row must wait on the bound");

    // Let the first lease return. Nothing else kicks the scheduler.
    gate.add_permits(1);
    wait_until_lease_calls(&cube, 2).await;
    assert_eq!(
        cube.peak_in_flight_leases.load(Ordering::SeqCst),
        1,
        "the bound must hold across the handoff"
    );
    gate.add_permits(1);
    for id in &execution_ids {
        wait_for_execution_status(db.as_ref(), id, ExecutionStatus::Running).await;
    }
}

/// A lease that has returned no longer occupies the per-repo bound while the
/// rest of the spawn pipeline runs.
#[tokio::test]
async fn lease_permit_is_not_held_through_the_spawn_pipeline() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution_ids = seed_ready_chores(&db, 1, "Spawn after lease");

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(2),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        )
        .with_max_concurrent_leases_per_repo(1),
    );
    coordinator.drain_ready_queue().await;
    wait_for_execution_status(db.as_ref(), &execution_ids[0], ExecutionStatus::Running).await;
    let execution = db.get_execution(&execution_ids[0]).unwrap();
    assert!(
        coordinator
            .repo_lease_limiter
            .try_begin(&execution.repo_remote_url, "another-exec")
            .is_some(),
        "the permit must be free once the lease has returned"
    );
}

/// The cap compares `working`, and a claim only becomes `working` when its
/// lease returns, so two claims admitted at `working == cap - 1` both go
/// live. The overshoot is bounded by the per-repo permit, and the next row is
/// held until `working` drops back under the cap.
#[tokio::test]
async fn cap_overshoot_is_bounded_by_the_per_repo_lease_bound() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution_ids = seed_ready_chores(&db, 3, "Cap overshoot");

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let cube = Arc::new(FakeCubeClient {
        lease_gate: Some(gate.clone()),
        ..FakeCubeClient::default()
    });
    const CAP: usize = 2;
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
        .with_max_concurrent_leases_per_repo(MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO),
    );
    coordinator.set_max_concurrent_interactive_workers(CAP).unwrap();
    coordinator
        .worker_pool()
        .claim_working("exec-already-working", None)
        .await
        .expect("idle slot");

    coordinator.drain_ready_queue().await;
    wait_until_in_flight_leases(&cube, MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO).await;
    gate.add_permits(MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO);
    for id in &execution_ids[..MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO] {
        wait_for_execution_status(db.as_ref(), id, ExecutionStatus::Running).await;
    }

    let occupancy = coordinator.worker_pool().occupancy().await;
    assert_eq!(
        occupancy.working,
        CAP - 1 + MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO,
        "overshoot is exactly the bounded number of in-flight leases"
    );
    let third = db.get_execution(&execution_ids[2]).unwrap();
    assert_eq!(third.status, ExecutionStatus::Ready);
    let expected = interactive_cap_wait_reason(occupancy.working, 0);
    assert_eq!(third.dispatch_wait_reason.as_deref(), Some(expected.as_str()));
}

/// With one cap slot and two successful leases in flight, the live count
/// ends at `cap + bound - 1` and nothing further is admitted.
#[tokio::test]
async fn cap_one_with_two_pending_successful_leases_stays_within_the_bound() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution_ids = seed_ready_chores(&db, 3, "Cap one");

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let cube = Arc::new(FakeCubeClient {
        lease_gate: Some(gate.clone()),
        ..FakeCubeClient::default()
    });
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
        .with_max_concurrent_leases_per_repo(MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO),
    );
    coordinator.set_max_concurrent_interactive_workers(1).unwrap();

    coordinator.drain_ready_queue().await;
    wait_until_in_flight_leases(&cube, MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO).await;
    gate.add_permits(MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO);
    for id in &execution_ids[..MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO] {
        wait_for_execution_status(db.as_ref(), id, ExecutionStatus::Running).await;
    }
    assert_eq!(
        coordinator.worker_pool().occupancy().await.working,
        MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO
    );
    assert_eq!(
        db.get_execution(&execution_ids[2]).unwrap().status,
        ExecutionStatus::Ready,
        "no further row is admitted once working is at or over the cap"
    );
}

/// A forced launch overrides the interactive cap but not the per-repo
/// lease bound.
#[tokio::test]
async fn force_dispatch_respects_the_per_repo_lease_bound() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution_ids = seed_ready_chores(&db, 2, "Forced during storm");

    let cube = Arc::new(FakeCubeClient {
        hang_lease: true,
        ..FakeCubeClient::default()
    });
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
        .with_max_concurrent_leases_per_repo(1),
    );
    coordinator.drain_ready_queue().await;
    wait_until_in_flight_leases(&cube, 1).await;

    let held_id = execution_ids
        .iter()
        .find(|id| db.get_execution(id).unwrap().status == ExecutionStatus::Ready)
        .expect("one row is held by the bound");
    let err = coordinator
        .force_dispatch(held_id, DispatchAdmission::OperatorForced)
        .await
        .expect_err("the saturated bound must refuse a forced launch");
    assert!(
        err.to_string().contains("cube workspace leases are already in flight"),
        "got: {err:#}"
    );
    assert_eq!(db.get_execution(held_id).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(cube.peak_in_flight_leases.load(Ordering::SeqCst), 1);
}

/// A timeout reported by the adapter/transport (not the outer backstop) is
/// still a timeout: no immediate second lease, and the row backs off.
#[tokio::test]
async fn adapter_reported_lease_timeout_backs_off_without_a_second_attempt() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution_ids = seed_ready_chores(&db, 1, "Transport timeout");

    let cube = Arc::new(FakeCubeClient {
        fail_lease_with_elapsed: true,
        ..FakeCubeClient::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(2),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![Duration::from_secs(5)]),
    );
    coordinator.drain_ready_queue().await;
    wait_for_execution_status(db.as_ref(), &execution_ids[0], ExecutionStatus::Ready).await;
    for _ in 0..100 {
        if db.get_execution(&execution_ids[0]).unwrap().pre_start_failure_count == 1 {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    let after = db.get_execution(&execution_ids[0]).unwrap();
    assert_eq!(after.pre_start_failure_count, 1);
    assert!(after.dispatch_not_before.is_some(), "the timeout must stamp a backoff");
    assert_eq!(
        cube.lease_calls.lock().await.len(),
        1,
        "a transport-reported timeout must not trigger the immediate fallback lease"
    );
}

/// Repeated lease timeouts back off for longer each time, and a backing-off
/// row holds no permit or slot.
#[tokio::test]
async fn repeated_lease_timeouts_back_off_for_longer_each_time() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution_ids = seed_ready_chores(&db, 1, "Repeated timeout");
    let id = &execution_ids[0];

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
        .with_pre_start_retry_delays(vec![Duration::from_secs(1), Duration::from_secs(6)])
        .with_cube_lease_timeout(Duration::from_millis(80)),
    );
    let epoch = || boss_engine_utils::epoch_time::now_epoch_secs() as i64;
    let not_before = |id: &str| -> i64 {
        db.get_execution(id)
            .unwrap()
            .dispatch_not_before
            .as_deref()
            .expect("timeout stamps dispatch_not_before")
            .parse()
            .unwrap()
    };
    let wait_for_failures = |n: i64| {
        let db = Arc::clone(&db);
        let id = id.clone();
        async move {
            for _ in 0..300 {
                let row = db.get_execution(&id).unwrap();
                if row.pre_start_failure_count == n && row.status == ExecutionStatus::Ready {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
            panic!("never reached {n} pre-start failures");
        }
    };

    coordinator.drain_ready_queue().await;
    wait_for_failures(1).await;
    let first_not_before = not_before(id);
    assert!(first_not_before <= epoch() + 2, "first backoff is the short delay");
    assert_eq!(coordinator.worker_pool().occupancy().await.lease_pending, 0);
    assert_eq!(
        coordinator
            .repo_lease_limiter
            .in_flight(&db.get_execution(id).unwrap().repo_remote_url),
        0,
        "a backing-off row holds no permit"
    );

    while epoch() <= first_not_before {
        sleep(Duration::from_millis(100)).await;
    }
    let before_second = epoch();
    coordinator.drain_ready_queue().await;
    wait_for_failures(2).await;
    assert!(
        not_before(id) >= before_second + 5,
        "second backoff must be the longer delay, got not_before={} from {before_second}",
        not_before(id)
    );
    assert_eq!(cube.lease_calls.lock().await.len(), 2);
}
