//! Socket-driven writes of a task's `deferred` / `autostart` flags land in the
//! field-change ledger with the resolved actor and a basis naming the handler
//! and the calling peer, through the real request handlers.

use super::*;
use crate::app::{attentions, executions, ideas, planner_ops, projects, work_items};
use boss_engine_runtime::test_support::{
    create_ready_chore_execution, create_test_chore, create_test_chore_manual, create_test_product_named,
    create_test_product_with_repo, finish_run_worker_pane_alive,
};
use boss_protocol::{
    BoardColumn, BoardDropTarget, CreateAttentionInput, CreateChoreInput, CreateIdeaInput, CreateInvestigationInput,
    CreateProjectInput, CreateTaskInput, FrontendRequest, IdeaGraduationKind, OperatorAnswer, OperatorAnswerType,
    OperatorQuestion, RequestExecutionInput, TaskFieldAuditRow, TaskKind, WorkItemPatch,
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

fn park_work_item_with_question(
    state: &ServerState,
    work_item_id: String,
    question: OperatorQuestion,
) -> (String, String) {
    let execution = create_ready_chore_execution(&state.work_db, work_item_id.clone());
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
    boss_work_db::insert_question(&state.work_db.connect().unwrap(), &execution.id, "proposal", &question).unwrap();
    state
        .work_db
        .record_worker_awaiting_operator_answer(&execution.id, "needs a decision", &question)
        .unwrap();
    let question_id: String = state
        .work_db
        .connect()
        .unwrap()
        .query_row(
            "SELECT id FROM operator_questions WHERE work_item_id = ?1 ORDER BY created_at DESC LIMIT 1",
            [&work_item_id],
            |row| row.get(0),
        )
        .unwrap();
    (work_item_id, question_id)
}

fn park_with_question(
    state: &ServerState,
    product_id: String,
    name: &str,
    question: OperatorQuestion,
) -> (String, String) {
    let chore = create_test_chore(&state.work_db, product_id, name);
    park_work_item_with_question(state, chore.id, question)
}

fn linked_prerequisite_id(state: &ServerState, dependent_id: &str) -> String {
    state
        .work_db
        .connect()
        .unwrap()
        .query_row(
            "SELECT prerequisite_id FROM work_item_dependencies WHERE dependent_id = ?1",
            [dependent_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn prerequisite_create_question(name: &str) -> OperatorQuestion {
    OperatorQuestion {
        text: "Create the missing work?".into(),
        explanation: "This cannot proceed without it.".into(),
        answer_type: OperatorAnswerType::CreatePrerequisiteTask {
            name: name.into(),
            brief: "Fix the shared fixture first.".into(),
            file_count_bypass: None,
            option_labels: None,
        },
    }
}

async fn answer_over_socket(state: &Arc<ServerState>, work_item_id: &str, request_id: &str, answer: OperatorAnswer) {
    let sink = make_session_sink();
    work_items::handle_operator_question(
        dispatch(state, &sink, request_id),
        FrontendRequest::AnswerOperatorQuestion {
            id: work_item_id.to_owned(),
            answer,
        },
    )
    .await;
}

#[tokio::test]
async fn answering_a_ladder_question_records_the_socket_caller_on_autostart_restore() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag ladder answer");
    let (chore_id, question_id) = park_with_question(
        &state,
        product.id,
        "Ladder restore",
        OperatorQuestion {
            text: boss_work_db::QUESTION_TEXT.into(),
            explanation: "Progression spent its attempts.".into(),
            answer_type: OperatorAnswerType::MultipleChoice {
                choices: vec![
                    boss_work_db::PARK_IT.into(),
                    boss_work_db::ARCHIVE_IT.into(),
                    boss_work_db::KEEP_TRYING.into(),
                ],
            },
        },
    );
    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "INSERT INTO progression_ladder (work_item_id, question_id, question_reason, question_filed_at, prior_state_json)
             VALUES (?1, ?2, 'ladder_exhausted', 1, ?3)",
            [
                &chore_id,
                &question_id,
                r#"{"status":"todo","blocked_reason":null,"blocked_detail":null,"blocked_attempt_id":null,"autostart":true,"dispatch_failed_reason":null,"dispatch_failed_error":null,"dispatch_failed_at":null,"last_status_actor":"engine","basis":null}"#,
            ],
        )
        .unwrap();
    answer_over_socket(
        &state,
        &chore_id,
        "req-ladder",
        OperatorAnswer::MultipleChoice {
            value: boss_work_db::KEEP_TRYING.into(),
        },
    )
    .await;
    let answer_basis = socket_basis("rpc:answer", "req-ladder");
    assert_eq!(
        history(&state, &chore_id, "autostart").last(),
        Some(&(
            Some("0".into()),
            Some("1".into()),
            "human".into(),
            format!("progression_ladder_restore_autostart {answer_basis}")
        ))
    );
}

#[tokio::test]
async fn answering_a_prerequisite_question_records_the_socket_caller() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag prereq answer");
    let existing = create_test_chore_manual(&state.work_db, product.id.clone(), "Shared fixture");
    let (dependent_id, _) = park_with_question(
        &state,
        product.id,
        "Needs the fixture",
        prerequisite_create_question("Shared fixture"),
    );
    answer_over_socket(
        &state,
        &dependent_id,
        "req-prereq",
        OperatorAnswer::YesNo { value: true },
    )
    .await;
    let answer_basis = socket_basis("rpc:answer", "req-prereq");
    let expected = (
        Some("0".into()),
        Some("1".into()),
        "human".into(),
        format!("approve_in_tx {answer_basis}"),
    );
    assert_eq!(history(&state, &dependent_id, "autostart").last(), Some(&expected));
    assert_eq!(history(&state, &existing.id, "autostart").last(), Some(&expected));
}

fn assert_created_prerequisite_flag_rows(
    state: &ServerState,
    prerequisite_id: &str,
    insert_helper: &str,
    request_id: &str,
) {
    let answer_basis = socket_basis("rpc:answer", request_id);
    let expected_actor = "human".to_owned();
    let expected_basis = format!("{insert_helper} {answer_basis}");
    assert_eq!(
        history(state, prerequisite_id, "autostart"),
        [(None, Some("1".into()), expected_actor.clone(), expected_basis.clone())]
    );
    assert_eq!(
        history(state, prerequisite_id, "deferred"),
        [(None, Some("0".into()), expected_actor, expected_basis)]
    );
}

#[tokio::test]
async fn answering_a_prerequisite_question_records_the_socket_caller_when_creating_a_chore() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag prereq create chore");
    let (dependent_id, _) = park_with_question(
        &state,
        product.id,
        "Needs a new chore",
        prerequisite_create_question("New shared chore"),
    );
    answer_over_socket(
        &state,
        &dependent_id,
        "req-prereq-chore",
        OperatorAnswer::YesNo { value: true },
    )
    .await;
    let created_id = linked_prerequisite_id(&state, &dependent_id);
    assert_ne!(created_id, dependent_id);
    assert_created_prerequisite_flag_rows(
        &state,
        &created_id,
        "insert_chore_with_project_in_tx_as_actor",
        "req-prereq-chore",
    );
}

#[tokio::test]
async fn answering_a_prerequisite_question_records_the_socket_caller_when_creating_a_project_task() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag prereq create task");
    let project = state
        .work_db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("Flag prereq project")
                .goal("Hold the dependent")
                .build(),
        )
        .unwrap();
    let dependent = state
        .work_db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id)
                .project_id(project.id)
                .name("Needs a new project task")
                .build(),
        )
        .unwrap();
    let (dependent_id, _) = park_work_item_with_question(
        &state,
        dependent.id,
        prerequisite_create_question("New shared project task"),
    );
    answer_over_socket(
        &state,
        &dependent_id,
        "req-prereq-task",
        OperatorAnswer::YesNo { value: true },
    )
    .await;
    let created_id = linked_prerequisite_id(&state, &dependent_id);
    assert_ne!(created_id, dependent_id);
    assert_created_prerequisite_flag_rows(&state, &created_id, "insert_task_in_tx_as_actor", "req-prereq-task");
}

#[tokio::test]
async fn answering_yes_on_a_deferred_task_records_the_socket_caller_clearing_deferred() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag deferred answer");
    let (chore_id, _) = park_with_question(
        &state,
        product.id,
        "Deferred until answered",
        OperatorQuestion {
            text: "Ship the change?".into(),
            explanation: "Need a decision.".into(),
            answer_type: OperatorAnswerType::YesNo,
        },
    );
    state
        .work_db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    answer_over_socket(&state, &chore_id, "req-deferred", OperatorAnswer::YesNo { value: true }).await;
    let answer_basis = socket_basis("rpc:answer", "req-deferred");
    assert_eq!(
        history(&state, &chore_id, "deferred").last(),
        Some(&(
            Some("1".into()),
            Some("0".into()),
            "human".into(),
            format!("explicit_start_clears_deferred {answer_basis}")
        ))
    );
}

fn seed_design_task_id(state: &ServerState, product_id: &str, project_id: &str) -> String {
    state
        .work_db
        .list_tasks(product_id, Some(project_id), None, false)
        .unwrap()
        .into_iter()
        .find(|task| task.kind == TaskKind::Design)
        .expect("seed design task")
        .id
}

#[tokio::test]
async fn creating_a_project_without_autostart_records_the_socket_caller() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag create project");
    let sink = make_session_sink();
    projects::handle_create_project(
        dispatch(&state, &sink, "req-create-project"),
        FrontendRequest::CreateProject {
            input: CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("Staged")
                .autostart(false)
                .build(),
        },
    )
    .await;
    sink.close();
    let project_id = match sink.next().await.expect("handler must reply").payload {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Project(project),
        } => project.id,
        other => panic!("expected WorkItemCreated project, got {other:?}"),
    };
    let design_id = seed_design_task_id(&state, &product.id, &project_id);
    let basis = socket_basis("rpc:create_project", "req-create-project");
    assert_eq!(
        history(&state, &design_id, "autostart"),
        [(
            None,
            Some("0".into()),
            "human".into(),
            format!("insert_design_task_for_project_in_tx {basis}")
        )]
    );
}

#[tokio::test]
async fn graduating_an_idea_to_a_chore_records_the_socket_caller() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag graduate chore");
    let idea = state
        .work_db
        .create_idea(
            CreateIdeaInput::builder()
                .product_id(product.id.clone())
                .name("Ship it")
                .body("do the thing")
                .build(),
        )
        .unwrap();
    let sink = make_session_sink();
    ideas::handle_graduate_idea(
        dispatch(&state, &sink, "req-graduate-chore"),
        FrontendRequest::GraduateIdea {
            id: idea.id,
            target: IdeaGraduationKind::Chore,
            name: None,
            effort_level: None,
            reasoning: None,
        },
    )
    .await;
    sink.close();
    let chore_id = match sink.next().await.expect("handler must reply").payload {
        FrontendEvent::IdeaGraduated { chore: Some(chore), .. } => chore.id,
        other => panic!("expected IdeaGraduated chore, got {other:?}"),
    };
    let basis = socket_basis("rpc:graduate_idea", "req-graduate-chore");
    assert_eq!(
        history(&state, &chore_id, "autostart"),
        [(
            None,
            Some("1".into()),
            "human".into(),
            format!("insert_chore_with_project_in_tx_as_actor {basis}")
        )]
    );
}

#[tokio::test]
async fn graduating_an_idea_to_a_project_records_the_socket_caller() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag graduate project");
    let idea = state
        .work_db
        .create_idea(
            CreateIdeaInput::builder()
                .product_id(product.id.clone())
                .name("Ideas layer")
                .body("# Proposal")
                .build(),
        )
        .unwrap();
    let sink = make_session_sink();
    ideas::handle_graduate_idea(
        dispatch(&state, &sink, "req-graduate-project"),
        FrontendRequest::GraduateIdea {
            id: idea.id,
            target: IdeaGraduationKind::Project,
            name: None,
            effort_level: None,
            reasoning: None,
        },
    )
    .await;
    sink.close();
    let project_id = match sink.next().await.expect("handler must reply").payload {
        FrontendEvent::IdeaGraduated {
            project: Some(project), ..
        } => project.id,
        other => panic!("expected IdeaGraduated project, got {other:?}"),
    };
    let design_id = seed_design_task_id(&state, &product.id, &project_id);
    let basis = socket_basis("rpc:graduate_idea", "req-graduate-project");
    assert_eq!(
        history(&state, &design_id, "autostart"),
        [(
            None,
            Some("0".into()),
            "human".into(),
            format!("insert_design_task_for_project_in_tx {basis}")
        )]
    );
}

#[tokio::test]
async fn creating_an_investigation_records_the_socket_caller() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag create investigation");
    let sink = make_session_sink();
    work_items::handle_create_investigation(
        dispatch(&state, &sink, "req-create-investigation"),
        FrontendRequest::CreateInvestigation {
            input: CreateInvestigationInput::builder()
                .product_id(product.id.clone())
                .name("Look into it")
                .autostart(false)
                .build(),
        },
    )
    .await;
    sink.close();
    let investigation_id = match sink.next().await.expect("handler must reply").payload {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(task),
        } => task.id,
        other => panic!("expected WorkItemCreated investigation, got {other:?}"),
    };
    let basis = socket_basis("rpc:create_investigation", "req-create-investigation");
    assert_eq!(
        history(&state, &investigation_id, "autostart"),
        [(
            None,
            Some("0".into()),
            "human".into(),
            format!("insert_investigation_in_tx_as_actor {basis}")
        )]
    );
    assert_eq!(
        history(&state, &investigation_id, "deferred"),
        [(
            None,
            Some("0".into()),
            "human".into(),
            format!("insert_investigation_in_tx_as_actor {basis}")
        )]
    );
}

#[tokio::test]
async fn actioning_a_question_group_records_the_socket_caller_on_the_seeded_design() {
    let (state, _dir) = test_server_state();
    let product = create_test_product_named(&state.work_db, "Flag seeded design");
    let project = state
        .work_db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("Seeded")
                .goal("Hold a design")
                .build(),
        )
        .unwrap();
    let (attention, group) = state
        .work_db
        .create_attention(
            CreateAttentionInput::builder()
                .kind("question")
                .association_project_id(&project.id)
                .source_kind("design_doc")
                .source_doc_path("docs/seeded.md")
                .question_type("prompt")
                .prompt_text("keep the table?")
                .build(),
        )
        .unwrap();
    state
        .work_db
        .answer_attention(&attention.id, Some("yes".to_owned()), false, false)
        .unwrap();
    let sink = make_session_sink();
    attentions::handle_action_attention_group(
        dispatch(&state, &sink, "req-action-questions"),
        FrontendRequest::ActionAttentionGroup {
            id: group.id,
            skip_unanswered: false,
            member_overrides: Default::default(),
        },
    )
    .await;
    sink.close();
    match sink.next().await.expect("handler must reply").payload {
        FrontendEvent::AttentionGroupActioned { .. } => {}
        other => panic!("expected AttentionGroupActioned, got {other:?}"),
    }
    let seeded_id = state
        .work_db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap()
        .into_iter()
        .find(|task| task.kind == TaskKind::Design && task.created_via == "attention")
        .expect("seeded design")
        .id;
    let basis = socket_basis("rpc:action_attention_group", "req-action-questions");
    assert_eq!(
        history(&state, &seeded_id, "autostart"),
        [(
            None,
            Some("1".into()),
            "human".into(),
            format!("insert_seeded_design_task_in_tx {basis}")
        )]
    );
}
