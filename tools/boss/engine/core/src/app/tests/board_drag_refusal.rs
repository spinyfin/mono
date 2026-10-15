//! A refused board drag is answered with a structured `WorkItemRefused`
//! (machine-readable code + plain-language reason + optional next step), not
//! a bare error string, so the app can show it next to the dragged card.

use super::*;
use crate::app::projects::handle_reorder_project_tasks;
use crate::app::work_items::{handle_move_work_item_on_board, handle_update_work_item};
use crate::test_support::{create_test_chore_manual, create_test_product_with_repo};
use boss_protocol::{
    BoardColumn, BoardDropTarget, BoardRefusalAction, BoardRefusalCode, FrontendRequest, TaskStatus, WorkItem,
    WorkItemPatch,
};

/// Like `test_server_state_with_fakes`, with the PR open/merged/closed probe
/// answering `pr_state` for every PR.
fn state_with_pr_state(pr_state: crate::work::PrOpenState) -> (Arc<ServerState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build(),
        None,
    ));
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(
        cfg,
        None,
        None,
        ServerStateOverrides {
            cube_client: Some(Arc::new(crate::test_support::AlwaysSucceedsCube)),
            execution_runner: Some(Arc::new(crate::test_support::AlwaysSucceedsRunner)),
            pr_state_checker: Some(Arc::new(crate::work::FakePrStateChecker::always(pr_state))),
            ..Default::default()
        },
    )
    .unwrap();
    (state, temp)
}

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
    let envelope = sink.next().await.expect("handler must send a response");
    assert_eq!(
        envelope.request_id.as_deref(),
        Some("req-board-drag-refusal"),
        "the response must carry the request's id so the app can correlate it"
    );
    assert!(sink.next().await.is_none(), "handler must send exactly one response");
    envelope.payload
}

fn drop_on(id: &str, column: BoardColumn) -> FrontendRequest {
    FrontendRequest::MoveWorkItemOnBoard {
        id: id.to_owned(),
        target: BoardDropTarget::new(column, None),
        bypass_dispatch_pause: false,
        observed_pause_since_epoch_s: None,
        explicit_status: None,
    }
}

/// The motivating case: an item that already owns a PR is dragged to Doing.
/// Dispatch refuses a fresh implementation run, the engine rolls the status
/// back, and the reply names the PR and points at revisions.
#[tokio::test]
async fn drag_to_doing_of_item_with_bound_pr_is_refused_with_code_and_reason() {
    let (state, _dir) = state_with_pr_state(crate::work::PrOpenState::Open);
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

/// A repo-less item dragged to Doing is refused up front with the missing-repo
/// code and a sentence naming the item by title, not by internal id.
#[tokio::test]
async fn drag_to_doing_without_a_repo_is_refused_in_plain_language() {
    let (state, _dir) = test_server_state_with_fakes();
    let product =
        create_test_product_with_repo(&state.work_db, "NoRepoBoard", Some("git@example.com:board/no-repo.git"));
    let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "Needs a home");
    // Clear the product default so the chore resolves to no repo.
    state
        .work_db
        .update_work_item(
            &product.id,
            WorkItemPatch {
                repo_remote_url: Some(String::new()),
                ..Default::default()
            },
        )
        .expect("clear product repo");

    let sink = make_session_sink();
    handle_move_work_item_on_board(dispatch(&state, &sink), drop_on(&chore.id, BoardColumn::Doing)).await;

    match sole_response(&sink).await {
        FrontendEvent::WorkItemRefused { code, message, .. } => {
            assert_eq!(code, BoardRefusalCode::MissingRepo);
            assert!(message.contains("Needs a home"), "must name the item, got: {message}");
            assert!(
                !message.contains(&chore.id),
                "must not leak the internal id, got: {message}"
            );
        }
        other => panic!("expected WorkItemRefused, got {other:?}"),
    }
}

/// Only board gestures get the structured refusal: a plain `UpdateWorkItem`
/// (the CLI's path) is still answered with a `WorkError`.
#[tokio::test]
async fn plain_update_work_item_refusal_stays_a_work_error() {
    let (state, _dir) = test_server_state_with_fakes();
    let product = create_test_product_with_repo(
        &state.work_db,
        "UpdateCompat",
        Some("git@example.com:board/update-compat.git"),
    );
    let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "Already has a PR");
    state
        .work_db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                pr_url: Some("https://github.com/example/repo/pull/3164".to_owned()),
                ..Default::default()
            },
        )
        .expect("bind a PR");

    let sink = make_session_sink();
    let req = FrontendRequest::UpdateWorkItem {
        id: chore.id.clone(),
        patch: WorkItemPatch::builder().status("active").build(),
    };
    handle_update_work_item(dispatch(&state, &sink), req).await;

    match sole_response(&sink).await {
        FrontendEvent::WorkError { message } => {
            assert_eq!(message, "Work already has a bound PR; use a revision to change it");
        }
        other => panic!("expected WorkError for a non-board caller, got {other:?}"),
    }
}

fn bind_pr(state: &Arc<ServerState>, id: &str, status: Option<&str>) {
    let mut patch = WorkItemPatch::builder()
        .pr_url("https://github.com/example/repo/pull/3170")
        .build();
    patch.status = status.map(str::to_owned);
    state.work_db.update_work_item(id, patch).expect("bind a PR");
}

/// A merged item (status `done`) can't take a revision, so its bound-PR
/// refusal must not offer one and must not tell the user to add one.
#[tokio::test]
async fn drag_of_merged_item_to_doing_offers_no_revision() {
    let (state, _dir) = test_server_state_with_fakes();
    let product =
        create_test_product_with_repo(&state.work_db, "MergedBoard", Some("git@example.com:board/merged.git"));
    let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "Already shipped");
    bind_pr(&state, &chore.id, Some("done"));

    let sink = make_session_sink();
    handle_move_work_item_on_board(dispatch(&state, &sink), drop_on(&chore.id, BoardColumn::Doing)).await;

    match sole_response(&sink).await {
        FrontendEvent::WorkItemRefused {
            code,
            message,
            suggested_action,
            ..
        } => {
            assert_eq!(code, BoardRefusalCode::BoundPrNotRevisable);
            assert_eq!(suggested_action, None, "a merged PR cannot take a revision");
            assert!(message.contains("#3170"), "must name the PR, got: {message}");
            assert!(!message.contains("add a revision"), "got: {message}");
        }
        other => panic!("expected WorkItemRefused, got {other:?}"),
    }
}

/// Drag an item with a bound PR (still `todo`) to Doing under a probe that
/// reports `pr_state`, and return the refusal.
async fn bound_pr_refusal(
    pr_state: crate::work::PrOpenState,
) -> (Option<BoardRefusalAction>, BoardRefusalCode, String) {
    let (state, _dir) = state_with_pr_state(pr_state);
    let product = create_test_product_with_repo(&state.work_db, "ProbeBoard", Some("git@example.com:board/probe.git"));
    let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "Probed");
    bind_pr(&state, &chore.id, None);
    let sink = make_session_sink();
    handle_move_work_item_on_board(dispatch(&state, &sink), drop_on(&chore.id, BoardColumn::Doing)).await;
    match sole_response(&sink).await {
        FrontendEvent::WorkItemRefused {
            code,
            message,
            suggested_action,
            ..
        } => (suggested_action, code, message),
        other => panic!("expected WorkItemRefused, got {other:?}"),
    }
}

/// A closed-unmerged PR cannot take a revision, so no revision is offered.
#[tokio::test]
async fn drag_of_item_with_closed_unmerged_pr_offers_no_revision() {
    let (action, code, message) = bound_pr_refusal(crate::work::PrOpenState::ClosedUnmerged).await;
    assert_eq!(code, BoardRefusalCode::BoundPrNotRevisable);
    assert_eq!(action, None);
    assert!(
        message.contains("#3170") && message.contains("closed"),
        "got: {message}"
    );
    assert!(!message.contains("add a revision"), "got: {message}");
}

/// A merged PR whose card has not reached Done yet (cached status is not
/// Done) is still not revisable: the live probe is what says so.
#[tokio::test]
async fn drag_of_merged_pr_item_not_yet_done_offers_no_revision() {
    let (action, code, message) = bound_pr_refusal(crate::work::PrOpenState::Merged).await;
    assert_eq!(code, BoardRefusalCode::BoundPrNotRevisable);
    assert_eq!(action, None);
    assert!(message.contains("merged"), "got: {message}");
    assert!(!message.contains("add a revision"), "got: {message}");
}

/// An open PR is the one case that offers the revision action.
#[tokio::test]
async fn drag_of_item_with_open_pr_offers_add_revision() {
    let (action, code, _) = bound_pr_refusal(crate::work::PrOpenState::Open).await;
    assert_eq!(code, BoardRefusalCode::BoundPr);
    assert_eq!(action, Some(BoardRefusalAction::AddRevision));
}

/// An explicit-status board request is refused with the same structured
/// result as a drop, not a bare `WorkError`.
#[tokio::test]
async fn refused_explicit_status_board_request_returns_work_item_refused() {
    let (state, _dir) = state_with_pr_state(crate::work::PrOpenState::Open);
    let product = create_test_product_with_repo(
        &state.work_db,
        "ExplicitRefused",
        Some("git@example.com:board/explicit.git"),
    );
    let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "Has a PR");
    bind_pr(&state, &chore.id, None);

    let sink = make_session_sink();
    let req = FrontendRequest::MoveWorkItemOnBoard {
        id: chore.id.clone(),
        target: BoardDropTarget::new(BoardColumn::Backlog, None),
        bypass_dispatch_pause: false,
        observed_pause_since_epoch_s: None,
        explicit_status: Some(TaskStatus::Active),
    };
    handle_move_work_item_on_board(dispatch(&state, &sink), req).await;

    match sole_response(&sink).await {
        FrontendEvent::WorkItemRefused { id, code, .. } => {
            assert_eq!(id, chore.id);
            assert_eq!(code, BoardRefusalCode::BoundPr);
        }
        other => panic!("expected WorkItemRefused, got {other:?}"),
    }
}

/// A blocked card dropped on its own column is a reorder, but an explicit
/// status choice still unblocks it.
#[tokio::test]
async fn explicit_status_unblocks_a_blocked_card_in_its_own_column() {
    let (state, _dir) = test_server_state_with_fakes();
    let product = create_test_product_with_repo(
        &state.work_db,
        "ExplicitUnblock",
        Some("git@example.com:board/unblock.git"),
    );
    let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "Stuck");
    state
        .work_db
        .update_work_item(&chore.id, WorkItemPatch::builder().status("blocked").build())
        .expect("block");

    let sink = make_session_sink();
    let req = FrontendRequest::MoveWorkItemOnBoard {
        id: chore.id.clone(),
        target: BoardDropTarget::new(BoardColumn::Backlog, None),
        bypass_dispatch_pause: false,
        observed_pause_since_epoch_s: None,
        explicit_status: Some(TaskStatus::Todo),
    };
    handle_move_work_item_on_board(dispatch(&state, &sink), req).await;

    match sole_response(&sink).await {
        FrontendEvent::WorkItemUpdated {
            item: WorkItem::Task(t) | WorkItem::Chore(t),
        } => assert_eq!(t.status, TaskStatus::Todo),
        other => panic!("expected WorkItemUpdated, got {other:?}"),
    }
}

/// A rejected Move Up/Down reorder (the ordering names a task that is not in
/// the project) is a coded refusal on the moved card; the CLI form (no moved
/// card) stays a plain `WorkError`.
#[tokio::test]
async fn rejected_reorder_is_refused_with_code_on_the_moved_card() {
    let (state, _dir) = test_server_state_with_fakes();
    let product = create_test_product_with_repo(
        &state.work_db,
        "ReorderRefused",
        Some("git@example.com:board/reorder.git"),
    );
    let project = state
        .work_db
        .create_project(boss_protocol::CreateProjectInput {
            product_id: product.id.clone(),
            name: "P".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .expect("project");

    let reorder = |moved_task_id: Option<String>| FrontendRequest::ReorderProjectTasks {
        project_id: project.id.clone(),
        task_ids: vec!["task_not_in_project".to_owned()],
        moved_task_id,
    };

    let sink = make_session_sink();
    handle_reorder_project_tasks(dispatch(&state, &sink), reorder(Some("task_moved".to_owned()))).await;
    match sole_response(&sink).await {
        FrontendEvent::WorkItemRefused { id, code, message, .. } => {
            assert_eq!(id, "task_moved");
            assert_eq!(code, BoardRefusalCode::ReorderRejected);
            assert!(!message.contains("task_not_in_project"), "got: {message}");
        }
        other => panic!("expected WorkItemRefused, got {other:?}"),
    }

    let sink = make_session_sink();
    handle_reorder_project_tasks(dispatch(&state, &sink), reorder(None)).await;
    match sole_response(&sink).await {
        FrontendEvent::WorkError { .. } => {}
        other => panic!("expected WorkError for a non-board caller, got {other:?}"),
    }
}
