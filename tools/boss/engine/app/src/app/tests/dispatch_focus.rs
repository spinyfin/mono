// Behaviour tests for `GetDispatchFocus` / `UpdateDispatchFocus`: the focus
// set edited with `bossctl dispatch focus`, its persistence
// across an engine restart, and the all-or-nothing handling of bad targets.

use super::*;
use crate::app::dispatch_focus;
use boss_protocol::{
    CreateProjectInput, DispatchFocusAction, DispatchFocusState, DispatchFocusTargetKind, ExecutionKind,
    ExecutionStatus,
};

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
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

async fn update(state: &Arc<ServerState>, action: DispatchFocusAction, targets: &[&str]) -> FrontendEvent {
    let sink = make_session_sink();
    dispatch_focus::handle_update_dispatch_focus(
        dispatch(state, &sink),
        FrontendRequest::UpdateDispatchFocus {
            action,
            targets: targets.iter().map(|t| (*t).to_owned()).collect(),
        },
    )
    .await;
    sole_response(&sink).await
}

async fn show(state: &Arc<ServerState>) -> DispatchFocusState {
    let sink = make_session_sink();
    dispatch_focus::handle_get_dispatch_focus(dispatch(state, &sink), FrontendRequest::GetDispatchFocus).await;
    focus_of(sole_response(&sink).await)
}

fn focus_of(event: FrontendEvent) -> DispatchFocusState {
    match event {
        FrontendEvent::DispatchFocusResult { focus } => focus,
        other => panic!("expected DispatchFocusResult, got {other:?}"),
    }
}

fn project(state: &ServerState, product_id: &str, name: &str) -> boss_protocol::Project {
    state
        .work_db
        .create_project(CreateProjectInput {
            product_id: product_id.to_owned(),
            name: name.to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap()
}

#[tokio::test]
async fn focus_starts_off_and_set_add_remove_clear_edit_the_set() {
    let (state, _dir) = test_server_state();
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let first = project(&state, &product.id, "First");
    let second = project(&state, &product.id, "Second");

    let initial = show(&state).await;
    assert!(!initial.active);
    assert!(initial.targets.is_empty());
    assert_eq!(initial.held_count, 0);

    let set = focus_of(update(&state, DispatchFocusAction::Set, &[&first.id]).await);
    assert!(set.active);
    assert_eq!(set.targets.len(), 1);
    assert_eq!(set.targets[0].kind, DispatchFocusTargetKind::Project);
    assert_eq!(set.targets[0].name, "First");

    let short = format!("P{}", second.short_id.unwrap());
    let added = focus_of(update(&state, DispatchFocusAction::Add, &[&short, &product.slug]).await);
    assert_eq!(added.targets.len(), 3, "{:?}", added.targets);

    let removed = focus_of(update(&state, DispatchFocusAction::Remove, &[&first.id]).await);
    assert_eq!(removed.targets.len(), 2);
    assert!(removed.targets.iter().all(|t| t.id != first.id));

    let replaced = focus_of(update(&state, DispatchFocusAction::Set, &[&second.id]).await);
    assert_eq!(replaced.targets.len(), 1, "set replaces the whole set");

    let cleared = focus_of(update(&state, DispatchFocusAction::Clear, &[]).await);
    assert!(!cleared.active);
    assert!(cleared.targets.is_empty());
    assert!(!state.execution_coordinator.dispatch_focus().is_active());
}

#[tokio::test]
async fn held_count_reflects_ready_rows_outside_the_set() {
    let (state, _dir) = test_server_state();
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let focused = project(&state, &product.id, "Focused");
    let other = project(&state, &product.id, "Other");
    for (project, name) in [(&focused, "In scope"), (&other, "Outside")] {
        let task = state
            .work_db
            .create_task(
                boss_protocol::CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name(name)
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        state
            .work_db
            .create_execution(
                boss_protocol::CreateExecutionInput::builder()
                    .work_item_id(task.id)
                    .kind(ExecutionKind::TaskImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
    }

    let focus = focus_of(update(&state, DispatchFocusAction::Set, &[&focused.id]).await);

    assert_eq!(focus.held_count, 1);
}

#[tokio::test]
async fn a_bad_target_rejects_the_whole_request_and_changes_nothing() {
    let (state, _dir) = test_server_state();
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let good = project(&state, &product.id, "Good");
    update(&state, DispatchFocusAction::Set, &[&good.id]).await;

    let rejected = update(&state, DispatchFocusAction::Add, &[&product.id, "proj_does_not_exist"]).await;

    assert!(matches!(rejected, FrontendEvent::WorkError { .. }), "{rejected:?}");
    let focus = show(&state).await;
    assert_eq!(
        focus.targets.len(),
        1,
        "the good target must not have been applied: {focus:?}"
    );
    assert_eq!(
        state.work_db.load_dispatch_focus().unwrap(),
        state.execution_coordinator.dispatch_focus(),
        "memory and disk agree after a rejected update"
    );
}

#[tokio::test]
async fn targets_are_required_except_for_clear() {
    let (state, _dir) = test_server_state();
    for action in [
        DispatchFocusAction::Set,
        DispatchFocusAction::Add,
        DispatchFocusAction::Remove,
    ] {
        let response = update(&state, action, &[]).await;
        assert!(
            matches!(response, FrontendEvent::WorkError { .. }),
            "{action:?}: {response:?}"
        );
    }
    let response = update(&state, DispatchFocusAction::Clear, &["proj_x"]).await;
    assert!(matches!(response, FrontendEvent::WorkError { .. }), "{response:?}");
}

#[tokio::test]
async fn a_task_is_not_a_focus_target() {
    let (state, _dir) = test_server_state();
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let chore = boss_engine_runtime::test_support::create_test_chore_manual(&state.work_db, product.id, "A chore");

    let response = update(&state, DispatchFocusAction::Set, &[&chore.id]).await;

    assert!(matches!(response, FrontendEvent::WorkError { .. }), "{response:?}");
    assert!(!state.execution_coordinator.dispatch_focus().is_active());
}

#[tokio::test]
async fn a_deleted_project_can_still_be_removed_from_the_set() {
    let (state, _dir) = test_server_state();
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let doomed = project(&state, &product.id, "Doomed");
    update(&state, DispatchFocusAction::Set, &[&doomed.id]).await;
    state
        .work_db
        .connect()
        .unwrap()
        .execute("DELETE FROM projects WHERE id = ?1", [&doomed.id])
        .unwrap();

    let shown = show(&state).await;
    assert_eq!(shown.targets[0].name, "", "a missing row is shown without a name");
    let removed = focus_of(update(&state, DispatchFocusAction::Remove, &[&doomed.id]).await);

    assert!(!removed.active);
}

/// Focus is persisted at every change; a fresh engine over the same
/// `state.db` comes up with the same set, before its first drain.
#[tokio::test]
async fn focus_survives_an_engine_restart() {
    let (state, dir) = test_server_state();
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let focused = project(&state, &product.id, "Focused");
    update(&state, DispatchFocusAction::Set, &[&focused.id, &product.id]).await;
    let before = state.execution_coordinator.dispatch_focus();
    assert!(before.is_active());

    let cfg = Arc::new(RuntimeConfig::from_parts(
        boss_engine_runtime::config::WorkConfig::builder()
            .cwd(dir.path().to_path_buf())
            .db_path(dir.path().join("state.db"))
            .build(),
        None,
    ));
    let restarted =
        ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, ServerStateOverrides::default()).unwrap();

    assert_eq!(restarted.execution_coordinator.dispatch_focus(), before);
    let shown = show(&restarted).await;
    assert!(shown.active);
    assert_eq!(shown.targets.len(), 2);

    // And clearing persists too: the next restart comes up unfocused.
    update(&restarted, DispatchFocusAction::Clear, &[]).await;
    assert!(!state.work_db.load_dispatch_focus().unwrap().is_active());
}

/// An unreadable persisted value starts dispatch unrestricted, but the focus
/// state reports the failure until the next successful update.
#[tokio::test]
async fn an_unreadable_persisted_focus_is_reported_in_the_focus_state() {
    let (state, dir) = test_server_state();
    state.work_db.set_metadata("dispatch_focus", "{not json").unwrap();

    let cfg = Arc::new(RuntimeConfig::from_parts(
        boss_engine_runtime::config::WorkConfig::builder()
            .cwd(dir.path().to_path_buf())
            .db_path(dir.path().join("state.db"))
            .build(),
        None,
    ));
    let restarted =
        ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, ServerStateOverrides::default()).unwrap();

    let shown = show(&restarted).await;
    assert!(!shown.active, "an unreadable set starts with focus off");
    assert!(shown.restore_error.is_some(), "the failure must be visible: {shown:?}");

    let cleared = focus_of(update(&restarted, DispatchFocusAction::Clear, &[]).await);
    assert_eq!(cleared.restore_error, None, "a successful update clears the report");
}
