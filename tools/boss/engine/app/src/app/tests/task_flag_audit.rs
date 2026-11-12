//! Socket-driven writes of a task's `deferred` / `autostart` flags land in the
//! field-change ledger with the resolved actor and a basis naming the handler
//! and the calling peer, through the real request handlers.

use super::*;
use crate::app::{executions, planner_ops, work_items};
use boss_engine_runtime::test_support::{
    create_ready_chore_execution, create_test_chore, create_test_chore_manual, create_test_product_named,
    create_test_product_with_repo, finish_run_worker_pane_alive,
};
use boss_protocol::{
    BoardColumn, BoardDropTarget, CreateChoreInput, CreateProjectInput, CreateTaskInput, FrontendRequest,
    OperatorAnswer, OperatorQuestion, RequestExecutionInput, TaskFieldAuditRow, WorkItemPatch,
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

#[tokio::test]
async fn creating_a_deferred_task_and_chore_records_the_socket_caller() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag create");
    let project = state
        .work_db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("Flag create project")
                .goal("Hold a task")
                .build(),
        )
        .unwrap();
    let sink = make_session_sink();

    work_items::handle_create_chore(
        dispatch(&state, &sink, "req-create-chore"),
        FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Deferred chore")
                .autostart(false)
                .deferred(true)
                .build(),
        },
    )
    .await;
    work_items::handle_create_task(
        dispatch(&state, &sink, "req-create-task"),
        FrontendRequest::CreateTask {
            input: CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Deferred task")
                .deferred(true)
                .build(),
        },
    )
    .await;

    sink.close();
    let mut created = Vec::new();
    while let Some(envelope) = sink.next().await {
        if let FrontendEvent::WorkItemCreated { item } = envelope.payload {
            created.push(item);
        }
    }
    assert_eq!(created.len(), 2, "{created:?}");
    let chore_id = created
        .iter()
        .find_map(|item| match item {
            WorkItem::Chore(task) => Some(task.id.clone()),
            _ => None,
        })
        .expect("chore");
    let task_id = created
        .iter()
        .find_map(|item| match item {
            WorkItem::Task(task) => Some(task.id.clone()),
            _ => None,
        })
        .expect("task");

    let chore_basis = socket_basis("rpc:create_chore", "req-create-chore");
    assert_eq!(
        history(&state, &chore_id, "deferred"),
        [(
            None,
            Some("1".into()),
            "human".into(),
            format!("insert_chore_with_project_in_tx_as_actor {chore_basis}")
        )]
    );
    let task_basis = socket_basis("rpc:create_task", "req-create-task");
    assert_eq!(
        history(&state, &task_id, "deferred"),
        [(
            None,
            Some("1".into()),
            "human".into(),
            format!("insert_task_in_tx_as_actor {task_basis}")
        )]
    );
}

#[tokio::test]
async fn request_execution_and_drag_to_doing_record_the_human_who_cleared_deferred() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_with_repo(&state.work_db, "Flag start", Some("git@example.com:flag/start.git"));
    let sink = make_session_sink();

    let started = create_test_chore_manual(&state.work_db, product.id.clone(), "Start me");
    state
        .work_db
        .update_work_item(
            &started.id,
            WorkItemPatch {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    executions::handle_request_execution(
        dispatch(&state, &sink, "req-start"),
        FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(started.id.clone())
                .build(),
        },
    )
    .await;
    let start_basis = socket_basis("rpc:request_execution", "req-start");
    assert_eq!(
        history(&state, &started.id, "deferred").last(),
        Some(&(
            Some("1".into()),
            Some("0".into()),
            "human".into(),
            format!("explicit_start_clears_deferred {start_basis}")
        ))
    );

    let dragged = create_test_chore_manual(&state.work_db, product.id.clone(), "Drag me");
    state
        .work_db
        .update_work_item(
            &dragged.id,
            WorkItemPatch {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    work_items::handle_move_work_item_on_board(
        dispatch(&state, &sink, "req-drag"),
        FrontendRequest::MoveWorkItemOnBoard {
            id: dragged.id.clone(),
            target: BoardDropTarget::new(BoardColumn::Doing, None),
            bypass_dispatch_pause: false,
            observed_pause_since_epoch_s: None,
        },
    )
    .await;
    let drag_basis = socket_basis("board_drop:request_execution", "req-drag");
    assert_eq!(
        history(&state, &dragged.id, "deferred").last(),
        Some(&(
            Some("1".into()),
            Some("0".into()),
            "human".into(),
            format!("explicit_start_clears_deferred {drag_basis}")
        ))
    );
}

#[tokio::test]
async fn answering_a_question_that_restarts_autostart_records_the_socket_caller() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag answer");
    let chore = create_test_chore(&state.work_db, product.id, "Needs an answer");
    let execution = create_ready_chore_execution(&state.work_db, chore.id.clone());
    let (execution, run) = state
        .work_db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/ws",
        )
        .unwrap();
    finish_run_worker_pane_alive(&state.work_db, &execution.id, &run.id, Some("spawned"));
    let question = OperatorQuestion {
        text: "Ship the change?".into(),
        explanation: "Need a decision.".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
    };
    boss_work_db::insert_question(&state.work_db.connect().unwrap(), &execution.id, "proposal", &question).unwrap();
    state
        .work_db
        .record_worker_awaiting_operator_answer(&execution.id, "needs a decision", &question)
        .unwrap();
    let sink = make_session_sink();
    work_items::handle_operator_question(
        dispatch(&state, &sink, "req-answer"),
        FrontendRequest::AnswerOperatorQuestion {
            id: chore.id.clone(),
            answer: OperatorAnswer::YesNo { value: true },
        },
    )
    .await;
    let answer_basis = socket_basis("rpc:answer", "req-answer");
    assert_eq!(
        history(&state, &chore.id, "autostart").last(),
        Some(&(
            Some("0".into()),
            Some("1".into()),
            "human".into(),
            format!("restart_after_answer {answer_basis}")
        ))
    );
}
