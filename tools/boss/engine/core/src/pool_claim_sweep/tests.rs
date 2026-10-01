use std::sync::Arc;

use boss_protocol::WorkItemBinding;

use super::*;
use crate::coordinator::MAX_AUTOMATION_POOL_SIZE;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::test_support::*;
use crate::work::WorkDb;

mod attention;

struct NoViewers;

#[async_trait::async_trait]
impl WorkerViewerDetach for NoViewers {
    async fn confirm_process_torn_down(&self, _: &str) -> Result<(), String> {
        Err("test teardown refused".into())
    }

    async fn confirm_viewers_detached(&self, run_ids: &[String]) -> Vec<Result<(), String>> {
        run_ids.iter().map(|_| Ok(())).collect()
    }
}

fn create_execution(db: &WorkDb, work_item_id: &str) -> String {
    use boss_protocol::RequestExecutionInput;
    db.request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
        .unwrap()
        .id
}

/// Raw UPDATE to drive an execution to `completed` — exercises the
/// completion-path terminal status without a full running-run setup.
fn force_completed(db: &WorkDb, execution_id: &str) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
        rusqlite::params![execution_id],
    )
    .unwrap();
}

/// Stamp `finished_at` to `secs_ago` seconds in the past so the
/// leak-grace guard treats the claim as genuinely stuck (the terminal
/// paths stamp `finished_at = now`, which is inside the grace).
fn age_finished_at(db: &WorkDb, execution_id: &str, secs_ago: i64) {
    let epoch = boss_engine_utils::epoch_time::now_epoch_secs() - secs_ago;
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
        rusqlite::params![execution_id, epoch.to_string()],
    )
    .unwrap();
}

fn register_live_pane(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str) {
    live_states.register_spawn(
        slot_id,
        execution_id,
        "claude-opus-4-8",
        std::process::id() as i32,
        Some(WorkItemBinding {
            work_item_id: "wi".to_owned(),
            work_item_name: "chore".to_owned(),
            execution_id: execution_id.to_owned(),
        }),
    );
}

// ─── tests ───────────────────────────────────────────────────────────────

/// The core regression: claim all 3 automation slots, terminate each
/// holder via a DIFFERENT terminal path (orphaned / cancelled /
/// completed), and assert the sweep returns the pool to 0/3 (so a new
/// triage can dispatch) and emits one `pool_claim_reconcile` event
/// per freed slot.
#[tokio::test]
async fn frees_every_leaked_automation_claim_across_terminal_paths() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let db = Arc::new(db);

    let coordinator = make_coordinator(db.clone(), 0);
    let pool = coordinator.automation_worker_pool();

    // Three leaked claims, each terminated via a distinct path.
    let exec_orphaned = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
    let exec_cancelled = create_execution(&db, &create_active_chore(&db, &product_id, "b"));
    let exec_completed = create_execution(&db, &create_active_chore(&db, &product_id, "c"));

    for exec in [&exec_orphaned, &exec_cancelled, &exec_completed] {
        let worker_id = pool.claim_worker(exec, None).await.unwrap();
        assert!(worker_id.starts_with("auto-worker-"));
    }
    assert_eq!(
        pool.idle_count().await,
        MAX_AUTOMATION_POOL_SIZE - 3,
        "pool must have exactly the three leaked claims outstanding",
    );

    // Terminate the holders, one per terminal path, then age each
    // past the leak grace (the terminal paths stamp finished_at=now).
    db.mark_execution_orphaned(&exec_orphaned, "test orphan").unwrap();
    assert!(db.cancel_running_execution(&exec_cancelled).unwrap());
    force_completed(&db, &exec_completed);
    for exec in [&exec_orphaned, &exec_cancelled, &exec_completed] {
        age_finished_at(&db, exec, 300);
    }

    // No live-state entries — this is the documented "3/3 busy, zero
    // live workers" wedge.
    let live_states = LiveWorkerStateRegistry::new();
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        &NoViewers,
        &mut TeardownRetries::default(),
    )
    .await;

    assert_eq!(outcome.released, 3, "all three leaked claims must be freed");
    assert_eq!(outcome.live_backed_skipped, 0);
    assert_eq!(outcome.non_terminal_skipped, 0);

    assert_eq!(
        pool.idle_count().await,
        MAX_AUTOMATION_POOL_SIZE,
        "automation pool must be fully idle after the sweep — dispatch unwedged",
    );
    assert!(pool.claimed_execution_ids().await.is_empty(), "no claims may remain",);

    // One pool_claim_reconcile event per freed slot, carrying the
    // worker_id and terminal status so the leak is diagnosable.
    let events = sink.events().await;
    assert_eq!(events.len(), 3, "expected one event per released claim");
    for event in &events {
        assert_eq!(event.stage, "pool_claim_reconcile");
        assert_eq!(event.outcome, "ok");
        assert_eq!(event.details["pool"], "automation");
        assert!(event.worker_id.as_deref().unwrap().starts_with("auto-worker-"));
    }
}

/// A claim whose execution is still non-terminal (a legitimately held
/// slot — claimed at dispatch, spawn in flight) is left alone.
#[tokio::test]
async fn leaves_non_terminal_claims_alone() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let db = Arc::new(db);

    let coordinator = make_coordinator(db.clone(), 0);
    let pool = coordinator.automation_worker_pool();

    let exec = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
    let worker_id = pool.claim_worker(&exec, None).await.unwrap();

    // Execution left in `ready` (non-terminal).
    let live_states = LiveWorkerStateRegistry::new();
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        &NoViewers,
        &mut TeardownRetries::default(),
    )
    .await;

    assert_eq!(outcome.released, 0);
    assert_eq!(outcome.non_terminal_skipped, 1);
    assert!(
        pool.claimed_execution_ids().await.contains(&exec),
        "non-terminal claim must remain held",
    );
    assert!(sink.events().await.is_empty());
    let _ = worker_id;
}

/// A terminal execution that STILL has a live worker pane is left to
/// the completion / dead-pid / stale-worker paths — releasing it here
/// would race a pane that may still be physically up (SlotBusy).
#[tokio::test]
async fn leaves_live_backed_claims_to_the_completion_path() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let db = Arc::new(db);

    let coordinator = make_coordinator(db.clone(), 0);
    let pool = coordinator.automation_worker_pool();

    let exec = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
    let worker_id = pool.claim_worker(&exec, None).await.unwrap();
    // Derive the automation slot from the claimed worker id rather than
    // hard-coding it: the automation range floats above the interactive
    // pool (auto-worker-1 → MAX_WORKER_POOL_SIZE + 1), so it moves when the
    // interactive pool grows a page.
    let slot = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
    db.mark_execution_orphaned(&exec, "terminal but pane still up").unwrap();

    let live_states = LiveWorkerStateRegistry::new();
    register_live_pane(&live_states, slot, &exec);
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        &NoViewers,
        &mut TeardownRetries::default(),
    )
    .await;

    assert_eq!(outcome.released, 0, "live-backed claim must not be released");
    assert_eq!(outcome.live_backed_skipped, 1);
    assert!(
        pool.claimed_execution_ids().await.contains(&exec),
        "live-backed claim must remain held",
    );
    assert!(sink.events().await.is_empty());
    let _ = worker_id;
}

/// A claim whose execution went terminal just now (within the leak
/// grace) is left alone — a legitimate teardown may still be in
/// flight; releasing it could race `run_execution`'s unconditional
/// tail release and double-free a re-claimed slot.
#[tokio::test]
async fn leaves_freshly_terminalized_claims_within_grace() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let db = Arc::new(db);

    let coordinator = make_coordinator(db.clone(), 0);
    let pool = coordinator.automation_worker_pool();

    let exec = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
    pool.claim_worker(&exec, None).await.unwrap();
    // Terminal, finished_at = now (inside the grace window).
    db.mark_execution_orphaned(&exec, "just terminated").unwrap();

    let live_states = LiveWorkerStateRegistry::new();
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        &NoViewers,
        &mut TeardownRetries::default(),
    )
    .await;

    assert_eq!(outcome.released, 0, "fresh terminal claim must wait out the grace");
    assert_eq!(outcome.grace_skipped, 1);
    assert!(
        pool.claimed_execution_ids().await.contains(&exec),
        "claim must remain held during the grace",
    );
    assert!(sink.events().await.is_empty());
}

/// A leaked MAIN-pool claim is also reconciled (the sweep walks both
/// pools), and the compare-and-release is idempotent across passes.
#[tokio::test]
async fn frees_main_pool_claim_and_is_idempotent() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let db = Arc::new(db);

    let coordinator = make_coordinator(db.clone(), 2);
    let pool = coordinator.worker_pool();

    let exec = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
    let worker_id = pool.claim_worker(&exec, None).await.unwrap();
    assert!(worker_id.starts_with("worker-"));
    db.mark_execution_orphaned(&exec, "test orphan").unwrap();
    age_finished_at(&db, &exec, 300);

    let live_states = LiveWorkerStateRegistry::new();
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let first = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        &NoViewers,
        &mut TeardownRetries::default(),
    )
    .await;
    assert_eq!(first.released, 1);
    assert_eq!(pool.idle_count().await, 2, "main pool fully idle after release");

    // Second pass: nothing left to release.
    let second = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        &NoViewers,
        &mut TeardownRetries::default(),
    )
    .await;
    assert_eq!(second.released, 0);
    assert_eq!(
        sink.events().await.len(),
        1,
        "no duplicate event on the idempotent pass"
    );
}

/// A SlotBusy-rejected spawn whose tmux teardown was unconfirmed still
/// has durable identity. Viewer absence is not proof the process is
/// gone, so the sweep must retain the claim.
#[tokio::test]
async fn retains_claim_when_tmux_identity_is_still_recorded() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let (exec, token) = start_tmux_run(&db);
    let coordinator = make_coordinator(db.clone(), 2);
    let pool = coordinator.worker_pool();
    pool.claim_worker(&exec, None).await.unwrap();
    db.mark_execution_orphaned(&exec, "viewer abort unconfirmed").unwrap();
    age_finished_at(&db, &exec, 300);

    let live_states = LiveWorkerStateRegistry::new();
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let mut retries = TeardownRetries::default();
    for pass in 1..=3 {
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            &NoViewers,
            &mut retries,
        )
        .await;
        assert_eq!(outcome.released, 0);
        assert_eq!(outcome.process_teardown_pending, 1);
        assert_eq!(db.list_attention_items(&exec).unwrap().len(), usize::from(pass == 3));
    }
    assert_eq!(pool.idle_count().await, 1, "the unreaped claim must stay held");
    assert!(sink.events().await.is_empty());
    // The session is now confirmed gone, but the database clear can still fail.
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_identity_clear BEFORE UPDATE OF tmux_spawn_token ON work_runs
         WHEN NEW.tmux_spawn_token IS NULL BEGIN SELECT RAISE(ABORT, 'clear unavailable'); END;",
        )
        .unwrap();
    let teardown = ClearIdentity { db: &db, token: &token };
    let failed_clear = run_one_pass(
        &db,
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        &teardown,
        &mut retries,
    )
    .await;
    assert_eq!(failed_clear.process_teardown_pending, 1);
    assert_eq!(failed_clear.released, 0);
    assert!(db.tmux_identity_for_execution(&exec).unwrap().is_some());
    db.connect()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_identity_clear")
        .unwrap();
    let recovered = run_one_pass(&db, &live_states, coordinator, sink.as_ref(), &teardown, &mut retries).await;
    assert_eq!(recovered.released, 1);
    assert_eq!(pool.idle_count().await, 2);
    assert!(db.tmux_identity_for_execution(&exec).unwrap().is_none());
    let attention = db.list_attention_items(&exec).unwrap();
    assert_eq!(attention.len(), 1);
    assert!(attention[0].resolved_at.is_some());
}

struct ClearIdentity<'a> {
    db: &'a WorkDb,
    token: &'a str,
}

#[async_trait::async_trait]
impl WorkerViewerDetach for ClearIdentity<'_> {
    async fn confirm_process_torn_down(&self, execution_id: &str) -> Result<(), String> {
        self.db
            .clear_tmux_identity_for_execution(execution_id, self.token)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    async fn confirm_viewers_detached(&self, run_ids: &[String]) -> Vec<Result<(), String>> {
        run_ids.iter().map(|_| Ok(())).collect()
    }
}
