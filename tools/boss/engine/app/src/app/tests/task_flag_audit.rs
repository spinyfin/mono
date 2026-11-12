//! Socket-driven writes of a task's `deferred` / `autostart` flags land in the
//! field-change ledger with the resolved actor and a basis naming the handler
//! and the calling peer, through the real request handlers.

use super::*;
use crate::app::{planner_ops, work_items};
use boss_engine_runtime::test_support::{create_test_chore, create_test_chore_manual, create_test_product_named};
use boss_protocol::{
    BoardColumn, BoardDropTarget, CreateProjectInput, CreateTaskInput, FrontendRequest, TaskFieldAuditRow,
    WorkItemPatch,
};

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>, request_id: &str) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-flag-audit")
        .request_id(request_id)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

/// The rows the ledger holds for `property`, as `(old, new, actor, basis)`.
fn history(state: &ServerState, id: &str, property: &str) -> Vec<(Option<String>, Option<String>, String, String)> {
    state
        .work_db
        .list_task_field_audit(id)
        .unwrap()
        .into_iter()
        .filter(|row: &TaskFieldAuditRow| row.property == property)
        .map(|row| (row.old_value, row.new_value, row.actor, row.basis))
        .collect()
}

fn socket_basis(source: &str, request_id: &str) -> String {
    format!("{source} peer=socket_client pid=unknown session=session-flag-audit request={request_id}")
}

#[tokio::test]
async fn update_work_item_records_who_deferred_and_who_cleared_it() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag RPC");
    let chore = create_test_chore_manual(&state.work_db, product.id, "Deferred over the socket");
    let sink = make_session_sink();

    for (request_id, patch) in [
        (
            "req-defer",
            WorkItemPatch {
                deferred: Some(true),
                ..Default::default()
            },
        ),
        (
            "req-approve",
            WorkItemPatch {
                deferred: Some(false),
                autostart: Some(true),
                ..Default::default()
            },
        ),
    ] {
        work_items::handle_update_work_item(
            dispatch(&state, &sink, request_id),
            FrontendRequest::UpdateWorkItem {
                id: chore.id.clone(),
                patch: Box::new(patch),
            },
        )
        .await;
    }

    let defer = socket_basis("rpc:update_work_item", "req-defer");
    let approve = socket_basis("rpc:update_work_item", "req-approve");
    let created = "insert_chore_with_project_in_tx_as_actor";
    assert_eq!(
        history(&state, &chore.id, "deferred"),
        [
            (None, Some("0".into()), "engine".into(), created.into()),
            (Some("0".into()), Some("1".into()), "human".into(), defer),
            (Some("1".into()), Some("0".into()), "human".into(), approve.clone()),
        ]
    );
    assert_eq!(
        history(&state, &chore.id, "autostart").last(),
        Some(&(Some("0".into()), Some("1".into()), "human".into(), approve))
    );
}

#[tokio::test]
async fn dropping_a_queued_card_into_backlog_records_the_board_gesture() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag drop");
    let chore = create_test_chore(&state.work_db, product.id, "Queued card");
    let sink = make_session_sink();

    work_items::handle_move_work_item_on_board(
        dispatch(&state, &sink, "req-drop"),
        FrontendRequest::MoveWorkItemOnBoard {
            id: chore.id.clone(),
            target: BoardDropTarget::new(BoardColumn::Backlog, None),
            bypass_dispatch_pause: false,
            observed_pause_since_epoch_s: None,
        },
    )
    .await;

    assert_eq!(
        history(&state, &chore.id, "autostart").last(),
        Some(&(
            Some("1".into()),
            Some("0".into()),
            "human".into(),
            socket_basis("board_drop:clear_autostart", "req-drop")
        ))
    );
}

#[tokio::test]
async fn releasing_a_staged_project_records_the_release_for_each_task() {
    let (state, _dir) = test_server_state();
    let db = &state.work_db;
    let product = create_test_product_named(db, "Flag release");
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("Staged")
                .goal("Release it")
                .build(),
        )
        .unwrap();
    let run = db
        .claim_planner_run(boss_work_db::ClaimPlannerRunInput {
            project_id: &project.id,
            product_id: &product.id,
            design_task_id: None,
            caller: "operator",
        })
        .unwrap()
        .expect("claim planner run");
    db.update_planner_run(
        &run.id,
        boss_work_db::PlannerRunPatch::builder()
            .outcome(boss_protocol::PLANNER_OUTCOME_STAGED)
            .build(),
    )
    .unwrap();
    let staged = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Staged task")
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET planner_run_id = ?2 WHERE id = ?1",
            [&staged.id, &run.id],
        )
        .unwrap();
    let sink = make_session_sink();

    planner_ops::handle_release_project(
        dispatch(&state, &sink, "req-release"),
        FrontendRequest::ReleaseProject {
            project_id: project.id.clone(),
        },
    )
    .await;

    assert_eq!(
        history(&state, &staged.id, "autostart").last(),
        Some(&(
            Some("0".into()),
            Some("1".into()),
            "human".into(),
            socket_basis("rpc:release_project", "req-release")
        ))
    );
}

#[tokio::test]
async fn list_task_field_audit_replies_with_the_flag_rows() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag list");
    let chore = create_test_chore_manual(&state.work_db, product.id, "Listed");
    let sink = make_session_sink();

    work_items::handle_list_task_field_audit(
        dispatch(&state, &sink, "req-list"),
        FrontendRequest::ListTaskFieldAudit { id: chore.id.clone() },
    )
    .await;

    sink.close();
    match sink.next().await.expect("handler must reply").payload {
        FrontendEvent::TaskFieldAuditList { work_item_id, rows } => {
            assert_eq!(work_item_id, chore.id);
            let properties: Vec<&str> = rows.iter().map(|row| row.property.as_str()).collect();
            assert!(
                properties.contains(&"deferred") && properties.contains(&"autostart"),
                "{properties:?}"
            );
        }
        other => panic!("expected TaskFieldAuditList, got {other:?}"),
    }
}
