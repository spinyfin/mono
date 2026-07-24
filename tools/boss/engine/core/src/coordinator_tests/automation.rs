//! The automation pool: routing, exhaustion isolation, spillover into the
//! interactive decks, and mainline preemption of spilled automation runs.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

/// Automation-produced tasks (stamped with `source_automation_id`) must be
/// routed to the automation pool, not the main pool.  A normal chore with no
/// `source_automation_id` must continue to route to the main pool.
#[tokio::test]
async fn automation_produced_task_routes_to_automation_pool() {
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

    // Create an automation-produced chore and stamp source_automation_id.
    let auto_chore = create_test_chore(&db, product.id.clone(), "Automation chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, auto_chore.id],
        )
        .unwrap();
    }

    // Create a regular chore with no source_automation_id.
    let main_chore = create_test_chore(&db, product.id.clone(), "Regular chore");

    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    // Wire in a 1-slot automation pool so we can check idle counts.
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);
    coordinator.kick();

    // Wait for both chores to be dispatched.
    for _ in 0..200 {
        let executions = db.list_executions(None).unwrap();
        if executions
            .iter()
            .filter(|e| e.status == ExecutionStatus::Running)
            .count()
            == 2
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let executions = db.list_executions(None).unwrap();
    let running: Vec<_> = executions
        .iter()
        .filter(|e| e.status == ExecutionStatus::Running)
        .collect();
    assert_eq!(running.len(), 2, "both chores must be running; got {running:?}");

    // The main pool slot should be claimed by the regular chore.
    assert_eq!(
        coordinator.worker_pool().idle_count().await,
        0,
        "main pool slot must be claimed by the regular chore"
    );
    // The automation pool slot should be claimed by the automation chore.
    assert_eq!(
        coordinator.automation_worker_pool().idle_count().await,
        0,
        "automation pool slot must be claimed by the automation-produced chore"
    );

    let _ = auto_chore;
    let _ = main_chore;
}

/// When the coordinator dispatches an automation-pool execution the
/// `worker_id` passed to the runner must carry the `"auto-worker-"`
/// prefix and decode (via `slot_id_from_worker_id`) to a slot id
/// that is strictly greater than `MAX_WORKER_POOL_SIZE` — i.e. it
/// must land in the automation-pool slot range (Kira/Dax/Bashir),
/// not the regular-pool range (Riker … O'Brien). This is the pane-
/// spawn correctness regression test for the slot-decoding incident where
/// `auto-worker-1` was decoded as slot 1 (Riker) instead of slot
/// 9 (Kira).
#[tokio::test]
async fn automation_dispatch_worker_id_maps_to_automation_pool_slot() {
    use crate::work::CreateAutomationInput;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    let automation = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Slot-range test".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "UTC".to_owned(),
            },
            standing_instruction: "do it".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let auto_chore = create_test_chore(&db, product.id.clone(), "Slot-range chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, auto_chore.id],
        )
        .unwrap();
    }
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(0), cube.clone(), runner.clone());
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);
    coordinator.kick();

    // Wait for the execution to reach running.
    for _ in 0..200 {
        let execs = db.list_executions(None).unwrap();
        if execs.iter().any(|e| e.status == ExecutionStatus::Running) {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let calls = runner.calls.lock().await;
    assert_eq!(calls.len(), 1, "exactly one run should have been dispatched");
    let (worker_id, _, _, _) = &calls[0];

    // The worker_id must carry the automation-pool prefix.
    assert!(
        worker_id.starts_with(AUTOMATION_WORKER_ID_PREFIX),
        "automation-pool execution must receive an auto-worker-N worker_id, got {worker_id:?}"
    );

    // Decoded slot must be in the automation-pool range (> MAX_WORKER_POOL_SIZE).
    let slot =
        slot_id_from_worker_id(worker_id).unwrap_or_else(|| panic!("slot_id_from_worker_id failed for {worker_id:?}"));
    assert!(
        slot as usize > MAX_WORKER_POOL_SIZE,
        "automation slot_id {slot} must be > {MAX_WORKER_POOL_SIZE} (the regular-pool ceiling); \
             got slot {slot} — automation pane would land on a regular-pool pane (slot-decoding regression)"
    );
}

/// Automation pool exhaustion must not block main-pool dispatch.
/// When the automation pool is full, regular chores continue to be
/// dispatched on the main pool.
#[tokio::test]
async fn automation_pool_exhaustion_does_not_block_main_pool() {
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
            open_task_limit: 5,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    // Two automation-produced chores (pool size will be 1, so the second stays ready).
    for n in 0..2 {
        let chore = create_test_chore(&db, product.id.clone(), format!("Auto chore {n}"));
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, chore.id],
        )
        .unwrap();
    }

    // One regular chore — must still be dispatched even when the automation pool is full.
    create_test_chore(&db, product.id.clone(), "Regular chore");

    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    // Main pool: 1 slot; automation pool: 1 slot.
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);
    coordinator.kick();

    // Wait for at least 2 executions to be running (1 main + 1 automation).
    for _ in 0..200 {
        let executions = db.list_executions(None).unwrap();
        if executions
            .iter()
            .filter(|e| e.status == ExecutionStatus::Running)
            .count()
            >= 2
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let executions = db.list_executions(None).unwrap();
    let running = executions
        .iter()
        .filter(|e| e.status == ExecutionStatus::Running)
        .count();
    assert_eq!(
        running, 2,
        "exactly 2 executions must be running (1 per pool); got {running}"
    );
    // The third execution (second auto chore) must remain ready — automation pool full.
    let ready = executions.iter().filter(|e| e.status == ExecutionStatus::Ready).count();
    assert_eq!(
        ready, 1,
        "the second auto chore must be deferred (automation pool full); got {ready} ready"
    );
}

// ---- Automation spillover + mainline preemption ----
//
// Geometry these tests rely on (see `dispatch_spillover`):
// `WORKER_PAGE_SIZE` is 8, so a main pool of N > 8 slots has Bridge
// Crew at indices 0..8 (`worker-1`..`worker-8`) and Lower Decks at
// indices 8.. (`worker-9`..). Automation may only ever spill into the
// Lower Decks page.

/// Create an automation plus `count` automation-produced chores whose
/// `source_automation_id` points at it (which is what routes their
/// executions to the automation pool). Returns the chore ids in
/// creation order.
fn create_automation_chores(db: &Arc<WorkDb>, product_id: &str, count: usize) -> Vec<String> {
    use crate::work::CreateAutomationInput;

    let automation = db
        .create_automation(CreateAutomationInput {
            product_id: product_id.to_owned(),
            name: "Spillover test automation".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "UTC".to_owned(),
            },
            standing_instruction: "do maintenance".to_owned(),
            open_task_limit: 50,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    (0..count)
        .map(|n| {
            let chore = create_test_chore(db, product_id.to_owned(), format!("Auto chore {n}"));
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
                rusqlite::params![automation.id, chore.id],
            )
            .unwrap();
            chore.id
        })
        .collect()
}

async fn wait_for_running_count(db: &Arc<WorkDb>, want: usize) {
    for _ in 0..300 {
        let running = db
            .list_executions(None)
            .unwrap()
            .iter()
            .filter(|e| e.status == ExecutionStatus::Running)
            .count();
        if running >= want {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// The `worker_id`s currently claimed in the main pool for the given
/// work items, resolved through the pool's claim table.
async fn claimed_worker_ids_for(
    coordinator: &Arc<ExecutionCoordinator>,
    db: &Arc<WorkDb>,
    chore_id: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    for claim in coordinator.worker_pool().claims().await {
        if let Ok(execution) = db.get_execution(&claim.execution_id)
            && execution.work_item_id == chore_id
        {
            out.push(claim.worker_id.clone());
        }
    }
    out
}

/// Test double for the preemption teardown. Mirrors the one
/// production-relevant side effect of `force_release` — the victim's
/// pool slot becomes free — so the dispatcher's "claim the slot the
/// teardown just freed" step exercises the real code path. Records
/// every call so tests can assert exactly how many preemptions fired.
struct FakePreemptor {
    pool: WorkerPool,
    outcome: PreemptOutcome,
    calls: Mutex<Vec<String>>,
}

impl FakePreemptor {
    fn new(pool: WorkerPool, outcome: PreemptOutcome) -> Self {
        Self {
            pool,
            outcome,
            calls: Mutex::new(Vec::new()),
        }
    }

    async fn calls(&self) -> Vec<String> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl AutomationPreemptor for FakePreemptor {
    async fn preempt_worker(&self, execution_id: &str) -> PreemptOutcome {
        self.calls.lock().await.push(execution_id.to_owned());
        if self.outcome == PreemptOutcome::Released {
            for claim in self.pool.claims().await {
                if claim.execution_id == execution_id {
                    self.pool.release_worker(&claim.worker_id, None).await;
                }
            }
        }
        self.outcome.clone()
    }
}

/// Spillover is a pressure valve, not a default: while the automation
/// pool still has room, automation runs there and never touches an
/// interactive slot — even though 16 of them are sitting idle.
#[tokio::test]
async fn automation_does_not_spill_while_its_own_pool_has_room() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    create_automation_chores(&db, &product.id, 2);
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(MAX_WORKER_POOL_SIZE),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coord.set_automation_pool(WorkerPool::new_automation(2));
    let coordinator = Arc::new(coord);
    coordinator.kick();
    wait_for_running_count(&db, 2).await;

    assert_eq!(
        coordinator.automation_worker_pool().idle_count().await,
        0,
        "both automation chores must occupy their own pool"
    );
    assert_eq!(
        coordinator.worker_pool().idle_count().await,
        MAX_WORKER_POOL_SIZE,
        "no automation may spill into an interactive slot while its own pool has room"
    );
}

/// Once the automation pool is full, the overflow spills — but into
/// Lower Decks (`worker-9`) only. Bridge Crew stays entirely free for
/// mainline even though `worker-1` is idle and would be the natural
/// lowest-index choice for an ordinary claim.
#[tokio::test]
async fn automation_spills_into_lower_decks_and_never_bridge_crew() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let auto_chores = create_automation_chores(&db, &product.id, 2);
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(MAX_WORKER_POOL_SIZE),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);
    coordinator.kick();
    wait_for_running_count(&db, 2).await;

    assert_eq!(
        coordinator.automation_worker_pool().idle_count().await,
        0,
        "the first automation chore takes the single automation slot"
    );

    let spilled: Vec<String> = {
        let mut all = Vec::new();
        for chore_id in &auto_chores {
            all.extend(claimed_worker_ids_for(&coordinator, &db, chore_id).await);
        }
        all
    };
    assert_eq!(
        spilled,
        vec!["worker-9".to_owned()],
        "the overflow automation chore must spill onto the first Lower Decks slot \
             (worker-9), leaving all 8 Bridge Crew slots free; got {spilled:?}"
    );
}

/// The core priority guarantee: mainline beats automation for an
/// interactive slot **regardless of arrival order**.
///
/// The automation chores are created FIRST here, so they sort ahead of
/// every mainline row in `list_ready_executions` (same dispatch class
/// and priority → `created_at ASC` decides). A single-pass dispatcher
/// that simply walked that order would hand the lone Lower Decks slot
/// to the earlier-arriving automation. The two-pass drain must not:
/// every mainline row claims before any automation row is allowed to
/// spill, so the later-arriving mainline chore takes the slot and the
/// automation stays queued.
#[tokio::test]
async fn mainline_beats_queued_spilled_automation_regardless_of_arrival_order() {
    const MAIN_POOL: usize = WORKER_PAGE_SIZE + 1; // 8 Bridge Crew + 1 Lower Decks

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    // Automation first → earlier `created_at` → sorts ahead.
    let auto_chores = create_automation_chores(&db, &product.id, 2);
    // Mainline second → later `created_at` → sorts behind, and there
    // are exactly enough of them to want every interactive slot.
    let main_chores: Vec<String> = (0..MAIN_POOL)
        .map(|n| create_test_chore(&db, product.id.clone(), format!("Regular chore {n}")).id)
        .collect();
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(MAIN_POOL),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    )
    .with_max_concurrent_interactive_workers(MAIN_POOL);
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);
    coordinator.kick();
    // 9 mainline + 1 automation (in its own pool) = 10.
    wait_for_running_count(&db, MAIN_POOL + 1).await;

    // Every interactive slot went to mainline.
    assert_eq!(
        coordinator.worker_pool().idle_count().await,
        0,
        "all interactive slots must be claimed"
    );
    for chore_id in &main_chores {
        assert_eq!(
            claimed_worker_ids_for(&coordinator, &db, chore_id).await.len(),
            1,
            "every mainline chore must hold an interactive slot, including the \
                 Lower Decks one the earlier-arriving automation wanted"
        );
    }

    // The overflow automation chore lost the Lower Decks slot despite
    // arriving first, and is still queued (not failed, not lost).
    let auto_executions: Vec<_> = auto_chores
        .iter()
        .flat_map(|id| db.list_executions(Some(id)).unwrap())
        .collect();
    let queued = auto_executions
        .iter()
        .filter(|e| e.status == ExecutionStatus::Ready)
        .count();
    assert_eq!(
        queued, 1,
        "the overflow automation chore must remain `ready` behind mainline; got {auto_executions:?}"
    );
}

/// Preemption is a last resort: while any interactive slot is free,
/// a mainline arrival claims it normally and no automation is touched.
#[tokio::test]
async fn preemption_does_not_fire_while_an_interactive_slot_is_free() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    create_automation_chores(&db, &product.id, 2);
    create_test_chore(&db, product.id.clone(), "Regular chore");
    db.reconcile_product_executions(&product.id).unwrap();

    let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE);
    let preemptor = Arc::new(FakePreemptor::new(pool.clone(), PreemptOutcome::Released));
    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        pool,
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coord.set_automation_pool(WorkerPool::new_automation(1));
    coord.set_automation_preemptor(preemptor.clone());
    let coordinator = Arc::new(coord);
    coordinator.kick();
    // 1 mainline + 1 automation in-pool + 1 automation spilled.
    wait_for_running_count(&db, 3).await;

    assert!(
        preemptor.calls().await.is_empty(),
        "no preemption may fire while interactive slots remain free; got {:?}",
        preemptor.calls().await
    );
}

/// When mainline is ready and every Bridge Crew AND Lower Decks slot
/// is occupied, the dispatcher preempts a spilled automation run —
/// and the preempted work is requeued losslessly: its execution goes
/// `cancelled` (never `failed`), a fresh execution is queued for the
/// same work item, and the item itself is untouched.
#[tokio::test]
async fn mainline_preempts_spilled_automation_when_every_interactive_slot_is_busy() {
    const MAIN_POOL: usize = WORKER_PAGE_SIZE + 1; // 8 Bridge Crew + 1 Lower Decks

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let auto_chores = create_automation_chores(&db, &product.id, 2);
    for n in 0..WORKER_PAGE_SIZE {
        create_test_chore(&db, product.id.clone(), format!("Bridge chore {n}"));
    }
    db.reconcile_product_executions(&product.id).unwrap();

    let pool = WorkerPool::new(MAIN_POOL);
    let preemptor = Arc::new(FakePreemptor::new(pool.clone(), PreemptOutcome::Released));
    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        pool,
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    )
    .with_max_concurrent_interactive_workers(MAIN_POOL);
    coord.set_automation_pool(WorkerPool::new_automation(1));
    coord.set_automation_preemptor(preemptor.clone());
    let coordinator = Arc::new(coord);
    coordinator.kick();
    // 8 Bridge Crew mainline + 1 automation in-pool + 1 spilled = 10.
    wait_for_running_count(&db, WORKER_PAGE_SIZE + 2).await;

    // Precondition: the interactive pool is full, with the Lower Decks
    // slot held by spilled automation.
    assert_eq!(
        coordinator.worker_pool().idle_count().await,
        0,
        "test precondition: every interactive slot must be busy"
    );
    let spilled_execution_id = coordinator
        .worker_pool()
        .claims()
        .await
        .into_iter()
        .find(|c| c.worker_id == "worker-9")
        .expect("test precondition: an automation run must hold the Lower Decks slot")
        .execution_id;
    let spilled_work_item = db.get_execution(&spilled_execution_id).unwrap().work_item_id;
    assert!(
        auto_chores.contains(&spilled_work_item),
        "test precondition: worker-9 must hold automation work"
    );
    assert!(preemptor.calls().await.is_empty(), "nothing preempted yet");

    // A mainline chore arrives with nowhere to go.
    let late = create_test_chore(&db, product.id.clone(), "Late mainline chore");
    db.request_execution(
        boss_protocol::RequestExecutionInput::builder()
            .work_item_id(late.id.clone())
            .build(),
    )
    .unwrap();
    coordinator.kick();

    for _ in 0..300 {
        if !preemptor.calls().await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        preemptor.calls().await,
        vec![spilled_execution_id.clone()],
        "the spilled automation run must be the preemption victim"
    );

    // The late mainline chore got the freed slot.
    for _ in 0..300 {
        if !claimed_worker_ids_for(&coordinator, &db, &late.id).await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        claimed_worker_ids_for(&coordinator, &db, &late.id).await,
        vec!["worker-9".to_owned()],
        "the preempting mainline chore must claim the slot the victim vacated"
    );

    // Lossless requeue: cancelled, not failed; work re-queued; item intact.
    let victim = db.get_execution(&spilled_execution_id).unwrap();
    assert_eq!(
        victim.status,
        ExecutionStatus::Cancelled,
        "a preempted execution must be cancelled, never failed — it did nothing wrong"
    );
    let replacements: Vec<_> = db
        .list_executions(Some(&spilled_work_item))
        .unwrap()
        .into_iter()
        .filter(|e| e.id != spilled_execution_id && !e.status.is_terminal())
        .collect();
    assert_eq!(
        replacements.len(),
        1,
        "the preempted automation work must be requeued as exactly one fresh \
             non-terminal execution; got {replacements:?}"
    );
}

/// No preemption cascade: a single starved mainline arrival takes out
/// at most ONE automation run, even when several spilled runs are
/// available to preempt. The second spilled run keeps going.
#[tokio::test]
async fn single_mainline_arrival_preempts_at_most_one_automation_run() {
    const MAIN_POOL: usize = WORKER_PAGE_SIZE + 2; // 8 Bridge Crew + 2 Lower Decks

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    create_automation_chores(&db, &product.id, 3);
    for n in 0..WORKER_PAGE_SIZE {
        create_test_chore(&db, product.id.clone(), format!("Bridge chore {n}"));
    }
    db.reconcile_product_executions(&product.id).unwrap();

    let pool = WorkerPool::new(MAIN_POOL);
    let preemptor = Arc::new(FakePreemptor::new(pool.clone(), PreemptOutcome::Released));
    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        pool,
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    )
    .with_max_concurrent_interactive_workers(MAIN_POOL);
    coord.set_automation_pool(WorkerPool::new_automation(1));
    coord.set_automation_preemptor(preemptor.clone());
    let coordinator = Arc::new(coord);
    coordinator.kick();
    // 8 Bridge Crew + 1 automation in-pool + 2 spilled = 11.
    wait_for_running_count(&db, WORKER_PAGE_SIZE + 3).await;
    assert_eq!(
        coordinator.worker_pool().idle_count().await,
        0,
        "test precondition: every interactive slot must be busy, two by spilled automation. \
             claims={:?} executions={:?}",
        coordinator.worker_pool().claims().await,
        db.list_executions(None)
            .unwrap()
            .iter()
            .map(|e| (e.id.clone(), e.status.to_string()))
            .collect::<Vec<_>>(),
    );

    let late = create_test_chore(&db, product.id.clone(), "Late mainline chore");
    db.request_execution(
        boss_protocol::RequestExecutionInput::builder()
            .work_item_id(late.id.clone())
            .build(),
    )
    .unwrap();
    coordinator.kick();

    for _ in 0..300 {
        if !claimed_worker_ids_for(&coordinator, &db, &late.id).await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    // Settle: give any (incorrect) follow-on drain a chance to preempt
    // again, so this assertion is meaningful rather than merely early.
    sleep(Duration::from_millis(150)).await;

    assert_eq!(
        preemptor.calls().await.len(),
        1,
        "one mainline arrival must preempt exactly one automation run; got {:?}",
        preemptor.calls().await
    );
}

/// Regression test: spilled automation must never push live
/// interactive-pool workers past `max_concurrent_interactive_workers`,
/// even when the automation pool is full, Lower Decks has physically
/// free slots, and no mainline row is ready to trigger preemption.
/// Before the fix, `claim_worker_spill` never consulted the cap at
/// all, so this scenario let live interactive workers double the cap.
#[tokio::test]
async fn spilled_automation_does_not_exceed_the_interactive_concurrency_cap() {
    const MAIN_POOL: usize = WORKER_PAGE_SIZE * 2; // 8 Bridge Crew + 8 Lower Decks
    const CAP: usize = WORKER_PAGE_SIZE; // cap == Bridge Crew size: no spill room at all

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    // Fill every Bridge Crew slot with mainline work, so busy_count()
    // already equals the cap before automation ever gets a look.
    for n in 0..WORKER_PAGE_SIZE {
        create_test_chore(&db, product.id.clone(), format!("Bridge chore {n}"));
    }
    // More automation work than the (small) automation pool can hold,
    // so several rows become Lower Decks spill candidates.
    create_automation_chores(&db, &product.id, WORKER_PAGE_SIZE + 2);
    db.reconcile_product_executions(&product.id).unwrap();

    let pool = WorkerPool::new(MAIN_POOL);
    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        pool,
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    )
    .with_max_concurrent_interactive_workers(CAP);
    coord.set_automation_pool(WorkerPool::new_automation(2));
    let coordinator = Arc::new(coord);
    coordinator.kick();
    // 8 Bridge Crew mainline + 2 automation running in their own pool.
    wait_for_running_count(&db, WORKER_PAGE_SIZE + 2).await;

    // Give any (incorrect) spill claim a chance to land.
    sleep(Duration::from_millis(150)).await;

    assert_eq!(
        coordinator.worker_pool().busy_count().await,
        CAP,
        "spilled automation must never push live interactive-pool workers past the cap; claims={:?}",
        coordinator.worker_pool().claims().await,
    );
    assert_eq!(
        coordinator.worker_pool().idle_count().await,
        MAIN_POOL - CAP,
        "Lower Decks slots must stay idle while the interactive cap is already \
             saturated by mainline, with no mainline row ready to trade via preemption"
    );
}

/// A preempted automation item is not dropped: once interactive
/// capacity frees up, its requeued execution dispatches normally, as
/// if it had just arrived.
#[tokio::test]
async fn preempted_automation_work_redispatches_once_capacity_frees() {
    const MAIN_POOL: usize = WORKER_PAGE_SIZE + 1;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    create_automation_chores(&db, &product.id, 2);
    for n in 0..WORKER_PAGE_SIZE {
        create_test_chore(&db, product.id.clone(), format!("Bridge chore {n}"));
    }
    db.reconcile_product_executions(&product.id).unwrap();

    let pool = WorkerPool::new(MAIN_POOL);
    let preemptor = Arc::new(FakePreemptor::new(pool.clone(), PreemptOutcome::Released));
    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        pool.clone(),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    )
    .with_max_concurrent_interactive_workers(MAIN_POOL);
    coord.set_automation_pool(WorkerPool::new_automation(1));
    coord.set_automation_preemptor(preemptor.clone());
    let coordinator = Arc::new(coord);
    coordinator.kick();
    wait_for_running_count(&db, WORKER_PAGE_SIZE + 2).await;

    let spilled_execution_id = coordinator
        .worker_pool()
        .claims()
        .await
        .into_iter()
        .find(|c| c.worker_id == "worker-9")
        .expect("test precondition: automation must hold the Lower Decks slot")
        .execution_id;
    let spilled_work_item = db.get_execution(&spilled_execution_id).unwrap().work_item_id;

    let late = create_test_chore(&db, product.id.clone(), "Late mainline chore");
    db.request_execution(
        boss_protocol::RequestExecutionInput::builder()
            .work_item_id(late.id.clone())
            .build(),
    )
    .unwrap();
    coordinator.kick();
    for _ in 0..300 {
        if !preemptor.calls().await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(preemptor.calls().await.len(), 1, "the spilled automation was preempted");

    // Free the automation pool's own slot, as a finishing automation
    // worker would, and kick. The requeued work must dispatch onto it.
    //
    // Note it must be an automation-pool slot (or a Lower Decks one) —
    // freeing a Bridge Crew slot deliberately would NOT help, since
    // automation may never claim page 0. That asymmetry is the feature,
    // not an oversight.
    coordinator
        .automation_worker_pool()
        .release_worker("auto-worker-1", None)
        .await;
    coordinator.kick();

    let mut redispatched = None;
    for _ in 0..300 {
        let running: Vec<_> = db
            .list_executions(Some(&spilled_work_item))
            .unwrap()
            .into_iter()
            .filter(|e| e.id != spilled_execution_id && e.status == ExecutionStatus::Running)
            .collect();
        if let Some(execution) = running.first() {
            redispatched = Some(execution.clone());
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    let redispatched = redispatched.expect(
        "the preempted automation work must redispatch on a later pass once a slot frees — \
             it must not be lost, failed, or left queued forever",
    );
    assert_ne!(
        redispatched.id, spilled_execution_id,
        "redispatch must be a fresh execution, not the cancelled victim"
    );
}
