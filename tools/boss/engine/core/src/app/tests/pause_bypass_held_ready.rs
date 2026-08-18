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
use crate::test_support::{create_ready_chore_execution, create_test_chore_manual, create_test_product_with_repo};
use boss_protocol::{
    BoardColumn, BoardDropTarget, ExecutionStatus, FrontendRequest, PauseReason, TaskStatus, WorkItem,
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
    let (server_state, _dir) = test_server_state();
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
