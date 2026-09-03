//! Shared progress-ingress writes the semantic checkpoint; engine-synthesized
//! display timestamps do not.

use super::*;

use crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS;
use crate::protocol::WorkerEvent;
use crate::semantic_progress::SemanticToolCondition;
use crate::test_support::*;
use boss_protocol::RequestExecutionInput;

const SLOT: u8 = 1;

fn spawned_worker(server_state: &ServerState) -> String {
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
    execution.id
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
