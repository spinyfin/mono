//! A worker launched to never ask must not be left holding its slot on an
//! approval prompt. See [`crate::app::approval_prompt`].

use super::*;

use boss_engine_runtime::events_socket::IncomingHookEvent;
use boss_engine_runtime::stale_worker_sweep::STALE_WORKER_ATTENTION_KIND;
use boss_protocol::{ExecutionStatus, WorkerEvent};

use crate::app::approval_prompt::{UNANSWERED_APPROVAL_PROMPT_GRACE, UnansweredPromptOutcome};

const SLOT: u8 = 1;
const SESSION: &str = "76744658-0402-42f7-a644-68dd52f6dad4";

/// A mid-turn worker of `driver`, optionally re-kinded as a progression run,
/// with the durable tmux identity a pane release needs.
fn working_run(state: &ServerState, driver: &str, progression: bool) -> String {
    let run = register_working_worker_with_driver(state, SLOT, Some(driver));
    if progression {
        state
            .work_db
            .connect()
            .unwrap()
            .execute("UPDATE work_executions SET kind = 'progression' WHERE id = ?1", [&run])
            .unwrap();
    }
    super::tmux_stub::install_teardown(state, &run, 4_194_303);
    run
}

fn driver(slug: &str) -> Arc<dyn AgentDriver> {
    boss_engine_driver::DriverRegistry::default().require(slug).unwrap()
}

/// The hook Grok raises when it opens a tool approval dialog, decoded by the
/// Grok driver exactly as the events socket would.
fn approval_prompt(run: &str, ingress_seq: u64) -> IncomingHookEvent {
    let grok = driver("grok");
    let event = grok
        .normalize_progress_event(&serde_json::json!({
            "hookEventName": "notification",
            "sessionId": SESSION,
            "notificationType": "permission_prompt",
            "message": "Tool permission requested",
            "permissionMode": "default",
        }))
        .unwrap();
    IncomingHookEvent::resolve(grok.as_ref(), event, Some(run.to_owned()), None, None).with_ingress_seq(ingress_seq)
}

fn task_complete(run: &str, ingress_seq: u64) -> IncomingHookEvent {
    let grok = driver("grok");
    let event = grok
        .normalize_progress_event(&serde_json::json!({
            "hookEventName": "notification",
            "sessionId": SESSION,
            "notificationType": "task_complete",
            "message": "Background task completed: abc",
        }))
        .unwrap();
    IncomingHookEvent::resolve(grok.as_ref(), event, Some(run.to_owned()), None, None).with_ingress_seq(ingress_seq)
}

fn tool_finished(run: &str, ingress_seq: u64) -> IncomingHookEvent {
    IncomingHookEvent::resolve(
        driver("grok").as_ref(),
        WorkerEvent::PostToolUse {
            session_id: SESSION.into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
        Some(run.to_owned()),
        None,
        None,
    )
    .with_ingress_seq(ingress_seq)
}

/// The wedge this exists for: a Grok progression run whose pane opened an
/// approval dialog. Nothing answers it, so after the grace period the run is
/// failed with the reason, its slot is free again, and the stuck item it was
/// inspecting is untouched.
#[tokio::test]
async fn a_progression_run_left_on_an_approval_prompt_is_failed_with_the_reason_and_frees_its_slot() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);
    let work_item_id = state.work_db.get_execution(&run).unwrap().work_item_id;
    let item_before = format!("{:?}", state.work_db.get_work_item(&work_item_id).unwrap());

    let prompt = approval_prompt(&run, 7);
    dispatch_live_worker_state(&state, &prompt).await;
    let (watched, mark) = state
        .approval_prompt_to_watch(&prompt)
        .expect("a deny-by-default run that reports an approval prompt is watched");
    assert_eq!(watched, run);

    assert_eq!(
        state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await,
        UnansweredPromptOutcome::Failed
    );

    assert_eq!(
        state.work_db.get_execution(&run).unwrap().status,
        ExecutionStatus::Failed
    );
    // The reason is recorded on the run, where a progression failure lives.
    let reason = state
        .work_db
        .list_runs(&run)
        .unwrap()
        .into_iter()
        .filter_map(|run| run.error_text)
        .next_back()
        .unwrap_or_default();
    assert!(reason.starts_with("progression: UnansweredApprovalPrompt:"), "{reason}");
    assert!(
        reason.contains("the grok worker opened an interactive approval prompt"),
        "{reason}"
    );
    assert!(reason.contains("for 120s"), "{reason}");
    assert!(
        state.worker_registry.slot_for_run(&run).is_none(),
        "the slot is released"
    );

    assert_eq!(
        format!("{:?}", state.work_db.get_work_item(&work_item_id).unwrap()),
        item_before,
        "the stuck item the run was inspecting is unchanged"
    );
    assert!(
        state
            .work_db
            .list_attention_items_for_work_item(&work_item_id)
            .unwrap()
            .iter()
            .all(|item| item.kind != STALE_WORKER_ATTENTION_KIND),
        "no attention is raised on the stuck item"
    );
    // A second check after the failure is a no-op, not a second failure.
    assert_eq!(
        state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await,
        UnansweredPromptOutcome::NotApplicable
    );
}

/// A dialog-closing event (PostToolUse) means the prompt was answered.
#[tokio::test]
async fn a_prompt_followed_by_a_dialog_closing_event_is_not_a_failure() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);

    let prompt = approval_prompt(&run, 7);
    dispatch_live_worker_state(&state, &prompt).await;
    let (_, mark) = state.approval_prompt_to_watch(&prompt).unwrap();
    let finished = tool_finished(&run, 8);
    dispatch_live_worker_state(&state, &finished).await;
    state.watch_approval_prompt(&finished);

    assert_eq!(
        state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await,
        UnansweredPromptOutcome::NotApplicable
    );
    assert!(!state.work_db.get_execution(&run).unwrap().status.is_terminal());
    assert_eq!(state.worker_registry.slot_for_run(&run), Some(SLOT));
}

/// A failed failure-write must not drop tracking: the pane may still be alive
/// and the run still holds its pool claim. The watch stays armed, and a retry
/// once writes work again records the failure, verifies the teardown and
/// frees the pool capacity.
#[tokio::test]
async fn a_failed_failure_write_keeps_tracking_and_the_retry_frees_the_pool_claim() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);
    let pool = state.execution_coordinator.worker_pool();
    let held = pool.claim_worker(&run, None).await.expect("test pool has a free slot");
    assert_eq!(held, "worker-1");
    let idle_while_held = pool.idle_count().await;
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;

    let prompt = approval_prompt(&run, 7);
    dispatch_live_worker_state(&state, &prompt).await;
    let (_, mark) = state.approval_prompt_to_watch(&prompt).unwrap();

    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("PRAGMA query_only = ON;")
        .unwrap();

    assert_eq!(
        state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await,
        UnansweredPromptOutcome::Incomplete,
        "the failure write failed, so the failure is not complete"
    );
    assert!(
        !state.work_db.get_execution(&run).unwrap().status.is_terminal(),
        "query_only blocked the durable failure write"
    );
    assert_eq!(
        state.worker_registry.slot_for_run(&run),
        Some(SLOT),
        "tracking is kept while the failure is incomplete"
    );
    assert!(pool.claimed_execution_ids().await.contains(&run));

    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("PRAGMA query_only = OFF;")
        .unwrap();
    // The first attempt's own teardown used the scripted replies; the retry
    // verifies and reaps the pane again.
    let env = format!("BOSS_SPAWN_TOKEN=token-{run}\n");
    let (tmux, _) = super::tmux_stub::fake_tmux([
        super::tmux_stub::ok(&env),
        super::tmux_stub::ok("0"),
        super::tmux_stub::ok(&env),
        super::tmux_stub::ok(""),
        super::tmux_stub::ok(""),
    ]);
    state.set_tmux_override_for_test(tmux);
    let ack = tokio::spawn({
        let state = state.clone();
        async move {
            let envelope = sink.next().await.expect("the app is asked to detach the pane");
            let FrontendEvent::EngineRequest {
                request_id, request, ..
            } = envelope.payload
            else {
                panic!("expected an EngineRequest, got {:?}", envelope.payload);
            };
            assert!(
                matches!(request, EngineToAppRequest::DetachWorkerPane(_)),
                "{request:?}"
            );
            state
                .deliver_app_response(
                    "session-app",
                    &request_id,
                    EngineToAppResponse::DetachWorkerPane {
                        result: Ok(boss_protocol::DetachWorkerPaneResult {}),
                    },
                )
                .await;
        }
    });

    assert_eq!(
        state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await,
        UnansweredPromptOutcome::Failed
    );
    assert_eq!(
        state.work_db.get_execution(&run).unwrap().status,
        ExecutionStatus::Failed
    );
    ack.await.unwrap();
    assert!(
        state.worker_registry.slot_for_run(&run).is_none(),
        "the verified teardown released the slot"
    );
    assert!(
        !pool.claimed_execution_ids().await.contains(&run),
        "the pool claim is freed"
    );
    assert!(pool.idle_count().await > idle_while_held, "pool capacity is back");
}

/// Grok's background `task_complete` is an ordinary Notification. It must
/// not disarm the watch: the dialog is still on the pane.
#[tokio::test]
async fn a_prompt_followed_by_task_complete_still_fails_after_the_grace() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);

    let prompt = approval_prompt(&run, 7);
    dispatch_live_worker_state(&state, &prompt).await;
    let (_, mark) = state.approval_prompt_to_watch(&prompt).unwrap();
    let complete = task_complete(&run, 8);
    dispatch_live_worker_state(&state, &complete).await;
    state.watch_approval_prompt(&complete);

    assert_eq!(
        state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await,
        UnansweredPromptOutcome::Failed
    );
    assert_eq!(
        state.work_db.get_execution(&run).unwrap().status,
        ExecutionStatus::Failed
    );
    assert!(
        state.worker_registry.slot_for_run(&run).is_none(),
        "the slot is released"
    );
}

/// A prompt whose answer was applied before the prompt itself reached the
/// dispatcher is never watched.
#[tokio::test]
async fn a_prompt_overtaken_by_a_newer_event_is_not_watched() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);

    let finished = tool_finished(&run, 8);
    dispatch_live_worker_state(&state, &finished).await;
    state.watch_approval_prompt(&finished);
    let prompt = approval_prompt(&run, 7);
    dispatch_live_worker_state(&state, &prompt).await;

    assert!(state.approval_prompt_to_watch(&prompt).is_none());
}

/// An ordinary worker runs under always-approve, where Grok raises the same
/// notification and answers it itself. It says nothing about that pane.
#[tokio::test]
async fn a_kind_launched_under_always_approve_is_not_watched() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", false);

    let prompt = approval_prompt(&run, 7);
    dispatch_live_worker_state(&state, &prompt).await;

    assert!(prompt.reports_approval_prompt());
    assert!(state.approval_prompt_to_watch(&prompt).is_none());
}

/// Only a driver that has a measured mapping reports an approval prompt. A
/// Claude notification with the same words is not one.
#[tokio::test]
async fn a_driver_that_reports_no_approval_prompt_is_not_watched() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "claude", true);

    let notification = IncomingHookEvent::resolve(
        driver("claude").as_ref(),
        WorkerEvent::Notification {
            session_id: SESSION.into(),
            message: "Tool permission requested".into(),
        },
        Some(run.clone()),
        None,
        None,
    )
    .with_ingress_seq(7);
    dispatch_live_worker_state(&state, &notification).await;

    assert!(!notification.reports_approval_prompt());
    assert!(state.approval_prompt_to_watch(&notification).is_none());
}

/// The whole path, from the hook arriving at the dispatcher: the prompt's own
/// event starts the watch, the run stays untouched inside the grace period,
/// and it is failed once the period has passed with nothing after the prompt.
#[tokio::test(start_paused = true)]
async fn the_prompt_event_starts_a_watch_that_fails_the_run_after_the_grace_period() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);

    dispatch_worker_event_fanout(&state, &approval_prompt(&run, 7)).await;

    tokio::time::sleep(UNANSWERED_APPROVAL_PROMPT_GRACE / 2).await;
    assert!(
        !state.work_db.get_execution(&run).unwrap().status.is_terminal(),
        "inside the grace period the run is left alone"
    );

    tokio::time::sleep(UNANSWERED_APPROVAL_PROMPT_GRACE).await;
    for _ in 0..200 {
        if state.work_db.get_execution(&run).unwrap().status.is_terminal() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        state.work_db.get_execution(&run).unwrap().status,
        ExecutionStatus::Failed
    );
    assert!(
        state.worker_registry.slot_for_run(&run).is_none(),
        "the slot is released"
    );
}

/// Hook connections are dispatched concurrently: a `task_complete` that was
/// sent after the prompt can be applied first. It is not evidence the dialog
/// closed, so the late prompt must still be watched and still fail the run.
#[tokio::test]
async fn a_prompt_dispatched_after_a_later_task_complete_is_still_watched() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);

    let complete = task_complete(&run, 8);
    dispatch_live_worker_state(&state, &complete).await;
    state.watch_approval_prompt(&complete);
    let prompt = approval_prompt(&run, 7);
    dispatch_live_worker_state(&state, &prompt).await;
    let (_, mark) = state
        .approval_prompt_to_watch(&prompt)
        .expect("a later Notification does not suppress an earlier prompt");

    assert_eq!(
        state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await,
        UnansweredPromptOutcome::Failed
    );
}

/// The PreToolUse for the very call that raised the prompt has a lower
/// sequence than the prompt. Dispatched after it, it must not clear it.
#[tokio::test]
async fn an_older_tool_event_dispatched_after_the_prompt_does_not_clear_it() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);

    let prompt = approval_prompt(&run, 7);
    dispatch_live_worker_state(&state, &prompt).await;
    let (_, mark) = state.approval_prompt_to_watch(&prompt).unwrap();
    let older = IncomingHookEvent::resolve(
        driver("grok").as_ref(),
        WorkerEvent::PreToolUse {
            session_id: SESSION.into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
        },
        Some(run.clone()),
        None,
        None,
    )
    .with_ingress_seq(6);
    dispatch_live_worker_state(&state, &older).await;
    state.watch_approval_prompt(&older);

    assert_eq!(
        state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await,
        UnansweredPromptOutcome::Failed
    );
}
