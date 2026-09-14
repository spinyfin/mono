//! Resume pacing follows startup proof, including the pre-registration gap.
use super::helpers::*;
use crate::live_worker_state::{DriverSignalKind, LiveWorkerStateRegistry};

#[tokio::test]
async fn resume_backlog_waits_for_driver_proof_then_drains_without_waiting_for_completion() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let live = Arc::new(LiveWorkerStateRegistry::new());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..Default::default()
    });
    let mut coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(8),
        Arc::new(FakeCubeClient::default()),
        runner.clone(),
    );
    coordinator.set_live_worker_states(live.clone());
    let coordinator = Arc::new(coordinator);
    coordinator.pause_dispatch(
        1,
        DispatchPauseOrigin::Breaker,
        boss_protocol::PauseReason::new("test startup pause").unwrap(),
    );
    for index in 0..6 {
        create_test_chore(&db, product.id.clone(), format!("Backlog {index}"));
    }
    db.reconcile_product_executions(&product.id).unwrap();
    coordinator.drain_ready_queue().await;
    assert!(runner.calls.lock().await.is_empty());
    coordinator.resume_dispatch();
    // Repeated resume must not recapture a new cohort.
    coordinator.resume_dispatch();
    for count in 1..=6 {
        coordinator.drain_ready_queue().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while runner.calls.lock().await.len() < count {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(runner.calls.lock().await.len(), count);
        assert_eq!(db.list_ready_executions().unwrap().len(), 6 - count);
        // No live registration yet: the pool claim alone must prevent a burst.
        coordinator.drain_ready_queue().await;
        assert_eq!(runner.calls.lock().await.len(), count);
        let run_id = runner.calls.lock().await[count - 1].1.clone();
        live.register_spawn(count as u8, &run_id, "codex", 123, None);
        // A shell acknowledgment is still not driver readiness.
        coordinator.drain_ready_queue().await;
        assert_eq!(runner.calls.lock().await.len(), count);
        live.record_driver_signal(&run_id, DriverSignalKind::HookEvent);
    }
    // All six workers remain running, yet the entire backlog was admitted.
    assert_eq!(live.snapshot().len(), 6);
    assert_eq!(runner.calls.lock().await.len(), 6);
    coordinator.set_max_concurrent_interactive_workers(8).unwrap();
    for index in 0..2 {
        create_test_chore(&db, product.id.clone(), format!("Fresh arrival {index}"));
    }
    db.reconcile_product_executions(&product.id).unwrap();
    coordinator.drain_ready_queue().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while runner.calls.lock().await.len() < 8 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Neither fresh worker has driver proof, yet both were admitted.
    assert_eq!(runner.calls.lock().await.len(), 8);
}

#[test]
fn startup_pressure_releases_on_proof_failure_and_cancellation() {
    let live = LiveWorkerStateRegistry::new();
    let reserved = std::collections::HashSet::from(["reserved".to_owned()]);
    assert!(live.startup_pending(&reserved));
    assert!(!live.startup_pending(&std::collections::HashSet::new()));
    live.register_spawn(1, "reserved", "codex", 123, None);
    assert!(live.startup_pending(&reserved));
    live.record_driver_signal("reserved", DriverSignalKind::HookEvent);
    assert!(!live.startup_pending(&reserved));
    live.register_spawn(2, "cancelled", "claude", 124, None);
    assert!(live.startup_pending(&reserved));
    live.release_slot(2);
    assert!(!live.startup_pending(&reserved));
}

#[test]
fn paced_dead_driver_backlog_still_trips_the_existing_breaker() {
    use crate::live_worker_state::DRIVER_START_GRACE_SECS;
    use crate::spawn_health::SpawnHealthTracker;
    use boss_startup_policy::ResumeAdmission;

    let now = std::time::Instant::now();
    let mut admission = ResumeAdmission::default();
    admission.resume(now);
    let mut ready: std::collections::VecDeque<String> = (0..6).map(|index| format!("dead-{index}")).collect();
    admission.observe_ready(ready.iter().cloned());
    let tracker = SpawnHealthTracker::new();
    let mut failures = std::collections::VecDeque::new();
    let mut tripped = false;
    // Drive the production 15s heartbeat and 60s reap cadence, including
    // healthy shell acknowledgments (which reset the existing breaker).
    // All drivers are dead; no readiness proof releases pacing early.
    for elapsed in (0..=900).step_by(15) {
        let tick = now + Duration::from_secs(elapsed as u64);
        if let Some(id) = ready.front()
            && !admission.holds(id, !failures.is_empty(), tick)
        {
            let id = ready.pop_front().unwrap();
            admission.admitted(&id, tick);
            tracker.record_success();
            failures.push_back((id, elapsed + DRIVER_START_GRACE_SECS));
        }
        if elapsed % 60 == 0 {
            while failures.front().is_some_and(|(_, due)| *due <= elapsed) {
                let (id, _) = failures.pop_front().unwrap();
                if tracker.record_failure(&id, elapsed).is_some() {
                    tripped = true;
                    break;
                }
            }
        }
        if tripped {
            break;
        }
    }
    assert!(tripped, "resume pacing must not hide dead drivers from the breaker");
}
