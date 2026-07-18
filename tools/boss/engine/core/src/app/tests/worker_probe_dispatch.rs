use super::super::worker_events::extract_last_assistant_text;
use super::*;

use crate::test_support::*;

#[test]
fn queue_probe_mints_unique_probe_ids() {
    let (server_state, _dir) = test_server_state();
    let id_one = server_state.queue_probe("run-x".into(), "first".into(), false);
    let id_two = server_state.queue_probe("run-x".into(), "second".into(), false);
    assert_ne!(id_one, id_two, "probe ids must be unique per call");
    assert!(id_one.starts_with("probe-"));
    assert!(id_two.starts_with("probe-"));
    let popped_one = server_state.pop_pending_probe("run-x").expect("first probe present");
    let popped_two = server_state.pop_pending_probe("run-x").expect("second probe present");
    assert_eq!(popped_one.probe_id, id_one);
    assert_eq!(popped_one.text, "first");
    assert_eq!(popped_two.probe_id, id_two);
    assert_eq!(popped_two.text, "second");
    assert!(
        server_state.pop_pending_probe("run-x").is_none(),
        "queue must be empty after both probes pop",
    );
}

#[test]
fn extract_last_assistant_text_handles_modern_content_blocks() {
    let chunk = r#"{"type":"user","message":{"content":[{"type":"text","text":"prompt"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"alpha "},{"type":"text","text":"beta"}]}}
{"type":"system","subtype":"ping"}
"#;
    let result = extract_last_assistant_text(chunk);
    assert_eq!(result.as_deref(), Some("alpha beta"));
}

#[test]
fn extract_last_assistant_text_picks_most_recent_when_multiple() {
    let chunk = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"new"}]}}
"#;
    let result = extract_last_assistant_text(chunk);
    assert_eq!(result.as_deref(), Some("new"));
}

#[test]
fn extract_last_assistant_text_returns_none_when_no_assistant_turn() {
    let chunk = r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}
{"type":"system","subtype":"compact"}
"#;
    assert_eq!(extract_last_assistant_text(chunk), None);
}

#[test]
fn extract_last_assistant_text_skips_unparseable_lines() {
    let chunk = "this is not json\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"survived\"}]}}\n";
    assert_eq!(extract_last_assistant_text(chunk).as_deref(), Some("survived"),);
}

#[tokio::test]
async fn dispatch_probe_reply_emits_probe_replied_after_followup_stop() {
    // End-to-end smoke for the ProbeReplied flow: call queue_probe,
    // dispatch the probe via the events-socket Stop hook, append an
    // assistant turn to the transcript, fire the follow-up Stop,
    // and observe ProbeReplied land on the per-run probe topic.
    // This locks in the wire shape a `bossctl probe --wait` (or
    // any other observer) would consume.
    use crate::protocol::WorkerEvent;
    use boss_protocol::RequestExecutionInput;

    let (server_state, _dir) = test_server_state();

    // Seed: product → chore → execution → run with a real
    // transcript path on disk. Without the run row the engine's
    // dispatch can't resolve a transcript path and would skip
    // emission — that's the production behaviour we want covered.
    let product = create_test_product_with_repo(&server_state.work_db, "p", Some("git@example.com:p.git"));
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "c");
    let execution = server_state
        .work_db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let transcript_dir = tempfile::tempdir().unwrap();
    let transcript_path = transcript_dir.path().join("transcript.jsonl");
    std::fs::write(
        &transcript_path,
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
    )
    .unwrap();
    // Create the work_runs row so transcript_path_for_execution(execution.id)
    // can resolve the path. The run.id is not used for hook correlation — in
    // production BOSS_RUN_ID carries execution.id (exec_*), not run.id (run_*).
    server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: Some(transcript_path.display().to_string()),
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();

    // Map the execution (via its exec_* id) to slot 1 so dispatch_probe_on_stop
    // has a target for `SendToPane`. In production BOSS_RUN_ID carries
    // execution.id (exec_*), not run.id (run_*).
    server_state.worker_registry.register_run_slot(execution.id.clone(), 1);

    // Subscribe a session to the per-run probe topic and pin the
    // ServerState so probe pushes have somewhere to land.
    let session_id = "session-probe-observer".to_owned();
    let sink = make_session_sink();
    server_state
        .topic_broker
        .register_session(&session_id, sink.clone())
        .await;
    server_state
        .topic_broker
        .subscribe(&session_id, &[probe_topic(&execution.id)])
        .await;

    // Register a fake "app session" to receive the SendToPane that
    // dispatch_probe_on_stop emits, and reply success to it on a
    // background task. Without this round-trip the dispatch errors
    // out, the probe text gets requeued, and no in-flight entry
    // is recorded.
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    let server_for_app = server_state.clone();
    let app_responder = tokio::spawn(async move {
        let envelope = app_sink
            .next()
            .await
            .expect("SendToPane EngineRequest should be enqueued");
        let request_id = match &envelope.payload {
            FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
            other => panic!("expected EngineRequest, got {other:?}"),
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
    });

    // Queue a probe and pull the minted probe_id back out of the
    // queue head so we can assert it threads through to ProbeReplied.
    // In production BOSS_RUN_ID is execution.id (exec_*), so probe
    // operations use execution.id, not run.id.
    let probe_id = server_state.queue_probe(execution.id.clone(), "what now?".into(), false);

    // Fire the first Stop boundary. This dispatches the probe to
    // the (fake) app session and records the in-flight entry.
    let first_stop = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: None,
        event: WorkerEvent::Stop {
            session_id: "claude-sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    };
    dispatch_probe_reply_on_stop(&server_state, &first_stop).await;
    dispatch_probe_on_stop(&server_state, &first_stop).await;
    app_responder.await.expect("app responder task");

    // Append an assistant turn — the worker has now "replied".
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&transcript_path).unwrap();
        let line = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"the answer\"}]}}";
        writeln!(file, "{line}").unwrap();
    }

    // Second Stop: the engine should see the in-flight probe,
    // read the new transcript bytes, and publish ProbeReplied on
    // the per-run probe topic.
    let second_stop = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: None,
        event: WorkerEvent::Stop {
            session_id: "claude-sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    };
    dispatch_probe_reply_on_stop(&server_state, &second_stop).await;

    let envelope = sink.next().await.expect("ProbeReplied envelope should be published");
    match envelope.payload {
        FrontendEvent::ProbeReplied {
            run_id: emitted_run,
            probe_id: emitted_probe,
            text,
        } => {
            assert_eq!(emitted_run, execution.id);
            assert_eq!(emitted_probe, probe_id);
            assert_eq!(text, "the answer");
        }
        other => panic!("expected ProbeReplied, got {other:?}"),
    }

    // Idempotency: a duplicate Stop with no in-flight entry must
    // not re-emit the same probe id.
    dispatch_probe_reply_on_stop(&server_state, &second_stop).await;
    let drain = tokio::time::timeout(Duration::from_millis(50), sink.next()).await;
    assert!(
        drain.is_err(),
        "duplicate Stop must not produce a second ProbeReplied for the same probe id",
    );
}

/// Happy path for the verified urgent-probe delivery: `SendToPane`
/// succeeds and the worker's CLI confirms it by firing a
/// `UserPromptSubmit` hook carrying the injected `[coordinator-nudge]`
/// text, all within the verification window. The probe must be
/// consumed (not left requeued) exactly as before this fix.
#[tokio::test]
async fn dispatch_urgent_probe_on_post_tool_use_confirms_via_user_prompt_submit() {
    use crate::protocol::WorkerEvent;

    let (server_state, _dir) = test_server_state();
    let run_id = "run-urgent-confirmed";
    server_state.worker_registry.register_run_slot(run_id, 5);

    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    let server_for_app = server_state.clone();
    let app_responder = tokio::spawn(async move {
        let envelope = app_sink
            .next()
            .await
            .expect("SendToPane must be enqueued for urgent probe");
        let request_id = match &envelope.payload {
            FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
            other => panic!("expected EngineRequest, got {other:?}"),
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
    });

    server_state.queue_probe(run_id.to_owned(), "what now?".into(), true);

    let post_tool_use = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(run_id.to_owned()),
        transcript_path: None,
        event: WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
    };
    let dispatch = tokio::spawn({
        let server_state = server_state.clone();
        async move { dispatch_urgent_probe_on_post_tool_use(&server_state, &post_tool_use).await }
    });

    app_responder.await.expect("app responder task");

    // Confirm delivery the way the worker's CLI would: the marked
    // "[coordinator-nudge] ..." text arrives as a UserPromptSubmit.
    dispatch_live_worker_state(
        &server_state,
        &crate::events_socket::IncomingHookEvent {
            peer_pid: None,
            run_id: Some(run_id.to_owned()),
            transcript_path: None,
            event: WorkerEvent::UserPromptSubmit {
                session_id: "claude-sess-1".into(),
                prompt: "[coordinator-nudge] what now?".into(),
            },
        },
    )
    .await;

    dispatch.await.expect("dispatch task");

    assert!(
        server_state.pop_pending_probe(run_id).is_none(),
        "confirmed urgent probe must be consumed, not left queued",
    );
}

/// Regression test for the probe-6 incident, corrected understanding
/// (2026-07-13): an urgent probe's pane write lands while the worker
/// is mid-turn, `SendToPane` reports success, but neither a
/// `UserPromptSubmit` hook nor a transcript scan confirms it within
/// the verification window. The original fix treated that as proof of
/// loss and auto-redelivered the text at the next Stop boundary — but
/// the corrected incident record shows the text likely *was* consumed
/// (the worker acted on it), so auto-redelivery would have handed the
/// worker a duplicate instruction. This asserts the corrected
/// behavior: the probe is NOT re-queued, its lifecycle state is
/// recorded as `Unconfirmed` (queryable rather than assumed), it is
/// still tracked in-flight so a reply that does arrive is captured,
/// and a `ProbeDeliveryEscalated` push tells observers delivery is
/// unverified without implying a duplicate was sent.
#[tokio::test(start_paused = true)]
async fn dispatch_urgent_probe_on_post_tool_use_records_unconfirmed_without_redelivery() {
    use crate::protocol::WorkerEvent;

    let (server_state, _dir) = test_server_state();
    let run_id = "run-urgent-midturn";
    server_state.worker_registry.register_run_slot(run_id, 6);

    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;

    // Observe the probe topic so we can assert the visibility push
    // lands alongside the escalation.
    let watch_session_id = "session-probe-watch".to_owned();
    let watch_sink = make_session_sink();
    server_state
        .topic_broker
        .register_session(&watch_session_id, watch_sink.clone())
        .await;
    server_state
        .topic_broker
        .subscribe(&watch_session_id, &[probe_topic(run_id)])
        .await;

    let server_for_app = server_state.clone();
    let app_responder = tokio::spawn(async move {
        let envelope = app_sink
            .next()
            .await
            .expect("SendToPane must be enqueued for urgent probe");
        let request_id = match &envelope.payload {
            FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
            other => panic!("expected EngineRequest, got {other:?}"),
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
    });

    let probe_id = server_state.queue_probe(run_id.to_owned(), "what now?".into(), true);

    let post_tool_use = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(run_id.to_owned()),
        transcript_path: None,
        event: WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
    };
    let dispatch = tokio::spawn({
        let server_state = server_state.clone();
        async move { dispatch_urgent_probe_on_post_tool_use(&server_state, &post_tool_use).await }
    });

    app_responder.await.expect("app responder task");

    // Simulate the incident: the worker is mid-turn and never fires a
    // UserPromptSubmit for the injected text. Drive virtual time past
    // the verification window so the dispatch task's wait times out
    // deterministically instead of the test blocking on real time.
    tokio::time::advance(Duration::from_secs(10)).await;

    dispatch.await.expect("dispatch task");

    // The probe must NOT be re-queued — that would duplicate delivery
    // in exactly the scenario the corrected incident record
    // establishes actually happened.
    assert!(
        server_state.pop_pending_probe(run_id).is_none(),
        "unconfirmed urgent probe must not be auto-redelivered",
    );
    // Its lifecycle must be observably Unconfirmed rather than silently
    // assumed either way.
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeLifecycleState::Unconfirmed),
        "unconfirmed delivery must be recorded, not left unknown",
    );
    // Still tracked as in-flight so a reply that does arrive (because
    // the text really was consumed) is captured rather than dropped.
    assert!(
        server_state.take_in_flight_probe(run_id).is_some(),
        "unconfirmed probe must still be tracked in-flight for reply capture",
    );

    let envelope = watch_sink
        .next()
        .await
        .expect("ProbeDeliveryEscalated should be published on the probe topic");
    match envelope.payload {
        FrontendEvent::ProbeDeliveryEscalated {
            run_id: emitted_run,
            probe_id: emitted_probe,
            ..
        } => {
            assert_eq!(emitted_run, run_id);
            assert_eq!(emitted_probe, probe_id);
        }
        other => panic!("expected ProbeDeliveryEscalated, got {other:?}"),
    }
}

/// Regression: `dispatch_probe_if_idle` must deliver a probe
/// immediately to a worker whose activity is `Idle` — i.e. one that
/// is between turns and has no Stop boundary coming. Before the fix,
/// `bossctl probe` targeted at an idle worker would stall forever
/// because `dispatch_probe_on_stop` only fires on Stop events and an
/// idle worker never produces another Stop without receiving input
/// first.
#[tokio::test]
async fn probe_queued_for_idle_worker_dispatches_immediately() {
    use boss_protocol::{RequestExecutionInput, WorkerActivity, WorkerEvent};

    let (server_state, _dir) = test_server_state();

    // Minimal DB rows so transcript lookup has something to resolve.
    let product = create_test_product_with_repo(&server_state.work_db, "p", Some("git@example.com:p.git"));
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "c");
    let execution = server_state
        .work_db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let run = server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: None,
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();

    // Register slot and set activity to Idle (worker between turns).
    server_state.worker_registry.register_run_slot(run.id.clone(), 1);
    server_state
        .live_worker_states
        .register_spawn(1, run.id.clone(), "claude-opus-4-7", 0, None);
    // Apply a Stop event to transition Spawning → Idle.
    server_state.live_worker_states.apply_event(
        1,
        &WorkerEvent::Stop {
            session_id: "sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    assert_eq!(
        server_state.live_worker_states.get(1).unwrap().activity,
        WorkerActivity::Idle,
        "precondition: worker must be idle",
    );

    // Register a fake app session to receive the SendToPane.
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    let server_for_app = server_state.clone();
    let app_responder = tokio::spawn(async move {
        let envelope = app_sink.next().await.expect("SendToPane must arrive for idle worker");
        let request_id = match &envelope.payload {
            FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
            other => panic!("expected EngineRequest, got {other:?}"),
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
    });

    // Queue the probe and call dispatch_probe_if_idle directly.
    server_state.queue_probe(run.id.clone(), "coordinator nudge".into(), false);
    dispatch_probe_if_idle(&server_state, &run.id).await;

    // The app_responder task must have seen the SendToPane by now.
    tokio::time::timeout(Duration::from_secs(2), app_responder)
        .await
        .expect("timed out waiting for SendToPane round-trip")
        .expect("app_responder panicked");

    // Probe must have been consumed (popped from pending_probes and
    // an in-flight entry recorded).
    assert!(
        server_state.pop_pending_probe(&run.id).is_none(),
        "probe must be consumed, not left in pending_probes",
    );
}

/// Regression: `dispatch_probe_if_idle` must also deliver a probe
/// immediately to a worker whose activity is `WaitingForInput` — the
/// state a session lands in when a `Stop` follows a `Notification`
/// (e.g. a worker parked at its prompt after the coordinator or a
/// permission dialog already resolved). Before the fix, a probe
/// queued against such a session stalled forever: it has already
/// produced its terminal `Stop` for this turn, so `dispatch_probe_on_stop`
/// never fires again on its own, and `dispatch_probe_if_idle` only
/// recognized `WorkerActivity::Idle`. Meanwhile `bossctl agents send`
/// (raw pane input, no activity check) reached the same session
/// immediately — this test locks in that probe delivery now matches.
#[tokio::test]
async fn probe_queued_for_waiting_for_input_worker_dispatches_immediately() {
    use boss_protocol::{RequestExecutionInput, WorkerActivity, WorkerEvent};

    let (server_state, _dir) = test_server_state();

    let product = create_test_product_with_repo(&server_state.work_db, "p", Some("git@example.com:p.git"));
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "c");
    let execution = server_state
        .work_db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let run = server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: None,
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();

    // Register slot and drive activity to WaitingForInput: a
    // Notification (permission prompt) immediately followed by a Stop.
    server_state.worker_registry.register_run_slot(run.id.clone(), 1);
    server_state
        .live_worker_states
        .register_spawn(1, run.id.clone(), "claude-opus-4-7", 0, None);
    server_state.live_worker_states.apply_event(
        1,
        &WorkerEvent::Notification {
            session_id: "sess-1".into(),
            message: "permission prompt".into(),
        },
    );
    server_state.live_worker_states.apply_event(
        1,
        &WorkerEvent::Stop {
            session_id: "sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    assert_eq!(
        server_state.live_worker_states.get(1).unwrap().activity,
        WorkerActivity::WaitingForInput,
        "precondition: worker must be parked in WaitingForInput",
    );

    // Register a fake app session to receive the SendToPane.
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    let server_for_app = server_state.clone();
    let app_responder = tokio::spawn(async move {
        let envelope = app_sink
            .next()
            .await
            .expect("SendToPane must arrive for waiting-for-input worker");
        let request_id = match &envelope.payload {
            FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
            other => panic!("expected EngineRequest, got {other:?}"),
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
    });

    // Queue the probe and call dispatch_probe_if_idle directly.
    server_state.queue_probe(run.id.clone(), "coordinator nudge".into(), false);
    dispatch_probe_if_idle(&server_state, &run.id).await;

    tokio::time::timeout(Duration::from_secs(2), app_responder)
        .await
        .expect("timed out waiting for SendToPane round-trip")
        .expect("app_responder panicked");

    assert!(
        server_state.pop_pending_probe(&run.id).is_none(),
        "probe must be consumed, not left in pending_probes",
    );
}

/// Regression: probes queued by the completion handler during a Stop
/// event must be dispatched on the SAME Stop, not stalled until the
/// next one. The event-loop order change (completion before probe
/// dispatch) enables this: `dispatch_completion_on_stop` adds to
/// `pending_probes`, then `dispatch_probe_on_stop` picks them up.
#[tokio::test]
async fn completion_probe_dispatched_on_same_stop_as_completion() {
    use crate::protocol::WorkerEvent;
    use boss_protocol::RequestExecutionInput;

    let (server_state, _dir) = test_server_state();

    let product = create_test_product_with_repo(&server_state.work_db, "p", Some("git@example.com:p.git"));
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "c");
    let execution = server_state
        .work_db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let run = server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: None,
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();

    server_state.worker_registry.register_run_slot(run.id.clone(), 1);

    // Queue a probe manually (simulating what the completion handler does)
    // BEFORE dispatch_probe_on_stop fires, to verify the dispatch picks it up.
    server_state.queue_probe(run.id.clone(), "push your PR".into(), false);

    // Register a fake app session to capture the SendToPane.
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    let server_for_app = server_state.clone();
    let app_responder = tokio::spawn(async move {
        let envelope = app_sink
            .next()
            .await
            .expect("SendToPane must arrive on the same Stop that completion queued it");
        let request_id = match &envelope.payload {
            FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
            other => panic!("expected EngineRequest, got {other:?}"),
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
    });

    // Fire the Stop event. With the new ordering, dispatch_probe_on_stop
    // runs after dispatch_completion_on_stop and sees the queued probe.
    let stop = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(run.id.clone()),
        transcript_path: None,
        event: WorkerEvent::Stop {
            session_id: "sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    };
    dispatch_probe_on_stop(&server_state, &stop).await;
    tokio::time::timeout(Duration::from_secs(2), app_responder)
        .await
        .expect("timed out waiting for SendToPane from completion probe")
        .expect("app_responder panicked");

    assert!(
        server_state.pop_pending_probe(&run.id).is_none(),
        "probe must be consumed by dispatch_probe_on_stop",
    );
}
