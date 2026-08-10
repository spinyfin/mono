//! Unit tests for the pause-bypass dispatch re-check refusal arm in
//! `apply_work_item_patch`: when the authoritative re-check refuses after the
//! status patch has already landed (the narrow window between the board-
//! handler pre-check and `dispatch_with_pause_bypass`), the row must be
//! reverted, no execution left behind, and the client answered with a
//! `work_error` rather than a success `WorkItemUpdated`.

use super::*;
use crate::app::work_items::{BoardDragPauseBypass, apply_work_item_patch};
use crate::coordinator::DispatchPauseOrigin;
use crate::test_support::{create_test_chore_manual, create_test_product_with_repo};
use boss_protocol::{PauseReason, TaskStatus, WorkItem, WorkItemPatch};

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-pause-bypass-recheck")
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

/// Drive the pause-bypass re-check refusal path directly: status is patched
/// to `active` first, then `dispatch_with_pause_bypass` refuses because the
/// observed pause generation is stale. Asserts the three contracts the
/// revert arm exists for: status is back at its previous value, no
/// execution row was created, and the client receives a `work_error`.
#[tokio::test]
async fn pause_bypass_recheck_refusal_reverts_status_and_answers_work_error() {
    let (server_state, _dir) = test_server_state();
    let product = create_test_product_with_repo(
        &server_state.work_db,
        "PauseBypassRecheck",
        Some("git@example.com:pause/recheck.git"),
    );
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "Held chore");
    assert_eq!(chore.status, TaskStatus::Todo);

    // Active operator pause with a known generation.
    let live_gen = 1_700_000_000_u64;
    server_state.execution_coordinator.pause_dispatch(
        live_gen,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: operator pause for recheck").unwrap(),
    );

    // Observed generation is older than the live one — the re-check inside
    // `dispatch_with_pause_bypass` must refuse. Calling `apply_work_item_patch`
    // directly (rather than `handle_move_work_item_on_board`) skips the
    // board-handler pre-check that would otherwise refuse before the status
    // patch, so this exercises the in-request TOCTOU revert arm itself.
    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink);
    let patch = WorkItemPatch::builder().status("active").build();
    apply_work_item_patch(
        ctx,
        chore.id.clone(),
        patch,
        Some(BoardDragPauseBypass {
            observed_pause_since_epoch_s: Some(1_600_000_000),
        }),
    )
    .await;

    let response = sole_response(&sink).await;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.to_lowercase().contains("changed")
                    || message.to_lowercase().contains("pause")
                    || message.to_lowercase().contains("force"),
                "refusal must name the pause/generation problem, got: {message}"
            );
        }
        other => panic!("expected WorkError after re-check refusal, got: {other:?}"),
    }

    let item = server_state
        .work_db
        .get_work_item(&chore.id)
        .expect("chore still exists");
    let (status, last_status_actor) = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => (t.status, t.last_status_actor),
        other => panic!("expected task/chore, got {other:?}"),
    };
    assert_eq!(
        status,
        TaskStatus::Todo,
        "re-check refusal must leave the card at its previous status, not stranded in active"
    );
    assert_eq!(
        last_status_actor, "engine",
        "the engine-initiated status revert must stamp last_status_actor=engine, not human"
    );

    let executions = server_state
        .work_db
        .list_executions(Some(&chore.id))
        .expect("list executions");
    assert!(
        executions.is_empty(),
        "a re-check-refused force drag must create no execution row; got {executions:?}"
    );
}
