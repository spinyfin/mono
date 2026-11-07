//! Pool routing and admission for `progression` executions: the administrative
//! agents that unstick a work item. They run in their own small pool, are
//! invisible to the interactive concurrency cap and to Lower Decks spillover,
//! honour both pause origins, and cannot spawn until the engine has a
//! progression worker kind.

use super::helpers::*;

use boss_protocol::{DispatchPool, PauseReason};

const NOTHING_HAPPENS_WINDOW: Duration = Duration::from_millis(150);

struct Fixture {
    db: Arc<WorkDb>,
    product: String,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db).id;
    Fixture { db, product, _dir: dir }
}

impl Fixture {
    /// A stuck item with a `ready` progression execution bound to it.
    fn progression_run(&self, name: &str) -> WorkExecution {
        let chore = create_test_chore_manual(&self.db, self.product.clone(), name);
        self.db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id)
                    .kind(ExecutionKind::Progression)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap()
    }

    fn coordinator(
        &self,
        progression_slots: usize,
        spawn_enabled: bool,
    ) -> (Arc<ExecutionCoordinator>, Arc<FakeCubeClient>, Arc<FakeExecutionRunner>) {
        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let mut coordinator =
            ExecutionCoordinator::new(self.db.clone(), WorkerPool::new(16), cube.clone(), runner.clone());
        coordinator.set_progression_pool(WorkerPool::new_progression(progression_slots));
        coordinator.set_progression_spawn_enabled(spawn_enabled);
        (Arc::new(coordinator), cube, runner)
    }
}

fn operator_pause(coordinator: &ExecutionCoordinator) {
    coordinator.pause_dispatch(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: operator pause").unwrap(),
    );
}

fn breaker_pause(coordinator: &ExecutionCoordinator) {
    coordinator.pause_dispatch(
        1_786_385_816,
        DispatchPauseOrigin::Breaker,
        PauseReason::new("test: spawn-capability breaker tripped").unwrap(),
    );
}

#[tokio::test]
async fn progression_routes_to_its_own_pool_whatever_produced_the_item_it_is_bound_to() {
    let f = fixture();
    let (coordinator, _, _) = f.coordinator(1, true);
    let plain = f.progression_run("Plain item");
    assert_eq!(coordinator.dispatch_pool_for(&plain), DispatchPool::Progression);
    assert_eq!(coordinator.attributed_pool_label(&plain), "progression");
    assert!(coordinator.execution_targets_progression_pool(&plain));

    // An item an automation produced is routed to the automation pool for its
    // own work, but a progression run bound to it is still a progression run.
    let automation = seed_daily_automation(&f.db, &f.product);
    let produced = create_test_chore_manual(&f.db, f.product.clone(), "Automation-produced");
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, produced.id],
        )
        .unwrap();
    let own_work = create_ready_chore_execution(&f.db, produced.id.clone());
    let progression =
        f.db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(produced.id.clone())
                .kind(ExecutionKind::Progression)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert_eq!(coordinator.dispatch_pool_for(&own_work), DispatchPool::Automation);
    assert_eq!(coordinator.dispatch_pool_for(&progression), DispatchPool::Progression);
    assert!(!coordinator.execution_targets_review_pool(&progression));

    // The pool a progression run draws from names its slots `progression-N`.
    let worker = coordinator
        .pool_for_execution_for_test(&progression)
        .claim_worker(&progression.id, None)
        .await
        .unwrap();
    assert_eq!(worker, "progression-1");
    assert!(worker.starts_with(PROGRESSION_WORKER_ID_PREFIX));
    assert_eq!(
        boss_execution_contracts::pool_dispatch_policy_for_worker_id(&worker),
        None
    );
    assert_eq!(
        coordinator.claim_holder(&worker).await.as_deref(),
        Some(progression.id.as_str())
    );
    assert!(coordinator.all_claimed_execution_ids().await.contains(&progression.id));
    assert_eq!(
        coordinator.worker_pool().busy_count().await,
        0,
        "no interactive slot is touched"
    );
}

#[tokio::test]
async fn a_progression_run_is_invisible_to_the_interactive_cap() {
    let f = fixture();
    let (coordinator, _cube, runner) = f.coordinator(1, true);
    for i in 0..MAX_CONCURRENT_INTERACTIVE_WORKERS {
        coordinator
            .worker_pool()
            .claim_working(&format!("busy-{i}"), None)
            .await
            .unwrap();
    }
    assert_eq!(
        coordinator.worker_pool().busy_count().await,
        coordinator.max_concurrent_interactive_workers(),
        "the interactive cap is saturated"
    );
    let run = f.progression_run("Stuck while the laptop is busy");

    coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(&f.db, &run.id, ExecutionStatus::Running).await;

    let claims = coordinator.progression_worker_pool().claims().await;
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].worker_id, "progression-1");
    assert_eq!(claims[0].execution_id, run.id);
    assert_eq!(
        coordinator.worker_pool().busy_count().await,
        MAX_CONCURRENT_INTERACTIVE_WORKERS,
        "the run neither took an interactive slot nor counted against the cap"
    );
    assert_eq!(runner.calls.lock().await.len(), 1);
    assert!(
        runner.calls.lock().await[0].0.starts_with(PROGRESSION_WORKER_ID_PREFIX),
        "the runner was handed a progression slot"
    );
}

#[tokio::test]
async fn nothing_spills_into_or_out_of_the_progression_pool() {
    let f = fixture();
    let (coordinator, _cube, runner) = f.coordinator(1, true);
    let older = f.progression_run("Stuck longer");
    let younger = f.progression_run("Stuck shorter");
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = CASE id WHEN ?1 THEN '100' ELSE '200' END WHERE id IN (?1, ?2)",
            rusqlite::params![older.id, younger.id],
        )
        .unwrap();
    let chore = create_test_chore_manual(&f.db, f.product.clone(), "Ordinary work");
    let mainline = create_ready_chore_execution(&f.db, chore.id.clone());

    coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(&f.db, &older.id, ExecutionStatus::Running).await;
    wait_for_execution_status(&f.db, &mainline.id, ExecutionStatus::Running).await;
    sleep(NOTHING_HAPPENS_WINDOW).await;

    // One slot, so the older candidate runs and the younger one waits its turn
    // as a queued `ready` row, not on a free Lower Decks slot.
    let waiting = f.db.get_execution(&younger.id).unwrap();
    assert_eq!(waiting.status, ExecutionStatus::Ready);
    assert_eq!(waiting.dispatch_wait_reason.as_deref(), Some("pool_exhausted"));
    assert_eq!(
        coordinator.worker_pool().busy_count().await,
        1,
        "only the mainline run counts toward the interactive pool"
    );
    let main_claims = coordinator.worker_pool().claims().await;
    assert_eq!(main_claims.len(), 1, "only the mainline run holds an interactive slot");
    assert_eq!(main_claims[0].execution_id, mainline.id);
    assert_eq!(runner.calls.lock().await.len(), 2);

    // The full progression pool did not latch "main pool exhausted" for the
    // ordinary row in the same pass, and a free slot dispatches the waiter.
    let held = coordinator.progression_worker_pool().claims().await;
    assert_eq!(held.len(), 1);
    coordinator
        .progression_worker_pool()
        .release_worker(&held[0].worker_id, None)
        .await;
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&older.id],
        )
        .unwrap();
    coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(&f.db, &younger.id, ExecutionStatus::Running).await;
}

#[tokio::test]
async fn both_pause_origins_hold_a_progression_run_and_resume_drains_it() {
    for origin in ["operator", "breaker"] {
        let f = fixture();
        let (coordinator, cube, runner) = f.coordinator(1, true);
        let run = f.progression_run("Stuck during a pause");
        match origin {
            "operator" => operator_pause(&coordinator),
            _ => breaker_pause(&coordinator),
        }

        coordinator.drain_ready_queue_for_test().await;
        sleep(NOTHING_HAPPENS_WINDOW).await;
        let held = f.db.get_execution(&run.id).unwrap();
        assert_eq!(held.status, ExecutionStatus::Ready, "{origin}");
        assert_eq!(
            held.dispatch_wait_reason.as_deref(),
            Some("dispatch_paused"),
            "{origin}"
        );
        assert!(runner.calls.lock().await.is_empty(), "{origin}: nothing spawns");
        assert!(cube.lease_calls.lock().await.is_empty(), "{origin}: nothing leases");
        assert_eq!(
            coordinator.progression_worker_pool().claims().await.len(),
            0,
            "{origin}"
        );

        coordinator.resume_dispatch();
        coordinator.drain_ready_queue_for_test().await;
        wait_for_execution_status(&f.db, &run.id, ExecutionStatus::Running).await;
    }
}

#[tokio::test]
async fn until_the_engine_can_spawn_one_a_progression_run_is_held_ready_and_touches_nothing() {
    let f = fixture();
    let (coordinator, cube, runner) = f.coordinator(1, false);
    assert!(!coordinator.progression_spawn_enabled());
    let run = f.progression_run("Minted before the worker kind exists");

    coordinator.drain_ready_queue_for_test().await;
    sleep(NOTHING_HAPPENS_WINDOW).await;
    let held = f.db.get_execution(&run.id).unwrap();
    assert_eq!(held.status, ExecutionStatus::Ready);
    assert_eq!(held.dispatch_wait_reason.as_deref(), Some("progression_unavailable"));
    assert!(runner.calls.lock().await.is_empty());
    assert!(cube.lease_calls.lock().await.is_empty());
    assert_eq!(coordinator.progression_worker_pool().claims().await.len(), 0);
    assert_eq!(coordinator.worker_pool().claims().await.len(), 0);

    // An operator forcing the row by hand is refused too: it must not start as
    // an ordinary coding worker with a repo checkout. The refusal is a hold, not
    // a spawn failure, so the row is neither failed nor bounced.
    let error = coordinator
        .force_dispatch(&run.id, DispatchAdmission::OperatorForced)
        .await
        .expect_err("a progression run cannot be force-dispatched yet");
    assert!(
        error.to_string().contains("cannot spawn progression agents yet"),
        "{error:#}"
    );
    assert!(runner.calls.lock().await.is_empty());
    assert!(cube.lease_calls.lock().await.is_empty());
    assert_eq!(
        coordinator.progression_worker_pool().idle_count().await,
        1,
        "the claim was handed back"
    );
    let after = f.db.get_execution(&run.id).unwrap();
    assert_eq!(after.status, ExecutionStatus::Ready);
    assert_eq!(after.pre_start_failure_count, 0);
    let item = f.db.get_work_item(&run.work_item_id).unwrap();
    assert!(matches!(item, WorkItem::Chore(t) if t.dispatch_failed_reason.is_none()));

    // Flipping the capability on dispatches it as normal.
    let (enabled, _, _) = f.coordinator(1, true);
    enabled.drain_ready_queue_for_test().await;
    wait_for_execution_status(&f.db, &run.id, ExecutionStatus::Running).await;
}

#[tokio::test]
async fn a_progression_run_is_never_the_breaker_recovery_canary() {
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::spawn_health::{SpawnHealthTracker, maybe_admit_recovery_probe};

    let f = fixture();
    let (coordinator, _cube, runner) = f.coordinator(1, true);
    let run = f.progression_run("Least urgent row in the queue");
    breaker_pause(&coordinator);

    // The coordinator's own gate refuses it as a canary...
    assert!(
        coordinator
            .dispatch_hold_for(&run, DispatchAdmission::BreakerRecoveryProbe)
            .is_some()
    );
    // ...and the probe does not pick it, even when it is the only ready row.
    let spawn_health = SpawnHealthTracker::new();
    let events = Arc::new(RecordingDispatchEventSink::default());
    maybe_admit_recovery_probe(&f.db, &coordinator, &spawn_health, events.as_ref(), 1000).await;
    assert!(!spawn_health.is_probe_execution(&run.id));
    sleep(NOTHING_HAPPENS_WINDOW).await;
    assert!(runner.calls.lock().await.is_empty());
    assert_eq!(f.db.get_execution(&run.id).unwrap().status, ExecutionStatus::Ready);
}
