//! Probe delivery: the mid-turn injection decision, and honest reporting.
//!
//! Two defects are covered here, both from the field report where two probes
//! (one of them `--urgent`) were reported queued and then never delivered to a
//! healthy `claude-sonnet-5` worker across ~27 tool boundaries:
//!
//! 1. The urgent path fires on `PostToolUse`, and the event fan-out sets the
//!    activity to `Working` before it runs. A parked-only guard is therefore
//!    false 100% of the time at that boundary, on every driver — `--urgent`
//!    could not deliver at all. The fix splits the decision into activity ×
//!    driver, so an interactive-TUI driver takes mid-turn input while
//!    `codex exec` (which leaves unread bytes in the tty for the shell to
//!    execute) is still refused.
//!
//! 2. `ProbeRun` accepted every probe unconditionally, so the CLI could not
//!    distinguish "arriving shortly" from "never going to arrive". The engine
//!    now evaluates delivery up front, refuses what it cannot deliver, states
//!    the boundary it committed to, and exposes a per-probe-id state.

use super::*;

use crate::app::executions;
use crate::protocol::WorkerEvent;

/// A `PostToolUse` hook event for `run_id`, the boundary the urgent probe
/// path fires on.
fn post_tool_use(run_id: &str) -> crate::events_socket::IncomingHookEvent {
    crate::events_socket::IncomingHookEvent::for_test(
        WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
        Some(run_id.to_owned()),
        None,
    )
}

/// Register a fake app session, answer the first `SendToPane` it receives with
/// success, and return the `(slot_id, text)` that reached the pane. Mirrors the
/// responder in `worker_probe_dispatch`, but reports the payload back so tests
/// can assert on what was actually written rather than only that *something*
/// was.
async fn app_session_capturing_one_send(
    server_state: &Arc<ServerState>,
) -> tokio::task::JoinHandle<Option<(u8, String)>> {
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    let server_for_app = server_state.clone();
    tokio::spawn(async move {
        let envelope = app_sink.next().await?;
        let (request_id, captured) = match &envelope.payload {
            FrontendEvent::EngineRequest {
                request_id,
                request: EngineToAppRequest::SendToPane(input),
            } => (request_id.clone(), (input.slot_id, input.text.clone())),
            other => panic!("expected an EngineRequest carrying SendToPane, got {other:?}"),
        };
        server_for_app
            .deliver_app_response(
                "session-app",
                &request_id,
                EngineToAppResponse::SendToPane {
                    result: Ok(crate::protocol::SendToPaneResult {}),
                },
            )
            .await;
        Some(captured)
    })
}

fn dispatch_for(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
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
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

// ── The delivery fix ────────────────────────────────────────────────────────

/// The headline regression: an urgent probe to a **working** Claude worker
/// must be written into the pane at the tool boundary. Before the fix this
/// wrote nothing, on every one of the worker's tool boundaries, because the
/// guard consulted activity alone and the fan-out had just set it to
/// `Working`.
#[tokio::test(start_paused = true)]
async fn urgent_probe_injects_mid_turn_for_a_working_claude_worker() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, None);
    let responder = app_session_capturing_one_send(&server_state).await;

    let probe_id = server_state.queue_probe(run_id.clone(), "re-read the spec".into(), true);
    dispatch_urgent_probe_on_post_tool_use(&server_state, &post_tool_use(&run_id)).await;

    let (slot, text) = responder
        .await
        .expect("app responder task")
        .expect("an urgent probe to a mid-turn Claude worker must issue a SendToPane");
    assert_eq!(slot, 3);
    assert_eq!(
        text, "[coordinator-nudge] re-read the spec",
        "urgent probes stay marked in the transcript so the worker and human readers can spot them",
    );
    // Popped from the queue (delivered), not left behind for Stop.
    assert!(
        server_state.pop_pending_probe(&run_id).is_none(),
        "a delivered urgent probe must not remain queued",
    );
    // No UserPromptSubmit arrives while the agent is still mid-turn, and
    // there is no transcript to scan — `Buffered` is the honest outcome, and
    // notably NOT `Unconfirmed`.
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Buffered),
    );
}

/// The safety half: the same mid-turn boundary on a `codex` worker must still
/// write nothing. `codex exec` runs one turn per process with stdin on
/// `/dev/null`, so injected bytes would survive in the tty and be executed by
/// the shell after it exits. The probe stays queued for the Stop boundary.
#[tokio::test]
async fn urgent_probe_still_refused_mid_turn_for_a_driver_that_rejects_stdin() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 4, Some("codex"));
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;

    let probe_id = server_state.queue_probe(run_id.clone(), "re-read the spec".into(), true);
    dispatch_urgent_probe_on_post_tool_use(&server_state, &post_tool_use(&run_id)).await;

    assert_eq!(
        app_sink.queue_stats().depth,
        0,
        "a mid-turn write to a driver that does not consume stdin must never reach the pane",
    );
    let still = server_state
        .pop_pending_probe(&run_id)
        .expect("refused urgent probe must remain queued for the Stop boundary");
    assert_eq!(still.probe_id, probe_id);
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Queued),
    );
}

/// Fail closed when the run's driver cannot be resolved at all (no execution
/// row, unregistered slug, DB error). An unresolvable driver is exactly the
/// case where the tty-leak cannot be ruled out, so it must be treated as
/// rejecting mid-turn input rather than as the common case.
#[tokio::test]
async fn mid_turn_posture_fails_closed_when_the_driver_cannot_be_resolved() {
    let (server_state, _dir) = test_server_state();
    // Bare run id: registered as a slot, but with no execution row behind it.
    register_working_worker(&server_state, "run-no-execution-row", 6);
    assert_eq!(
        server_state.pane_input_posture_for_run("run-no-execution-row", 6),
        PaneInputPosture::Refused,
    );
    assert!(
        !server_state.run_mid_turn_pane_input("run-no-execution-row").buffers(),
        "an unresolvable driver must not be treated as buffering mid-turn input",
    );
}

/// A parked worker is injectable under every driver — the driver only enters
/// the decision mid-turn. Asserted for `codex` specifically, so a future
/// tightening of the mid-turn rule cannot accidentally lock out the ordinary
/// Stop-boundary path.
#[tokio::test]
async fn parked_workers_are_injectable_regardless_of_driver() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 7, Some("codex"));
    // Flip the same slot to parked-at-prompt.
    server_state.live_worker_states.apply_event(
        7,
        &WorkerEvent::Stop {
            session_id: "test-sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    assert_eq!(
        server_state.pane_input_posture_for_run(&run_id, 7),
        PaneInputPosture::Parked,
    );
}

// ── Honest reporting ────────────────────────────────────────────────────────

/// A probe for a run with no live pane cannot be delivered, so it must be
/// refused rather than accepted. This is the case that burned the operator:
/// the CLI reported "queued" and there was no way to learn otherwise.
#[tokio::test]
async fn probe_run_refuses_when_the_run_has_no_live_pane() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id: "run-with-no-pane".into(),
            text: "hello?".into(),
            urgent: false,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeRefused { run_id, reason } => {
            assert_eq!(run_id, "run-with-no-pane");
            assert!(
                reason.contains("no live worker pane"),
                "refusal must name the blocking condition: {reason}"
            );
        }
        other => panic!("expected ProbeRefused, got {other:?}"),
    }
}

/// `--urgent` promises tool-boundary delivery. Against a driver that cannot
/// take mid-turn input that promise is unkeepable, so the engine refuses
/// instead of silently downgrading to Stop-boundary delivery under a flag that
/// says otherwise — and the reason tells the operator what to do instead.
#[tokio::test]
async fn probe_run_refuses_urgent_for_a_driver_that_rejects_mid_turn_input() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 2, Some("codex"));
    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text: "course-correct now".into(),
            urgent: true,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeRefused { reason, .. } => {
            assert!(
                reason.contains("mid-turn") && reason.contains("--urgent"),
                "refusal must explain why urgent cannot be honoured and what to do: {reason}"
            );
        }
        other => panic!("expected ProbeRefused, got {other:?}"),
    }
    // Nothing was queued: a refused probe must not leave state behind that a
    // later boundary could deliver.
    assert!(server_state.pop_pending_probe(&run_id).is_none());
}

/// The same worker accepts a *non-urgent* probe, because waiting for the next
/// turn boundary is a promise the engine can keep. Refusal must be scoped to
/// genuinely undeliverable probes, not to "the worker is busy".
#[tokio::test]
async fn probe_run_accepts_non_urgent_against_a_working_worker_with_a_turn_boundary_promise() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 2, Some("codex"));
    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text: "check in later".into(),
            urgent: false,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeQueued {
            urgent,
            expected_delivery,
            ..
        } => {
            assert!(!urgent);
            assert_eq!(expected_delivery, Some(ProbeDeliveryExpectation::NextTurnBoundary));
        }
        other => panic!("expected ProbeQueued, got {other:?}"),
    }
}

/// An urgent probe to a mid-turn Claude worker is accepted *and* the engine
/// says which boundary it committed to. The old response carried only the
/// echoed `urgent` flag, which is what let the CLI print "will inject at next
/// tool boundary" for a path that could never fire.
#[tokio::test]
async fn probe_run_accepts_urgent_against_a_claude_worker_with_a_tool_boundary_promise() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 2, None);
    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text: "course-correct now".into(),
            urgent: true,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeQueued {
            urgent,
            expected_delivery,
            probe_id,
            ..
        } => {
            assert!(urgent, "urgent probe must echo urgent: true");
            assert_eq!(expected_delivery, Some(ProbeDeliveryExpectation::NextToolBoundary));
            assert_eq!(
                server_state.probe_lifecycle_state(&probe_id),
                Some(ProbeDeliveryState::Queued),
                "an accepted probe must be queryable from the moment it is accepted",
            );
        }
        other => panic!("expected ProbeQueued, got {other:?}"),
    }
}

/// A terminated worker will never read anything again, so probing it is
/// refused even without the `--urgent` flag.
#[tokio::test]
async fn probe_run_refuses_a_terminal_worker() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 2, None);
    server_state.live_worker_states.apply_event(
        2,
        &WorkerEvent::SessionEnd {
            session_id: "test-sess".into(),
            reason: "exit".into(),
        },
    );
    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text: "anyone home?".into(),
            urgent: false,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeRefused { reason, .. } => {
            assert!(
                reason.contains("terminated"),
                "refusal must name the terminal state: {reason}"
            );
        }
        other => panic!("expected ProbeRefused, got {other:?}"),
    }
}

/// `ProbeStatus` reports the state the delivery path recorded, so an operator
/// can tell a probe that landed from one that is still waiting.
#[tokio::test]
async fn probe_status_reports_the_recorded_delivery_state() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 5, None);
    let probe_id = server_state.queue_probe(run_id.clone(), "status me".into(), true);

    let sink = make_session_sink();
    executions::handle_probe_status(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeStatus {
            probe_id: probe_id.clone(),
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeStatusResult {
            run_id: reported_run,
            probe_id: reported_id,
            state,
            urgent,
            detail,
        } => {
            assert_eq!(reported_run, run_id);
            assert_eq!(reported_id, probe_id);
            assert_eq!(state, ProbeDeliveryState::Queued);
            assert!(urgent, "the queued urgency must round-trip to the status answer");
            assert_eq!(detail, None);
        }
        other => panic!("expected ProbeStatusResult, got {other:?}"),
    }

    // Advance the lifecycle the way the delivery path does, with a note, and
    // confirm both are reported.
    server_state.set_probe_lifecycle_detail(
        &probe_id,
        ProbeDeliveryState::Unconfirmed,
        Some("no UserPromptSubmit observed".to_owned()),
    );
    let sink = make_session_sink();
    executions::handle_probe_status(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeStatus {
            probe_id: probe_id.clone(),
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeStatusResult { state, detail, .. } => {
            assert_eq!(state, ProbeDeliveryState::Unconfirmed);
            assert_eq!(detail.as_deref(), Some("no UserPromptSubmit observed"));
        }
        other => panic!("expected ProbeStatusResult, got {other:?}"),
    }
}

/// An unknown probe id is an error, not a fabricated state. Probe ids are
/// minted per engine process, so "unknown" covers both a typo and an id from
/// before the last restart — either way, reporting a state would be a guess.
#[tokio::test]
async fn probe_status_errors_for_an_unknown_probe_id() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    executions::handle_probe_status(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeStatus {
            probe_id: "probe-does-not-exist".into(),
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("unknown probe id"),
                "status for an unknown id must say so: {message}"
            );
        }
        other => panic!("expected WorkError, got {other:?}"),
    }
}

/// A lifecycle transition for an id that was never queued is dropped rather
/// than inventing a record: `run_id`/`urgent` are only knowable at queue time,
/// and a status answer that guessed them would be worse than "unknown".
#[tokio::test]
async fn lifecycle_transition_for_an_unqueued_id_creates_no_record() {
    let (server_state, _dir) = test_server_state();
    server_state.set_probe_lifecycle("probe-never-queued", ProbeDeliveryState::Consumed);
    assert_eq!(server_state.probe_lifecycle_state("probe-never-queued"), None);
}
