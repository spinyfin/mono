//! A refused board drag is answered with a structured `WorkItemRefused`
//! (machine-readable code + plain-language reason + optional next step), not
//! a bare error string, so the app can show it next to the dragged card.

use super::*;
use crate::app::work_items::handle_move_work_item_on_board;
use crate::test_support::{create_test_chore_manual, create_test_product_with_repo};
use boss_protocol::{
    BoardColumn, BoardDropTarget, BoardRefusalAction, BoardRefusalCode, FrontendRequest, TaskStatus, WorkItem,
    WorkItemPatch,
};

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-board-drag-refusal")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(sink.next().await.is_none(), "handler must send exactly one response");
    response
}

fn drop_on(id: &str, column: BoardColumn) -> FrontendRequest {
    FrontendRequest::MoveWorkItemOnBoard {
        id: id.to_owned(),
        target: BoardDropTarget::new(column, None),
        bypass_dispatch_pause: false,
        observed_pause_since_epoch_s: None,
    }
}

/// The motivating case: an item that already owns a PR is dragged to Doing.
/// Dispatch refuses a fresh implementation run, the engine rolls the status
/// back, and the reply names the PR and points at revisions.
#[tokio::test]
async fn drag_to_doing_of_item_with_bound_pr_is_refused_with_code_and_reason() {
    let (state, _dir) = test_server_state_with_fakes();
    let product = create_test_product_with_repo(
        &state.work_db,
        "BoardDragRefusal",
        Some("git@example.com:board/drag-refusal.git"),
    );
    let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "Post-merge follow-up");
    state
        .work_db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                pr_url: Some("https://github.com/example/repo/pull/3163".to_owned()),
                ..Default::default()
            },
        )
        .expect("bind a PR");

    let sink = make_session_sink();
    handle_move_work_item_on_board(dispatch(&state, &sink), drop_on(&chore.id, BoardColumn::Doing)).await;

    match sole_response(&sink).await {
        FrontendEvent::WorkItemRefused {
            id,
            code,
            message,
            suggested_action,
        } => {
            assert_eq!(id, chore.id);
            assert_eq!(code, BoardRefusalCode::BoundPr);
            assert_eq!(
                message,
                "This item already has a PR (#3163). To change it, add a revision."
            );
            assert_eq!(suggested_action, Some(BoardRefusalAction::AddRevision));
        }
        other => panic!("expected WorkItemRefused, got {other:?}"),
    }

    match state.work_db.get_work_item(&chore.id).expect("get") {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Todo, "the refused drag must roll the status back")
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// A drop that names something that is not a card is refused with a code too,
/// instead of a bare error.
#[tokio::test]
async fn drop_of_unknown_item_is_refused_as_not_a_card() {
    let (state, _dir) = test_server_state_with_fakes();
    let sink = make_session_sink();
    handle_move_work_item_on_board(dispatch(&state, &sink), drop_on("task_missing", BoardColumn::Doing)).await;
    match sole_response(&sink).await {
        FrontendEvent::WorkItemRefused { code, message, .. } => {
            assert_eq!(code, BoardRefusalCode::NotACard);
            assert!(!message.is_empty());
        }
        other => panic!("expected WorkItemRefused, got {other:?}"),
    }
}
