//! The stored execution status and the live pane must agree about whether
//! a worker is blocked on a human.
//!
//! These tests drive a real `ServerState` and a real DB through the real
//! hook ingress, and assert **both** surfaces after every event — because
//! the defect they guard was precisely that the two disagreed and only one
//! of them was ever checked. Asserting the row alone, or the pane alone,
//! would have passed happily throughout the entire pre-fix period:
//!
//! ```text
//! bossctl agents:  slot 4  activity: working  tool: Bash
//! work_executions: exec_…  status: waiting_human
//! ```
//!
//! See `crate::awaiting_input_status`.

use super::*;

use crate::live_worker_state::LiveSpawnRouting;
use crate::protocol::WorkerEvent;
use crate::test_support::*;
use crate::work::ExecutionStatus;
use boss_protocol::{RequestExecutionInput, WorkerActivity};

const SLOT: u8 = 1;

/// Seed a chore with a live pane-hosted worker in the post-spawn shape —
/// the row `running`, a registered slot whose driver declares the
/// awaiting-input capability — and return its execution id.
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
    // What `PaneSpawnRunner` records once the pane is up: the spawn run is
    // closed and the execution stays `running` (RunWaitState::WorkerPaneAlive).
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

fn notification(execution_id: &str) -> crate::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::Notification {
            session_id: "claude-sess-1".into(),
            message: "Claude needs your permission to use Bash".into(),
        },
        execution_id,
    )
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

/// Assert both surfaces at once, so neither can drift without a failure.
fn assert_surfaces_agree(
    server_state: &ServerState,
    execution_id: &str,
    expected_status: ExecutionStatus,
    expected_activity: WorkerActivity,
    stage: &str,
) {
    let status = server_state.work_db.get_execution(execution_id).unwrap().status;
    let activity = server_state.live_worker_states.get(SLOT).unwrap().activity;
    assert_eq!(
        activity, expected_activity,
        "{stage}: the pane's reported activity is wrong",
    );
    assert_eq!(
        status, expected_status,
        "{stage}: the stored execution status is wrong (pane says {activity:?})",
    );
}

/// **The whole defect, end to end.** A worker goes into a genuine wait and
/// back out; the stored status must be correct at each step, and must never
/// contradict the pane.
#[tokio::test]
async fn a_worker_enters_a_genuine_wait_and_comes_back_out_with_the_row_correct_throughout() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);

    // 1. Post-spawn. Nothing is waiting on a human: the pane is up and the
    //    row says so. Before mono#2673 this instant already stored
    //    `waiting_human`, and stayed there for the rest of the run.
    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running,
        "a freshly spawned worker's row must say running",
    );

    // 2. The worker works. Both surfaces stay on `running`/Working.
    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id)).await;
    assert_surfaces_agree(
        &server_state,
        &execution_id,
        ExecutionStatus::Running,
        WorkerActivity::Working,
        "mid-tool-call",
    );

    // 3. The worker hits a permission prompt. This is the genuine wait —
    //    the driver's own awaiting-input notification, not an inference
    //    from tool timing — and the row must follow it.
    dispatch_worker_event_fanout(&server_state, &notification(&execution_id)).await;
    assert_surfaces_agree(
        &server_state,
        &execution_id,
        ExecutionStatus::WaitingHuman,
        WorkerActivity::WaitingForInput,
        "blocked on a permission prompt",
    );

    // 4. A `Stop` while the notification is still pending keeps the wait —
    //    the turn ended, but the worker is still blocked.
    dispatch_worker_event_fanout(
        &server_state,
        &hook(
            WorkerEvent::Stop {
                session_id: "claude-sess-1".into(),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::AwaitingInput,
            },
            &execution_id,
        ),
    )
    .await;
    assert_surfaces_agree(
        &server_state,
        &execution_id,
        ExecutionStatus::WaitingHuman,
        WorkerActivity::WaitingForInput,
        "still blocked at the turn boundary",
    );

    // 5. The human answers and the worker resumes. THIS is the transition
    //    that never existed: nothing used to clear `waiting_human`, so the
    //    row stayed there for the rest of the execution's life.
    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id)).await;
    assert_surfaces_agree(
        &server_state,
        &execution_id,
        ExecutionStatus::Running,
        WorkerActivity::Working,
        "resumed after the human answered",
    );
}

/// A worker that never blocks on anyone must never store `waiting_human`.
///
/// This is the reproduction from the field inverted into an assertion: the
/// live incident showed `activity: working, tool: Bash` alongside a stored
/// `waiting_human`, with a tool call having completed seconds earlier.
#[tokio::test]
async fn a_worker_that_only_ever_works_never_stores_waiting_human() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);

    for _ in 0..3 {
        dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id)).await;
        assert_surfaces_agree(
            &server_state,
            &execution_id,
            ExecutionStatus::Running,
            WorkerActivity::Working,
            "tool call in flight",
        );
        dispatch_worker_event_fanout(
            &server_state,
            &hook(
                WorkerEvent::PostToolUse {
                    session_id: "claude-sess-1".into(),
                    tool_name: "Bash".into(),
                    tool_input: serde_json::Value::Null,
                    tool_response: serde_json::Value::Null,
                },
                &execution_id,
            ),
        )
        .await;
        assert_surfaces_agree(
            &server_state,
            &execution_id,
            ExecutionStatus::Running,
            WorkerActivity::Working,
            "tool call returned",
        );
    }
}

/// A driver that does not declare `Capability::AwaitingInputSignal` gets no
/// `WaitingForInput` promotion from the registry, so the row must not move
/// either. The two surfaces have to agree about the *absence* of a signal
/// as much as its presence — a row that parked here would be a wait the
/// pane never reported and nothing would ever clear.
#[tokio::test]
async fn an_untrusted_notification_moves_neither_surface() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);
    server_state.live_worker_states.set_awaiting_input_capable(SLOT, false);

    dispatch_worker_event_fanout(&server_state, &pre_tool_use(&execution_id)).await;
    dispatch_worker_event_fanout(&server_state, &notification(&execution_id)).await;

    assert_surfaces_agree(
        &server_state,
        &execution_id,
        ExecutionStatus::Running,
        WorkerActivity::Working,
        "notification from a driver that does not back the signal",
    );
}

/// The mirror must never resurrect a row another path has already moved on.
/// A completion that advanced the execution to `waiting_review` outranks a
/// late `Notification` from the same pane — [`WorkDb::set_execution_awaiting_human`]
/// applies that guard in SQL, and this pins it through the real ingress.
#[tokio::test]
async fn a_late_notification_never_drags_a_settled_row_back() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);
    let work_item_id = server_state.work_db.get_execution(&execution_id).unwrap().work_item_id;
    server_state
        .work_db
        .force_execution_status_for_test(&work_item_id, ExecutionStatus::WaitingReview)
        .unwrap();

    dispatch_worker_event_fanout(&server_state, &notification(&execution_id)).await;

    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingReview,
        "a settled row is owned by the completion path, not by a late hook",
    );
}

/// The directory-trust-prompt stall: a pid-bearing, capability-declaring
/// slot that never receives a single hook. `mark_stalled_spawns` (the
/// timer-driven sweep in `app/server.rs`, not the hook-dispatch path) is
/// the only thing that ever observes this, so it must mirror the promotion
/// onto the row itself — see `awaiting_input_status::mirror_stalled_spawn_wait`.
#[tokio::test]
async fn a_stalled_spawn_with_no_hooks_ever_mirrors_onto_the_row() {
    let (server_state, _dir) = test_server_state();
    let execution_id = spawned_worker(&server_state);

    // No hook ever arrives — advance straight past the threshold.
    let now =
        boss_engine_utils::epoch_time::now_epoch_secs() + crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS + 1;
    let stalled = server_state
        .live_worker_states
        .mark_stalled_spawns(now, crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS);
    assert_eq!(stalled, vec![SLOT], "the pid-bearing, capable slot must be promoted");

    for slot_id in &stalled {
        let execution_id = server_state
            .live_worker_states
            .get(*slot_id)
            .map(|state| state.run_id)
            .expect("slot must resolve to a run id");
        crate::awaiting_input_status::mirror_stalled_spawn_wait(
            &server_state.work_db,
            &server_state.publisher,
            &execution_id,
        )
        .await;
    }

    assert_surfaces_agree(
        &server_state,
        &execution_id,
        ExecutionStatus::WaitingHuman,
        WorkerActivity::WaitingForInput,
        "stalled on the directory-trust prompt with no hook ever received",
    );
}

/// A `pr_review` reviewer must never be mirrored out of `running`: the
/// kanban "AI reviewing" badge queries `kind = pr_review AND status =
/// running` with no `is_live()` widening, so moving the row to
/// `waiting_human` would silently drop the badge for the duration of an
/// ordinary permission-prompt notification — which reviewer sessions (run
/// with tool calls denied) hit routinely, not just on a genuine wait.
#[tokio::test]
async fn a_pr_review_notification_never_moves_the_row_off_running() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product = create_test_product_with_repo(db, "p", Some("git@example.com:p.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), "c");
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let (_exec, run) = db
        .start_execution_run(&execution.id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    finish_run_worker_pane_alive(db, &execution.id, &run.id, Some("Spawned reviewer pane in slot 1."));
    server_state.live_worker_states.register_spawn_with_capabilities(
        SLOT,
        execution.id.clone(),
        "claude-opus-4-7",
        4242,
        None,
        true,
        LiveSpawnRouting::new("review", "pr_review"),
    );
    server_state.worker_registry.register_run_slot(&execution.id, SLOT);

    dispatch_worker_event_fanout(&server_state, &notification(&execution.id)).await;

    let status = server_state.work_db.get_execution(&execution.id).unwrap().status;
    let activity = server_state.live_worker_states.get(SLOT).unwrap().activity;
    assert_eq!(
        activity,
        WorkerActivity::WaitingForInput,
        "the pane still honestly reports the notification",
    );
    assert_eq!(
        status,
        ExecutionStatus::Running,
        "a pr_review row must stay running so the AI-reviewing badge survives a permission prompt",
    );
}
