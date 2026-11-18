//! A worker launched to never ask must not be left holding its slot on an
//! approval prompt. See [`crate::app::approval_prompt`].

use super::*;

use boss_engine_runtime::events_socket::IncomingHookEvent;
use boss_engine_runtime::stale_worker_sweep::STALE_WORKER_ATTENTION_KIND;
use boss_protocol::{ExecutionStatus, WorkerEvent};

use crate::app::approval_prompt::UNANSWERED_APPROVAL_PROMPT_GRACE;

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

    assert!(
        state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await
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
    assert!(
        !state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await
    );
}

/// Any event after the prompt means it was answered: the run is left alone.
#[tokio::test]
async fn a_prompt_followed_by_any_worker_event_is_not_a_failure() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);

    let prompt = approval_prompt(&run, 7);
    dispatch_live_worker_state(&state, &prompt).await;
    let (_, mark) = state.approval_prompt_to_watch(&prompt).unwrap();
    dispatch_live_worker_state(&state, &tool_finished(&run, 8)).await;

    assert!(
        !state
            .fail_if_approval_prompt_unanswered(&run, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
            .await
    );
    assert!(!state.work_db.get_execution(&run).unwrap().status.is_terminal());
    assert_eq!(state.worker_registry.slot_for_run(&run), Some(SLOT));
}

/// A prompt whose answer was applied before the prompt itself reached the
/// dispatcher is never watched.
#[tokio::test]
async fn a_prompt_overtaken_by_a_newer_event_is_not_watched() {
    let (state, _dir) = test_server_state();
    let run = working_run(&state, "grok", true);

    dispatch_live_worker_state(&state, &tool_finished(&run, 8)).await;
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
