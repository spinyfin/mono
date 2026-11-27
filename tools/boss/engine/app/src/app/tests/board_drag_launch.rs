//! A card moved to Doing is the owner's explicit start gesture. It launches
//! the item the way `bossctl agents launch` does: now, past the focus set and
//! the interactive cap, with the launch recorded on the item. A start the
//! engine has to refuse reverts the move and says why.
//!
//! The fixture is a project task with `autostart` off whose earlier attempts
//! all failed: a row that nothing starts automatically, so the move is the
//! only thing that can.

use super::*;
use crate::app::work_items::{handle_move_work_item_on_board, handle_update_work_item};
use boss_engine_dispatch::coordinator::DispatchPauseOrigin;
use boss_engine_runtime::test_support::{create_test_product_with_repo, release_finished_execution_lease};
use boss_protocol::{
    BoardColumn, BoardDropTarget, BoardRefusalCode, CreateExecutionInput, CreateProjectInput, CreateTaskInput,
    DispatchFocus, DispatchFocusTargetKind, ExecutionKind, ExecutionStatus, FrontendRequest, PauseReason, Task,
    TaskStatus, WorkExecution, WorkItem, WorkItemPatch,
};

const TASK_NAME: &str = "Precompute panel figures";

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-board-drag-launch")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

struct Seeded {
    project_id: String,
    /// A second project in the same product, to focus on something else.
    other_project_id: String,
    task: Task,
    /// The executions the task had before the move, all terminal.
    prior: Vec<String>,
}

fn project(state: &ServerState, product_id: &str, name: &str) -> String {
    state
        .work_db
        .create_project(CreateProjectInput {
            product_id: product_id.to_owned(),
            name: name.to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .expect("create project")
        .id
}

fn project_task(state: &ServerState, product_id: &str, project_id: &str, name: &str) -> Task {
    state
        .work_db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product_id)
                .project_id(project_id)
                .name(name)
                .autostart(false)
                .build(),
        )
        .expect("create project task")
}

/// A project task with `autostart` off and four earlier attempts: one
/// orphaned, two that failed before starting, and a newest one that started
/// and then failed, which is what left the task `blocked: worker_failed` in
/// Backlog.
fn seed(state: &Arc<ServerState>) -> Seeded {
    let db = &state.work_db;
    let product = create_test_product_with_repo(db, "DragLaunch", Some("git@example.com:drag/launch.git"));
    let project_id = project(state, &product.id, "Focused project");
    let other_project_id = project(state, &product.id, "Another project");
    let task = project_task(state, &product.id, &project_id, TASK_NAME);
    let execution = |status: ExecutionStatus| {
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task.id.clone())
                .kind(ExecutionKind::TaskImplementation)
                .status(status)
                .build(),
        )
        .expect("create execution")
    };
    let mut prior = vec![
        execution(ExecutionStatus::Orphaned).id,
        execution(ExecutionStatus::Failed).id,
        execution(ExecutionStatus::Failed).id,
    ];
    let started = execution(ExecutionStatus::Ready);
    db.start_execution_run(&started.id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .expect("start the newest attempt");
    db.record_worker_failure(&started.id, "the worker gave up")
        .expect("fail the newest attempt");
    release_finished_execution_lease(db, &started.id);
    prior.push(started.id);

    let task = current_task(state, &task.id);
    assert_eq!(
        task.status,
        TaskStatus::Blocked,
        "precondition: the failed task is parked"
    );
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    assert!(!task.autostart, "precondition: autostart is off");
    assert!(
        executions(state, &task.id).iter().all(|e| e.status.is_terminal()),
        "precondition: every earlier attempt is finished"
    );
    Seeded {
        project_id,
        other_project_id,
        task,
        prior,
    }
}

fn current_task(state: &ServerState, id: &str) -> Task {
    match state.work_db.get_work_item(id).expect("get work item") {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

fn executions(state: &ServerState, task_id: &str) -> Vec<WorkExecution> {
    state.work_db.list_executions(Some(task_id)).expect("list executions")
}

fn focus_on(state: &ServerState, project_id: &str) -> DispatchFocus {
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Project, project_id.to_owned());
    state.work_db.save_dispatch_focus(&focus).expect("persist focus");
    state.execution_coordinator.restore_dispatch_focus(focus.clone());
    focus
}

/// Whether the owner's launch is recorded on `task_id`: a focus set that
/// names only an unrelated project admits the item exactly when it is.
fn launch_recorded(state: &ServerState, seeded: &Seeded, task_id: &str) -> bool {
    let mut elsewhere = DispatchFocus::default();
    elsewhere.insert(DispatchFocusTargetKind::Project, seeded.other_project_id.clone());
    state
        .work_db
        .work_item_in_dispatch_focus(task_id, &elsewhere)
        .expect("resolve focus scope")
}

async fn drop_on_doing(state: &Arc<ServerState>, task_id: &str) -> FrontendEvent {
    let sink = make_session_sink();
    handle_move_work_item_on_board(
        dispatch(state, &sink),
        FrontendRequest::MoveWorkItemOnBoard {
            id: task_id.to_owned(),
            target: BoardDropTarget::new(BoardColumn::Doing, None),
            bypass_dispatch_pause: false,
            observed_pause_since_epoch_s: None,
            explicit_status: None,
        },
    )
    .await;
    sink.close();
    let response = sink.next().await.expect("handler must respond").payload;
    assert!(sink.next().await.is_none(), "handler must send exactly one response");
    response
}

fn assert_moved_to_active(response: FrontendEvent) {
    match response {
        FrontendEvent::WorkItemUpdated {
            item: WorkItem::Task(task) | WorkItem::Chore(task),
        } => assert_eq!(task.status, TaskStatus::Active),
        other => panic!("expected the move to be accepted, got {other:?}"),
    }
}

/// The execution the move's `status_transition` event reports as dispatched.
fn dispatched_by_the_move(state: &ServerState, task_id: &str) -> String {
    let transition = boss_dispatch_reader::read_current(&state.dispatch_event_root)
        .expect("read dispatch events")
        .events
        .into_iter()
        .rfind(|event| event.stage == "status_transition" && event.work_item_id.as_deref() == Some(task_id))
        .expect("the move emits a status_transition");
    assert_eq!(
        transition.details["did_dispatch"],
        serde_json::Value::Bool(true),
        "the move must report a dispatch: {}",
        transition.details
    );
    transition.details["dispatched_execution_id"]
        .as_str()
        .expect("the transition names the execution it dispatched")
        .to_owned()
}

/// Wait for `execution_id`'s worker to start and return the execution. Only
/// the request under test can start it: these tests run no sweep and never
/// drain the queue themselves. The dispatch log is the evidence rather than
/// the row's status, which a fixture worker does not hold for long.
async fn wait_until_started(state: &ServerState, execution_id: &str) -> WorkExecution {
    let events = || {
        boss_dispatch_reader::read_current(&state.dispatch_event_root)
            .expect("read dispatch events")
            .events
    };
    for _ in 0..200 {
        if events()
            .iter()
            .any(|event| event.stage == "run_started" && event.execution_id == execution_id)
        {
            return state.work_db.get_execution(execution_id).expect("get execution");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let execution = state.work_db.get_execution(execution_id).expect("get execution");
    let stages: Vec<String> = events()
        .into_iter()
        .map(|event| format!("{} {} {}", event.stage, event.outcome, event.execution_id))
        .collect();
    panic!(
        "execution {execution_id} never started: status {:?}, wait reason {:?}, error {:?}; dispatch log: {stages:#?}",
        execution.status, execution.dispatch_wait_reason, execution.last_error
    );
}

/// Move the seeded task to Doing and assert a new implementation run started
/// for it, with the launch recorded.
async fn assert_move_starts_a_worker(state: &Arc<ServerState>, seeded: &Seeded) -> WorkExecution {
    assert_moved_to_active(drop_on_doing(state, &seeded.task.id).await);
    let execution_id = dispatched_by_the_move(state, &seeded.task.id);
    assert!(
        !seeded.prior.contains(&execution_id),
        "the move must start a new attempt, not report a finished one"
    );
    let execution = wait_until_started(state, &execution_id).await;
    assert_eq!(execution.kind, ExecutionKind::TaskImplementation);
    assert_eq!(current_task(state, &seeded.task.id).status, TaskStatus::Active);
    assert!(
        launch_recorded(state, seeded, &seeded.task.id),
        "the move must record the launch so the item's later work stays admitted under focus"
    );
    execution
}

#[tokio::test]
async fn moving_a_previously_failed_task_to_doing_starts_it_with_focus_off() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    assert!(!state.execution_coordinator.dispatch_focus().is_active());
    assert!(!launch_recorded(&state, &seeded, &seeded.task.id));

    assert_move_starts_a_worker(&state, &seeded).await;
}

#[tokio::test]
async fn moving_a_previously_failed_task_to_doing_starts_it_with_focus_on_its_project() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    focus_on(&state, &seeded.project_id);

    assert_move_starts_a_worker(&state, &seeded).await;
}

/// The move is a launch, so it is admitted past the focus set as well. Before
/// the move the item is out of scope; afterwards the recorded launch is what
/// keeps its review and repair work in scope.
#[tokio::test]
async fn moving_a_task_outside_the_focus_set_to_doing_starts_it() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    let focus = focus_on(&state, &seeded.other_project_id);
    assert!(
        !state
            .work_db
            .work_item_in_dispatch_focus(&seeded.task.id, &focus)
            .unwrap(),
        "precondition: focus holds this item"
    );

    let execution = assert_move_starts_a_worker(&state, &seeded).await;

    assert_ne!(
        execution.dispatch_wait_reason.as_deref(),
        Some(boss_protocol::DISPATCH_WAIT_REASON_FOCUS_MODE)
    );
    assert!(
        state
            .work_db
            .work_item_in_dispatch_focus(&seeded.task.id, &focus)
            .unwrap()
    );
}

/// A stuck item's progression run is keyed by the item itself, so while one
/// is queued it is the item's newest execution. It does not own the item: a
/// move that mistook it for the item's own run would flip the status to
/// `active`, request nothing, and report success.
#[tokio::test]
async fn moving_a_task_to_doing_starts_it_while_its_progression_run_is_queued() {
    for focused in [false, true] {
        let (state, _dir) = test_server_state_with_fakes();
        let seeded = seed(&state);
        if focused {
            focus_on(&state, &seeded.project_id);
        }
        let progression = state
            .work_db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(seeded.task.id.clone())
                    .kind(ExecutionKind::Progression)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .expect("queue a progression run for the stuck item");

        let execution = assert_move_starts_a_worker(&state, &seeded).await;

        assert_ne!(execution.id, progression.id, "focused={focused}");
    }
}

/// An ordinary status edit to `active` (the CLI, an agent) is not a launch,
/// but it must not be fooled by a queued progression run either: it queues
/// the item's own execution.
#[tokio::test]
async fn a_status_edit_to_active_queues_the_task_while_its_progression_run_is_queued() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    state
        .work_db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(seeded.task.id.clone())
                .kind(ExecutionKind::Progression)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .expect("queue a progression run for the stuck item");

    let sink = make_session_sink();
    handle_update_work_item(
        dispatch(&state, &sink),
        FrontendRequest::UpdateWorkItem {
            id: seeded.task.id.clone(),
            patch: Box::new(WorkItemPatch::builder().status("active").build()),
        },
    )
    .await;
    sink.close();
    assert_moved_to_active(sink.next().await.expect("handler must respond").payload);

    let execution_id = dispatched_by_the_move(&state, &seeded.task.id);
    let execution = state.work_db.get_execution(&execution_id).expect("get execution");
    assert_eq!(execution.kind, ExecutionKind::TaskImplementation);
    assert!(
        !launch_recorded(&state, &seeded, &seeded.task.id),
        "a status edit is not the owner's launch gesture"
    );
}

/// Like a launch, the move is not held by the interactive concurrency cap.
#[tokio::test]
async fn moving_a_task_to_doing_starts_it_past_the_interactive_cap() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    let coordinator = &state.execution_coordinator;
    let pool = coordinator.worker_pool();
    pool.claim_working("exec-occupant", None)
        .await
        .expect("occupy a slot with a live worker");
    coordinator
        .set_max_concurrent_interactive_workers(1)
        .expect("set the interactive cap");
    assert!(
        pool.occupancy().await.working >= coordinator.max_concurrent_interactive_workers(),
        "precondition: the interactive cap is reached"
    );

    assert_move_starts_a_worker(&state, &seeded).await;
}

fn assert_refused(response: FrontendEvent, task_id: &str) -> String {
    match response {
        FrontendEvent::WorkItemRefused { id, code, message, .. } => {
            assert_eq!(id, task_id);
            assert_eq!(code, BoardRefusalCode::DispatchRefused);
            assert!(
                message.contains(TASK_NAME),
                "the refusal names the item by title: {message}"
            );
            assert!(
                !message.contains(task_id) && !message.contains("exec_"),
                "the refusal must not leak internal ids: {message}"
            );
            message
        }
        other => panic!("expected the move to be refused, got {other:?}"),
    }
}

/// A refused move leaves the card where it was, with nothing queued to start
/// later and no launch recorded.
fn assert_nothing_left_behind(state: &ServerState, seeded: &Seeded) {
    let task = current_task(state, &seeded.task.id);
    assert_eq!(task.status, TaskStatus::Blocked, "the refused move must be reverted");
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    let left: Vec<_> = executions(state, &seeded.task.id)
        .into_iter()
        .filter(|execution| !execution.status.is_terminal())
        .collect();
    assert!(left.is_empty(), "a refused move must leave nothing queued: {left:?}");
    assert!(
        !launch_recorded(state, seeded, &seeded.task.id),
        "a refused move must not record a launch"
    );
}

/// The move does not override a dispatch pause the owner has not confirmed.
/// The app asks first when it sees the pause; a move that reaches the engine
/// without that confirmation is refused with the pause's reason instead of
/// leaving the card in Doing with nothing running.
#[tokio::test]
async fn moving_a_task_to_doing_while_dispatch_is_paused_is_refused_with_the_reason() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    focus_on(&state, &seeded.project_id);
    state.execution_coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("investigating worker failures").unwrap(),
    );

    let message = assert_refused(drop_on_doing(&state, &seeded.task.id).await, &seeded.task.id);

    assert!(
        message.contains("dispatch is paused") && message.contains("investigating worker failures"),
        "{message}"
    );
    assert_nothing_left_behind(&state, &seeded);
    assert_eq!(
        executions(&state, &seeded.task.id).len(),
        seeded.prior.len(),
        "a paused engine refuses before it creates anything"
    );
}

/// Occupy every slot the main pool can ever have.
async fn fill_pool_to_its_hard_cap(state: &ServerState) {
    let pool = state.execution_coordinator.worker_pool();
    let mut filled = 0;
    while pool
        .claim_worker_force(&format!("exec-occupant-{filled}"), None)
        .await
        .is_some()
    {
        filled += 1;
    }
    assert_eq!(pool.capacity().await, pool.hard_cap(), "precondition: the pool is full");
}

/// A pool at its hard cap cannot grow, so the launch has no slot to take.
#[tokio::test]
async fn moving_a_task_to_doing_with_the_pool_at_its_hard_cap_is_refused_with_the_reason() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    fill_pool_to_its_hard_cap(&state).await;

    let message = assert_refused(drop_on_doing(&state, &seeded.task.id).await, &seeded.task.id);

    assert!(message.contains("worker slots are in use"), "{message}");
    assert_nothing_left_behind(&state, &seeded);
    assert_eq!(
        executions(&state, &seeded.task.id).last().map(|e| e.status.clone()),
        Some(ExecutionStatus::Cancelled),
        "the execution the refused move created is cancelled, not left to start later"
    );
}

/// A refusal cancels only what the move created. An execution that was
/// already queued for the item before the move stays queued, so whatever was
/// going to start it still can.
#[tokio::test]
async fn a_refused_move_leaves_an_execution_that_was_already_queued() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    let queued = state
        .work_db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(seeded.task.id.clone())
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .expect("queue an execution before the move");
    fill_pool_to_its_hard_cap(&state).await;

    assert_refused(drop_on_doing(&state, &seeded.task.id).await, &seeded.task.id);

    assert_eq!(
        state.work_db.get_execution(&queued.id).expect("get execution").status,
        ExecutionStatus::Ready,
        "the move did not create this execution, so its refusal must not cancel it"
    );
    assert_eq!(current_task(&state, &seeded.task.id).status, TaskStatus::Blocked);
    assert!(!launch_recorded(&state, &seeded, &seeded.task.id));
}

/// A `critical` item is exempt from a default-scope operator pause in the
/// queue, and the move keeps that exemption instead of refusing it.
#[tokio::test]
async fn moving_a_critical_task_to_doing_starts_it_under_an_operator_pause() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    state
        .work_db
        .update_work_item_as_actor(
            &seeded.task.id,
            WorkItemPatch::builder().priority("critical").build(),
            boss_protocol::LAST_STATUS_ACTOR_HUMAN,
        )
        .expect("the owner raises the task to critical");
    state.execution_coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("holding new work").unwrap(),
    );

    assert_move_starts_a_worker(&state, &seeded).await;
}

#[tokio::test]
async fn a_progression_reservation_refuses_the_move_even_at_the_interactive_cap() {
    for at_cap in [false, true] {
        let (state, _dir) = test_server_state_with_fakes();
        let seeded = seed(&state);
        if at_cap {
            state
                .execution_coordinator
                .worker_pool()
                .claim_working("occupant", None)
                .await
                .unwrap();
            state
                .execution_coordinator
                .set_max_concurrent_interactive_workers(1)
                .unwrap();
        }
        let progression = state
            .work_db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(seeded.task.id.clone())
                    .kind(ExecutionKind::Progression)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let reservation = state
            .execution_coordinator
            .reserve_board_dispatch_for_test(&progression);
        assert_refused(drop_on_doing(&state, &seeded.task.id).await, &seeded.task.id);
        assert_eq!(current_task(&state, &seeded.task.id).status, TaskStatus::Blocked);
        assert!(!launch_recorded(&state, &seeded, &seeded.task.id));
        assert!(
            executions(&state, &seeded.task.id)
                .iter()
                .filter(|e| e.kind != ExecutionKind::Progression)
                .all(|e| e.status.is_terminal())
        );
        drop(reservation);
    }
}

#[tokio::test]
async fn a_failed_launch_intent_write_refuses_and_releases_the_claim_and_slot() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    let occupied = state.execution_coordinator.worker_pool().occupancy().await;
    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_launch BEFORE UPDATE OF explicitly_launched ON tasks
         BEGIN SELECT RAISE(ABORT, 'launch intent unavailable'); END;",
        )
        .unwrap();

    assert_refused(drop_on_doing(&state, &seeded.task.id).await, &seeded.task.id);

    assert_nothing_left_behind(&state, &seeded);
    assert_eq!(
        state.execution_coordinator.worker_pool().occupancy().await.working,
        occupied.working
    );
    let cancelled = executions(&state, &seeded.task.id).pop().unwrap();
    assert_eq!(cancelled.status, ExecutionStatus::Cancelled);
    // A retry on the same item proves the reservation and repo permit were released too.
    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_launch")
        .unwrap();
    assert_move_starts_a_worker(&state, &seeded).await;
}

#[tokio::test]
async fn a_board_launch_durably_claims_before_recording_intent_and_replying() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER require_claim BEFORE UPDATE OF explicitly_launched ON tasks
         WHEN NOT EXISTS (SELECT 1 FROM work_executions
                          WHERE work_item_id = NEW.id AND status = 'claimed')
         BEGIN SELECT RAISE(ABORT, 'launch must already hold durable claim'); END;",
        )
        .unwrap();

    assert_move_starts_a_worker(&state, &seeded).await;
}

#[tokio::test]
async fn a_refused_move_restores_a_reused_waiting_dependency_execution() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    let queued = state
        .work_db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(seeded.task.id.clone())
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::WaitingDependency)
                .build(),
        )
        .unwrap();
    fill_pool_to_its_hard_cap(&state).await;

    assert_refused(drop_on_doing(&state, &seeded.task.id).await, &seeded.task.id);

    assert_eq!(
        state.work_db.get_execution(&queued.id).unwrap().status,
        ExecutionStatus::WaitingDependency
    );
    assert_eq!(current_task(&state, &seeded.task.id).status, TaskStatus::Blocked);
    assert!(!launch_recorded(&state, &seeded, &seeded.task.id));
}

#[tokio::test]
async fn a_rejected_durable_claim_refuses_before_replying_and_releases_the_slot() {
    let (state, _dir) = test_server_state_with_fakes();
    let seeded = seed(&state);
    let occupied = state.execution_coordinator.worker_pool().occupancy().await.working;
    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER repark_before_claim BEFORE UPDATE OF status ON work_executions
         WHEN NEW.status = 'claimed'
         BEGIN
             UPDATE work_executions SET status = 'waiting_dependency' WHERE id = OLD.id;
             SELECT RAISE(IGNORE);
         END;",
        )
        .unwrap();

    assert_refused(drop_on_doing(&state, &seeded.task.id).await, &seeded.task.id);

    assert_nothing_left_behind(&state, &seeded);
    assert_eq!(
        state.execution_coordinator.worker_pool().occupancy().await.working,
        occupied
    );
    assert_eq!(
        executions(&state, &seeded.task.id).pop().unwrap().status,
        ExecutionStatus::Cancelled
    );
}
