//! Probe delivery: the mid-turn injection decision, and honest reporting.
//!
//! Three defects are covered here. The first two come from the field report
//! where two probes (one of them `--urgent`) were reported queued and then
//! never delivered to a healthy `claude-sonnet-5` worker across ~27 tool
//! boundaries:
//!
//! 1. The tool-boundary path fires on `PostToolUse`, and the event fan-out
//!    sets the activity to `Working` before it runs. A parked-only guard is
//!    therefore false 100% of the time at that boundary, on every driver — so
//!    it could not deliver at all. The fix splits the decision into activity ×
//!    driver, so an interactive-TUI driver takes mid-turn input while
//!    `codex exec` (which leaves unread bytes in the tty for the shell to
//!    execute) is still refused.
//!
//! 2. `ProbeRun` accepted every probe unconditionally, so the CLI could not
//!    distinguish "arriving shortly" from "never going to arrive". The engine
//!    now evaluates delivery up front, refuses what it cannot deliver, states
//!    the boundary it committed to, and exposes a per-probe-id state.
//!
//! 3. Mid-turn delivery was gated behind the caller's `--urgent` flag, so a
//!    plain `bossctl probe` still waited for a `Stop` — which, for a worker in
//!    a long autonomous run, is effectively its terminal one. Transport now
//!    follows the worker's pane posture and the flag is queue priority only;
//!    the expectation tests below pin which posture yields which boundary.

use super::*;

use crate::app::executions;
use crate::protocol::WorkerEvent;

/// A `PostToolUse` hook event for `run_id`, the boundary the mid-turn probe
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

/// The headline regression: a probe to a **working** Claude worker must be
/// written into the pane at the tool boundary. Before the first fix this wrote
/// nothing, on every one of the worker's tool boundaries, because the guard
/// consulted activity alone and the fan-out had just set it to `Working`.
///
/// Queued here with `urgent: false` deliberately: mid-turn delivery is the
/// default now, not something the caller opts into.
#[tokio::test(start_paused = true)]
async fn probe_injects_mid_turn_for_a_working_claude_worker() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, None);
    let responder = app_session_capturing_one_send(&server_state).await;

    let probe_id = server_state.queue_probe(run_id.clone(), "re-read the spec".into(), false);
    dispatch_probe_on_post_tool_use(&server_state, &post_tool_use(&run_id)).await;

    let (slot, text) = responder
        .await
        .expect("app responder task")
        .expect("a probe to a mid-turn Claude worker must issue a SendToPane");
    assert_eq!(slot, 3);
    assert_eq!(
        text, "[coordinator-nudge] re-read the spec",
        "mid-turn probes stay marked in the transcript so the worker and human readers can spot them",
    );
    // Popped from the queue (delivered), not left behind for Stop.
    assert!(
        server_state.pop_pending_probe(&run_id).is_none(),
        "a delivered probe must not remain queued",
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
async fn probe_still_refused_mid_turn_for_a_driver_that_rejects_stdin() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 4, Some("codex"));
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;

    let probe_id = server_state.queue_probe(run_id.clone(), "re-read the spec".into(), false);
    dispatch_probe_on_post_tool_use(&server_state, &post_tool_use(&run_id)).await;

    assert_eq!(
        app_sink.queue_stats().depth,
        0,
        "a mid-turn write to a driver that does not consume stdin must never reach the pane",
    );
    let still = server_state
        .pop_pending_probe(&run_id)
        .expect("refused probe must remain queued for the Stop boundary");
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

/// End to end, and the point of the whole change: `bossctl probe` against a
/// worker in the middle of a turn writes into that worker's composer **during
/// the call**, with no hook event of any kind. No `Stop`, no `PostToolUse` —
/// the test fires neither. This is the same transport the chore-update notice
/// uses, and it is what makes a probe able to steer a worker that is 40
/// minutes into a single autonomous turn instead of reaching it as it exits.
#[tokio::test(start_paused = true)]
async fn probe_run_steers_a_mid_turn_worker_without_any_boundary() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, None);
    let responder = app_session_capturing_one_send(&server_state).await;

    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text: "stop — you are building the wrong target".into(),
            urgent: false,
        },
    )
    .await;
    let probe_id = match sole_response(&sink).await {
        FrontendEvent::ProbeQueued { probe_id, .. } => probe_id,
        other => panic!("expected ProbeQueued, got {other:?}"),
    };

    let (slot, text) = responder
        .await
        .expect("app responder task")
        .expect("a probe to a mid-turn worker must be written during the call, not held for Stop");
    assert_eq!(slot, 3);
    assert_eq!(text, "[coordinator-nudge] stop — you are building the wrong target");
    assert!(
        server_state.pop_pending_probe(&run_id).is_none(),
        "a probe written during the call must not also be left queued for a boundary",
    );
    // The handler issues the write from a spawned task, so let the
    // verification window elapse before reading the settled state.
    let mut settled = None;
    for _ in 0..20 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        settled = server_state.probe_lifecycle_state(&probe_id);
        if settled != Some(ProbeDeliveryState::Injected) {
            break;
        }
    }
    assert_eq!(
        settled,
        Some(ProbeDeliveryState::Buffered),
        "mid-turn text sits in the composer until the agent picks it up — Buffered, not Consumed",
    );
}

/// Ordering: probes drain one per reply cycle. The in-flight slot that carries
/// a pending `ProbeReplied` is single-valued per run, so a second mid-turn
/// delivery before the first probe's reply boundary would silently discard it.
/// The second probe waits instead — and is delivered, in order, once the turn
/// boundary has taken the first one's reply.
#[tokio::test(start_paused = true)]
async fn a_second_probe_waits_for_the_first_probes_reply_cycle() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, None);
    let responder = app_session_capturing_one_send(&server_state).await;

    let first = server_state.queue_probe(run_id.clone(), "first".into(), false);
    let second = server_state.queue_probe(run_id.clone(), "second".into(), false);

    dispatch_probe_on_post_tool_use(&server_state, &post_tool_use(&run_id)).await;
    let (_, text) = responder
        .await
        .expect("app responder task")
        .expect("the first probe must be written at the tool boundary");
    assert_eq!(text, "[coordinator-nudge] first");

    // Second tool boundary while the first probe still owes a reply: no write.
    dispatch_probe_on_post_tool_use(&server_state, &post_tool_use(&run_id)).await;
    assert_eq!(
        server_state.probe_lifecycle_state(&second),
        Some(ProbeDeliveryState::Queued),
        "the second probe must not be delivered while the first is in flight",
    );
    assert!(server_state.has_pending_probe(&run_id));
    assert!(server_state.has_in_flight_probe(&run_id));

    // The turn boundary takes the first probe's reply, freeing the slot.
    let stop = crate::events_socket::IncomingHookEvent::for_test(
        WorkerEvent::Stop {
            session_id: "claude-sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
        Some(run_id.clone()),
        None,
    );
    dispatch_probe_reply_on_stop(&server_state, &stop).await;
    assert!(!server_state.has_in_flight_probe(&run_id));
    assert_ne!(first, second);

    // Next tool boundary of the following turn delivers the second, in order.
    let responder = app_session_capturing_one_send(&server_state).await;
    dispatch_probe_on_post_tool_use(&server_state, &post_tool_use(&run_id)).await;
    let (_, text) = responder
        .await
        .expect("app responder task")
        .expect("the second probe must be delivered once the reply cycle completed");
    assert_eq!(text, "[coordinator-nudge] second");
}

/// The `[effort-escalation-ack]` protocol must keep working unchanged: a
/// worker that emitted `[effort-escalation]` in its final response sits parked
/// at its prompt, producing no further boundary of its own. The ack is written
/// straight in — verbatim, with no `[coordinator-nudge]` marking, since a
/// parked write becomes the prompt itself — and recorded as `Consumed`.
#[tokio::test]
async fn effort_escalation_ack_reaches_a_parked_worker_immediately() {
    let (server_state, _dir) = test_server_state();
    let run_id = "run-escalation-ack";
    register_idle_worker(&server_state, run_id, 8);
    let responder = app_session_capturing_one_send(&server_state).await;

    let ack = "[effort-escalation-ack] approved: large. next_dispatch=true";
    let probe_id = server_state.queue_probe(run_id.to_owned(), ack.to_owned(), false);
    dispatch_probe_now(&server_state, run_id).await;

    let (slot, text) = responder
        .await
        .expect("app responder task")
        .expect("a parked worker has no boundary coming, so the ack must be written immediately");
    assert_eq!(slot, 8);
    assert_eq!(text, ack, "the ack must reach the worker verbatim");
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Consumed),
    );
}

// ── Honest reporting ────────────────────────────────────────────────────────

/// A probe for a run with no live pane can never be delivered, so it must be
/// refused rather than reported queued — a queued report is indistinguishable
/// from one that is about to arrive.
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

/// A worker whose driver cannot take mid-turn input is **accepted**, not
/// refused, even with `--urgent`: the flag is queue priority now, so it never
/// promises a boundary the driver cannot honour. What the caller gets instead
/// is an expectation that names the boundary the probe really will wait for.
/// Refusal stays scoped to probes that will never arrive at all.
#[tokio::test]
async fn probe_run_accepts_urgent_for_a_driver_that_rejects_mid_turn_input() {
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
        FrontendEvent::ProbeQueued {
            urgent,
            expected_delivery,
            ..
        } => {
            assert!(urgent, "the priority flag still round-trips");
            assert_eq!(
                expected_delivery,
                Some(ProbeDeliveryExpectation::NextTurnBoundary),
                "a driver that reads no mid-turn stdin has no earlier boundary to offer",
            );
        }
        other => panic!("expected ProbeQueued, got {other:?}"),
    }
    assert!(
        server_state.pop_pending_probe(&run_id).is_some(),
        "an accepted probe must be queued for the boundary it was promised",
    );
}

/// A `Spawning` worker is accepted too, and — because its driver buffers
/// mid-turn input — promised the *tool* boundary rather than the turn
/// boundary: the first `PostToolUse` of its first turn can deliver it. This
/// is the one case where the engine cannot write during the call but still
/// has something earlier than `Stop` to offer.
#[tokio::test]
async fn probe_run_promises_the_tool_boundary_for_a_spawning_buffering_worker() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_spawning_worker_with_driver(&server_state, 4, None);
    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id,
            text: "are you up yet".into(),
            urgent: true,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeQueued { expected_delivery, .. } => {
            assert_eq!(expected_delivery, Some(ProbeDeliveryExpectation::NextToolBoundary));
        }
        other => panic!("expected ProbeQueued, got {other:?}"),
    }
}

/// A `Spawning` worker whose driver rejects mid-turn input has no tool
/// boundary to aim at either, so it is promised the turn boundary. Same
/// acceptance, honestly weaker promise.
#[tokio::test]
async fn probe_run_promises_the_turn_boundary_for_a_spawning_non_buffering_worker() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_spawning_worker_with_driver(&server_state, 4, Some("codex"));
    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id,
            text: "no rush".into(),
            urgent: false,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeQueued { expected_delivery, .. } => {
            assert_eq!(expected_delivery, Some(ProbeDeliveryExpectation::NextTurnBoundary));
        }
        other => panic!("expected ProbeQueued, got {other:?}"),
    }
}

/// The headline reporting change: a plain (non-urgent) probe against a
/// mid-turn Claude worker is promised **immediate** delivery, because the
/// engine writes into the agent's composer during the call rather than
/// holding the text for a `Stop` that a long autonomous turn never reaches.
#[tokio::test]
async fn probe_run_promises_immediate_delivery_against_a_mid_turn_claude_worker() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 2, None);
    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text: "course-correct now".into(),
            urgent: false,
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
            assert!(!urgent, "no priority flag was set");
            assert_eq!(
                expected_delivery,
                Some(ProbeDeliveryExpectation::Immediate),
                "a mid-turn worker on a buffering driver is written to during the call",
            );
            assert!(
                server_state.probe_lifecycle_state(&probe_id).is_some(),
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

/// `ProbeStatus` reports the state the delivery path recorded, so a probe that
/// landed is distinguishable from one that is still waiting.
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
