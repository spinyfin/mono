//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).
//!
//! Covers the driver-terminal-error gate in `on_stop_inner`: a codex
//! rollout `task_complete` carrying a non-null `error` must fail the
//! execution, release its cube lease, file an attention item naming the
//! provider's error text, and never queue a nudge probe — instead of being
//! read as a clean turn boundary and re-prompting a process that has
//! already exited.

use super::*;

use crate::driver::{AgentDriver, CodexDriver, ProgressSessionConfig, ProgressStreamSource, TurnEnd};
use serde_json::json;

/// Drive a codex rollout `task_complete` envelope through the real
/// `CodexDriver` progress-session normalizer and its `turn_boundary`
/// resolution, exactly as the engine's JSONL ingress does — so this test
/// exercises the adapter, not a hand-built `TurnEnd`.
fn codex_task_complete_turn_end(task_complete_payload: serde_json::Value) -> TurnEnd {
    let driver = CodexDriver::default();
    let mut session = driver
        .progress_session(&ProgressSessionConfig {
            source: ProgressStreamSource::AgentJsonlFile,
            ..Default::default()
        })
        .expect("CodexDriver declares a rollout AgentJsonlFile progress session");

    session
        .normalize_progress_events(&json!({
            "type": "session_meta",
            "payload": {"id": "thread-1"},
        }))
        .expect("session_meta must normalize");

    let events = session
        .normalize_progress_events(&json!({
            "type": "event_msg",
            "payload": task_complete_payload,
        }))
        .expect("task_complete must normalize");
    let stop_event = events
        .last()
        .cloned()
        .expect("task_complete must yield at least a Stop event");

    driver
        .turn_boundary(&stop_event)
        .expect("a Stop event must always resolve to a turn boundary")
}

/// The exact incident shape: `task_complete` with `last_agent_message: null`
/// and a non-null `error` carrying the provider's own diagnostic.
fn fatal_task_complete_payload() -> serde_json::Value {
    json!({
        "type": "task_complete",
        "turn_id": "019faec4-b59d-7210-84d8-21e3e0144a63",
        "last_agent_message": null,
        "error": {
            "message": "{\"type\":\"error\",\"status\":400,\"code\":\"bad_request\"}",
            "codex_error_info": "other",
        },
    })
}

#[test]
fn codex_task_complete_error_yields_other_turn_end() {
    let turn_end = codex_task_complete_turn_end(fatal_task_complete_payload());
    assert_eq!(
        turn_end.reason,
        boss_protocol::StopReason::Other,
        "a task_complete carrying a non-null error must surface as an unrecoverable-error turn \
         boundary, not a clean completion",
    );
}

/// A clean `task_complete` (no `error` field at all) must still produce an
/// ordinary successful turn boundary — the fatal-error gate must not
/// misfire on the common case.
#[test]
fn codex_task_complete_clean_yields_completed_turn_end() {
    let turn_end = codex_task_complete_turn_end(json!({
        "type": "task_complete",
        "turn_id": "turn-clean-1",
        "last_agent_message": "All done — opened the PR.",
    }));
    assert_eq!(turn_end.reason, boss_protocol::StopReason::Completed);
}

/// End-to-end: feed the adapter-produced `TurnEnd` for a fatal codex error
/// into `on_stop_with_turn_end` and assert every piece of the fix — the
/// execution ends `failed` (not left `running`/`waiting_human`), the cube
/// lease is released, an attention item names the provider's error text,
/// and no nudge probe is ever queued.
#[tokio::test]
async fn driver_terminal_error_fails_execution_releases_lease_and_skips_nudge() {
    let workspace = tempdir().unwrap();
    // No ReviewResult artifact/transcript written — mirrors the incident,
    // where the worker died before producing anything. Absent the fix, this
    // is exactly the shape that fell into the `pr_review` re-prompt loop.
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) = pr_review_exec_fixture(workspace.path(), None);

    // The attention detail is recovered from the run's transcript, read
    // through the run's own driver (see `driver_transcript::driver_for_execution`).
    // A `pr_review` execution always dispatches on the review pool
    // (`pr_review_exec_fixture`'s fixed `"review-worker-1"` worker id), which
    // always runs Claude regardless of the producing chore's own `driver`
    // column (`coordinator::pool_dispatch_policy_for_worker_id`) — so the
    // reviewer's transcript is Claude-shaped, not a raw codex rollout, and
    // that is the dialect this fixture's transcript must be written in for
    // the read path to recover the diagnostic. (`codex_task_complete_turn_end`
    // below still exercises `CodexDriver`'s own rollout normalizer directly,
    // independent of which driver reads this stored transcript — that proves
    // the adapter, this proves the Stop-boundary read.)
    let transcript_path = workspace.path().join(format!("transcript-{pr_review_exec_id}.jsonl"));
    let transcript_jsonl = format!(
        "{}\n",
        json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "Fatal error: {\"type\":\"error\",\"status\":400,\"code\":\"bad_request\"} (other)",
                }],
            },
        }),
    );
    std::fs::write(&transcript_path, transcript_jsonl).unwrap();
    db.set_run_transcript_path_if_unset(&pr_review_exec_id, transcript_path.to_str().unwrap())
        .unwrap();

    let harness = TestHarness::new(db.clone(), StubPrDetector::ok(None));

    let turn_end = codex_task_complete_turn_end(fatal_task_complete_payload());
    let outcome = harness
        .handler
        .on_stop_with_turn_end(&pr_review_exec_id, Some(&turn_end))
        .await;

    let detail = match &outcome {
        StopOutcome::DriverTerminalError { detail } => detail.clone(),
        other => panic!("expected DriverTerminalError, got {other:?}"),
    };
    assert!(
        detail.contains("400") || detail.contains("other"),
        "attention detail must name the provider's own diagnostic; got: {detail}",
    );

    let execution = db.get_execution(&pr_review_exec_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Failed,
        "a driver-reported fatal error must fail the execution, not leave it running",
    );

    assert_eq!(
        harness.cube.release_calls.lock().await.as_slice(),
        &["lease-review-1".to_owned()],
        "the cube lease must be released, not leaked",
    );

    let attentions = db.list_attention_items(&pr_review_exec_id).unwrap();
    assert!(
        attentions
            .iter()
            .any(|i| i.kind == DRIVER_TERMINAL_ERROR_ATTENTION_KIND && i.body_markdown.contains(&detail)),
        "an attention item naming the provider error text must be filed; got {attentions:?}",
    );

    assert!(
        harness.probes.snapshot().is_empty(),
        "no nudge probe may be queued for a driver-reported fatal error; got {:?}",
        harness.probes.snapshot(),
    );

    // The producing task must not have silently advanced to in_review —
    // failing the reviewer execution is not the same as completing review.
    let item = db.get_work_item(&chore_id).unwrap();
    let task = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_ne!(task.status, TaskStatus::InReview);
}

/// The companion clean-path proof: a `task_complete` with no error still
/// goes through the ordinary `pr_review` finalizer (re-prompting for a
/// missing `ReviewResult`, in this fixture) rather than being treated as a
/// driver-reported failure.
#[tokio::test]
async fn clean_stop_does_not_take_the_driver_terminal_error_path() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _chore_id_unused, _chore_id, pr_review_exec_id, _pr_url) =
        pr_review_exec_fixture(workspace.path(), None);

    let harness = TestHarness::new(db.clone(), StubPrDetector::ok(None));

    let turn_end = codex_task_complete_turn_end(json!({
        "type": "task_complete",
        "turn_id": "turn-clean-2",
        "last_agent_message": "All done.",
    }));
    let outcome = harness
        .handler
        .on_stop_with_turn_end(&pr_review_exec_id, Some(&turn_end))
        .await;

    assert!(
        matches!(outcome, StopOutcome::ReviewPassAwaitingResult),
        "a clean turn boundary must fall through to the normal pr_review finalizer; got {outcome:?}",
    );
    let execution = db.get_execution(&pr_review_exec_id).unwrap();
    assert_ne!(execution.status, ExecutionStatus::Failed);
    assert!(harness.cube.release_calls.lock().await.is_empty());
}
