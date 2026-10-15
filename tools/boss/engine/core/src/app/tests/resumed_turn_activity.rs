//! A worker that stops, then resumes a turn *without* a `UserPromptSubmit`
//! — Claude Code waking the session on its own because a background
//! command finished — must read as Working again from its first tool hook,
//! and a wait it declared before stopping must not linger on the pane.
//!
//! The 2026-10-15 shape this guards: a worker started a long `bazel test`
//! in the background, declared a wait, and ended its turn (`Stop` →
//! Idle). When the command completed the session re-woke, read a file,
//! and ran `checkleft run` for minutes — and the engine kept reporting
//! `activity=idle tool=-` with the old wait reason the whole time.
//!
//! These tests drive the real hook ingress (`dispatch_worker_event_fanout`)
//! against a real `ServerState`, and assert the same snapshot the app and
//! `bossctl agents` consume (`live_worker_states_snapshot`), so the wait
//! overlay is covered as well as the raw registry.

use super::*;

use crate::protocol::WorkerEvent;
use crate::test_support::*;
use boss_protocol::{RequestExecutionInput, WorkerActivity};

const SLOT: u8 = 1;
const WAIT_REASON: &str = "bazel test //tools/boss/engine/core/... compiling and running";

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
        .register_spawn(SLOT, execution.id.clone(), "claude-fable-5-1", 4242, None);
    server_state.worker_registry.register_run_slot(&execution.id, SLOT);
    execution.id
}

fn hook(event: WorkerEvent, execution_id: &str, seq: u64) -> crate::events_socket::IncomingHookEvent {
    crate::events_socket::IncomingHookEvent::for_test(event, Some(execution_id.to_owned()), None).with_ingress_seq(seq)
}

fn pre_tool_use(execution_id: &str, tool: &str, seq: u64) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::PreToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: tool.into(),
            tool_input: serde_json::Value::Null,
        },
        execution_id,
        seq,
    )
}

fn post_tool_use(execution_id: &str, tool: &str, seq: u64) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: tool.into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        },
        execution_id,
        seq,
    )
}

fn stop(execution_id: &str, seq: u64) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::Stop {
            session_id: "claude-sess-1".into(),
            stop_hook_active: false,
            stop_reason: boss_protocol::StopReason::Completed,
        },
        execution_id,
        seq,
    )
}

/// What `bossctl agents` and the app see for the slot.
fn snapshot(server_state: &ServerState) -> crate::protocol::LiveWorkerState {
    server_state
        .live_worker_states_snapshot()
        .into_iter()
        .find(|s| s.slot_id == SLOT)
        .expect("slot is registered")
}

fn declare_wait(server_state: &ServerState, execution_id: &str) {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    server_state
        .wait_registry
        .declare(execution_id, WAIT_REASON.to_owned(), None, 45 * 60, now)
        .expect("wait is within budget");
}

#[tokio::test]
async fn a_background_wakeup_turn_reads_as_working_and_ends_the_declared_wait() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);

    // The turn that declares the wait: `propose wait` is itself a Bash
    // call, followed by a couple more tools, then the Stop.
    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id, "Bash", 1)).await;
    declare_wait(&server_state, &execution_id);
    dispatch_worker_event_fanout(&server_state, &post_tool_use(&execution_id, "Bash", 2)).await;
    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id, "Bash", 3)).await;
    dispatch_worker_event_fanout(&server_state, &post_tool_use(&execution_id, "Bash", 4)).await;
    let mid_turn = snapshot(&server_state);
    assert_eq!(mid_turn.activity, WorkerActivity::Working);
    assert_eq!(
        mid_turn.wait_reason.as_deref(),
        Some(WAIT_REASON),
        "a wait declared mid-turn survives the rest of that turn's tool calls",
    );

    dispatch_worker_event_fanout(&server_state, &stop(&execution_id, 5)).await;
    let idle = snapshot(&server_state);
    assert_eq!(idle.activity, WorkerActivity::Idle);
    assert!(idle.current_tool.is_none());
    assert_eq!(
        idle.wait_reason.as_deref(),
        Some(WAIT_REASON),
        "the wait covers the idle period"
    );

    // The background command finishes; Claude Code re-wakes the session
    // with no user prompt. First evidence of the new turn is a tool hook.
    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id, "Read", 6)).await;
    let resumed = snapshot(&server_state);
    assert_eq!(
        resumed.activity,
        WorkerActivity::Working,
        "a tool hook after Stop is a new turn"
    );
    assert_eq!(resumed.current_tool.as_deref(), Some("Read"));
    assert!(
        resumed.wait_reason.is_none(),
        "the wait described the idle period that just ended; it must not linger on a working pane",
    );
    assert!(resumed.wait_expires_at.is_none());
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    assert!(
        server_state.wait_registry.active(&execution_id, now).is_none(),
        "the registry no longer holds the nudge ladder for it either",
    );
    assert_eq!(
        server_state.wait_registry.total_granted_secs(&execution_id),
        45 * 60,
        "ending a wait early does not refund the per-execution cap",
    );

    // The rest of the resumed turn, then its own Stop.
    dispatch_worker_event_fanout(&server_state, &post_tool_use(&execution_id, "Read", 7)).await;
    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id, "Bash", 8)).await;
    assert_eq!(snapshot(&server_state).current_tool.as_deref(), Some("Bash"));
    dispatch_worker_event_fanout(&server_state, &post_tool_use(&execution_id, "Bash", 9)).await;
    dispatch_worker_event_fanout(&server_state, &stop(&execution_id, 10)).await;
    let idle_again = snapshot(&server_state);
    assert_eq!(idle_again.activity, WorkerActivity::Idle);
    assert!(idle_again.current_tool.is_none());
    assert!(idle_again.wait_reason.is_none(), "no wait was re-declared");
}

#[tokio::test]
async fn a_stop_whose_ingress_finishes_after_the_next_turns_tool_hook_does_not_rewind_the_slot() {
    // Every hook connection is dispatched on its own task, and a Stop does
    // strictly more work (transcript persist, cost capture) before it
    // reaches the registry than a PreToolUse does. Accept order is stamped
    // as `ingress_seq`; the reducer applies in that order.
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);

    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id, "Bash", 1)).await;
    dispatch_worker_event_fanout(&server_state, &post_tool_use(&execution_id, "Bash", 2)).await;
    // seq 3 is the Stop — still in flight. seq 4, the resumed turn's first
    // tool hook, gets there first.
    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id, "Bash", 4)).await;
    assert_eq!(snapshot(&server_state).activity, WorkerActivity::Working);

    dispatch_worker_event_fanout(&server_state, &stop(&execution_id, 3)).await;
    let after_late_stop = snapshot(&server_state);
    assert_eq!(
        after_late_stop.activity,
        WorkerActivity::Working,
        "the Stop the worker emitted before its current tool call must not win the race",
    );
    assert_eq!(after_late_stop.current_tool.as_deref(), Some("Bash"));

    // The resumed turn's own Stop ends it.
    dispatch_worker_event_fanout(&server_state, &post_tool_use(&execution_id, "Bash", 5)).await;
    dispatch_worker_event_fanout(&server_state, &stop(&execution_id, 6)).await;
    let idle = snapshot(&server_state);
    assert_eq!(idle.activity, WorkerActivity::Idle);
    assert!(idle.current_tool.is_none());
}

#[tokio::test]
async fn a_user_prompt_after_stop_also_ends_the_declared_wait() {
    // Whatever starts the new turn — a prompt, not just a tool hook — the
    // idle period the wait described is over.
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);
    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id, "Bash", 1)).await;
    declare_wait(&server_state, &execution_id);
    dispatch_worker_event_fanout(&server_state, &post_tool_use(&execution_id, "Bash", 2)).await;
    dispatch_worker_event_fanout(&server_state, &stop(&execution_id, 3)).await;
    assert_eq!(snapshot(&server_state).wait_reason.as_deref(), Some(WAIT_REASON));

    dispatch_worker_event_fanout(
        &server_state,
        &hook(
            WorkerEvent::UserPromptSubmit {
                session_id: "claude-sess-1".into(),
                prompt: "status?".into(),
            },
            &execution_id,
            4,
        ),
    )
    .await;
    let resumed = snapshot(&server_state);
    assert_eq!(resumed.activity, WorkerActivity::Working);
    assert!(resumed.wait_reason.is_none());
}
