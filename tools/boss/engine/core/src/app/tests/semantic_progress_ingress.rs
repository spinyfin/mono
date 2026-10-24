//! Shared progress-ingress writes the semantic checkpoint; engine-synthesized
//! display timestamps do not.

use super::*;

use crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS;
use crate::protocol::WorkerEvent;
use crate::semantic_progress::{SemanticToolCondition, SemanticTurnCondition};
use crate::test_support::*;
use boss_protocol::RequestExecutionInput;

const SLOT: u8 = 1;

fn spawned_worker(server_state: &ServerState) -> String {
    spawned_worker_for_item(server_state).0
}

/// Like [`spawned_worker`], also returning the governing work item id.
fn spawned_worker_for_item(server_state: &ServerState) -> (String, String) {
    let db = server_state.work_db.as_ref();
    let product = create_test_product_with_repo(db, "p", Some("git@example.com:p.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), "c");
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let (_exec, run) = db
        .start_execution_run(&execution.id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    finish_run_worker_pane_alive(db, &execution.id, &run.id, Some("Spawned worker pane in slot 1."));
    server_state
        .live_worker_states
        .register_spawn(SLOT, execution.id.clone(), "claude-opus-4-7", 4242, None);
    server_state.worker_registry.register_run_slot(&execution.id, SLOT);
    (execution.id, chore.id)
}

fn hook(event: WorkerEvent, execution_id: &str) -> crate::events_socket::IncomingHookEvent {
    crate::events_socket::IncomingHookEvent::for_test(event, Some(execution_id.to_owned()), None)
}

fn pre_tool_use(execution_id: &str) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::PreToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
        },
        execution_id,
    )
}

fn post_tool_use(execution_id: &str) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        },
        execution_id,
    )
}

fn session_start(execution_id: &str) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::SessionStart {
            session_id: "claude-sess-1".into(),
            source: crate::protocol::SessionStartSource::Startup,
            model: None,
        },
        execution_id,
    )
}

fn user_prompt_submit(execution_id: &str) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::UserPromptSubmit {
            session_id: "claude-sess-1".into(),
            prompt: String::new(),
        },
        execution_id,
    )
}

fn stop(execution_id: &str) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::Stop {
            session_id: "claude-sess-1".into(),
            stop_hook_active: false,
            stop_reason: boss_protocol::StopReason::Completed,
        },
        execution_id,
    )
}

fn notification(execution_id: &str) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::Notification {
            session_id: "claude-sess-1".into(),
            message: "guard-trace replay".into(),
        },
        execution_id,
    )
}

#[tokio::test]
async fn ingress_persists_driver_originated_progress_and_tool_condition() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);

    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id)).await;
    let in_flight = server_state
        .work_db
        .get_run_semantic_progress_checkpoint(&execution_id)
        .unwrap()
        .expect("pre-tool-use must persist a checkpoint");
    assert_eq!(in_flight.tool_condition, SemanticToolCondition::InFlight);
    assert_eq!(
        server_state
            .live_worker_states
            .semantic_progress_for_slot(SLOT)
            .unwrap()
            .tool_condition,
        SemanticToolCondition::InFlight,
    );

    dispatch_worker_event_fanout(&server_state, &post_tool_use(&execution_id)).await;
    let idle = server_state
        .work_db
        .get_run_semantic_progress_checkpoint(&execution_id)
        .unwrap()
        .unwrap();
    assert_eq!(idle.tool_condition, SemanticToolCondition::Idle);
}

/// End to end through the durable checkpoint. A
/// worker is mid-turn with its last tool call balanced — exactly what a Codex
/// worker waiting on its own background command looks like, since the
/// rollout translator announces the command once and its continuation polls
/// emit nothing. The engine restarts; boot adoption rebuilds the slot in a
/// fresh registry from the checkpoint alone. The rebuilt slot must read
/// `Working`, not `Idle`: the driver never reported a turn boundary.
#[tokio::test]
async fn a_rebuilt_slot_reads_working_for_a_turn_the_driver_never_ended() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);

    dispatch_worker_event_fanout(&server_state, &user_prompt_submit(&execution_id)).await;
    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id)).await;
    dispatch_worker_event_fanout(&server_state, &post_tool_use(&execution_id)).await;

    let checkpoint = server_state
        .work_db
        .get_run_semantic_progress_checkpoint(&execution_id)
        .unwrap()
        .expect("tool boundaries persist a checkpoint");
    assert_eq!(checkpoint.tool_condition, SemanticToolCondition::Idle);
    assert_eq!(
        checkpoint.turn_condition,
        SemanticTurnCondition::InTurn,
        "a balanced tool call leaves the turn open",
    );

    // Engine restart: a fresh registry, seeded only from the row.
    let rebuilt = crate::live_worker_state::LiveWorkerStateRegistry::new();
    rebuilt.register_readoption(
        SLOT,
        execution_id.clone(),
        "codex",
        4242,
        None,
        false,
        crate::live_worker_state::LiveSpawnRouting::none(),
        crate::live_worker_state::ReadoptionEvidence::LiveShellPid,
    );
    rebuilt.seed_semantic_progress(SLOT, &checkpoint);
    let seeded = rebuilt.get(SLOT).unwrap();
    assert_eq!(
        seeded.activity,
        boss_protocol::WorkerActivity::Working,
        "a worker whose turn is in progress is busy for the whole turn, \
         including while it waits on its own background command",
    );
    assert_eq!(seeded.current_tool, None);

    // And once the driver does end the turn, the same path reads idle.
    dispatch_worker_event_fanout(&server_state, &stop(&execution_id)).await;
    let ended = server_state
        .work_db
        .get_run_semantic_progress_checkpoint(&execution_id)
        .unwrap()
        .unwrap();
    assert_eq!(ended.turn_condition, SemanticTurnCondition::Ended);
    let rebuilt_after_stop = crate::live_worker_state::LiveWorkerStateRegistry::new();
    rebuilt_after_stop.register_readoption(
        SLOT,
        execution_id.clone(),
        "codex",
        4242,
        None,
        false,
        crate::live_worker_state::LiveSpawnRouting::none(),
        crate::live_worker_state::ReadoptionEvidence::LiveShellPid,
    );
    rebuilt_after_stop.seed_semantic_progress(SLOT, &ended);
    assert_eq!(
        rebuilt_after_stop.get(SLOT).unwrap().activity,
        boss_protocol::WorkerActivity::Idle,
    );
}

#[tokio::test]
async fn session_start_ingress_does_not_coerce_unknown_to_idle() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);

    dispatch_worker_event_fanout(&server_state, &session_start(&execution_id)).await;
    let checkpoint = server_state
        .work_db
        .get_run_semantic_progress_checkpoint(&execution_id)
        .unwrap()
        .expect("session start is driver-originated progress");
    assert_eq!(checkpoint.tool_condition, SemanticToolCondition::Unknown);
    assert_eq!(
        checkpoint.turn_condition,
        SemanticTurnCondition::Unknown,
        "session start is not turn activity either",
    );
}

#[tokio::test]
async fn notification_after_pre_tool_use_leaves_the_checkpoint_in_flight() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);

    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id)).await;
    dispatch_worker_event_fanout(&server_state, &notification(&execution_id)).await;

    let checkpoint = server_state
        .work_db
        .get_run_semantic_progress_checkpoint(&execution_id)
        .unwrap()
        .expect("notification is driver-originated progress time");
    assert_eq!(
        checkpoint.tool_condition,
        SemanticToolCondition::InFlight,
        "a Notification must not durably clear an in-flight tool for a driver that doesn't trust it",
    );
}

#[test]
fn mark_stalled_spawns_does_not_write_semantic_progress() {
    let reg = crate::live_worker_state::LiveWorkerStateRegistry::new();
    reg.register_spawn(SLOT, "run-a", "claude-opus-4-7", 4242, None);
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    reg.set_spawn_time_for_test(SLOT, now - (STALLED_SPAWN_THRESHOLD_SECS + 5));
    assert_eq!(reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS), vec![SLOT]);
    assert!(
        reg.get(SLOT).unwrap().last_event_at.is_some(),
        "stall handling synthesizes a display timestamp",
    );
    assert!(
        reg.semantic_progress_for_slot(SLOT).is_none(),
        "an engine-synthesized display timestamp must not count as driver-originated progress",
    );
}

#[test]
fn mark_errored_does_not_write_semantic_progress() {
    let reg = crate::live_worker_state::LiveWorkerStateRegistry::new();
    reg.register_spawn(SLOT, "run-a", "claude-opus-4-7", 4242, None);
    assert!(reg.mark_errored(SLOT));
    assert!(
        reg.get(SLOT).unwrap().last_event_at.is_some(),
        "error handling synthesizes a display timestamp",
    );
    assert!(
        reg.semantic_progress_for_slot(SLOT).is_none(),
        "mark_errored is engine inference, not driver progress",
    );
}

#[tokio::test]
async fn no_slot_subagent_tool_hooks_preserve_an_ended_turn() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);
    dispatch_worker_event_fanout(&server_state, &stop(&execution_id)).await;
    server_state.worker_registry.take_slot_for_run(&execution_id);
    server_state.live_worker_states.release_slot(SLOT);
    for mut event in [pre_tool_use(&execution_id), post_tool_use(&execution_id)] {
        event.connection.from_subagent = true;
        dispatch_worker_event_fanout(&server_state, &event).await;
        assert_reseeded_turn(
            &server_state,
            &execution_id,
            SemanticTurnCondition::Ended,
            boss_protocol::WorkerActivity::Idle,
        );
    }
}

fn assert_reseeded_turn(
    server_state: &ServerState,
    execution_id: &str,
    turn: SemanticTurnCondition,
    activity: boss_protocol::WorkerActivity,
) {
    let checkpoint = server_state
        .work_db
        .get_run_semantic_progress_checkpoint(execution_id)
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.turn_condition, turn);
    let rebuilt = crate::live_worker_state::LiveWorkerStateRegistry::new();
    rebuilt.register_readoption(
        SLOT,
        execution_id,
        "codex",
        4242,
        None,
        false,
        crate::live_worker_state::LiveSpawnRouting::none(),
        crate::live_worker_state::ReadoptionEvidence::LiveShellPid,
    );
    rebuilt.seed_semantic_progress(SLOT, &checkpoint);
    assert_eq!(rebuilt.get(SLOT).unwrap().activity, activity);
}

#[tokio::test]
async fn an_older_accepted_dispatch_persists_the_latest_checkpoint() {
    use crate::live_worker_state::HookOrigin;
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);
    let (captured_tx, captured_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let older_state = server_state.clone();
    let older_id = execution_id.clone();
    let older = std::thread::spawn(move || {
        let applied = older_state.live_worker_states.apply_hook_event(
            SLOT,
            HookOrigin {
                ingress_seq: Some(3),
                from_subagent: false,
            },
            &stop(&older_id).event,
        );
        assert!(matches!(
            applied.outcome,
            crate::live_worker_state::ApplyEventOutcome::Applied { .. }
        ));
        let captured = older_state.live_worker_states.semantic_progress_for_slot(SLOT).unwrap();
        assert_eq!(captured.turn_condition, SemanticTurnCondition::Ended);
        captured_tx.send(()).unwrap();
        resume_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        // The dispatcher must refresh under the lock and keep it through
        // persistence, never write the snapshot captured before the pause.
        crate::app::worker_events::persist_applied_semantic_progress(
            &older_state,
            &older_id,
            SLOT,
            &stop(&older_id),
            &applied.outcome,
        );
    });
    captured_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    let mut newer = pre_tool_use(&execution_id);
    newer.connection.ingress_seq = Some(4);
    dispatch_worker_event_fanout(&server_state, &newer).await;
    assert_reseeded_turn(
        &server_state,
        &execution_id,
        SemanticTurnCondition::InTurn,
        boss_protocol::WorkerActivity::Working,
    );
    resume_tx.send(()).unwrap();
    older.join().unwrap();
    assert_reseeded_turn(
        &server_state,
        &execution_id,
        SemanticTurnCondition::InTurn,
        boss_protocol::WorkerActivity::Working,
    );
}

/// The persistence callback talks to the database, whose connection holders
/// (dispatch's live-run check) take the registry mutex. The registry mutex
/// must therefore not be held while the callback runs, or the two orders
/// deadlock.
#[test]
fn semantic_checkpoint_persistence_does_not_hold_the_registry_mutex() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let state = server_state.clone();
    std::thread::spawn(move || {
        let run_id = execution_id.clone();
        state.live_worker_states.with_semantic_progress(SLOT, &run_id, |_| {
            // Re-taking the registry mutex here self-deadlocks if it is held.
            let _ = state.live_worker_states.is_run_live(&execution_id);
        });
        done_tx.send(()).unwrap();
    });
    done_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("with_semantic_progress held the registry mutex across the callback");
}

/// `request_execution_with_live_check` holds the DB connection while its
/// closure takes the registry mutex; hook dispatch applies an event and then
/// persists its checkpoint to the DB. Overlap the two deterministically: the
/// live check parks inside the closure (DB connection held) until dispatch has
/// applied its event, then checks liveness. Both must finish.
#[test]
fn live_check_and_hook_dispatch_do_not_deadlock() {
    let wait = std::time::Duration::from_secs(20);
    let (server_state, _dir) = test_server_state();
    let (execution_id, work_item_id) = spawned_worker_for_item(&server_state);
    let (inside_tx, inside_rx) = std::sync::mpsc::channel();
    let (live_done_tx, live_done_rx) = std::sync::mpsc::channel();
    let (dispatch_done_tx, dispatch_done_rx) = std::sync::mpsc::channel();
    let (persist_boundary_tx, persist_boundary_rx) = std::sync::mpsc::channel();
    *server_state
        .live_worker_states
        .test_hooks
        .persist_boundary
        .lock()
        .unwrap() = Some(persist_boundary_tx);

    let live_state = server_state.clone();
    let live_check = std::thread::spawn(move || {
        let registry = live_state.live_worker_states.clone();
        let result = live_state.work_db.request_execution_with_live_check(
            RequestExecutionInput::builder().work_item_id(work_item_id).build(),
            |run_id| {
                inside_tx.send(()).unwrap();
                // Wait until dispatch is inside persistence (persist lock held,
                // checkpoint captured), then check liveness while still holding
                // the DB connection. Dispatch cannot finish until we return.
                persist_boundary_rx
                    .recv_timeout(wait)
                    .expect("dispatch never reached the persistence boundary");
                registry.is_run_live(run_id)
            },
        );
        live_done_tx.send(result.is_ok()).unwrap();
    });
    inside_rx.recv_timeout(wait).expect("live-check closure never ran");

    let dispatch_state = server_state.clone();
    let dispatch_id = execution_id.clone();
    let dispatch = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(dispatch_live_worker_state(&dispatch_state, &pre_tool_use(&dispatch_id)));
        dispatch_done_tx.send(()).unwrap();
    });
    assert!(
        live_done_rx
            .recv_timeout(wait)
            .expect("request_execution_with_live_check deadlocked"),
        "live check should succeed"
    );
    dispatch_done_rx.recv_timeout(wait).expect("hook dispatch deadlocked");
    live_check.join().unwrap();
    dispatch.join().unwrap();
}

/// A dispatch paused after applying its event must not write another run's
/// checkpoint if its slot is released and re-leased in the meantime.
#[tokio::test]
async fn a_recycled_slot_never_leaks_its_checkpoint_to_the_prior_run() {
    use crate::live_worker_state::HookOrigin;
    let (server_state, _dir) = test_server_state();
    let run_a = spawned_worker(&server_state);
    let applied = server_state.live_worker_states.apply_hook_event(
        SLOT,
        HookOrigin {
            ingress_seq: Some(1),
            from_subagent: false,
        },
        &stop(&run_a).event,
    );
    assert!(matches!(
        applied.outcome,
        crate::live_worker_state::ApplyEventOutcome::Applied { .. }
    ));

    // A newer event persists the opposite state before the old Stop resumes.
    let mut newer = pre_tool_use(&run_a);
    newer.connection.ingress_seq = Some(2);
    dispatch_live_worker_state(&server_state, &newer).await;
    let newer_checkpoint = server_state
        .work_db
        .get_run_semantic_progress_checkpoint(&run_a)
        .unwrap()
        .unwrap();
    assert_eq!(newer_checkpoint.turn_condition, SemanticTurnCondition::InTurn);

    // Run A's slot is recycled to run B, which has its own (ended-turn) state.
    server_state.live_worker_states.release_slot(SLOT);
    server_state
        .live_worker_states
        .register_spawn(SLOT, "run-b", "claude-opus-4-7", 4343, None);
    server_state.live_worker_states.apply_hook_event(
        SLOT,
        HookOrigin {
            ingress_seq: Some(1),
            from_subagent: false,
        },
        &stop("run-b").event,
    );
    let b_checkpoint = server_state
        .live_worker_states
        .semantic_progress_for_slot(SLOT)
        .unwrap();
    assert_eq!(b_checkpoint.turn_condition, SemanticTurnCondition::Ended);

    // A's paused persistence resumes: it must not see B's checkpoint.
    let seen = server_state
        .live_worker_states
        .with_semantic_progress(SLOT, &run_a, |settled| settled);
    assert!(seen.is_none(), "run A captured run B's checkpoint: {seen:?}");
    crate::app::worker_events::persist_applied_semantic_progress(
        &server_state,
        &run_a,
        SLOT,
        &stop(&run_a),
        &applied.outcome,
    );
    let a_row = server_state
        .work_db
        .get_run_semantic_progress_checkpoint(&run_a)
        .unwrap()
        .expect("run A still records its own progress");
    assert_eq!(a_row.turn_condition, newer_checkpoint.turn_condition);
    assert_eq!(a_row.tool_condition, newer_checkpoint.tool_condition);
    assert!(
        server_state
            .work_db
            .get_run_semantic_progress_checkpoint("run-b")
            .unwrap()
            .is_none(),
        "A's persistence must not write B's row"
    );
}
