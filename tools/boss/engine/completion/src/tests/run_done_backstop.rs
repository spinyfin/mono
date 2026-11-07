use super::*;
use crate::run_done_backstop::DEFAULT_DECLARATION_HORIZON_SECS as HORIZON;

use boss_protocol::{ExecutionStatus, StopReason, WorkerEvent};

const PR_URL: &str = "https://github.com/spinyfin/mono/pull/7000";

fn stop() -> WorkerEvent {
    WorkerEvent::Stop {
        session_id: "session".into(),
        stop_reason: StopReason::Completed,
        stop_hook_active: false,
    }
}

fn post_tool() -> WorkerEvent {
    WorkerEvent::PostToolUse {
        session_id: "session".into(),
        tool_name: "Bash".into(),
        tool_input: serde_json::json!({}),
        tool_response: serde_json::json!({}),
    }
}

fn armed_harness(
    db: Arc<WorkDb>,
    execution_id: &str,
) -> (TestHarness, Arc<crate::live_worker_state::LiveWorkerStateRegistry>) {
    let mut harness = TestHarness::new(db, StubPrDetector::ok(None));
    let states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    states.register_spawn(1, execution_id, "claude", 42, None);
    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("run_done_proposals_seam", true).unwrap();
    let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged.record_command_observation(execution_id, PR_URL, true);
    harness.handler = harness
        .handler
        .with_staged_pr_urls(staged)
        .with_live_worker_states(states.clone())
        .with_feature_flags(flags)
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));
    (harness, states)
}

fn expire_silence(
    handler: &WorkerCompletionHandler,
    states: &crate::live_worker_state::LiveWorkerStateRegistry,
    execution_id: &str,
) {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    states.set_declaration_activity_at_for_test(1, "2000-01-01T00:00:00Z".into());
    handler.run_done_silence_tracker.forget(execution_id);
    handler
        .run_done_silence_tracker
        .record(execution_id, now - HORIZON - 1, HORIZON);
}

#[tokio::test]
async fn armed_pr_with_continuous_tools_never_asks_or_spends_strikes() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = fixture(workspace.path());
    let (harness, states) = armed_harness(db.clone(), &execution_id);
    // Each pass starts with the old, already-expired clock from the incident.
    // Real PostToolUse ingress must protect the worker without any Stop.
    for _ in 0..8 {
        expire_silence(&harness.handler, &states, &execution_id);
        states.apply_event(1, &post_tool());
        assert!(matches!(
            harness.handler.recheck_for_pr(&execution_id).await,
            StopOutcome::AwaitingRunDoneDeclaration { .. }
        ));
        assert_eq!(harness.handler.nudge_breaker.counts(&execution_id), None);
    }
    states.apply_event(1, &stop());
    assert!(matches!(
        harness.handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingRunDoneDeclaration { .. }
    ));
    assert!(harness.probes.snapshot().is_empty());
    assert!(harness.probes.deliver_snapshot().is_empty());
    assert!(harness.pane.calls.lock().await.is_empty());
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running
    );
}

#[tokio::test]
async fn silence_during_an_open_turn_waits_for_stop_then_asks() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = fixture(workspace.path());
    let (harness, states) = armed_harness(db, &execution_id);
    states.apply_event(
        1,
        &WorkerEvent::PreToolUse {
            session_id: "session".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
        },
    );
    expire_silence(&harness.handler, &states, &execution_id);
    for _ in 0..5 {
        harness.handler.recheck_for_pr(&execution_id).await;
    }
    assert!(harness.probes.snapshot().is_empty());
    assert_eq!(harness.handler.nudge_breaker.counts(&execution_id), None);
    // Only the silence after the tool returns counts. The turn can remain
    // open, and its eventual Stop is the first safe delivery opportunity.
    states.apply_event(1, &post_tool());
    expire_silence(&harness.handler, &states, &execution_id);
    harness.handler.recheck_for_pr(&execution_id).await;
    assert!(harness.probes.snapshot().is_empty());
    states.apply_event(1, &stop());
    assert!(matches!(
        harness.handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingRunDoneDeclaration { .. }
    ));
    assert_eq!(harness.probes.snapshot().len(), 1);
    assert_eq!(harness.handler.nudge_breaker.counts(&execution_id), Some((1, 1)));
    assert!(harness.probes.snapshot()[0].1.contains("boss propose wait"));
}

#[tokio::test]
async fn declared_wait_answers_the_question_and_clears_strikes() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = fixture(workspace.path());
    let (harness, states) = armed_harness(db, &execution_id);
    states.apply_event(1, &stop());
    expire_silence(&harness.handler, &states, &execution_id);
    harness.handler.on_stop(&execution_id).await;
    assert_eq!(harness.handler.nudge_breaker.counts(&execution_id), Some((1, 1)));
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    harness
        .handler
        .wait_registry
        .declare(&execution_id, "validation running".into(), None, 1800, now)
        .unwrap();
    for _ in 0..5 {
        assert!(matches!(
            harness.handler.recheck_for_pr(&execution_id).await,
            StopOutcome::WorkerWaitPending { .. }
        ));
        assert!(matches!(
            harness.handler.on_stop(&execution_id).await,
            StopOutcome::WorkerWaitPending { .. }
        ));
        assert_eq!(harness.handler.nudge_breaker.counts(&execution_id), None);
        assert!(matches!(
            harness.handler.recheck_background_nudge(&execution_id).await,
            Some(StopOutcome::WorkerWaitPending { .. })
        ));
    }
    assert_eq!(harness.probes.snapshot().len(), 1);
    assert!(harness.pane.calls.lock().await.is_empty());
    // Once the bounded answer expires, genuinely silent work is still asked.
    harness
        .handler
        .wait_registry
        .declare(&execution_id, "expired wait".into(), None, 1, now - 2)
        .unwrap();
    expire_silence(&harness.handler, &states, &execution_id);
    harness.handler.recheck_for_pr(&execution_id).await;
    assert_eq!(harness.probes.snapshot().len(), 2);
    assert_eq!(harness.handler.nudge_breaker.counts(&execution_id), Some((1, 1)));
}

#[tokio::test]
async fn unanswered_asks_park_with_attention_without_failure_or_teardown() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture(workspace.path());
    let (harness, states) = armed_harness(db.clone(), &execution_id);
    let before = db.get_execution(&execution_id).unwrap();
    states.apply_event(1, &stop());
    expire_silence(&harness.handler, &states, &execution_id);
    for _ in 0..3 {
        assert!(matches!(
            harness.handler.on_stop(&execution_id).await,
            StopOutcome::AwaitingRunDoneDeclaration { .. }
        ));
    }
    assert!(matches!(
        harness.handler.recheck_for_pr(&execution_id).await,
        StopOutcome::RunUndeclaredParked { .. }
    ));
    let after = db.get_execution(&execution_id).unwrap();
    assert_eq!(after.status, ExecutionStatus::WaitingHuman);
    assert!(after.finished_at.is_none());
    assert!(db.execution_run_undeclared(&execution_id).unwrap());
    assert_eq!(after.workspace_path, before.workspace_path);
    assert_eq!(after.cube_lease_id, before.cube_lease_id);
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(task) => assert_eq!(task.status, TaskStatus::Active),
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(harness.pane.calls.lock().await.is_empty());
    assert!(harness.cube.release_calls.lock().await.is_empty());
    assert_eq!(harness.probes.snapshot().len(), 3);
    let attention = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        attention
            .iter()
            .filter(|item| item.kind == RUN_UNDECLARED_ATTENTION_KIND && item.status != "resolved")
            .count(),
        1
    );
    harness.handler.recheck_for_pr(&execution_id).await;
    assert_eq!(harness.probes.snapshot().len(), 3);
}
