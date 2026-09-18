use std::sync::Arc;

use super::*;
use crate::coordinator::{ExecutionCoordinator, WorkerPool};
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::test_support::*;
use crate::work::{CreateRevisionInput, ExecutionStatus, PrOpenState, StaticPrStateChecker, WorkDb, WorkItemPatch};
use crate::worker_readoption::NoopLiveWorkerConvergence;

/// Stamp tasks.updated_at to 10 minutes ago so the age guard passes.
fn make_old(db: &WorkDb, work_item_id: &str) {
    let old_epoch = boss_engine_utils::epoch_time::now_epoch_secs() - 600;
    db.force_updated_at_for_test(work_item_id, old_epoch).unwrap();
}

/// Like `make_coordinator` but also installs a review pool of `review_pool_size`.
/// Returns both the coordinator and the review pool so the caller can claim slots.
fn make_coordinator_with_review_pool(
    db: Arc<WorkDb>,
    pool_size: usize,
    review_pool_size: usize,
) -> (Arc<ExecutionCoordinator>, WorkerPool) {
    let review_pool = WorkerPool::new_review(review_pool_size);
    let mut coordinator =
        ExecutionCoordinator::new(db, WorkerPool::new(pool_size), Arc::new(NoopCube), Arc::new(NoopRunner));
    coordinator.set_review_pool(review_pool.clone());
    (Arc::new(coordinator), review_pool)
}

/// A pid guaranteed not to exist, so `kill(pid, 0)` returns `ESRCH`.
/// Mirrors the same helper in `dead_pid_sweep`'s tests.
fn dead_pid() -> i64 {
    4_194_303
}

/// Records every convergence trigger so a test can assert the sweep did
/// not merely *skip* the row but handed the contradiction on to be
/// resolved.
#[derive(Default)]
struct RecordingConvergence {
    converged: std::sync::Mutex<Vec<(String, String)>>,
}

impl RecordingConvergence {
    fn converged(&self) -> Vec<(String, String)> {
        self.converged.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LiveWorkerConvergence for RecordingConvergence {
    async fn converge_live_worker(&self, execution_id: &str, trigger: &str) {
        self.converged
            .lock()
            .unwrap()
            .push((execution_id.to_owned(), trigger.to_owned()));
    }
}

// ─── tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn deferred_review_admission_is_not_an_orphaned_implementation() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "delivered chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    let pr_url = "https://github.com/spinyfin/mono/pull/4044";
    db.record_worker_pr_completion(
        &execution_id,
        pr_url,
        None,
        None,
        crate::work::WorkerPrCompletionTarget::PendingReview,
        None,
    )
    .unwrap();
    crate::completion::file_admission_deferred_attention(&db, &work_item_id, pr_url);
    make_old(&db, &work_item_id);
    let before = db.list_executions(Some(&work_item_id)).unwrap().len();
    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = RecordingDispatchEventSink::new();
    let outcome = run_one_pass(db.as_ref(), coordinator, &sink, &NoopLiveWorkerConvergence).await;
    assert_eq!(outcome.redispatched, 0);
    assert_eq!(db.list_executions(Some(&work_item_id)).unwrap().len(), before);
    assert!(db.list_orphan_active_candidates(0).unwrap().is_empty());
}

/// **The 2026-07-28 duplicate-dispatch regression.**
///
/// Reproduces the exact production shape: an execution the engine
/// terminalized (`orphaned`) whose worker process is still running, on an
/// item that every pre-existing guard reads as a legitimate orphan — its
/// status is terminal so no live-execution lookup finds it, its pool claim
/// was released so `claimed` does not contain it, and the churn window is
/// empty. The durable-pid guard must still prevent duplicate workers.
///
/// A redispatch
/// attempt for a row whose prior process is still running must not produce
/// a second live worker.
#[tokio::test]
async fn does_not_redispatch_over_a_still_running_worker_process() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    // Our own pid stands in for the worker's still-running shell.
    let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));
    db.mark_execution_orphaned(&execution_id, "spawn-ack timeout; worker presumed dead")
        .unwrap();
    // Age the item LAST: the execution/run writes above touch
    // `tasks.updated_at`, so ageing first would be undone by them and the
    // item would never clear ORPHAN_MIN_AGE_SECS.
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    // Nothing claimed: the pool released the slot when the execution was
    // terminalized; durable process liveness must still prevent redispatch.
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let convergence = RecordingConvergence::default();

    let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

    assert_eq!(
        outcome.redispatched, 0,
        "a second worker must never be dispatched onto a row whose first worker is alive",
    );
    assert_eq!(outcome.live_process_skipped, 1);

    // No new execution row at all — a `ready` row here would be dispatched
    // by the scheduler on its next drain, which is the duplicate.
    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert!(
        executions.iter().all(|e| e.status != ExecutionStatus::Ready),
        "no fresh ready execution may be created while the prior process lives",
    );

    let events = sink.events().await;
    let blocked: Vec<_> = events
        .iter()
        .filter(|e| e.stage == "redispatch_blocked_live_process")
        .collect();
    assert_eq!(blocked.len(), 1, "the prevented duplicate must be observable");
    assert_eq!(blocked[0].outcome, "skipped");
    assert_eq!(
        blocked[0].details["blocking_execution_id"],
        serde_json::json!(execution_id)
    );
    assert_eq!(
        blocked[0].details["blocking_execution_status"],
        serde_json::json!("orphaned"),
        "the blocking row being TERMINAL is the whole point — that is what every other \
         guard reads as 'safe to redispatch'",
    );
    assert!(
        events.iter().all(|e| e.stage != "orphan_active_redispatch"),
        "no redispatch event may fire",
    );

    // Blocking alone would park the row forever; the contradiction must be
    // handed on for resolution.
    assert_eq!(
        convergence.converged(),
        vec![(execution_id, "redispatch_guard".to_owned())],
        "the guard must trigger convergence, not just decline",
    );
}

/// The guard must not become a permanent block. Once the worker process is
/// genuinely gone, the same row redispatches exactly as before — this is
/// what keeps the post-crash recovery the sweep exists for working.
#[tokio::test]
async fn redispatches_normally_once_the_prior_process_is_gone() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let convergence = RecordingConvergence::default();

    let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

    assert_eq!(
        outcome.redispatched, 1,
        "a dead prior process must not block recovery — that is what this sweep is for",
    );
    assert_eq!(outcome.live_process_skipped, 0);
    assert!(
        convergence.converged().is_empty(),
        "there is no contradiction to converge when the process is really gone",
    );
}

/// **The redispatch-guard half of the "live workers false-reaped as
/// orphaned" incident.** The row's tracked pid probes dead — same as
/// `redispatches_normally_once_the_prior_process_is_gone` — but the
/// execution has emitted a hook well within the corroboration window.
/// Without corroboration this guard reads the same wrong `Gone` verdict
/// a false-reaping sweep just acted on and fails open, letting a second
/// worker dispatch onto a row whose first worker is still running. With
/// it, the guard must block exactly as if the probe had said `Alive`.
#[tokio::test]
async fn corroborated_activity_blocks_redispatch_despite_a_dead_probe() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&execution_id, "worker presumed dead")
        .unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(1, &execution_id, "claude-opus-4-7", 424242, None);
    live_states.apply_event(
        1,
        &boss_protocol::WorkerEvent::PreToolUse {
            session_id: "s".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
        },
    );
    live_states.apply_event(
        1,
        &boss_protocol::WorkerEvent::PostToolUse {
            session_id: "s".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
    );

    let mut coordinator =
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), Arc::new(NoopCube), Arc::new(NoopRunner));
    coordinator.set_live_worker_states(live_states);
    let coordinator = Arc::new(coordinator);

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let convergence = RecordingConvergence::default();

    let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

    assert_eq!(
        outcome.redispatched, 0,
        "corroborated activity must block the redispatch even though the tracked pid probed dead",
    );
    assert_eq!(outcome.live_process_skipped, 1);

    let events = sink.events().await;
    let blocked: Vec<_> = events
        .iter()
        .filter(|e| e.stage == "redispatch_blocked_live_process")
        .collect();
    assert_eq!(blocked.len(), 1, "the corroborated block must be observable");
    assert_eq!(
        blocked[0].details["corroborated_alive"],
        serde_json::json!(true),
        "the event must record that corroboration (not a raw Alive probe) is what blocked this",
    );
    assert!(
        events.iter().all(|e| e.stage != "redispatch_guard_declined"),
        "a corroborated block is not a decline",
    );
    assert_eq!(
        convergence.converged(),
        vec![(execution_id, "redispatch_guard".to_owned())],
        "a corroborated block must still hand the contradiction on for resolution",
    );
}

/// The instrumentation gap this closes: before this event existed, the
/// guard was silent whenever it declined to block — diagnosing a wrongly
/// -declined redispatch required cross-referencing this sweep's trace
/// lines against a different sweep's, 45ms apart. Every decline (with an
/// actual probed pid to report on) must now be self-diagnosing from a
/// single dispatch tail.
#[tokio::test]
async fn declined_guard_emits_instrumentation_event() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let convergence = RecordingConvergence::default();

    let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

    assert_eq!(
        outcome.redispatched, 1,
        "a genuinely gone process must not block recovery"
    );

    let events = sink.events().await;
    let declined: Vec<_> = events
        .iter()
        .filter(|e| e.stage == "redispatch_guard_declined")
        .collect();
    assert_eq!(declined.len(), 1, "the guard's decline must be observable, not silent");
    assert_eq!(declined[0].outcome, "ok");
    assert_eq!(
        declined[0].details["blocking_execution_id"],
        serde_json::json!(execution_id)
    );
    assert_eq!(declined[0].details["probe_result"], serde_json::json!("process_gone"),);
    assert!(
        declined[0].details["shell_pid"].is_number(),
        "the probed pid must be carried for diagnosis: {:?}",
        declined[0].details,
    );
}

/// The acceptance criterion for the decline event: when a registry entry
/// exists, the payload must carry `last_event_age_secs` so an operator
/// reading `bossctl dispatch diagnose` can tell a correct decline (hook
/// aged out of the corroboration window) from a wrong one (recent hook
/// that should have blocked). The no-registry
/// [`declined_guard_emits_instrumentation_event`] case leaves these null.
#[tokio::test]
async fn declined_guard_event_carries_last_hook_age() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(1, &execution_id, "claude-opus-4-7", 424242, None);
    live_states.apply_event(
        1,
        &boss_protocol::WorkerEvent::PreToolUse {
            session_id: "s".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
        },
    );
    live_states.apply_event(
        1,
        &boss_protocol::WorkerEvent::PostToolUse {
            session_id: "s".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
    );
    // Older than the corroboration window so the guard still declines
    // (a recent hook would block redispatch via corroboration instead).
    let seeded_age_secs = crate::durable_liveness::CORROBORATION_WINDOW_SECS + 90;
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    live_states.set_last_event_at_for_test(1, crate::live_worker_state::iso8601_utc(now - seeded_age_secs));

    let mut coordinator =
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), Arc::new(NoopCube), Arc::new(NoopRunner));
    coordinator.set_live_worker_states(live_states);
    let coordinator = Arc::new(coordinator);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let convergence = RecordingConvergence::default();

    let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

    assert_eq!(
        outcome.redispatched, 1,
        "a hook aged past the corroboration window must not block recovery"
    );

    let events = sink.events().await;
    let declined: Vec<_> = events
        .iter()
        .filter(|e| e.stage == "redispatch_guard_declined")
        .collect();
    assert_eq!(declined.len(), 1, "the guard's decline must be observable");
    let age = declined[0].details["last_event_age_secs"]
        .as_i64()
        .expect("last_event_age_secs must be a number when a registry hook exists");
    assert!(
        (age - seeded_age_secs).abs() <= 5,
        "last_event_age_secs ({age}) must roughly match the seeded age ({seeded_age_secs})",
    );
    assert!(
        declined[0].details["last_event_at"].is_string(),
        "last_event_at must also be present: {:?}",
        declined[0].details,
    );
}

/// A work item with no recorded worker process at all has nothing for the
/// guard to decline — no instrumentation event may fire for it, or every
/// ordinary redispatch of a never-dispatched item would emit noise.
#[tokio::test]
async fn no_recorded_pid_emits_no_decline_event() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_old_execution(&db, &work_item_id);
    db.mark_execution_orphaned(&execution_id, "spawn produced no shell")
        .unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.redispatched, 1);
    let events = sink.events().await;
    assert!(
        events.iter().all(|e| e.stage != "redispatch_guard_declined"),
        "a work item with no recorded pid has nothing to decline",
    );
}

/// A worker that never reported a pid (mid-spawn, or a spawn that never
/// produced a shell) must not be treated as alive. `Unknown` is not
/// `Alive`: reading it as such would disable orphan recovery for every
/// execution that dies before `UpdateWorkerShellPid`.
#[tokio::test]
async fn a_never_reported_pid_does_not_block_redispatch() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_old_execution(&db, &work_item_id);
    db.mark_execution_orphaned(&execution_id, "spawn produced no shell")
        .unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.live_process_skipped, 0);
    assert_eq!(outcome.redispatched, 1);
}

/// Orphan with NO execution → gets redispatched; dispatch event emitted.
#[tokio::test]
async fn redispatches_active_item_with_no_execution() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.redispatched, 1, "should have redispatched one item");

    let events = sink.events().await;
    assert_eq!(events.len(), 1, "expected exactly one dispatch event");
    assert_eq!(events[0].stage, "orphan_active_redispatch");
    assert_eq!(events[0].outcome, "ok");
    assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert!(
        executions.iter().any(|e| e.status == ExecutionStatus::Ready),
        "expected a ready execution after redispatch"
    );
}

/// Active item with a live execution claimed by a worker slot → no-op.
#[tokio::test]
async fn skips_item_with_live_execution() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    // Insert a ready execution and claim it in the pool — this makes
    // the item appear "already queued" (no-candidate via DB query).
    let execution = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
        )
        .unwrap();
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution.id, None).await;

    let sink = Arc::new(RecordingDispatchEventSink::new());
    // With a `ready` execution the DB query filters the item out.
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.redispatched, 0);
    assert!(sink.events().await.is_empty());
}

/// All worker slots busy → no replacement execution is minted.
#[tokio::test]
async fn no_redispatch_when_all_workers_busy() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker("dummy-exec-id", None).await;

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.redispatched, 0);
    assert_eq!(outcome.no_worker_skipped, 1);
    assert!(sink.events().await.is_empty());
}

// ─── admission gates (pause / autostart) ────────────────────────────

/// **The 2026-09-13 paused-dispatch redispatch.** Global dispatch was
/// paused, and this sweep minted a fresh execution anyway — abandoning
/// the predecessor's work in the process. Its only admission gate was
/// `has_idle_worker()`, which is a slot-occupancy question, not an
/// admission question; worse, a pause stops anything *consuming* worker
/// slots, so pausing dispatch made this sweep strictly more likely to
/// fire, not less.
#[tokio::test]
async fn does_not_redispatch_while_dispatch_is_paused() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let now = boss_engine_utils::epoch_time::now_epoch_secs().max(0) as u64;
    coordinator.pause_dispatch(
        now,
        crate::coordinator::DispatchPauseOrigin::Operator,
        boss_protocol::PauseReason::new("test: operator paused dispatch").unwrap(),
    );

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.redispatched, 0,
        "a paused dispatcher must not have work redispatched onto it behind its back",
    );
    assert_eq!(outcome.dispatch_paused_skipped, 1);

    // The destructive half is creating the row at all: doing so marks
    // the predecessor `abandoned` and discards its workspace.
    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert_eq!(
        executions.len(),
        1,
        "no fresh execution may be minted while dispatch is paused; got {executions:?}",
    );

    let events = sink.events().await;
    let held: Vec<_> = events.iter().filter(|e| e.stage == "dispatch_held_by_pause").collect();
    assert_eq!(held.len(), 1, "the hold must be visible in the dispatch stream");
    assert_eq!(held[0].outcome, "skipped");
    assert_eq!(
        held[0].details["admission"],
        serde_json::json!("orphan_sweep_redispatch"),
    );
    assert!(
        events.iter().all(|e| e.stage != "orphan_active_redispatch"),
        "no redispatch event may fire while dispatch is paused",
    );
}

/// The pause gate holds the redispatch; it does not disable the sweep.
/// The same row recovers on the first pass after the pause lifts —
/// orphan recovery is deferred by a pause, never cancelled by one.
#[tokio::test]
async fn redispatches_once_the_pause_lifts() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let now = boss_engine_utils::epoch_time::now_epoch_secs().max(0) as u64;
    coordinator.pause_dispatch(
        now,
        crate::coordinator::DispatchPauseOrigin::Operator,
        boss_protocol::PauseReason::new("test: operator paused dispatch").unwrap(),
    );
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let held = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;
    assert_eq!(held.redispatched, 0, "precondition: the pause held it");

    coordinator.resume_dispatch();
    let resumed = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        resumed.redispatched, 1,
        "a lifted pause must let the genuinely-orphaned row recover — the sweep is deferred \
         by a pause, not disabled by one",
    );
    assert_eq!(resumed.dispatch_paused_skipped, 0);
}

/// A breaker-origin pause holds the redispatch exactly as an operator
/// one does. The sweep asks the shared admission evaluator rather than
/// carrying its own idea of what a pause means, so it inherits every
/// pause's real scope instead of re-deciding it.
#[tokio::test]
async fn a_breaker_pause_also_holds_the_redispatch() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let now = boss_engine_utils::epoch_time::now_epoch_secs().max(0) as u64;
    coordinator.pause_dispatch(
        now,
        crate::coordinator::DispatchPauseOrigin::Breaker,
        boss_protocol::PauseReason::new("test: breaker tripped").unwrap(),
    );

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.redispatched, 0);
    assert_eq!(outcome.dispatch_paused_skipped, 1);
    let events = sink.events().await;
    let held: Vec<_> = events.iter().filter(|e| e.stage == "dispatch_held_by_pause").collect();
    assert_eq!(held[0].details["origin"], serde_json::json!("breaker"));
    assert_eq!(held[0].details["overridable"], serde_json::json!(false));
}

/// Admission constraint under which a park bounce must still fire.
/// `Unconstrained` is the ordinary sweep pass; the other two pin that
/// pause and a full worker pool cannot skip the mutating bounce.
enum ParkBounceAdmission {
    Unconstrained,
    DispatchPaused,
    AllWorkersBusy,
}

/// A deliberate park moves to Backlog with a waiting-on-you banner and
/// never mints a replacement execution. The open attention item, rather
/// than the single-shot autostart flag, identifies the park. Work start
/// then clears the halt and mints a fresh ready execution.
#[tokio::test]
async fn does_not_revive_a_deliberately_parked_row() {
    assert_park_bounces_under_admission_gate(ParkBounceAdmission::Unconstrained).await;
}

#[tokio::test]
async fn parked_row_bounces_with_all_workers_busy() {
    assert_park_bounces_under_admission_gate(ParkBounceAdmission::AllWorkersBusy).await;
}

#[tokio::test]
async fn parked_row_bounces_while_dispatch_is_paused() {
    assert_park_bounces_under_admission_gate(ParkBounceAdmission::DispatchPaused).await;
}

async fn assert_park_bounces_under_admission_gate(admission: ParkBounceAdmission) {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.record_worker_idle_abandonment(&execution_id, "worker declared itself blocked")
        .unwrap();
    db.create_attention_item(boss_protocol::CreateAttentionItemInput {
        execution_id: Some(execution_id.clone()),
        work_item_id: None,
        kind: crate::completion::RUN_DONE_BLOCKED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Run ended: worker declared itself blocked".to_owned(),
        body_markdown: "blocked".to_owned(),
        resolved_at: None,
    })
    .unwrap();
    // Age LAST: the writes above touch `tasks.updated_at`.
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    match admission {
        ParkBounceAdmission::Unconstrained => {}
        ParkBounceAdmission::DispatchPaused => {
            coordinator.pause_dispatch(
                boss_engine_utils::epoch_time::now_epoch_secs().max(0) as u64,
                crate::coordinator::DispatchPauseOrigin::Breaker,
                boss_protocol::PauseReason::new("test pause").unwrap(),
            );
        }
        ParkBounceAdmission::AllWorkersBusy => {
            coordinator
                .worker_pool()
                .claim_worker("busy-execution", None)
                .await
                .unwrap();
            assert!(!coordinator.worker_pool().has_idle_worker().await);
        }
    }

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.redispatched, 0,
        "a row whose run was deliberately parked must not get a replacement worker",
    );
    assert_eq!(outcome.deliberate_park_skipped, 1);
    assert_eq!(
        outcome.deliberate_park_bounced, 1,
        "the park must reach the mutating halted-state bounce, not just the skip counter",
    );
    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert_eq!(executions.len(), 1, "no replacement execution may be minted");
    let events = sink.events().await;
    assert!(
        events.iter().all(|e| e.stage != "orphan_active_redispatch"),
        "no redispatch event may fire for a parked row",
    );
    let skipped: Vec<_> = events
        .iter()
        .filter(|e| e.stage == "dispatch_decision" && e.details["skipped_reason"] == "deliberate_park")
        .collect();
    assert_eq!(skipped.len(), 1, "the park must be visible in the dispatch stream");

    let task = get_task(&db, &work_item_id);
    assert_eq!(
        task.status.as_str(),
        "todo",
        "the halted state must move the card off Doing, the same as a churn trip",
    );
    assert_eq!(
        task.dispatch_failed_reason.as_deref(),
        Some("deliberate_park"),
        "must use its own reason, distinct from churn_guard, so the recovery sweep never auto-retries it",
    );
    assert!(
        task.dispatch_failed_error
            .as_deref()
            .is_some_and(|e| e.contains("blocked") || e.contains("nudge")),
        "the halted-state text must say the row is waiting on a human decision, not that it failed: {:?}",
        task.dispatch_failed_error,
    );
    assert!(
        !task.autostart,
        "must not redispatch as a side effect of adding visibility"
    );

    if !matches!(admission, ParkBounceAdmission::Unconstrained) {
        return;
    }

    // `bossctl work start` / drag-to-Doing must un-park the row: clear
    // the halt stamp and mint a fresh ready execution. The recovery
    // sweep will not do this for `deliberate_park`.
    db.request_execution_with_live_check(
        RequestExecutionInput::builder()
            .work_item_id(work_item_id.clone())
            .build(),
        |_| false,
    )
    .unwrap();
    let task_after = get_task(&db, &work_item_id);
    assert!(
        task_after.dispatch_failed_reason.is_none(),
        "work start must clear the deliberate_park halt, the same as a churn bounce"
    );
    let executions_after = db.list_executions(Some(&work_item_id)).unwrap();
    assert!(
        executions_after.iter().any(|e| e.status == ExecutionStatus::Ready),
        "work start must mint a fresh ready execution; got: {executions_after:?}"
    );
}

/// A row can be BOTH deliberately parked and churn-tripped at once — a
/// worker can decide it is blocked only after several unproductive runs.
/// The halted-state surface must name both conditions
/// rather than picking one; this pins that the `deliberate_park` reason
/// wins (it is the stronger, human-only-clearable condition) while the
/// body text still names the churn trip.
#[tokio::test]
async fn a_deliberately_parked_row_that_is_also_churning_names_both_conditions() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");

    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
        db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "failed", now_epoch - i)
            .unwrap();
    }

    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.record_worker_idle_abandonment(&execution_id, "worker declared itself blocked")
        .unwrap();
    db.create_attention_item(boss_protocol::CreateAttentionItemInput {
        execution_id: Some(execution_id.clone()),
        work_item_id: None,
        kind: crate::completion::RUN_DONE_BLOCKED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Run ended: worker declared itself blocked".to_owned(),
        body_markdown: "blocked".to_owned(),
        resolved_at: None,
    })
    .unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.redispatched, 0);
    assert_eq!(outcome.deliberate_park_bounced, 1);
    assert_eq!(
        outcome.churn_skipped, 0,
        "when both conditions hold, the park path bounces — not the separate churn path",
    );

    let task = get_task(&db, &work_item_id);
    assert_eq!(
        task.dispatch_failed_reason.as_deref(),
        Some("deliberate_park"),
        "the park reason wins over churn_guard so the recovery sweep never auto-retries this row",
    );
    let error_text = task.dispatch_failed_error.unwrap_or_default();
    assert!(
        error_text.contains("blocked") || error_text.contains("nudge"),
        "must name the park condition: {error_text:?}",
    );
    assert!(
        error_text.contains(crate::work::DELIBERATE_PARK_CHURN_COMBINED_MARKER),
        "must ALSO name the churn condition (proves the combined branch ran; \
         the marker's value is pinned against WorkBoardBanners.swift by \
         combined_park_churn_marker_matches_swift_banner): {error_text:?}",
    );
}

/// The durable-process guard must win over a deliberate-park bounce
/// exactly as it already does over a churn bounce
/// (`churn_trip_does_not_bounce_while_prior_process_is_alive`): never
/// mutate a row whose previous worker process is still alive, park or
/// no park.
#[tokio::test]
async fn deliberate_park_does_not_bounce_while_prior_process_is_alive() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");

    // Our own pid stands in for a still-running worker shell.
    let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));
    db.record_worker_idle_abandonment(&execution_id, "worker declared itself blocked")
        .unwrap();
    db.create_attention_item(boss_protocol::CreateAttentionItemInput {
        execution_id: Some(execution_id.clone()),
        work_item_id: None,
        kind: crate::completion::RUN_DONE_BLOCKED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Run ended: worker declared itself blocked".to_owned(),
        body_markdown: "blocked".to_owned(),
        resolved_at: None,
    })
    .unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let convergence = RecordingConvergence::default();

    let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

    assert_eq!(
        outcome.live_process_skipped, 1,
        "the durable-process guard must fire before the deliberate-park bounce",
    );
    assert_eq!(
        outcome.deliberate_park_bounced, 0,
        "the park bounce must not run while a prior process is alive",
    );
    assert_eq!(outcome.redispatched, 0);

    let task = get_task(&db, &work_item_id);
    assert_eq!(
        task.status.as_str(),
        "active",
        "the row must not be bounced to Backlog while its prior worker is still alive",
    );
    assert!(
        task.dispatch_failed_reason.is_none(),
        "no park bounce while the process is alive",
    );
}

/// The park is a park, not a tombstone. Only an OPEN park item holds
/// the row: once it is resolved — by a human reviewing it, or
/// automatically by `ClearedBy::WorkResumed` when a fresh run starts —
/// the same row is recovered normally.
#[tokio::test]
async fn a_resolved_park_attention_does_not_hold_the_row() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.record_worker_idle_abandonment(&execution_id, "nudge breaker parked the run")
        .unwrap();
    db.create_attention_item(boss_protocol::CreateAttentionItemInput {
        execution_id: Some(execution_id.clone()),
        work_item_id: None,
        kind: crate::completion::NUDGE_BREAKER_ATTENTION_KIND.to_owned(),
        status: Some("resolved".to_owned()),
        title: "Worker parked: auto-nudge loop bounded".to_owned(),
        body_markdown: "parked".to_owned(),
        resolved_at: Some(boss_engine_utils::iso8601::format_epoch_iso8601(
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )),
    })
    .unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.redispatched, 1,
        "a settled park must not keep the row out of the sweep's hands forever",
    );
    assert_eq!(outcome.deliberate_park_skipped, 0);
}

/// Regression for the latch this park gate must not have:
/// `work_item_is_deliberately_parked` (`work/dispatch_admission.rs`)
/// scopes both halves of the fact to the *latest* execution. An OPEN
/// `run_done_declared_blocked` item filed against an OLDER, superseded
/// execution (the sweep that resolves it via `ClearedBy::WorkResumed`
/// is asynchronous, so it need not have run yet) must not keep a
/// resumed row parked forever — the row's own latest execution, here a
/// genuinely dead pane, must be judged on its own merits.
#[tokio::test]
async fn a_resumed_row_is_not_held_by_a_park_on_the_superseded_execution() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");

    let first_execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.record_worker_idle_abandonment(&first_execution_id, "worker declared itself blocked")
        .unwrap();
    db.create_attention_item(boss_protocol::CreateAttentionItemInput {
        execution_id: Some(first_execution_id.clone()),
        work_item_id: None,
        kind: crate::completion::RUN_DONE_BLOCKED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Run ended: worker declared itself blocked".to_owned(),
        body_markdown: "blocked".to_owned(),
        resolved_at: None,
    })
    .unwrap();

    // An operator resumes the row (`bossctl work start` mints a fresh
    // latest execution) and that replacement's own pane later dies —
    // a genuine orphan on the row the sweep must still be free to act on.
    let second_execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&second_execution_id, "worker died").unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.redispatched, 1,
        "an open park item on a superseded execution must not hold a resumed row forever, \
         got {outcome:?}",
    );
    assert_eq!(outcome.deliberate_park_skipped, 0);
}

/// **The gate that would have switched the sweep off.** `autostart` is
/// single-shot — `start_execution_run` clears it the first time a row
/// enters `active` — so the flag reads `false` on *every* row this
/// sweep exists to recover, a genuinely orphaned pane included. This
/// test pins that: a row whose worker really died, with `autostart`
/// consumed exactly as production leaves it, must still recover.
#[tokio::test]
async fn does_not_gate_recovery_on_the_single_shot_autostart_flag() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
    make_old(&db, &work_item_id);

    assert!(
        !get_task(&db, &work_item_id).autostart,
        "precondition: a row that has run carries autostart = false — if this ever changes, \
         the reasoning behind the park gate needs revisiting",
    );

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.redispatched, 1,
        "post-crash orphan recovery is the reason this sweep exists; a consumed autostart \
         flag must never stop it",
    );
}

/// **The churn guard a slow loop outruns.** Three unproductive terminal
/// executions 45 minutes apart: at every redispatch only two of them are
/// inside the one-hour trailing window, so the windowed count plateaus
/// one short of the threshold forever, however many workers the row
/// burns. The time-independent half must trip on the same evidence.
#[tokio::test]
async fn churn_guard_trips_on_a_slow_loop_the_window_cannot_catch() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    let cycle_secs = 45 * 60;
    for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_CONSECUTIVE_THRESHOLD {
        db.insert_terminal_execution_for_test(
            &work_item_id,
            "chore_implementation",
            "abandoned",
            now_epoch - i * cycle_secs,
        )
        .unwrap();
    }

    // Precondition: the windowed half genuinely cannot see this. If this
    // assertion ever fails the test has stopped exercising a slow loop.
    let windowed = db
        .count_recent_terminal_executions(
            &work_item_id,
            now_epoch - ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS,
            None,
        )
        .unwrap();
    assert!(
        windowed < ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
        "precondition: the trailing window must NOT be able to trip here (saw {windowed})",
    );

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.churn_skipped, 1,
        "an unbroken streak of dead runs must park the row however long it took",
    );
    assert_eq!(outcome.redispatched, 0);

    let task = get_task(&db, &work_item_id);
    assert_eq!(task.dispatch_failed_reason.as_deref(), Some("churn_guard"));
    assert!(
        task.dispatch_failed_error
            .as_deref()
            .is_some_and(|e| e.contains("consecutive")),
        "the park text must name the half that actually tripped: {:?}",
        task.dispatch_failed_error,
    );
}

/// The consecutive half counts a *streak*, not a lifetime total: a run
/// that completed resets it. Without this the guard would park any
/// long-lived row that had accumulated enough failures across its whole
/// history, which is not churn.
#[tokio::test]
async fn a_completed_run_resets_the_consecutive_churn_count() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    let cycle_secs = 45 * 60;
    // Old failures, then a success, then one fresh failure: the streak
    // is 1, even though the row's lifetime failure count is over the
    // threshold.
    for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_CONSECUTIVE_THRESHOLD {
        db.insert_terminal_execution_for_test(
            &work_item_id,
            "chore_implementation",
            "abandoned",
            now_epoch - (i + 2) * cycle_secs,
        )
        .unwrap();
    }
    db.insert_terminal_execution_for_test(
        &work_item_id,
        "chore_implementation",
        "completed",
        now_epoch - cycle_secs,
    )
    .unwrap();
    db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "abandoned", now_epoch)
        .unwrap();

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.churn_skipped, 0,
        "a row that delivered since its failures is not churning",
    );
    assert_eq!(outcome.redispatched, 1);
}

/// Churn guard: item with ≥ threshold recent terminal executions is skipped.
#[tokio::test]
async fn churn_guard_skips_repeatedly_failing_item() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
        db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "orphaned", now_epoch - i)
            .unwrap();
    }

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.churn_skipped, 1, "churn guard should have fired");
    assert_eq!(outcome.redispatched, 0);
    assert!(sink.events().await.is_empty(), "no event on churn skip");
}

/// The halted-state bounce runs ahead of the dispatch-pause gate for every
/// candidate, not only parked ones: the park half must be able to mutate a
/// row under a pause, and splitting the two halves across the gate would
/// leave a churn-tripped row's halt invisible for as long as the pause
/// lasted. Surfacing a halt never mints an execution, so this does not
/// weaken what a pause governs. Pinned here so a future reordering cannot
/// move the bounce below the gate.
#[tokio::test]
async fn churn_only_row_bounces_while_dispatch_is_paused() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
        db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "orphaned", now_epoch - i)
            .unwrap();
    }

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.pause_dispatch(
        boss_engine_utils::epoch_time::now_epoch_secs().max(0) as u64,
        crate::coordinator::DispatchPauseOrigin::Breaker,
        boss_protocol::PauseReason::new("test pause").unwrap(),
    );

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.churn_skipped, 1,
        "the churn bounce must fire under a pause exactly as it does unpaused",
    );
    assert_eq!(outcome.redispatched, 0);

    let task = get_task(&db, &work_item_id);
    assert_eq!(task.status.as_str(), "todo");
    assert_eq!(task.dispatch_failed_reason.as_deref(), Some("churn_guard"));
}

/// The churn guard trip must be operator-visible on the board itself,
/// not just in a trace WARN or an attention item nobody renders: the
/// work item bounces to Backlog (`status = "todo"`, `autostart =
/// false`) with `dispatch_failed_reason = "churn_guard"` and an
/// explanatory `dispatch_failed_error` — the same surface
/// `WorkDispatchFailureBanner` (macOS app) already renders for a
/// pre-spawn dispatch failure. It resolves automatically the next time
/// a dispatch attempt is made against the item — whether that's a
/// later sweep pass once the window drains, or an explicit `bossctl
/// work start` bypassing the guard.
#[tokio::test]
async fn churn_guard_trip_bounces_to_backlog_and_clears_on_retry() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
        db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "orphaned", now_epoch - i)
            .unwrap();
    }

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;
    assert_eq!(outcome.churn_skipped, 1);

    // No attention item — the park is engine/dispatch state, not a
    // human-judgment question, so it never touches `work_attention_items`.
    let items = db.list_attention_items_for_work_item(&work_item_id).unwrap();
    assert!(
        items
            .iter()
            .all(|i| i.kind != crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND),
        "the active-task churn park must not file a churn_guard_parked attention item; got: {items:?}"
    );

    let task = get_task(&db, &work_item_id);
    assert_eq!(task.status.as_str(), "todo", "bounced item returns to Backlog");
    assert!(!task.autostart, "autostart must be cleared so the park doesn't loop");
    assert_eq!(task.dispatch_failed_reason.as_deref(), Some("churn_guard"));
    assert!(
        task.dispatch_failed_error
            .as_deref()
            .is_some_and(|e| e.contains("bossctl work start")),
        "dispatch_failed_error should point at the manual bypass verb: {:?}",
        task.dispatch_failed_error
    );

    // Bypassing the guard (the `bossctl work start` path) clears the
    // bounce immediately, without needing another sweep pass.
    db.request_execution_with_live_check(
        RequestExecutionInput::builder()
            .work_item_id(work_item_id.clone())
            .build(),
        |_| false,
    )
    .unwrap();

    let task_after = get_task(&db, &work_item_id);
    assert!(
        task_after.dispatch_failed_reason.is_none(),
        "dispatch_failed_reason should clear on the next dispatch attempt"
    );
}

/// Regression: the churn guard must not bounce a row to Backlog while
/// the row's previous worker process is still alive. The durable-process
/// guard must win: no bounce, status stays `active`, and convergence runs.
#[tokio::test]
async fn churn_trip_does_not_bounce_while_prior_process_is_alive() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");

    // Enough terminal executions to trip the churn guard...
    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
        db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "orphaned", now_epoch - i)
            .unwrap();
    }
    // ...but the most recent run's shell_pid is still alive (our own
    // pid stands in for the still-running worker shell).
    let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));
    db.mark_execution_orphaned(&execution_id, "spawn-ack timeout; worker presumed dead")
        .unwrap();
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let convergence = RecordingConvergence::default();

    let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

    assert_eq!(
        outcome.live_process_skipped, 1,
        "the durable-process guard must fire before the churn guard's bounce"
    );
    assert_eq!(
        outcome.churn_skipped, 0,
        "the churn bounce must not run while a prior process is alive"
    );
    assert_eq!(outcome.redispatched, 0);

    let task = get_task(&db, &work_item_id);
    assert_eq!(
        task.status.as_str(),
        "active",
        "the row must not be bounced to Backlog while its prior worker is still alive"
    );
    assert!(
        task.dispatch_failed_reason.is_none(),
        "no churn-guard park while the process is alive"
    );
}

fn get_task(db: &WorkDb, work_item_id: &str) -> boss_protocol::Task {
    match db.get_work_item(work_item_id).unwrap() {
        boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t,
        other => panic!("expected a task/chore work item, got {other:?}"),
    }
}

/// Recent-transition guard: freshly-activated item is skipped even with
/// no execution, because its updated_at is within ORPHAN_MIN_AGE_SECS.
#[tokio::test]
async fn no_redispatch_for_recently_activated_item() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let _work_item_id = create_active_chore(&db, &product_id, "test chore");
    // Deliberately do NOT call make_old — item's updated_at is NOW.

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.redispatched, 0, "should skip recently activated item");
    assert!(sink.events().await.is_empty());
}

/// Regression: a waiting_human execution must never be abandoned and
/// re-dispatched by the orphan sweep. The worker parks for human input
/// and then exits (releasing its pool slot), so the execution is not
/// claimed — but it is still alive and waiting for a response.
///
/// An unclaimed non-terminal execution is not sufficient evidence of death.
#[tokio::test]
async fn skips_item_with_waiting_human_execution() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    // Create a ready execution then force it to waiting_human to simulate
    // a worker that parked for human input and then released its slot.
    let execution = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
        )
        .unwrap();
    db.force_execution_status_for_test(&work_item_id, ExecutionStatus::WaitingHuman)
        .unwrap();

    let db = Arc::new(db);
    // Deliberately do NOT claim the execution — simulates the worker
    // process having exited after entering waiting_human.
    let coordinator = make_coordinator(db.clone(), 1);

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.redispatched, 0,
        "sweep must not re-dispatch a waiting_human execution"
    );
    let events = sink.events().await;
    assert!(
        events.iter().all(|e| e.stage != "orphan_active_redispatch"),
        "no orphan_active_redispatch event should fire for waiting_human"
    );

    // The waiting_human execution must remain intact — not abandoned.
    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert!(
        executions
            .iter()
            .any(|e| e.id == execution.id && e.status == ExecutionStatus::WaitingHuman),
        "waiting_human execution must not be abandoned by the sweep"
    );
}

/// The same protection for `running`, which is the status EVERY healthy
/// pane worker sits in for its whole life — making this the common
/// case, not an edge one.
///
/// Deciding a live row is actually dead belongs to the death sweeps
/// (`dead_pane_sweep`, `husk_pane_sweep`, `lost_workspace_sweep`,
/// `dead_pid_sweep`, `spawn_ack_sweep`); this sweep picks the item up
/// on the pass after one of them reconciles it to `orphaned`.
#[tokio::test]
async fn skips_item_with_running_worker_execution() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let execution = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
        )
        .unwrap();
    db.force_execution_status_for_test(&work_item_id, ExecutionStatus::Running)
        .unwrap();

    let db = Arc::new(db);
    // An unclaimed running execution must still be treated as live.
    let coordinator = make_coordinator(db.clone(), 1);

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.redispatched, 0,
        "sweep must not re-dispatch on top of a running worker"
    );
    assert!(
        !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
            .unwrap()
            .contains(&work_item_id),
        "a work item with a running execution must not be an orphan candidate at all"
    );
    let events = sink.events().await;
    assert!(
        events.iter().all(|e| e.stage != "orphan_active_redispatch"),
        "no orphan_active_redispatch event should fire for a running worker"
    );

    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert!(
        executions
            .iter()
            .any(|e| e.id == execution.id && e.status == ExecutionStatus::Running),
        "running execution must not be abandoned by the sweep"
    );
}

/// A running review-pool execution must stay live even when the main
/// pool has no claim for it. The claim snapshot must union all three pools.
#[tokio::test]
async fn running_pr_review_in_review_pool_is_not_abandoned() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    // Create a pr_review execution and force it to `running` to simulate
    // a reviewer pane that was successfully spawned.
    let execution = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
        )
        .unwrap();
    // Override kind to PrReview — the execution was created with the
    // default kind; we force the DB value directly so the sweep reads it.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET kind = 'pr_review', status = 'running' WHERE id = ?1",
            rusqlite::params![execution.id],
        )
        .unwrap();
    }

    let db = Arc::new(db);
    // Build a coordinator with a 1-slot main pool AND a 1-slot review pool.
    // Claim the pr_review execution in the REVIEW pool (not the main pool)
    // to simulate the production layout: main pool has an idle slot,
    // but the reviewer is live in the review pool.
    let (coordinator, review_pool) = make_coordinator_with_review_pool(db.clone(), 1, 1);
    review_pool.claim_worker(&execution.id, None).await;
    // The idle main pool must not hide the review pool's live claim.

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.redispatched, 0,
        "sweep must not re-dispatch when the pr_review execution is claimed in the review pool"
    );
    assert_eq!(
        outcome.running_reviewer_skipped, 0,
        "defense-in-depth skip must not fire when pool union correctly identifies the reviewer as live"
    );
    let events = sink.events().await;
    assert!(
        events.iter().all(|e| e.stage != "orphan_active_redispatch"),
        "no orphan_active_redispatch event must fire for a live review-pool-claimed reviewer"
    );

    // The running pr_review execution must remain intact — not abandoned.
    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert!(
        executions
            .iter()
            .any(|e| e.id == execution.id && e.status == ExecutionStatus::Running),
        "running pr_review execution must not be abandoned by the sweep"
    );
}

/// A live reviewer claimed in NO pool at all — the "pool union absent"
/// scenario — must still survive the sweep.
///
/// This is enforced one layer before the in-loop guard:
/// `list_orphan_active_candidates` excludes every work item with a live
/// (`running`/`waiting_human`) execution, so the item never reaches the
/// in-loop guard and `running_reviewer_skipped` stays 0. The guard is
/// retained as genuine defense-in-depth; the assertion below on the
/// candidate list is what pins the mechanism, so a future change that
/// re-admits live rows to the candidate set fails here rather than
/// silently falling back on the guard.
#[tokio::test]
async fn running_pr_review_not_in_any_pool_survives_the_sweep() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let execution = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
        )
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET kind = 'pr_review', status = 'running' WHERE id = ?1",
            rusqlite::params![execution.id],
        )
        .unwrap();
    }

    let db = Arc::new(db);
    // Claim nothing in any pool — simulates the "pool union absent" scenario.
    let coordinator = make_coordinator(db.clone(), 1);

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(
        outcome.redispatched, 0,
        "the sweep must not re-dispatch on top of a running pr_review execution"
    );
    assert!(
        !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
            .unwrap()
            .contains(&work_item_id),
        "a work item with a live execution must be excluded at the candidate query, before \
         the in-loop defense-in-depth guard is ever consulted"
    );
    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert!(
        executions
            .iter()
            .any(|e| e.id == execution.id && e.status == ExecutionStatus::Running),
        "running pr_review execution must survive the sweep even when not in any pool"
    );
}

// ── pending-review hold vs. genuine orphan ──────────────────────────────

/// Insert a minimal `pr_review_batches` row directly. Test-only: the
/// production path (`WorkDb::create_pre_merge_review_batch_for_pool`)
/// requires a `gh pr view` round trip and pool-admission bookkeeping
/// this suite has no need to exercise — only the row shape
/// `list_orphan_active_candidates`'s exclusion reads matters here.
/// Does not touch `tasks.pr_head_sha`: the hold path never writes that
/// column, so tests must not stamp it either.
fn insert_review_batch(db: &WorkDb, cycle_root_id: &str, status: &str, target_sha: &str, pr_url: &str) {
    let conn = db.connect().unwrap();
    let now = boss_engine_utils::epoch_time::now_epoch_secs().to_string();
    conn.execute(
        "INSERT INTO pr_review_batches (
             id, cycle_root_id, base_sha, classification_json, created_at,
             phase, pr_number, pr_url, status, target_sha, updated_at
         ) VALUES (?1, ?2, 'base-sha', '{}', ?3, 'pre_merge', 1, ?4, ?5, ?6, ?3)",
        rusqlite::params![
            format!("batch-{cycle_root_id}-{status}-{target_sha}"),
            cycle_root_id,
            now,
            pr_url,
            status,
            target_sha
        ],
    )
    .unwrap();
}

/// The ReviewerEnqueued hold shape: a completed producing execution on
/// an `active` task, with no live execution left. Age last — execution
/// writes bump `tasks.updated_at`. The `connect()` guard is dropped
/// before `make_old`, which also connects; holding both deadlocks the
/// single-connection pool.
fn hold_with_completed_producer(db: &WorkDb, work_item_id: &str) {
    let execution = db
        .create_execution(
            crate::work::CreateExecutionInput::builder()
                .work_item_id(work_item_id.to_owned())
                .kind(boss_protocol::ExecutionKind::ChoreImplementation)
                .build(),
        )
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed', finished_at = '1' WHERE id = ?1",
            rusqlite::params![execution.id],
        )
        .unwrap();
    }
    make_old(db, work_item_id);
}

/// Stamp `pr_head_after` on the work item's latest execution. Production
/// writes this from `record_worker_pr_completion`; the hold helper above
/// uses a direct status update so tests that care about freshness must
/// set it themselves.
fn stamp_latest_pr_head_after(db: &WorkDb, work_item_id: &str, sha: &str) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET pr_head_after = ?1
         WHERE id = (
             SELECT id FROM work_executions
             WHERE work_item_id = ?2
             ORDER BY created_at DESC, id DESC
             LIMIT 1
         )",
        rusqlite::params![sha, work_item_id],
    )
    .unwrap();
}

/// First-PR chore: the chore is its own cycle root, `pr_head_sha` is
/// NULL (the hold path never writes it), and a live pre_merge batch
/// exists. This is the common production hold; a sha-keyed exclusion
/// against `tasks.pr_head_sha` would miss it. An unknown
/// `pr_head_after` still excludes.
#[tokio::test]
async fn first_pr_chore_with_live_pre_merge_batch_and_null_pr_head_sha_is_not_an_orphan_candidate() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    hold_with_completed_producer(&db, &work_item_id);

    insert_review_batch(&db, &work_item_id, "supervising", "sha-current", "https://example/pr/1");

    assert!(
        !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
            .unwrap()
            .contains(&work_item_id),
        "a first-PR chore held pending a live pre_merge batch must not be an orphan candidate, \
         even with tasks.pr_head_sha still NULL"
    );
}

/// Acceptance: the same task IS a candidate again once the review batch
/// reaches a terminal state — the hold must not become permanent immunity.
#[tokio::test]
async fn held_task_becomes_orphan_candidate_once_review_batch_terminates() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    hold_with_completed_producer(&db, &work_item_id);

    insert_review_batch(&db, &work_item_id, "completed", "sha-current", "https://example/pr/1");

    assert!(
        db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
            .unwrap()
            .contains(&work_item_id),
        "a task whose review batch has already terminated must become a candidate again"
    );
}

/// Acceptance: a genuinely orphaned `active` task — no live execution, no
/// open review batch at all — is still returned, so orphan recovery for
/// the ordinary case is not weakened by this exclusion.
#[tokio::test]
async fn genuinely_orphaned_task_with_no_review_batch_is_still_a_candidate() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    assert!(
        db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
            .unwrap()
            .contains(&work_item_id),
        "a task with no live execution and no review batch at all must remain an orphan candidate"
    );
}

/// Acceptance: a held task whose latest producer `pr_head_after` has
/// moved on from the live batch's `target_sha` is an orphan candidate
/// again — a stale batch must not grant immunity until the reaper
/// fires. Uses `pr_head_after` (written at hold time), not
/// `tasks.pr_head_sha`.
#[tokio::test]
async fn held_task_whose_producer_head_moved_past_batch_target_is_an_orphan_candidate() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    hold_with_completed_producer(&db, &work_item_id);
    stamp_latest_pr_head_after(&db, &work_item_id, "sha-new");

    insert_review_batch(&db, &work_item_id, "supervising", "sha-old", "https://example/pr/1");

    assert!(
        db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
            .unwrap()
            .contains(&work_item_id),
        "a live pre_merge batch whose target_sha is behind the producer head must not mask the task"
    );
}

/// Matching `pr_head_after` and `target_sha` is the live-hold shape
/// once GitHub head was captured at completion: still excluded.
#[tokio::test]
async fn live_pre_merge_batch_with_matching_producer_head_is_not_an_orphan_candidate() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    hold_with_completed_producer(&db, &work_item_id);
    stamp_latest_pr_head_after(&db, &work_item_id, "sha-current");

    insert_review_batch(&db, &work_item_id, "supervising", "sha-current", "https://example/pr/1");

    assert!(
        !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
            .unwrap()
            .contains(&work_item_id),
        "a live pre_merge batch still targeting the producer head must exclude the task"
    );
}

/// ReviewerEnqueued on a revision: the batch is keyed on the PR-owning
/// ancestor (the cycle root), not the revision's own id. The recursive
/// walk's UNION ALL branch has to fire for the exclusion to see it.
/// `pr_head_sha` stays NULL on both rows — production never stamps it
/// at hold time.
#[tokio::test]
async fn held_revision_with_batch_keyed_on_cycle_root_is_not_an_orphan_candidate() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let root_id = create_active_chore(&db, &product_id, "pr-owning chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET pr_url = ?1, status = 'in_review' WHERE id = ?2",
            rusqlite::params!["https://example/pr/1", root_id],
        )
        .unwrap();
    }
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(root_id.clone())
                .description("address review findings")
                .autostart(false)
                .build(),
            &StaticPrStateChecker(PrOpenState::Open),
        )
        .unwrap();
    db.update_work_item(
        &revision.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    hold_with_completed_producer(&db, &revision.id);
    insert_review_batch(&db, &root_id, "supervising", "sha-current", "https://example/pr/1");

    assert!(
        !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
            .unwrap()
            .contains(&revision.id),
        "a held revision must be excluded via the cycle-root walk, with the batch keyed on the \
         PR-owning ancestor and pr_head_sha left NULL"
    );
}

/// Two completed producers on the same held task: the older one stamped
/// `pr_head_after='sha-A'`, the latest left unstamped (the fail-open
/// `fetch_pr_head_after` outcome). A live batch targeting `sha-B` must
/// still exclude — unknown latest head is not replaced by the stale SHA.
#[tokio::test]
async fn held_task_latest_unstamped_producer_excludes_even_when_older_producer_has_a_different_sha() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    hold_with_completed_producer(&db, &work_item_id);
    stamp_latest_pr_head_after(&db, &work_item_id, "sha-A");

    let later = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
        )
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed', finished_at = '2' WHERE id = ?1",
            rusqlite::params![later.id],
        )
        .unwrap();
    }
    make_old(&db, &work_item_id);

    insert_review_batch(&db, &work_item_id, "supervising", "sha-B", "https://example/pr/1");

    assert!(
        !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
            .unwrap()
            .contains(&work_item_id),
        "the latest producer's unknown pr_head_after must still exclude, even when an older \
         producer recorded a SHA that does not match the live batch target"
    );
}

/// Two active tasks can share one cycle root (chain root + revision). A
/// live pre_merge batch must exclude only the held producer, not a
/// sibling that has never completed a producer execution — that sibling
/// is a genuine orphan and the COALESCE-to-target fallback must not
/// shield it.
#[tokio::test]
async fn live_batch_does_not_exclude_same_cycle_root_sibling_with_no_producer() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let root_id = create_active_chore(&db, &product_id, "pr-owning chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
            rusqlite::params!["https://example/pr/1", root_id],
        )
        .unwrap();
    }
    hold_with_completed_producer(&db, &root_id);
    insert_review_batch(&db, &root_id, "supervising", "sha-current", "https://example/pr/1");

    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(root_id.clone())
                .description("address review findings")
                .autostart(false)
                .build(),
            &StaticPrStateChecker(PrOpenState::Open),
        )
        .unwrap();
    db.update_work_item(
        &revision.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    make_old(&db, &revision.id);

    let candidates = db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS).unwrap();
    assert!(
        !candidates.contains(&root_id),
        "the held producer under a live pre_merge batch must still be excluded"
    );
    assert!(
        candidates.contains(&revision.id),
        "an active sibling with no producer completion must remain an orphan candidate, even \
         while a live pre_merge batch is open on the shared cycle root"
    );
}

// ── event-driven path (run_one_pass_for_item / spawn_event_subscriber) ──

/// `run_one_pass_for_item` redispatches the named orphan, same as a full
/// `run_one_pass` would, when it is a genuine candidate.
#[tokio::test]
async fn run_one_pass_for_item_redispatches_matching_orphan() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let outcome = run_one_pass_for_item(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
        &work_item_id,
    )
    .await;

    assert_eq!(outcome.redispatched, 1);
    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert!(
        executions.iter().any(|e| e.status == ExecutionStatus::Ready),
        "expected a ready execution after the event-driven redispatch"
    );
}

/// `run_one_pass_for_item` never acts on a work item other than the one
/// named — an `ExecutionTerminal` event for a different task must not
/// cause an unrelated orphan to be touched.
#[tokio::test]
async fn run_one_pass_for_item_ignores_other_work_items() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let outcome = run_one_pass_for_item(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
        "task_unrelated",
    )
    .await;

    assert_eq!(outcome.redispatched, 0);
    let executions = db.list_executions(Some(&work_item_id)).unwrap();
    assert!(
        executions.is_empty(),
        "the named-but-unrelated work item must be left untouched"
    );
}

/// Idempotency: once the periodic sweep (`run_one_pass`) has already
/// redispatched an orphan, a subsequent event-driven pass
/// (`run_one_pass_for_item`) for the same work item — e.g. the
/// `ExecutionTerminal` event racing the sweep that already reconciled
/// it — must be a no-op rather than double-dispatching.
#[tokio::test]
async fn event_driven_pass_is_idempotent_with_periodic_sweep() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let sink = Arc::new(RecordingDispatchEventSink::new());

    let first = run_one_pass(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
    )
    .await;
    assert_eq!(
        first.redispatched, 1,
        "periodic sweep should redispatch the orphan once"
    );

    let second = run_one_pass_for_item(
        db.as_ref(),
        coordinator.clone(),
        sink.as_ref(),
        &NoopLiveWorkerConvergence,
        &work_item_id,
    )
    .await;
    assert_eq!(
        second.redispatched, 0,
        "event-driven pass for the same work item must be a no-op once already redispatched"
    );

    let ready_count = db
        .list_executions(Some(&work_item_id))
        .unwrap()
        .into_iter()
        .filter(|e| e.status == ExecutionStatus::Ready)
        .count();
    assert_eq!(ready_count, 1, "exactly one ready execution, not a duplicate");
}

/// End-to-end: `spawn_event_subscriber` actually redispatches an
/// orphan when an `ExecutionTerminal` event is published on the bus.
/// The subscriber's initial full reconcile pass is deliberately
/// starved of an idle worker slot (the pool's one slot is pre-claimed)
/// so it observes `no_worker_skipped` and does nothing — isolating the
/// assertion to the event-driven path rather than the startup
/// reconcile that every subscriber also runs.
#[tokio::test]
async fn spawn_event_subscriber_redispatches_on_execution_terminal() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    make_old(&db, &work_item_id);

    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let dummy_worker_id = coordinator
        .worker_pool()
        .claim_worker("dummy-exec-id", None)
        .await
        .expect("test pool must have a slot to claim");

    let sink = Arc::new(RecordingDispatchEventSink::new());
    let bus = Arc::new(EventBus::new());

    let _handle = spawn_event_subscriber(
        db.clone(),
        coordinator.clone(),
        sink.clone(),
        Arc::new(NoopLiveWorkerConvergence),
        bus.clone(),
    );

    // Let the subscriber's initial full-reconcile pass run and observe
    // no idle worker before we free the slot below.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        db.list_executions(Some(&work_item_id)).unwrap().is_empty(),
        "startup reconcile must not have redispatched while the pool was fully claimed"
    );

    coordinator.worker_pool().release_worker(&dummy_worker_id, None).await;

    bus.publish(Event::ExecutionTerminal {
        execution_id: "dummy-exec-id".to_owned(),
        task_id: work_item_id.clone(),
        host_id: "local".to_owned(),
        pool_claim: None,
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let executions = db.list_executions(Some(&work_item_id)).unwrap();
            if executions.iter().any(|e| e.status == ExecutionStatus::Ready) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("event-driven redispatch did not happen before the timeout");
}
