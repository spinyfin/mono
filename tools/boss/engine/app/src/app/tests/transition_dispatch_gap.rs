//! Regression: a task moved to `active` must be started, or queued for a
//! worker, by the same request that reports it dispatched.
//!
//! The move mints a `ready` execution, emits `status_transition` with
//! `did_dispatch: true`, and then reconciles the product. For a later task of
//! a project that reconcile applied the project's ordinal chain to the row it
//! had just minted and parked it in `waiting_dependency`, where no scheduler
//! pass looks. No `request_recorded` followed; the card read "Queued — reason
//! unknown" until the orphan sweep found an active item with nothing live and
//! promoted the row, 90 s later.
//!
//! A card moved to Doing on the board is launched by the request itself, so
//! its row is claimed before that reconcile runs. A status edit to `active`
//! (the CLI, an agent) still queues the row and leaves it to the scheduler,
//! which is where the row has to survive the reconcile.

use super::*;
use crate::app::work_items::{handle_move_work_item_on_board, handle_update_work_item};
use boss_engine_dispatch::coordinator::DispatchPauseOrigin;
use boss_engine_runtime::test_support::{create_test_chore_manual, create_test_product_with_repo};
use boss_protocol::{
    AddDependencyInput, BoardColumn, BoardDropTarget, CreateProjectInput, CreateTaskInput, ExecutionStatus,
    FrontendRequest, PauseReason, Task, TaskKind, TaskStatus, WorkItem, WorkItemPatch,
};

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-transition-dispatch-gap")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

/// A project whose design is done, whose first task is held by a
/// prerequisite nobody has started, and whose second task waits for a person
/// to move it. The first task stays the head of the project's ordinal chain
/// for the whole test, which is what made the second one's execution the
/// chain's to demote.
fn seed_project(state: &Arc<ServerState>) -> (Task, Task) {
    let db = &state.work_db;
    let product = create_test_product_with_repo(
        db,
        "TransitionDispatchGap",
        Some("git@example.com:transition/dispatch-gap.git"),
    );
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Ordered project".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .expect("create project");
    let design = db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .expect("list project tasks")
        .into_iter()
        .find(|task| task.kind == TaskKind::Design)
        .expect("a project is created with a design task");
    db.update_work_item(
        &design.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..Default::default()
        },
    )
    .expect("complete the design task");
    let task = |name: &str, autostart: bool| {
        db.create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name(name)
                .autostart(autostart)
                .build(),
        )
        .expect("create project task")
    };
    let first = task("First", true);
    let second = task("Second", false);
    let prerequisite = create_test_chore_manual(db, product.id.clone(), "Not started yet");
    db.add_dependency(AddDependencyInput {
        dependent: first.id.clone(),
        prerequisite: prerequisite.id,
        relation: None,
    })
    .expect("gate the first task");
    db.reconcile_product_executions(&product.id).expect("reconcile");
    (first, second)
}

/// How a task is moved to `active`.
enum Move {
    /// The card is dropped on Doing: the owner's explicit start.
    BoardDrop,
    /// A status edit, as the CLI sends it: an ordinary queued start.
    StatusEdit,
}

/// Move `task` to `active` and return the execution its `status_transition`
/// event reports as dispatched.
async fn move_to_active(state: &Arc<ServerState>, task: &Task, how: Move) -> String {
    let sink = make_session_sink();
    match how {
        Move::BoardDrop => {
            handle_move_work_item_on_board(
                dispatch(state, &sink),
                FrontendRequest::MoveWorkItemOnBoard {
                    id: task.id.clone(),
                    target: BoardDropTarget::new(BoardColumn::Doing, None),
                    bypass_dispatch_pause: false,
                    observed_pause_since_epoch_s: None,
                    explicit_status: None,
                },
            )
            .await
        }
        Move::StatusEdit => {
            handle_update_work_item(
                dispatch(state, &sink),
                FrontendRequest::UpdateWorkItem {
                    id: task.id.clone(),
                    patch: Box::new(WorkItemPatch::builder().status("active").build()),
                },
            )
            .await
        }
    }
    sink.close();
    match sink.next().await.expect("handler must respond").payload {
        FrontendEvent::WorkItemUpdated { item } => {
            let (WorkItem::Task(moved) | WorkItem::Chore(moved)) = item else {
                panic!("expected a task row in the move response");
            };
            assert_eq!(moved.status, TaskStatus::Active);
        }
        other => panic!("expected WorkItemUpdated, got: {other:?}"),
    }
    let transition = dispatch_events(state)
        .into_iter()
        .find(|event| event.stage == "status_transition" && event.work_item_id.as_deref() == Some(task.id.as_str()))
        .expect("the move emits a status_transition");
    assert_eq!(transition.details["did_dispatch"], serde_json::Value::Bool(true));
    transition.details["dispatched_execution_id"]
        .as_str()
        .expect("the transition names the execution it dispatched")
        .to_owned()
}

fn dispatch_events(state: &ServerState) -> Vec<boss_dispatch_events::DispatchEvent> {
    boss_dispatch_reader::read_current(&state.dispatch_event_root)
        .expect("read dispatch events")
        .events
}

/// Wait for `stage` to be recorded for `execution_id`: `request_recorded`
/// when the scheduler picked a queued row up, `run_started` when a launch
/// started its worker. No sweep runs in these tests, so only the request
/// under test and the scheduler's own pass can produce either.
async fn assert_reached_without_a_sweep(state: &ServerState, execution_id: &str, stage: &str) {
    let mut requested = false;
    for _ in 0..100 {
        requested = dispatch_events(state)
            .iter()
            .any(|event| event.stage == stage && event.execution_id == execution_id);
        if requested {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let events = dispatch_events(state);
    assert!(
        requested,
        "no {stage} for {execution_id} (status {:?}); stages seen: {:?}",
        state
            .work_db
            .get_execution(execution_id)
            .map(|execution| execution.status),
        events.iter().map(|event| event.stage.as_str()).collect::<Vec<_>>()
    );
    assert!(
        events.iter().all(|event| event.stage != "orphan_active_redispatch"),
        "the request must come from the move itself, not from an orphan redispatch"
    );
}

fn assert_preconditions(state: &ServerState, first: &Task, second: &Task) {
    let db = &state.work_db;
    assert_eq!(
        db.list_executions(Some(&first.id)).expect("list executions")[0].status,
        ExecutionStatus::WaitingDependency,
        "precondition: the first task is the chain's head and is not dispatchable"
    );
    assert!(
        db.list_executions(Some(&second.id))
            .expect("list executions")
            .is_empty(),
        "precondition: nothing is queued for the second task until it is moved"
    );
}

/// A card dropped on Doing on an idle engine: the request launches the row
/// itself, so its worker starts with no scheduler pass and no sweep.
#[tokio::test]
async fn a_later_project_task_moved_to_doing_is_started_without_a_sweep() {
    let (server_state, _dir) = test_server_state_with_fakes();
    let (first, second) = seed_project(&server_state);
    assert_preconditions(&server_state, &first, &second);

    let execution_id = move_to_active(&server_state, &second, Move::BoardDrop).await;

    assert_ne!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .expect("get execution")
            .status,
        ExecutionStatus::WaitingDependency,
        "the execution reported dispatched was parked off the ready queue by the same request"
    );
    assert_reached_without_a_sweep(&server_state, &execution_id, "run_started").await;
}

/// A status edit to `active` on an idle engine: the kick it issues reaches
/// the scheduler, which records the request.
#[tokio::test]
async fn a_later_project_task_set_active_is_requested_without_a_sweep() {
    let (server_state, _dir) = test_server_state_with_fakes();
    let (first, second) = seed_project(&server_state);
    assert_preconditions(&server_state, &first, &second);

    let execution_id = move_to_active(&server_state, &second, Move::StatusEdit).await;

    assert_ne!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .expect("get execution")
            .status,
        ExecutionStatus::WaitingDependency,
        "the execution reported dispatched was parked off the ready queue by the same request"
    );
    assert_reached_without_a_sweep(&server_state, &execution_id, "request_recorded").await;
}

/// The status edit while the scheduler cannot take the row before the request
/// finishes. On an idle test engine the scheduler's claim usually lands
/// before the handler's reconcile, which hides the defect; an operator pause
/// holds the row `ready` across the whole request, the ordering the incident
/// had. The row must survive the reconcile as a queued request, name the
/// pause as its reason, and be picked up by the scheduler's first pass after
/// the pause lifts.
///
/// A card dropped on Doing under the same pause is not queued at all: the
/// app asks before overriding a pause, and an unconfirmed drop is refused
/// (see `board_drag_launch`).
#[tokio::test]
async fn a_task_set_active_while_the_scheduler_is_held_stays_queued() {
    let (server_state, _dir) = test_server_state_with_fakes();
    let (first, second) = seed_project(&server_state);
    assert_preconditions(&server_state, &first, &second);
    server_state.execution_coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: hold the scheduler across the move").unwrap(),
    );

    let execution_id = move_to_active(&server_state, &second, Move::StatusEdit).await;

    let db = &server_state.work_db;
    assert_eq!(
        db.get_execution(&execution_id).expect("get execution").status,
        ExecutionStatus::Ready,
        "the execution reported dispatched must still be a queued request after the request that minted it"
    );
    // Let the kick's pass see the row and record what is holding it.
    let mut reason = None;
    for _ in 0..100 {
        reason = db
            .get_task_runtime(&second.id)
            .expect("task runtime")
            .dispatch_wait_reason;
        if reason.as_deref() == Some("dispatch_paused") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(reason.as_deref(), Some("dispatch_paused"));
    assert!(
        dispatch_events(&server_state)
            .iter()
            .all(|event| event.stage != "request_recorded" || event.execution_id != execution_id),
        "a paused engine must not have picked the row up"
    );

    server_state.execution_coordinator.resume_dispatch();
    server_state.execution_coordinator.kick();
    assert_reached_without_a_sweep(&server_state, &execution_id, "request_recorded").await;
}
