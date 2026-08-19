//! Regression: a confirmed board-drag pause-bypass must dispatch a
//! held `ready` execution, not skip because `work_item_needs_dispatch`
//! is false for any non-terminal row.
//!
//! The dispatch pause is admission-only, so the normal outcome of a
//! row becoming eligible while paused is a `ready` execution sitting
//! held. Gating `dispatch_with_pause_bypass` on `needs_dispatch` made
//! the force gesture a guaranteed no-op in exactly that state.

use super::*;
use crate::app::handler_helpers::work_item_needs_dispatch;
use crate::app::work_items::handle_move_work_item_on_board;
use crate::coordinator::DispatchPauseOrigin;
use crate::test_support::{
    create_ready_chore_execution, create_spawned_execution, create_test_chore_manual, create_test_product_with_repo,
};
use boss_protocol::{
    BoardColumn, BoardDropTarget, ExecutionStatus, FrontendRequest, PauseReason, TaskStatus, WorkItem, WorkItemPatch,
};

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-pause-bypass-held-ready")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

fn pause_operator(state: &ServerState, since_epoch_s: u64) {
    state.execution_coordinator.pause_dispatch(
        since_epoch_s,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: operator pause holding a ready row").unwrap(),
    );
}

/// The exact production failure: dispatch is paused, a `ready` execution
/// already owns the slot (`work_item_needs_dispatch` is therefore false),
/// and the operator consents to a board-drag override. The held row must
/// be claimed past the pause, and the ledger must record
/// `dispatch_pause_override` with `entry_point: app_drag`.
#[tokio::test]
async fn board_drag_pause_bypass_dispatches_held_ready_execution() {
    let (server_state, _dir) = test_server_state_with_fakes();
    let product = create_test_product_with_repo(
        &server_state.work_db,
        "PauseBypassHeldReady",
        Some("git@example.com:pause/held-ready.git"),
    );
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "Held ready chore");
    assert_eq!(chore.status, TaskStatus::Todo);

    let held = create_ready_chore_execution(&server_state.work_db, &chore.id);
    assert_eq!(held.status, ExecutionStatus::Ready);
    assert!(
        !work_item_needs_dispatch(&server_state.work_db, &chore.id),
        "precondition: a held ready row must make work_item_needs_dispatch false — \
         that is the gate that used to drop the force gesture"
    );

    let live_gen = 1_700_000_000_u64;
    pause_operator(&server_state, live_gen);

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink);
    handle_move_work_item_on_board(
        ctx,
        FrontendRequest::MoveWorkItemOnBoard {
            id: chore.id.clone(),
            target: BoardDropTarget::new(BoardColumn::Doing, None),
            bypass_dispatch_pause: true,
            observed_pause_since_epoch_s: Some(live_gen),
        },
    )
    .await;

    let response = sole_response(&sink).await;
    match response {
        FrontendEvent::WorkItemUpdated { item } => {
            let status = match item {
                WorkItem::Task(t) | WorkItem::Chore(t) => t.status,
                other => panic!("expected task/chore, got {other:?}"),
            };
            assert_eq!(status, TaskStatus::Active, "force drag must land the card in Doing");
        }
        other => panic!("expected WorkItemUpdated after a consented override, got: {other:?}"),
    }

    let executions = server_state
        .work_db
        .list_executions(Some(&chore.id))
        .expect("list executions");
    assert_eq!(
        executions.len(),
        1,
        "force-start must reuse the held ready row, not mint another; got {executions:?}"
    );
    assert_eq!(
        executions[0].id, held.id,
        "the held ready row is the one that dispatched"
    );

    let events = crate::dispatch_reader::read_current(&server_state.dispatch_event_root)
        .expect("read dispatch events")
        .events;
    let override_event = events
        .iter()
        .find(|e| e.stage == "dispatch_pause_override")
        .unwrap_or_else(|| panic!("expected a dispatch_pause_override event, got: {events:?}"));
    assert_eq!(override_event.outcome, "ok");
    assert_eq!(override_event.details["entry_point"], "app_drag");
    assert_eq!(override_event.execution_id, held.id);

    let transition = events
        .iter()
        .find(|e| e.stage == "status_transition")
        .unwrap_or_else(|| panic!("expected a status_transition event, got: {events:?}"));
    assert_eq!(
        transition.details.get("did_dispatch"),
        Some(&serde_json::Value::Bool(true)),
        "consented override must report did_dispatch=true; got {:?}",
        transition.details
    );
    assert_eq!(
        transition.details.get("dispatched_execution_id"),
        Some(&serde_json::Value::String(held.id.clone())),
        "status_transition must name the reused ready execution; got {:?}",
        transition.details
    );
}

/// Ordinary (no-bypass) drag must still skip when a non-terminal
/// execution already owns the slot. The force path is the exception;
/// `work_item_needs_dispatch` semantics for every other caller stay.
#[tokio::test]
async fn board_drag_without_bypass_skips_when_ready_execution_exists() {
    let (server_state, _dir) = test_server_state();
    let product = create_test_product_with_repo(
        &server_state.work_db,
        "PauseBypassNoForceSkip",
        Some("git@example.com:pause/no-force.git"),
    );
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "Already queued");
    let held = create_ready_chore_execution(&server_state.work_db, &chore.id);
    pause_operator(&server_state, 1_700_000_000);

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink);
    handle_move_work_item_on_board(
        ctx,
        FrontendRequest::MoveWorkItemOnBoard {
            id: chore.id.clone(),
            target: BoardDropTarget::new(BoardColumn::Doing, None),
            bypass_dispatch_pause: false,
            observed_pause_since_epoch_s: None,
        },
    )
    .await;

    let response = sole_response(&sink).await;
    assert!(
        matches!(response, FrontendEvent::WorkItemUpdated { .. }),
        "ordinary drag must still succeed (status flip) when a ready row exists, got: {response:?}"
    );

    let executions = server_state
        .work_db
        .list_executions(Some(&chore.id))
        .expect("list executions");
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].id, held.id);
    assert_eq!(
        executions[0].status,
        ExecutionStatus::Ready,
        "ordinary drag must not force-dispatch the held ready row"
    );

    let events = crate::dispatch_reader::read_current(&server_state.dispatch_event_root)
        .expect("read dispatch events")
        .events;
    assert!(
        events.iter().all(|e| e.stage != "dispatch_pause_override"),
        "ordinary drag must not record a pause override; got: {events:?}"
    );
    let transition = events
        .iter()
        .find(|e| e.stage == "status_transition")
        .unwrap_or_else(|| panic!("expected a status_transition event, got: {events:?}"));
    assert_eq!(
        transition.details.get("did_dispatch"),
        Some(&serde_json::Value::Bool(false)),
        "ordinary skip must report did_dispatch=false; got {:?}",
        transition.details
    );
    let skip = transition
        .details
        .get("reason_if_skipped")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        skip.contains("work_item_needs_dispatch=false"),
        "ordinary skip reason must still name the needs_dispatch gate, got: {skip}"
    );
    assert!(
        skip.contains("no pause-bypass requested"),
        "ordinary skip reason must not imply a progressing worker when nothing was forced, got: {skip}"
    );
}

/// A consented bypass drag onto a work item whose latest execution is
/// already live (non-`ready` — e.g. a worker is genuinely running) must
/// NOT report `did_dispatch: true`. `dispatch_with_pause_bypass` reuses
/// the already-live execution and never claims anything on this request's
/// behalf, so the event must say so honestly rather than claiming a
/// dispatch that did not happen.
async fn assert_board_drag_pause_bypass_onto_already_live_execution_reports_no_dispatch(pause_lifted: bool) {
    let (server_state, _dir) = test_server_state_with_fakes();
    let product = create_test_product_with_repo(
        &server_state.work_db,
        "PauseBypassAlreadyLive",
        Some("git@example.com:pause/already-live.git"),
    );
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "Genuinely running chore");

    // A spawned execution is `waiting_human` (non-terminal, non-`ready`) —
    // exactly the "a worker is genuinely running" shape this test covers.
    let live_execution_id = create_spawned_execution(&server_state.work_db, &chore.id, 999_999);
    let live_execution = server_state.work_db.get_execution(&live_execution_id).unwrap();
    assert_ne!(
        live_execution.status,
        ExecutionStatus::Ready,
        "precondition: the execution must already be live (non-ready), not held"
    );
    // The live-worker check `request_execution_with_live_check` runs keys
    // off `execution.id` treated as a registry run id (see
    // `dispatch_admission.rs`'s and `work_items.rs`'s `is_run_live(&execution.id)`
    // callers) — register it as a live slot so the coordinator treats this
    // as "genuinely running" rather than "worker died, re-dispatch it".
    register_idle_worker(&server_state, &live_execution_id, 1);

    // Move the chore itself to a non-active status (`in_review`) so the
    // drag-to-Doing below is a genuine `task_transitioned_to_active`.
    server_state
        .work_db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

    let live_gen = 1_700_000_000_u64;
    pause_operator(&server_state, live_gen);
    if pause_lifted {
        server_state.execution_coordinator.resume_dispatch();
    }

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink);
    handle_move_work_item_on_board(
        ctx,
        FrontendRequest::MoveWorkItemOnBoard {
            id: chore.id.clone(),
            target: BoardDropTarget::new(BoardColumn::Doing, None),
            bypass_dispatch_pause: true,
            observed_pause_since_epoch_s: Some(live_gen),
        },
    )
    .await;

    let response = sole_response(&sink).await;
    match response {
        FrontendEvent::WorkItemUpdated { item } => {
            let status = match item {
                WorkItem::Task(t) | WorkItem::Chore(t) => t.status,
                other => panic!("expected task/chore, got {other:?}"),
            };
            assert_eq!(
                status,
                TaskStatus::Active,
                "the card must still land in Doing — reusing the live execution is not a refusal"
            );
        }
        other => panic!("expected WorkItemUpdated after a consented override, got: {other:?}"),
    }

    let executions = server_state
        .work_db
        .list_executions(Some(&chore.id))
        .expect("list executions");
    assert_eq!(executions.len(), 1, "no new execution should have been minted");
    assert_eq!(executions[0].id, live_execution_id);

    let events = crate::dispatch_reader::read_current(&server_state.dispatch_event_root)
        .expect("read dispatch events")
        .events;
    assert!(
        events.iter().all(|e| e.stage != "dispatch_pause_override"),
        "reusing an already-live execution must not record a dispatch_pause_override event; got: {events:?}"
    );
    let transition = events
        .iter()
        .find(|e| e.stage == "status_transition")
        .unwrap_or_else(|| panic!("expected a status_transition event, got: {events:?}"));
    assert_eq!(
        transition.details.get("did_dispatch"),
        Some(&serde_json::Value::Bool(false)),
        "reusing an already-live execution must report did_dispatch=false; got {:?}",
        transition.details
    );
    assert_eq!(
        transition.details.get("dispatched_execution_id"),
        Some(&serde_json::Value::String(live_execution_id.clone())),
        "the event must still name the reused (not newly dispatched) execution; got {:?}",
        transition.details
    );
    let skip = transition
        .details
        .get("reason_if_skipped")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        skip.contains(&live_execution_id),
        "the skip reason must name the already-live execution that owns the slot, got: {skip}"
    );
}

#[tokio::test]
async fn board_drag_pause_bypass_onto_already_live_execution_reports_no_dispatch() {
    assert_board_drag_pause_bypass_onto_already_live_execution_reports_no_dispatch(false).await;
}

#[tokio::test]
async fn board_drag_pause_bypass_after_pause_lifts_onto_already_live_execution_reports_no_dispatch() {
    assert_board_drag_pause_bypass_onto_already_live_execution_reports_no_dispatch(true).await;
}
