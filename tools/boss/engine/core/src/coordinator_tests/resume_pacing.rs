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
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let window = crate::live_worker_state::DRIVER_START_GRACE_SECS;
    let reserved = std::collections::HashMap::from([("reserved".to_owned(), now)]);
    assert!(live.startup_pending(&reserved, now, window));
    assert!(!live.startup_pending(&std::collections::HashMap::new(), now, window));
    live.register_spawn(1, "reserved", "codex", 123, None);
    assert!(live.startup_pending(&reserved, now, window));
    live.record_driver_signal("reserved", DriverSignalKind::HookEvent);
    assert!(!live.startup_pending(&reserved, now, window));
    live.register_spawn(2, "cancelled", "claude", 124, None);
    assert!(live.startup_pending(&reserved, now, window));
    live.release_slot(2);
    assert!(!live.startup_pending(&reserved, now, window));
}

#[test]
fn startup_pressure_ignores_unproven_slots_past_the_grace_window() {
    let live = LiveWorkerStateRegistry::new();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let window = crate::live_worker_state::DRIVER_START_GRACE_SECS;
    live.register_spawn(1, "stuck", "codex", 123, None);
    live.set_spawn_time_for_test(1, now - window - 1);
    let claimed = std::collections::HashMap::from([("stuck".to_owned(), now - window - 1)]);
    assert!(
        !live.startup_pending(&claimed, now, window),
        "a slot already past driver-start grace must not pin resumed admission"
    );
    assert!(!live.startup_pending(&std::collections::HashMap::new(), now, window));
    live.register_spawn(2, "fresh", "claude", 124, None);
    assert!(live.startup_pending(&claimed, now, window));
}

#[test]
fn live_unproven_slots_raise_jsonl_discovery_deadline() {
    let live = LiveWorkerStateRegistry::new();
    let load = std::sync::Arc::new(boss_startup_policy::DiscoveryLoad::default());
    let first = load.begin();
    assert_eq!(first.timeout(), Duration::from_secs(120));
    live.set_discovery_load(load);
    live.register_spawn(1, "claude-1", "opus", 1, None);
    live.register_spawn(2, "claude-2", "opus", 2, None);
    live.register_spawn(3, "grok-1", "grok-4.6", 3, None);
    // Three non-JSONL live slots raise the already-armed Codex discovery
    // (peak 3 → 120 + 2*9).
    assert_eq!(first.timeout(), Duration::from_secs(138));
}

#[test]
fn paced_dead_driver_backlog_still_trips_the_existing_breaker() {
    use crate::live_worker_state::DRIVER_START_GRACE_SECS;
    use crate::spawn_health::SpawnHealthTracker;
    use boss_startup_policy::ResumeAdmission;

    let now = std::time::Instant::now();
    let mut admission = ResumeAdmission::default();
    admission.resume(now);
    // Long enough that admissions are still flowing when the third
    // driver-start failure lands — the six-row queue emptied before that
    // and could not prove the property.
    let mut ready: std::collections::VecDeque<String> = (0..20).map(|index| format!("dead-{index}")).collect();
    admission.observe_ready(ready.iter().cloned());
    let tracker = SpawnHealthTracker::new();
    let mut failures = std::collections::VecDeque::new();
    let mut occupied = 0usize;
    const POOL_SLOTS: usize = 8;
    let mut tripped = false;
    let mut remaining_when_tripped = 0usize;
    // Drive the production 15s heartbeat and 60s reap cadence, including
    // healthy shell acknowledgments. Shell acks must not wipe driver-start
    // failures; reaps release pool slots so later rows keep admitting.
    // All drivers are dead; no readiness proof releases pacing early.
    for elapsed in (0..=900).step_by(15) {
        let tick = now + Duration::from_secs(elapsed as u64);
        if occupied < POOL_SLOTS
            && let Some(id) = ready.front()
            && !admission.holds(id, occupied > 0, tick)
        {
            let id = ready.pop_front().unwrap();
            admission.admitted(&id, tick);
            tracker.record_shell_ack();
            occupied += 1;
            failures.push_back((id, elapsed + DRIVER_START_GRACE_SECS));
        }
        if elapsed % 60 == 0 {
            while failures.front().is_some_and(|(_, due)| *due <= elapsed) {
                let (id, _) = failures.pop_front().unwrap();
                occupied = occupied.saturating_sub(1);
                if tracker.record_driver_start_failure(&id, elapsed).is_some() {
                    tripped = true;
                    remaining_when_tripped = ready.len();
                    break;
                }
            }
        }
        if tripped {
            break;
        }
    }
    assert!(tripped, "resume pacing must not hide dead drivers from the breaker");
    assert!(
        remaining_when_tripped > 0,
        "the breaker must trip while the resumed backlog still has ready work, remaining={remaining_when_tripped}"
    );
}

#[test]
fn startup_pressure_ignores_aged_claims_without_live_entries() {
    let live = LiveWorkerStateRegistry::new();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let window = crate::live_worker_state::DRIVER_START_GRACE_SECS;
    let claims = std::collections::HashMap::from([("unregistered".to_owned(), now - window - 1)]);
    assert!(!live.startup_pending(&claims, now, window));
    let fresh = std::collections::HashMap::from([("unregistered".to_owned(), now)]);
    assert!(live.startup_pending(&fresh, now, window));
}

#[test]
fn refreshing_contention_expires_quiet_slots() {
    let live = LiveWorkerStateRegistry::new();
    let load = Arc::new(boss_startup_policy::DiscoveryLoad::default());
    live.set_discovery_load(load.clone());
    live.register_spawn(1, "quiet", "codex", 123, None);
    live.set_spawn_time_for_test(
        1,
        boss_engine_utils::epoch_time::now_epoch_secs() - crate::live_worker_state::DRIVER_START_GRACE_SECS - 1,
    );
    live.publish_startup_contention();
    assert_eq!(load.begin().timeout(), Duration::from_secs(120));
}
