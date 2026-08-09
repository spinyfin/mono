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
//!    driver, so a driver measured to buffer mid-turn input takes it while one
//!    that has not measured it (which could leave unread bytes in the tty for
//!    the shell to execute) is still refused.
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
//!
//! A fourth concern joins them here: a driver that buffers mid-turn input may
//! **fold** the delivered prompt into the turn that is already running rather
//! than starting a new one for it. Codex's TUI does, measured. Such a prompt
//! is acted on but produces no turn boundary of its own, so the tests below
//! also pin that the probe machinery reaches its reply on the *single*
//! boundary that turn emits rather than waiting for a second one that never
//! comes.

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

/// The same delivery, on Codex. This is what flipping
/// `CodexDriver::mid_turn_pane_input` to `Buffers` buys: before it, a probe
/// against a mid-turn Codex worker wrote nothing at every tool boundary and
/// waited for a `Stop` that a long autonomous turn may never reach.
#[tokio::test(start_paused = true)]
async fn probe_injects_mid_turn_for_a_working_codex_worker() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, Some("codex"));
    let responder = app_session_capturing_one_send(&server_state).await;

    let probe_id = server_state.queue_probe(run_id.clone(), "re-read the spec".into(), false);
    dispatch_probe_on_post_tool_use(&server_state, &post_tool_use(&run_id)).await;

    let (slot, text) = responder
        .await
        .expect("app responder task")
        .expect("a probe to a mid-turn Codex worker must issue a SendToPane");
    assert_eq!(slot, 3);
    assert_eq!(text, "[coordinator-nudge] re-read the spec");
    assert!(
        server_state.pop_pending_probe(&run_id).is_none(),
        "a delivered probe must not remain queued",
    );
    // `Buffered`, and it can never be anything better on this driver: Codex's
    // progress normaliser mints `UserPromptSubmit` from `task_started` alone,
    // with an empty prompt, so the delivery waiter has nothing to match — and
    // a folded prompt never starts a turn, so no `task_started` follows it
    // either. The transcript scan is the only channel that could confirm, and
    // `Buffered` is the honest answer until it does.
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Buffered),
    );
}

/// The safety half: the same mid-turn boundary on a driver that has not
/// measured its mid-turn stdin behaviour must still write nothing. `grok` is
/// that driver today — it deliberately leaves `mid_turn_pane_input` at the
/// trait default (`Rejects`) rather than flipping it on a structural argument
/// — so injected bytes could survive in the tty and be executed by the shell.
/// The probe stays queued for the Stop boundary.
///
/// This used to assert against `codex`, which has since *measured* that its
/// TUI buffers mid-turn input and now declares `Buffers`. The property under
/// test is unchanged; only the driver that still has it has moved.
#[tokio::test]
async fn probe_still_refused_mid_turn_for_a_driver_that_rejects_stdin() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 4, Some("grok"));
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
    let run_id = register_working_worker_with_driver(&server_state, 2, Some("grok"));
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
    let run_id = register_spawning_worker_with_driver(&server_state, 4, Some("grok"));
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

// ── The accepted commitment is honoured or visibly broken ───────────────────
//
// A probe accepted with an `expected_delivery` commitment was silently
// dropped at the boundary it named: it left the pending queue without a
// lifecycle transition and without a log line, so it reported `queued`
// against a run whose pane had already been reaped, forever. The tests below
// pin the three properties that make that impossible to repeat — leaving the
// queue always settles the record, a dying run settles what it leaves behind,
// and every exit from the drain path is named.

/// A `Stop`/turn-boundary hook event for `run_id`.
fn stop_event(run_id: &str) -> crate::events_socket::IncomingHookEvent {
    crate::events_socket::IncomingHookEvent::for_test(
        WorkerEvent::Stop {
            session_id: "claude-sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
        Some(run_id.to_owned()),
        None,
    )
}

/// A pid that is definitely not a live process: spawn a trivial child, reap
/// it, and hand back its pid. Reaped means the kernel has released the entry,
/// so `kill(pid, 0)` answers `ESRCH` — the same verdict the engine would get
/// for a worker whose process exited out from under its pane.
fn a_definitely_dead_pid() -> i32 {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawning a trivial child must succeed");
    let pid = child.id() as i32;
    child.wait().expect("waiting on the trivial child must succeed");
    assert!(
        matches!(
            crate::dead_pid_sweep::probe_pid(pid),
            crate::dead_pid_sweep::PidStatus::Dead
        ),
        "fixture precondition: a reaped child's pid must probe dead",
    );
    pid
}

/// The headline regression. A probe queued against a run whose pane is then
/// released must end at a terminal state — never `queued`, which is a live
/// promise that nothing is left to keep. Before the fix nothing swept the
/// pending queue on teardown, so the record sat at `queued` against a dead
/// run indefinitely and `bossctl probe-status` reported delivery as pending.
#[tokio::test]
async fn a_probe_outliving_its_run_is_abandoned_not_left_queued() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, None);
    let probe_id = server_state.queue_probe(run_id.clone(), "still there?".into(), false);
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Queued),
    );

    // The run ends: its pane is torn down and its slot freed. No app session
    // is registered, which is the same shape as a teardown the app cannot be
    // reached for.
    server_state.release_worker_pane(&run_id).await;

    let state = server_state
        .probe_lifecycle_state(&probe_id)
        .expect("the probe record must survive the run it targeted");
    assert_eq!(
        state,
        ProbeDeliveryState::Abandoned,
        "a probe whose run died before its delivery boundary must be reported abandoned",
    );
    assert!(state.is_terminal() && state.is_undeliverable());
    assert!(
        !state.is_delivered(),
        "an abandoned probe was never delivered and must not claim to be",
    );
    let record = server_state
        .probe_record(&probe_id)
        .expect("probe record must still be queryable");
    assert!(
        record.detail.is_some_and(|d| d.contains("pane released")),
        "the terminal record must explain how the probe got there",
    );
    assert_eq!(
        server_state.pending_probe_count(&run_id),
        0,
        "the drained queue must not leave the probe behind for a boundary that will never come",
    );
}

/// The same guarantee for a probe queued before the worker ever mapped a slot
/// — a spawning worker that dies during startup. The drain runs ahead of the
/// no-slot early return in `release_worker_pane` precisely so this case is not
/// a hole: `dispatch_probe_if_idle` deliberately leaves probes queued when
/// there is no slot yet, so this is exactly where one can be stranded.
#[tokio::test]
async fn a_probe_queued_before_a_slot_existed_is_still_settled_when_the_run_dies() {
    let (server_state, _dir) = test_server_state();
    let probe_id = server_state.queue_probe("run-that-never-spawned".into(), "hello?".into(), false);

    let outcome = server_state.release_worker_pane("run-that-never-spawned").await;
    assert_eq!(
        outcome,
        PaneReleaseOutcome::NoLiveWorker,
        "fixture precondition: this run never mapped a slot",
    );
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Abandoned),
        "a probe must not survive the run it was addressed to, slot mapping or not",
    );
}

/// The root cause of the silent drop: the completion handler's discard of a
/// stale queued nudge removed the probe from the queue while leaving the
/// lifecycle table reading `queued`. Discarding is legitimate — reporting it
/// as still-on-its-way is not. The discard must land on `Dropped` and carry
/// the reason it was discarded for.
#[tokio::test]
async fn clearing_a_stale_queued_probe_records_that_it_was_dropped_and_why() {
    use crate::completion::ProbeQueuer;

    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, None);
    let probe_id = server_state.queue_probe(run_id.clone(), "produce a PR".into(), false);

    // Go through the production adapter the completion handler holds, not the
    // inherent method, so the wiring is covered too.
    let queuer = crate::app::probes::ServerStateProbeQueuer::default();
    queuer.set_server_state(Arc::downgrade(&server_state));
    queuer.clear_pending_probes(&run_id, "worker reported [blocked]");

    let record = server_state
        .probe_record(&probe_id)
        .expect("a discarded probe must remain queryable — that is the whole point");
    assert_eq!(
        record.state,
        ProbeDeliveryState::Dropped,
        "a discarded probe must not keep reporting `queued`",
    );
    assert!(
        record.detail.is_some_and(|d| d.contains("[blocked]")),
        "the discard reason must reach the status record, not just the log",
    );
    assert_eq!(server_state.pending_probe_count(&run_id), 0);
}

/// `consumed` must mean consumed. `SendToPane` returning `Ok` only proves the
/// app wrote bytes into the pty; a pane whose foreground process has already
/// exited accepts them with nobody reading. That is how a probe injected into
/// a dead `codex` pane came to be reported `consumed`, which made the state
/// useless as evidence that a worker had seen anything.
#[tokio::test]
async fn a_write_into_a_pane_whose_process_is_gone_is_not_recorded_as_consumed() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, None);
    // Park the worker so the Stop-path posture guard permits the write, then
    // point its live state at a pid that is definitely gone — the pane is
    // still there and still accepts bytes, but nothing is reading them.
    server_state.live_worker_states.apply_event(
        3,
        &WorkerEvent::Stop {
            session_id: "test-sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    server_state
        .live_worker_states
        .update_shell_pid(&run_id, a_definitely_dead_pid())
        .expect("fixture precondition: the run must have a live-state entry");

    let responder = app_session_capturing_one_send(&server_state).await;
    let probe_id = server_state.queue_probe(run_id.clone(), "anyone home?".into(), false);
    let outcome = dispatch_probe_on_stop(&server_state, &stop_event(&run_id)).await;

    responder
        .await
        .expect("app responder task")
        .expect("the write is still issued — the engine cannot know it is unread until it checks");
    assert_eq!(outcome, ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Orphaned));
    let state = server_state
        .probe_lifecycle_state(&probe_id)
        .expect("probe must have a record");
    assert_eq!(
        state,
        ProbeDeliveryState::Orphaned,
        "a write into a pane with no live process must not be reported as consumed",
    );
    assert!(
        !state.is_delivered(),
        "`delivered` is what a caller trusts; an orphaned write delivered nothing",
    );
}

/// The liveness check is deliberately one-sided: it downgrades only on a
/// positive dead verdict. A live worker — and the test fixtures' unreported
/// pid `0`, where liveness is simply unknowable — must keep the honest
/// `Consumed`, or every probe in the fleet would be libelled as orphaned.
#[tokio::test]
async fn a_live_worker_still_records_consumed() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, None);
    server_state.live_worker_states.apply_event(
        3,
        &WorkerEvent::Stop {
            session_id: "test-sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    // This process is unambiguously alive.
    server_state
        .live_worker_states
        .update_shell_pid(&run_id, std::process::id() as i32)
        .expect("fixture precondition: the run must have a live-state entry");

    let responder = app_session_capturing_one_send(&server_state).await;
    let probe_id = server_state.queue_probe(run_id.clone(), "still with me?".into(), false);
    let outcome = dispatch_probe_on_stop(&server_state, &stop_event(&run_id)).await;

    responder.await.expect("app responder task").expect("SendToPane issued");
    assert_eq!(outcome, ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Consumed));
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Consumed),
    );
}

/// No silent exits. Each way out of the Stop drain path answers with a
/// distinct outcome, so a trace (and a test) can tell "nothing was queued"
/// from "something was queued and could not be delivered" — the distinction
/// that was missing when the dropped probe had to be diagnosed, where both
/// cases produced an identical empty trace.
#[tokio::test]
async fn every_exit_from_the_stop_drain_path_names_itself() {
    let (server_state, _dir) = test_server_state();

    // Not a delivery boundary for this path at all.
    assert_eq!(
        dispatch_probe_on_stop(&server_state, &post_tool_use("run-anything")).await,
        ProbeDispatchOutcome::NotADeliveryBoundary,
    );

    // A boundary with no run id: nothing to look a queue up by.
    let anonymous = crate::events_socket::IncomingHookEvent::for_test(
        WorkerEvent::Stop {
            session_id: "sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
        None,
        None,
    );
    assert_eq!(
        dispatch_probe_on_stop(&server_state, &anonymous).await,
        ProbeDispatchOutcome::NoRunId,
    );

    // The common case, and the one that used to be indistinguishable from a
    // probe that had gone missing.
    let idle_run = register_working_worker_with_driver(&server_state, 3, None);
    server_state.live_worker_states.apply_event(
        3,
        &WorkerEvent::Stop {
            session_id: "test-sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    assert_eq!(
        dispatch_probe_on_stop(&server_state, &stop_event(&idle_run)).await,
        ProbeDispatchOutcome::NothingQueued,
    );

    // Queued, but the run has no pane to write into. The probe stays put.
    let orphan_probe = server_state.queue_probe("run-with-no-slot".into(), "hi".into(), false);
    assert_eq!(
        dispatch_probe_on_stop(&server_state, &stop_event("run-with-no-slot")).await,
        ProbeDispatchOutcome::NoSlotMapping,
    );
    assert_eq!(
        server_state.probe_lifecycle_state(&orphan_probe),
        Some(ProbeDeliveryState::Queued),
        "an undeliverable-for-now probe is still genuinely queued; only a dead run settles it",
    );

    // Queued against a slot whose posture forbids a write (mid-turn on a
    // driver whose mid-turn stdin behaviour is unmeasured, so it holds the
    // `Rejects` default). Fail closed, probe stays queued.
    let unmeasured_run = register_working_worker_with_driver(&server_state, 4, Some("grok"));
    let deferred_probe = server_state.queue_probe(unmeasured_run.clone(), "hi".into(), false);
    assert_eq!(
        dispatch_probe_on_stop(&server_state, &stop_event(&unmeasured_run)).await,
        ProbeDispatchOutcome::PostureRefused,
    );
    assert_eq!(
        server_state.probe_lifecycle_state(&deferred_probe),
        Some(ProbeDeliveryState::Queued),
    );
}

// ── The folded turn ─────────────────────────────────────────────────────────

/// Attach a `work_runs` row carrying `content` as this execution's transcript,
/// so `transcript_path_for_execution` resolves and the probe-reply read has
/// real bytes to work from. Returns the path; the `TempDir` must outlive it.
fn attach_transcript(server_state: &ServerState, execution_id: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(&path, content).unwrap();
    server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution_id.to_owned(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: Some(path.display().to_string()),
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();
    (dir, path)
}

/// The folded turn, end to end, and the reason `mid_turn_pane_input` could not
/// be flipped as a declaration on its own.
///
/// A probe is injected into a mid-turn Codex worker. Codex buffers it, folds
/// it into the turn that was already running, answers it there, and emits
/// **one** turn boundary for the two prompts — the rollout carries two
/// `user_message` records but a single `task_started`/`task_complete` pair
/// (codex-tui-pivot-pricing V4).
///
/// So the whole reply cycle has to complete on that single boundary. This
/// pins both halves:
///
/// * `ProbeReplied` carries the answer to the *folded* prompt, read out of
///   the rollout dialect. Before the probe-reply read went through the run's
///   driver it used a Claude-shaped scan, which matches no rollout record —
///   the probe was delivered, answered, and still never replied.
/// * the in-flight slot is released by that same boundary, so the next queued
///   probe is not stranded behind a second boundary that a folding driver
///   never produces.
#[tokio::test(start_paused = true)]
async fn a_folded_codex_turn_completes_the_probe_cycle_on_its_single_boundary() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, Some("codex"));
    // The rollout as it stands mid-turn: the session is open, the turn has
    // started, and the worker has produced prose for the original prompt.
    let (_transcript_dir, transcript_path) = attach_transcript(
        &server_state,
        &run_id,
        concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-1"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"working on the original ask"}}"#,
            "\n",
        ),
    );

    let observer = "session-folded-probe-observer".to_owned();
    let sink = make_session_sink();
    server_state
        .topic_broker
        .register_session(&observer, sink.clone())
        .await;
    server_state
        .topic_broker
        .subscribe(&observer, &[probe_topic(&run_id)])
        .await;

    let responder = app_session_capturing_one_send(&server_state).await;
    let probe_id = server_state.queue_probe(run_id.clone(), "what is your status?".into(), false);
    dispatch_probe_on_post_tool_use(&server_state, &post_tool_use(&run_id)).await;
    responder
        .await
        .expect("app responder task")
        .expect("the mid-turn probe must reach the pane");
    assert!(
        server_state.has_in_flight_probe(&run_id),
        "a delivered probe holds the run's single reply slot until a boundary clears it",
    );

    // Codex delivers the buffered prompt at its next tool-call boundary and
    // answers it *inside* the running turn — one `task_complete` for both
    // prompts. Note there is no second `task_started`: that absence is the
    // whole caveat.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&transcript_path).unwrap();
        for line in [
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"[coordinator-nudge] what is your status?"}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"status: pushed, waiting on CI"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"status: pushed, waiting on CI"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ] {
            writeln!(file, "{line}").unwrap();
        }
    }

    dispatch_probe_reply_on_stop(&server_state, &stop_event(&run_id)).await;

    let envelope = sink.next().await.expect("ProbeReplied must be published");
    match envelope.payload {
        FrontendEvent::ProbeReplied {
            run_id: emitted_run,
            probe_id: emitted_probe,
            text,
        } => {
            assert_eq!(emitted_run, run_id);
            assert_eq!(emitted_probe, probe_id);
            assert_eq!(
                text, "status: pushed, waiting on CI",
                "the reply must be the answer to the folded prompt, read out of the rollout dialect",
            );
        }
        other => panic!("expected ProbeReplied, got {other:?}"),
    }
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Replied),
    );
    assert!(
        !server_state.has_in_flight_probe(&run_id),
        "the single boundary a folded turn produces must release the reply slot; waiting for a \
         second one would strand every later probe for this run",
    );
}

/// The queue is non-empty at the pre-pop peek but empty at the pop: another
/// dispatch path already holds the run's single delivery slot. Simulated
/// deterministically by claiming that slot directly before driving the Stop
/// path, rather than relying on real concurrency.
#[tokio::test]
async fn stop_drain_names_raced_to_empty_when_another_path_holds_the_slot() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 5, None);
    server_state.live_worker_states.apply_event(
        5,
        &WorkerEvent::Stop {
            session_id: "test-sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );

    let first = server_state.queue_probe(run_id.clone(), "first".into(), false);
    let second = server_state.queue_probe(run_id.clone(), "second".into(), false);

    // Another dispatch path already claimed the run's only delivery slot.
    let claimed = server_state
        .try_reserve_probe_for_delivery(&run_id, None, 0)
        .expect("claiming the first probe must succeed");
    assert_eq!(claimed.probe_id, first);

    let outcome = dispatch_probe_on_stop(&server_state, &stop_event(&run_id)).await;
    assert_eq!(outcome, ProbeDispatchOutcome::RacedToEmpty);
    assert_eq!(
        server_state.probe_lifecycle_state(&second),
        Some(ProbeDeliveryState::Queued),
        "the probe that lost the race stays queued rather than being lost",
    );
}

/// A pane write that fails (no app session registered, so `SendToPane`
/// errors immediately) must push the probe back to the front of the queue
/// with its id intact and report `RequeuedAfterFailure`, rather than exiting
/// silently — a probe that is popped and then lost is indistinguishable in
/// the trace from one that was never queued.
#[tokio::test]
async fn stop_drain_requeues_after_a_failed_pane_write() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 6, None);
    server_state.live_worker_states.apply_event(
        6,
        &WorkerEvent::Stop {
            session_id: "test-sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    // Deliberately no app session registered, so `SendToPane` fails fast
    // with `SendToAppError::NotRegistered` instead of timing out.
    let probe_id = server_state.queue_probe(run_id.clone(), "still there?".into(), false);

    let outcome = dispatch_probe_on_stop(&server_state, &stop_event(&run_id)).await;

    assert_eq!(outcome, ProbeDispatchOutcome::RequeuedAfterFailure);
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Queued),
        "a failed write returns the probe to Queued, not stuck at Injected",
    );
    assert!(
        !server_state.has_in_flight_probe(&run_id),
        "the failed claim must free the run's delivery slot",
    );
    let requeued = server_state
        .pop_pending_probe(&run_id)
        .expect("probe must be back on the queue");
    assert_eq!(
        requeued.probe_id, probe_id,
        "the requeued probe keeps its original id across the retry",
    );
}

/// If the run's slot mapping is already gone by the time a failed pane write
/// tries to requeue (the run was released — e.g. the pane was torn down
/// between the claim and the failed write), the probe must not go back onto
/// a queue nothing will ever drain again. It is settled `Abandoned` instead.
/// Regression test for the hole where `requeue_probe_front` pushed
/// unconditionally, leaving a probe reading `queued` forever against a dead
/// run.
#[test]
fn release_probe_reservation_abandons_instead_of_requeuing_against_a_released_run() {
    let (server_state, _dir) = test_server_state();
    let run_id = "run-released-mid-flight";
    server_state.worker_registry.register_run_slot(run_id, 7);
    let probe_id = server_state.queue_probe(run_id.to_owned(), "hello".into(), false);
    let claimed = server_state
        .try_reserve_probe_for_delivery(run_id, None, 0)
        .expect("claiming the probe must succeed");
    assert_eq!(claimed.probe_id, probe_id);

    // The run's pane is released while the write is still in flight: the
    // slot mapping disappears out from under the claim.
    assert_eq!(server_state.worker_registry.take_slot_for_run(run_id), Some(7));

    let requeued = server_state.release_probe_reservation(run_id, claimed);
    assert!(!requeued, "a released run must refuse the requeue");
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Abandoned),
        "refusing the requeue must settle the probe, not leave it unrecorded",
    );
    assert!(
        !server_state.has_pending_probe(run_id),
        "an abandoned probe must not sit back on a queue nothing will ever drain",
    );
}

/// Every outcome renders as a distinct, non-empty label. The labels are what
/// a human reads out of the trace when a probe goes missing, so two branches
/// sharing one name would reintroduce the ambiguity this type exists to
/// remove.
#[test]
fn dispatch_outcome_labels_are_distinct() {
    let all = [
        ProbeDispatchOutcome::NotADeliveryBoundary,
        ProbeDispatchOutcome::NoRunId,
        ProbeDispatchOutcome::NothingQueued,
        ProbeDispatchOutcome::NoSlotMapping,
        ProbeDispatchOutcome::PostureRefused,
        ProbeDispatchOutcome::AlreadyInFlight,
        ProbeDispatchOutcome::RacedToEmpty,
        ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Consumed),
        ProbeDispatchOutcome::RequeuedAfterFailure,
    ];
    let labels: std::collections::HashSet<&str> = all.iter().map(|o| o.as_str()).collect();
    assert_eq!(labels.len(), all.len(), "each dispatch outcome needs its own label");
    assert!(labels.iter().all(|l| !l.is_empty()));
}

// ── The abandoned probe nobody was told about ───────────────────────────────
//
// A coordinator probe against a live `grok` worker was accepted with a
// `next_turn_boundary` commitment, the worker ran on, opened its PR and
// completed, and the probe settled `abandoned`. Everything the engine recorded
// was correct; nothing carried it to the issuer, who had no reason to poll
// `probe-status` and assumed the instruction had landed.
//
// Two separate defects hide behind that one observation, and the tests below
// pin them apart:
//
// 1. **The promised boundary was consumed by the run's own teardown.** For a
//    driver holding the safe `MidTurnPaneInput::Rejects` default there is no
//    earlier delivery opportunity than a turn boundary, and the fan-out ran
//    `dispatch_completion_on_stop` — which may conclude the run is finished
//    and call `release_worker_pane` — *before* the probe dispatcher. On a
//    worker whose next turn boundary is also its last, that made abandonment
//    a certainty rather than a race. `dispatch_probe_on_stop` now runs on both
//    sides of completion.
//
// 2. **Nothing surfaced the abandonment.** It is now pushed on the run's probe
//    topic *and* filed as a `probe_undelivered` attention item against the
//    execution, which outlives the run and its topic subscribers.

/// The delivery half of the fix, at the composition that mattered: a probe
/// still queued when a turn boundary arrives is written into the pane
/// *before* anything can tear the run down, so teardown finds nothing left to
/// abandon.
#[tokio::test]
async fn a_probe_queued_at_a_turn_boundary_is_delivered_before_teardown_can_abandon_it() {
    let (server_state, _dir) = test_server_state();
    // `grok` holds the trait-default `MidTurnPaneInput::Rejects`, so a turn
    // boundary is this worker's only delivery opportunity — the exact shape
    // the field report hit.
    let run_id = register_working_worker_with_driver(&server_state, 3, Some("grok"));
    let probe_id = server_state.queue_probe(run_id.clone(), "do not open the PR yet".into(), false);
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Queued),
        "precondition: a mid-turn probe on this driver cannot be written yet",
    );

    let captured = app_session_capturing_one_send(&server_state).await;
    // Production fan-out order on a Stop: live-state apply parks the worker,
    // then the pre-completion probe pass runs.
    let stop = stop_event(&run_id);
    dispatch_live_worker_state(&server_state, &stop).await;
    let outcome = dispatch_probe_on_stop(&server_state, &stop).await;
    assert_eq!(
        outcome,
        ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Consumed),
        "the boundary the engine promised must be the boundary it delivers on",
    );
    let (_, text) = tokio::time::timeout(Duration::from_secs(2), captured)
        .await
        .expect("timed out waiting for SendToPane")
        .expect("app responder panicked")
        .expect("the probe text must reach the pane");
    assert!(text.contains("do not open the PR yet"));

    // Now the run ends, as it would have milliseconds later. The probe is
    // past the queue, so teardown has nothing to abandon — it settles the
    // delivered-but-unanswered probe honestly instead.
    server_state.release_worker_pane(&run_id).await;
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Orphaned),
        "a probe delivered on the run's last boundary reached the pane but was never answered",
    );
}

/// Teardown must settle the probe *past* the queue too. Its reply would have
/// arrived at the worker's next turn boundary and there is not going to be
/// one, so leaving it at `consumed` reports a delivery nobody acted on as a
/// success — the same class of lie as leaving a queued probe at `queued`.
#[tokio::test]
async fn a_delivered_but_unanswered_probe_is_orphaned_when_its_run_ends() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 4, None);
    let probe_id = server_state.queue_probe(run_id.clone(), "status?".into(), false);
    let claimed = server_state
        .try_reserve_probe_for_delivery(&run_id, None, 0)
        .expect("the queued probe must be claimable");
    assert_eq!(claimed.probe_id, probe_id);
    server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Consumed);

    server_state.release_worker_pane(&run_id).await;

    let record = server_state
        .probe_record(&probe_id)
        .expect("the probe record must survive its run");
    assert_eq!(record.state, ProbeDeliveryState::Orphaned);
    assert!(
        record.state.is_undeliverable(),
        "nobody acted on it, so it must not read as delivered",
    );
    assert!(
        !record.state.is_terminal(),
        "a reply that somehow still arrives must be able to correct the record",
    );
    assert!(
        record.detail.is_some_and(|d| d.contains("turn boundary")),
        "the record must say why no reply is coming",
    );
    assert!(
        !server_state.has_in_flight_probe(&run_id),
        "the run's delivery slot must not outlive the run",
    );
}

/// The converse: a probe the worker already answered is settled, and teardown
/// must not overwrite a real reply with an undeliverable state.
#[tokio::test]
async fn teardown_leaves_an_already_replied_probe_alone() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 5, None);
    let probe_id = server_state.queue_probe(run_id.clone(), "status?".into(), false);
    let claimed = server_state
        .try_reserve_probe_for_delivery(&run_id, None, 0)
        .expect("the queued probe must be claimable");
    assert_eq!(claimed.probe_id, probe_id);
    server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Replied);

    server_state.release_worker_pane(&run_id).await;

    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Replied),
        "the worker's answer is the end of the story; teardown must not rewrite it",
    );
}

/// The reporting half of the fix. An abandonment must reach an observer
/// without being asked for: pushed on the run's probe topic for anything
/// watching while the run is live, and filed against the execution so the
/// record outlives the run, the pane, and every topic subscriber.
#[tokio::test]
async fn an_abandoned_probe_is_surfaced_actively_not_only_on_query() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 6, Some("grok"));

    let watch_session_id = "session-probe-abandon-watch".to_owned();
    let watch_sink = make_session_sink();
    server_state
        .topic_broker
        .register_session(&watch_session_id, watch_sink.clone())
        .await;
    server_state
        .topic_broker
        .subscribe(&watch_session_id, &[probe_topic(&run_id)])
        .await;

    let probe_id = server_state.queue_probe(run_id.clone(), "do not open the PR yet".into(), false);
    server_state.release_worker_pane(&run_id).await;

    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Abandoned),
        "precondition: this is the abandonment path",
    );

    let pushed = tokio::time::timeout(Duration::from_secs(2), watch_sink.next())
        .await
        .expect("an abandonment must be pushed, not left for a probe-status query")
        .expect("the probe topic must carry the push");
    match pushed.payload {
        FrontendEvent::ProbeDeliveryEscalated {
            probe_id: escalated,
            reason,
            ..
        } => {
            assert_eq!(escalated, probe_id);
            assert!(
                reason.contains("abandoned"),
                "the push must name the terminal state, not just that something happened: {reason}",
            );
        }
        other => panic!("expected ProbeDeliveryEscalated on the probe topic, got {other:?}"),
    }

    let items = server_state
        .work_db
        .list_attention_items(&run_id)
        .expect("the execution's attention items must be readable");
    let filed = items
        .iter()
        .find(|item| item.kind == crate::app::probes::PROBE_UNDELIVERED_ATTENTION_KIND)
        .expect("an undelivered probe must be recorded against the execution it targeted");
    assert!(
        filed.body_markdown.contains(&probe_id),
        "the item must name the probe so `bossctl probe-status` can be run against it",
    );
    assert!(
        filed.body_markdown.contains("never delivered"),
        "the item must say the text did not land, not merely that a probe existed",
    );
}

/// The counterfactual, kept beside
/// [`a_probe_queued_at_a_turn_boundary_is_delivered_before_teardown_can_abandon_it`]
/// so the ordering the fan-out depends on is visible as an executable
/// *difference* rather than a claim about where a line sits in a file.
///
/// Reverse the two steps — teardown first, probe dispatch second, which is
/// what the fan-out did while `dispatch_completion_on_stop` ran ahead of
/// `dispatch_probe_on_stop` — and the identical probe is abandoned on the
/// very boundary it was promised, with the dispatcher then finding an empty
/// queue. That is the reported incident in miniature: for a driver whose only
/// delivery opportunity is a turn boundary, a worker whose next boundary is
/// also its last loses the probe every time, not sometimes.
#[tokio::test]
async fn the_same_probe_is_abandoned_when_teardown_reaches_the_boundary_first() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 7, Some("grok"));
    let probe_id = server_state.queue_probe(run_id.clone(), "do not open the PR yet".into(), false);

    // Teardown wins the boundary.
    server_state.release_worker_pane(&run_id).await;
    let outcome = dispatch_probe_on_stop(&server_state, &stop_event(&run_id)).await;

    assert_eq!(
        outcome,
        ProbeDispatchOutcome::NothingQueued,
        "the dispatcher arrives to an empty queue — the probe is already gone",
    );
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Abandoned),
        "and the probe never reached the worker",
    );
}

/// Wiring check on the real fan-out: a probe queued before a turn boundary is
/// delivered by `dispatch_worker_event_fanout` itself, not merely by the
/// dispatcher called in isolation. Pins that adding the pre-completion pass
/// did not break the pass that was already there — both are called, and the
/// run's single in-flight slot keeps them from double-delivering.
#[tokio::test]
async fn the_turn_boundary_fanout_delivers_a_probe_queued_before_it() {
    use crate::app::worker_events::dispatch_worker_event_fanout;

    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 8, Some("grok"));
    let probe_id = server_state.queue_probe(run_id.clone(), "hold the PR".into(), false);
    let captured = app_session_capturing_one_send(&server_state).await;

    dispatch_worker_event_fanout(&server_state, &stop_event(&run_id)).await;

    let (_, text) = tokio::time::timeout(Duration::from_secs(5), captured)
        .await
        .expect("timed out waiting for SendToPane")
        .expect("app responder panicked")
        .expect("the probe text must reach the pane");
    assert!(text.contains("hold the PR"));
    assert!(
        server_state
            .probe_lifecycle_state(&probe_id)
            .is_some_and(|s| s.is_delivered()),
        "the fan-out must deliver on the boundary, not leave the probe queued",
    );
    assert_eq!(
        server_state.pending_probe_count(&run_id),
        0,
        "and must not leave a duplicate of it behind for the second pass",
    );
}
