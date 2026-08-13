// Regression coverage for the engine→app dispatch-pause banner.
//
// The macOS app's pause banner is driven entirely by `engine.health`
// pushes. These tests deliberately drive a **programmatic** pauser
// (`trip_spawn_capability_circuit` / `resume_dispatch_after_breaker_recovery`)
// rather than the CLI verb, because a fix wired only into the RPC handler
// would pass a CLI-path test and still leave breaker-originated pauses
// invisible. They assert on what a subscribed frontend actually receives,
// not on internal coordinator state.

use super::*;

use crate::spawn_health::{TripSignal, resume_dispatch_after_breaker_recovery, trip_spawn_capability_circuit};

const TEST_SESSION: &str = "session-engine-health";

/// Epoch seconds used as the pause timestamp. Any fixed value works; a real
/// one keeps the rendered "paused since" phrasing realistic.
const PAUSED_AT_EPOCH_S: i64 = 1_786_385_816;

/// A session sink registered with the topic broker and subscribed to
/// `engine.health` — exactly what the macOS app does on connect
/// (`ChatViewModel.desiredWorkTopics`).
async fn subscribed_health_session(state: &Arc<ServerState>) -> Arc<SessionSink> {
    let sink = make_session_sink();
    state.topic_broker.register_session(TEST_SESSION, sink.clone()).await;
    let added = state
        .topic_broker
        .subscribe(TEST_SESSION, &[TOPIC_ENGINE_HEALTH.to_owned()])
        .await;
    assert_eq!(
        added,
        vec![TOPIC_ENGINE_HEALTH.to_owned()],
        "fixture precondition: the session must actually hold the engine.health topic",
    );
    sink
}

/// The next engine-health report pushed to `sink`. Bounded, so a broadcast
/// that never fires fails loudly here instead of hanging the suite — which
/// is the exact failure this file exists to catch.
async fn next_health_report(sink: &SessionSink) -> boss_protocol::EngineHealthReport {
    let envelope = tokio::time::timeout(std::time::Duration::from_secs(10), sink.next())
        .await
        .expect("no engine.health push arrived within 10s — the running app would show nothing")
        .expect("sink closed before the engine.health push arrived");
    match envelope.payload {
        FrontendEvent::EngineHealthResult { report } => report,
        other => panic!("expected EngineHealthResult on engine.health, got {other:?}"),
    }
}

/// The `dispatch_paused` issue from a report, if present. This is the entry
/// `EngineHealthBanner` keys its headline and its Unpause button off.
fn dispatch_paused_issue(report: &boss_protocol::EngineHealthReport) -> Option<&boss_protocol::EngineHealthIssue> {
    report.issues.iter().find(|issue| issue.kind == "dispatch_paused")
}

/// The `automation_paused` issue from a report, if present.
fn automation_paused_issue(report: &boss_protocol::EngineHealthReport) -> Option<&boss_protocol::EngineHealthIssue> {
    report.issues.iter().find(|issue| issue.kind == "automation_paused")
}

/// Trip the spawn-capability circuit breaker — a pauser that never goes
/// anywhere near a `FrontendRequest` handler.
async fn trip_breaker(state: &Arc<ServerState>) {
    trip_spawn_capability_circuit(
        &state.work_db,
        &state.execution_coordinator,
        state.dispatch_events.as_ref(),
        &state.spawn_health,
        TripSignal {
            tripping_execution_id: "exec-spawn-capability-under-test",
            tripping_work_item_id: "work-item-spawn-capability-under-test",
            distinct_work_items: 3,
            now_epoch_secs: PAUSED_AT_EPOCH_S,
        },
    )
    .await;
    assert!(
        state.execution_coordinator.is_dispatch_paused(),
        "fixture precondition: the breaker must actually have paused dispatch",
    );
}

/// The bug, stated as a test: a breaker-originated pause must reach a
/// running app. No RPC handler runs here at all.
#[tokio::test]
async fn breaker_pause_pushes_engine_health_to_a_running_app() {
    let (state, _dir) = test_server_state();
    let sink = subscribed_health_session(&state).await;
    let _broadcaster = state.spawn_pause_state_health_broadcaster();

    let initial = next_health_report(&sink).await;
    assert!(
        !initial.dispatch_paused,
        "fixture precondition: dispatch starts unpaused, got {initial:?}",
    );

    trip_breaker(&state).await;

    let report = next_health_report(&sink).await;
    assert!(
        report.dispatch_paused,
        "a breaker-originated pause must push a paused health report, got {report:?}",
    );
    let issue = dispatch_paused_issue(&report).expect("a paused report must carry a dispatch_paused issue");
    assert!(
        issue.body.contains("spawn-capability circuit breaker tripped"),
        "the banner must carry the reason, not just the paused fact — otherwise the \
         banner shows a pause with no reason and the cause is only discoverable from the CLI: {}",
        issue.body,
    );
}

/// The other direction: a breaker auto-resume must clear the banner live
/// too. This is the path a fresh app session and the half-open recovery
/// probe both take, and it is likewise not an RPC handler.
#[tokio::test]
async fn breaker_auto_resume_pushes_a_cleared_engine_health_to_a_running_app() {
    let (state, _dir) = test_server_state();
    let sink = subscribed_health_session(&state).await;
    let _broadcaster = state.spawn_pause_state_health_broadcaster();

    let _initial = next_health_report(&sink).await;
    trip_breaker(&state).await;
    let paused = next_health_report(&sink).await;
    assert!(paused.dispatch_paused, "fixture precondition: got {paused:?}");

    let resumed = resume_dispatch_after_breaker_recovery(
        &state.work_db,
        &state.execution_coordinator,
        state.dispatch_events.as_ref(),
        Some("exec-spawn-capability-under-test"),
        "test recovery",
    )
    .await;
    assert!(resumed, "fixture precondition: the breaker pause must be resumable");

    let report = next_health_report(&sink).await;
    assert!(
        !report.dispatch_paused,
        "a programmatic resume must push a cleared health report so the banner \
         disappears without an app restart, got {report:?}",
    );
    assert!(
        dispatch_paused_issue(&report).is_none(),
        "a cleared report must not still carry a dispatch_paused issue: {report:?}",
    );
}

/// An app attaching while a pause is already in force must see it. The
/// broadcaster's start-up reconcile pass covers the engine side of that
/// (the app also asks directly via `get_engine_health` on connect), so a
/// pause restored from `state.db` at boot — before any subscriber exists —
/// is not silently invisible.
#[tokio::test]
async fn broadcaster_start_pushes_a_pause_that_was_already_in_force() {
    let (state, _dir) = test_server_state();
    // Pause BEFORE anything is listening, the way engine boot restores a
    // persisted pause.
    trip_breaker(&state).await;

    let sink = subscribed_health_session(&state).await;
    let _broadcaster = state.spawn_pause_state_health_broadcaster();

    let report = next_health_report(&sink).await;
    assert!(
        report.dispatch_paused,
        "the first push after a subscriber attaches must already report the \
         in-force pause, got {report:?}",
    );
    assert!(
        dispatch_paused_issue(&report).is_some_and(|issue| issue.body.contains("Reason:")),
        "an already-in-force pause must surface its reason too: {report:?}",
    );
}

/// A `SetDispatchPaused` RPC pause reaches a running app: the handler does
/// not push health itself — the pause-state transition it causes does.
#[tokio::test]
async fn operator_pause_rpc_still_pushes_engine_health() {
    let (state, _dir) = test_server_state();
    let sink = subscribed_health_session(&state).await;
    let _broadcaster = state.spawn_pause_state_health_broadcaster();

    let _initial = next_health_report(&sink).await;

    let request_sink = make_session_sink();
    crate::app::engine_meta::handle_set_dispatch_paused(
        Dispatch::builder()
            .server_state(state.clone())
            .work_db(state.work_db.clone())
            .sink(request_sink.clone())
            .session_id("session-operator")
            .request_id("req-pause")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build(),
        FrontendRequest::SetDispatchPaused {
            paused: true,
            reason: Some("operator is rebooting the build host".to_owned()),
        },
    )
    .await;

    let report = next_health_report(&sink).await;
    assert!(
        report.dispatch_paused,
        "an operator pause must still reach a running app, got {report:?}",
    );
    assert!(
        dispatch_paused_issue(&report).is_some_and(|issue| issue.body.contains("operator is rebooting the build host")),
        "the operator's own reason must reach the banner: {report:?}",
    );
}

/// Automation pause/resume must also push `engine.health` live. Change
/// detection on this path is hand-rolled (flag / since / reason), not a
/// single `PartialEq` snapshot comparison, so a silent regression there
/// would leave the automation banner stale on a running app.
#[tokio::test]
async fn automation_pause_and_resume_push_engine_health_to_a_running_app() {
    let (state, _dir) = test_server_state();
    let sink = subscribed_health_session(&state).await;
    let _broadcaster = state.spawn_pause_state_health_broadcaster();

    let initial = next_health_report(&sink).await;
    assert!(
        !initial.automation_paused,
        "fixture precondition: automation starts unpaused, got {initial:?}",
    );

    state.execution_coordinator.pause_automation(
        PAUSED_AT_EPOCH_S as u64,
        boss_protocol::PauseReason::new("operator is holding automation while debugging triage").unwrap(),
    );

    let paused = next_health_report(&sink).await;
    assert!(
        paused.automation_paused,
        "an automation pause must push a paused health report, got {paused:?}",
    );
    assert!(
        !paused.dispatch_paused,
        "pausing automation must not flip the independent dispatch_paused flag, got {paused:?}",
    );
    let issue = automation_paused_issue(&paused).expect("a paused report must carry an automation_paused issue");
    assert!(
        issue
            .body
            .contains("operator is holding automation while debugging triage"),
        "the automation pause reason must reach the banner, not just the paused fact: {}",
        issue.body,
    );

    state.execution_coordinator.resume_automation();

    let resumed = next_health_report(&sink).await;
    assert!(
        !resumed.automation_paused,
        "an automation resume must push a cleared health report so the banner \
         disappears without an app restart, got {resumed:?}",
    );
    assert!(
        automation_paused_issue(&resumed).is_none(),
        "a cleared report must not still carry an automation_paused issue: {resumed:?}",
    );
}
