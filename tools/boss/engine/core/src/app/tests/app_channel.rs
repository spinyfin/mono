use super::*;

#[tokio::test]
async fn send_to_app_returns_not_registered_when_no_app() {
    let (server_state, _dir) = test_server_state();
    let result = server_state
        .send_to_app(
            EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                run_id: "r".into(),
                workspace_path: "/tmp".into(),
                slot_id: 1,
                initial_input: "claude\n".into(),
                env: vec![],
                summary: None,
                task_title: None,
            }),
            Duration::from_millis(50),
        )
        .await;
    assert!(matches!(result, Err(SendToAppError::NotRegistered)));
}

#[tokio::test]
async fn send_to_app_round_trips_via_deliver_response() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_to_app(
                EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                    run_id: "run-7".into(),
                    workspace_path: "/tmp".into(),
                    slot_id: 1,
                    initial_input: "claude\n".into(),
                    env: vec![],
                    summary: None,
                    task_title: None,
                }),
                Duration::from_secs(2),
            )
            .await
    });

    // Pull the EngineRequest event off the sink; that gives us
    // the request_id the engine assigned.
    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let request_id = match &envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    // Deliver a response for that id.
    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::SpawnWorkerPane {
                result: Ok(crate::protocol::SpawnWorkerPaneResult {
                    slot_id: 4,
                    shell_pid: 9001,
                }),
            },
        )
        .await;

    let response = send.await.expect("send_to_app task panicked").expect("ok");
    match response {
        EngineToAppResponse::SpawnWorkerPane { result } => {
            let result = result.expect("ok variant");
            assert_eq!(result.slot_id, 4);
            assert_eq!(result.shell_pid, 9001);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[tokio::test]
async fn send_to_app_resolves_app_disconnected_on_session_drop() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_to_app(
                EngineToAppRequest::ReleaseWorkerPane(crate::protocol::ReleaseWorkerPaneInput {
                    slot_id: 1,
                    kill_grace_seconds: 2,
                }),
                Duration::from_secs(5),
            )
            .await
    });

    // Drain the EngineRequest event so the test isn't racy on
    // sink ordering.
    let _ = sink.next().await;

    // Simulate the app session disconnecting.
    server_state.drop_app_session_if_matches("session-app").await;

    let response = send.await.expect("send task panicked").expect("ok");
    match response {
        EngineToAppResponse::SpawnWorkerPane {
            result: Err(EngineToAppError::AppDisconnected),
        } => {} // currently the cleanup path uses SpawnWorkerPane variant uniformly; ok.
        EngineToAppResponse::ReleaseWorkerPane {
            result: Err(EngineToAppError::AppDisconnected),
        } => {}
        other => panic!("expected AppDisconnected, got {other:?}"),
    }
}

#[tokio::test]
async fn send_to_app_times_out_when_app_silent() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    server_state.register_app_session("session-app".into(), sink).await;

    let result = server_state
        .send_to_app(
            EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                run_id: "r".into(),
                workspace_path: "/tmp".into(),
                slot_id: 1,
                initial_input: "claude\n".into(),
                env: vec![],
                summary: None,
                task_title: None,
            }),
            Duration::from_millis(50),
        )
        .await;
    assert!(matches!(result, Err(SendToAppError::Timeout)));
}

/// A saturated **priority lane** — the app not draining even tiny control
/// frames — must fail fast with the distinct `SessionWedged` error (not a 5s
/// `Timeout`, and not `NotRegistered`), and tear the wedged session down so a
/// reconnecting app re-registers cleanly. This is the acceptance criterion:
/// "no app session" vs "session wedged" are distinguishable. Note the setup
/// saturates the *priority* lane, not the bulk lane: a full bulk lane no
/// longer wedges a control push (that's exactly what the priority lane fixes —
/// see `send_to_app_admitted_when_only_bulk_lane_saturated`).
#[tokio::test]
async fn send_to_app_reports_session_wedged_when_priority_lane_saturated() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    // Fill the priority lane to its cap so the send_to_app enqueue reports Slow.
    {
        let mut q = sink.queue.lock().unwrap();
        for i in 0..MAX_PRIORITY_QUEUE {
            assert_eq!(
                q.enqueue(engine_request_envelope(&format!("p-{i}"))),
                EnqueueOutcome::Enqueued
            );
        }
    }
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    // A generous timeout: if fail-fast regresses, this hangs instead of
    // returning immediately.
    let result = server_state
        .send_to_app(
            EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                run_id: "r".into(),
                workspace_path: "/tmp".into(),
                slot_id: 1,
                initial_input: "claude\n".into(),
                env: vec![],
                summary: None,
                task_title: None,
            }),
            Duration::from_secs(30),
        )
        .await;
    assert!(
        matches!(result, Err(SendToAppError::SessionWedged)),
        "saturated priority lane must fail fast as SessionWedged, got {result:?}",
    );
    // The wedged session must be torn down — `send_to_app` closes the sink.
    assert!(
        sink.queue.lock().unwrap().closed,
        "wedged session sink must be closed so the reader loop exits and the app can re-register",
    );
    // And the failure is recorded on the channel-health signal.
    assert_eq!(server_state.app_channel_health.snapshot().consecutive_failures, 1);
}

/// The acceptance criterion for the `reveal_work_item` / `release_worker_pane`
/// incident: with the **bulk** lane saturated (a ~2,000-item `WorkTree` drain
/// backed up and latched `slow`), an engine→app control push must still be
/// delivered — it no longer fails fast as `SessionWedged` the way it did when
/// both shared one FIFO lane — and it drains *ahead* of the bulk backlog.
#[tokio::test]
async fn send_to_app_admitted_when_only_bulk_lane_saturated() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    {
        let mut q = sink.queue.lock().unwrap();
        for i in 0..MAX_SESSION_QUEUE {
            assert_eq!(
                q.enqueue(response_envelope(&format!("bulk-{i}"))),
                EnqueueOutcome::Enqueued
            );
        }
        // Back-date the head-of-line entry so overflow latches `slow`
        // (a genuine stuck-client signal) instead of degrading gracefully.
        q.backdate_oldest_bulk_entry(STUCK_CLIENT_AGE_MS + 100);
        // One past the cap latches the bulk-lane `slow` backpressure flag.
        assert_eq!(q.enqueue(response_envelope("bulk-overflow")), EnqueueOutcome::Slow);
        assert!(q.slow, "bulk lane must have latched slow past the cap");
    }
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_to_app(
                EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                    run_id: "r".into(),
                    workspace_path: "/tmp".into(),
                    slot_id: 1,
                    initial_input: "claude\n".into(),
                    env: vec![],
                    summary: None,
                    task_title: None,
                }),
                Duration::from_secs(5),
            )
            .await
    });

    // Wait for the spawned send to enqueue its control push. With the bulk
    // lane pre-filled, `next()` would otherwise pop a bulk item the instant
    // it's called — we must not drain until the EngineRequest is actually in
    // the priority lane. Its mere presence there already proves the fix:
    // under the old single-lane design the enqueue would have failed fast as
    // `SessionWedged` against the saturated queue instead of being admitted.
    let mut admitted = false;
    for _ in 0..200 {
        if !sink.queue.lock().unwrap().priority.is_empty() {
            admitted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        admitted,
        "control push must be admitted to the priority lane despite the saturated bulk lane",
    );

    // The control push jumps ahead of the saturated bulk lane: it is the
    // first envelope the writer drains, not the 257th.
    let envelope = sink
        .next()
        .await
        .expect("priority EngineRequest must drain ahead of the bulk backlog");
    let request_id = match &envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
        other => panic!("expected EngineRequest first, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::SpawnWorkerPane {
                result: Ok(crate::protocol::SpawnWorkerPaneResult {
                    slot_id: 1,
                    shell_pid: 42,
                }),
            },
        )
        .await;

    let response = send.await.expect("send task panicked").expect("ok");
    assert!(
        matches!(response, EngineToAppResponse::SpawnWorkerPane { result: Ok(_) }),
        "a control push must round-trip even while the bulk lane is saturated, got {response:?}",
    );
}

/// Consecutive engine→app send failures must surface as a single
/// `app_session_unresponsive` engine-health issue once the streak crosses
/// the threshold, and a successful round-trip must clear it. This replaces
/// the old "per-call `Send(Timeout)` WARN only" behaviour with a visible
/// health signal.
#[tokio::test]
async fn engine_health_report_flags_unresponsive_app_channel() {
    let (state, _dir) = test_server_state();
    let stats = QueueStats {
        depth: 220,
        priority_depth: 0,
        oldest_age_ms: 9_000,
        slow: true,
        closed: false,
    };

    let has_issue =
        |report: &boss_protocol::EngineHealthReport| report.issues.iter().any(|i| i.kind == "app_session_unresponsive");

    // A single failure is below the streak threshold — no banner yet.
    state.app_channel_health.record_failure(&stats);
    assert!(
        !has_issue(&build_engine_health_report(&state)),
        "a single send failure must not raise the banner",
    );

    // Second consecutive failure crosses the threshold.
    state.app_channel_health.record_failure(&stats);
    let report = build_engine_health_report(&state);
    let issue = report
        .issues
        .iter()
        .find(|i| i.kind == "app_session_unresponsive")
        .expect("app_session_unresponsive issue must be present after the streak threshold");
    assert_eq!(issue.severity, "error");
    assert!(
        !issue.title.is_empty() && !issue.body.is_empty(),
        "title and body must be populated so the banner has user-visible text",
    );

    // A successful round-trip resets the streak and clears the banner.
    state.app_channel_health.record_success();
    assert!(
        !has_issue(&build_engine_health_report(&state)),
        "a successful round-trip must clear the unhealthy signal",
    );
}

#[tokio::test]
async fn second_register_invalidates_first() {
    let (server_state, _dir) = test_server_state();
    let first_sink = make_session_sink();
    server_state
        .register_app_session("session-1".into(), first_sink.clone())
        .await;

    let server_clone = server_state.clone();
    let in_flight = tokio::spawn(async move {
        server_clone
            .send_to_app(
                EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                    run_id: "r".into(),
                    workspace_path: "/tmp".into(),
                    slot_id: 1,
                    initial_input: "claude\n".into(),
                    env: vec![],
                    summary: None,
                    task_title: None,
                }),
                Duration::from_secs(5),
            )
            .await
    });
    let _ = first_sink.next().await; // drain queued event

    // A second registration replaces the first and resolves
    // pending requests as AppDisconnected.
    let second_sink = make_session_sink();
    server_state.register_app_session("session-2".into(), second_sink).await;

    let response = in_flight.await.expect("send task").expect("ok");
    match response {
        EngineToAppResponse::SpawnWorkerPane {
            result: Err(EngineToAppError::AppDisconnected),
        } => {}
        other => panic!("expected AppDisconnected, got {other:?}"),
    }
}
