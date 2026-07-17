use super::*;

#[tokio::test]
async fn focus_worker_pane_unknown_run_returns_unknown_run() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    server_state.register_app_session("session-app".into(), sink).await;
    let err = server_state
        .focus_worker_pane("never-allocated")
        .await
        .expect_err("unknown run should fail");
    assert!(matches!(err, FocusPaneError::UnknownRun));
}

#[tokio::test]
async fn focus_worker_pane_round_trips_to_app() {
    // End-to-end smoke: engine resolves run_id → slot via the
    // worker registry, sends a FocusWorkerPane EngineRequest to
    // the registered app session, and surfaces the slot id once
    // the app replies success.
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-focus", 5);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let focus = tokio::spawn(async move { server_clone.focus_worker_pane("run-focus").await });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::FocusWorkerPane(input) => {
            assert_eq!(input.slot_id, 5);
        }
        other => panic!("expected FocusWorkerPane, got {other:?}"),
    }

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::FocusWorkerPane {
                result: Ok(crate::protocol::FocusWorkerPaneResult {}),
            },
        )
        .await;

    let slot = focus.await.expect("focus task").expect("focus ok");
    assert_eq!(slot, 5);
}

#[tokio::test]
async fn focus_worker_pane_surfaces_app_error() {
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-focus", 3);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let focus = tokio::spawn(async move { server_clone.focus_worker_pane("run-focus").await });

    let envelope = sink.next().await.expect("EngineRequest enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::FocusWorkerPane {
                result: Err(EngineToAppError::UnknownSlot),
            },
        )
        .await;

    let err = focus.await.expect("focus task").expect_err("expect err");
    match err {
        FocusPaneError::App(EngineToAppError::UnknownSlot) => {}
        other => panic!("expected App(UnknownSlot), got {other:?}"),
    }
}

#[tokio::test]
async fn send_input_to_worker_unknown_run_returns_unknown_run() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    server_state.register_app_session("session-app".into(), sink).await;
    let err = server_state
        .send_input_to_worker("never-allocated", "/help\n".into())
        .await
        .expect_err("unknown run should fail");
    assert!(matches!(err, SendInputError::UnknownRun));
}

#[tokio::test]
async fn send_input_to_worker_round_trips_to_app() {
    // End-to-end smoke: engine resolves run_id → slot via the
    // worker registry, sends a SendToPane EngineRequest carrying
    // the text payload to the registered app session, waits for a
    // `UserPromptSubmit` hook confirming the CLI actually enqueued
    // it (not just that the app accepted the pty write), and
    // surfaces the slot id once both land.
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-send", 7);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move { server_clone.send_input_to_worker("run-send", "/help\n".into()).await });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::SendToPane(input) => {
            assert_eq!(input.slot_id, 7);
            assert_eq!(input.text, "/help\n");
        }
        other => panic!("expected SendToPane, got {other:?}"),
    }

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::SendToPane {
                result: Ok(crate::protocol::SendToPaneResult {}),
            },
        )
        .await;

    // Confirm delivery the way the worker's CLI would: fire the
    // `UserPromptSubmit` hook that lands once it actually enqueues
    // the injected text as the next prompt. Without this the pane
    // write is never verified and `send_input_to_worker` falls back
    // to the probe queue instead of returning promptly — see
    // `send_input_to_worker_falls_back_to_probe_when_unverified`.
    dispatch_live_worker_state(
        &server_state,
        &crate::events_socket::IncomingHookEvent {
            peer_pid: None,
            run_id: Some("run-send".to_owned()),
            transcript_path: None,
            event: crate::protocol::WorkerEvent::UserPromptSubmit {
                session_id: "claude-sess-1".into(),
                prompt: "/help\n".into(),
            },
        },
    )
    .await;

    let slot = send.await.expect("send task").expect("send ok");
    assert_eq!(slot, 7);
}

#[tokio::test(start_paused = true)]
async fn send_input_to_worker_records_unconfirmed_without_probe_fallback() {
    // Regression test, corrected understanding (2026-07-13): the
    // chore-update auto-notice (routed through `send_input_to_worker`)
    // originally looked like it silently vanished — `SendToPane`
    // returned Ok, no WARN was logged, no `UserPromptSubmit` followed.
    // The incident record was later corrected: the worker had in fact
    // acted on the updated text, so the write was delivered but
    // unverifiable, not lost. Falling back to `queue_probe` (the
    // original fix) would hand the worker the same notice a second
    // time at its next Stop boundary. This locks in the corrected
    // behavior: an unconfirmed write returns Ok (the pane write did
    // succeed) without being queued again.
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-unverified", 3);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker("run-unverified", "[chore-update] spec changed".into())
            .await
    });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    // The app accepts the pty write (this is exactly what happened in
    // production) — but no `UserPromptSubmit` hook ever follows,
    // simulating the worker being mid-turn when the write landed.
    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::SendToPane {
                result: Ok(crate::protocol::SendToPaneResult {}),
            },
        )
        .await;

    // Drive virtual time past the verification window so the send
    // task's wait for a `UserPromptSubmit` confirmation times out
    // deterministically, instead of the test blocking on real time.
    tokio::time::advance(Duration::from_secs(10)).await;

    let slot = send
        .await
        .expect("send task")
        .expect("unconfirmed delivery must still return Ok — the pane write itself succeeded");
    assert_eq!(slot, 3);

    assert!(
        server_state.pop_pending_probe("run-unverified").is_none(),
        "unconfirmed pane write must not be re-queued as a probe — that would duplicate delivery \
         if the worker really did consume the original write",
    );
}

#[tokio::test]
async fn send_input_to_worker_surfaces_app_error() {
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-send", 2);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move { server_clone.send_input_to_worker("run-send", "hi\n".into()).await });

    let envelope = sink.next().await.expect("EngineRequest enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::SendToPane {
                result: Err(EngineToAppError::UnknownSlot),
            },
        )
        .await;

    let err = send.await.expect("send task").expect_err("expect err");
    match err {
        SendInputError::App(EngineToAppError::UnknownSlot) => {}
        other => panic!("expected App(UnknownSlot), got {other:?}"),
    }
}

#[tokio::test]
async fn interrupt_worker_pane_unknown_run_returns_unknown_run() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    server_state.register_app_session("session-app".into(), sink).await;
    let err = server_state
        .interrupt_worker_pane("never-allocated")
        .await
        .expect_err("unknown run should fail");
    assert!(matches!(err, InterruptPaneError::UnknownRun));
}

#[tokio::test]
async fn interrupt_worker_pane_round_trips_to_app() {
    // End-to-end smoke: engine resolves run_id → slot via the
    // worker registry, sends an InterruptWorkerPane EngineRequest
    // to the registered app session, and surfaces the slot id
    // once the app replies success.
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-int", 6);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let interrupt = tokio::spawn(async move { server_clone.interrupt_worker_pane("run-int").await });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::InterruptWorkerPane(input) => {
            assert_eq!(input.slot_id, 6);
        }
        other => panic!("expected InterruptWorkerPane, got {other:?}"),
    }

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::InterruptWorkerPane {
                result: Ok(crate::protocol::InterruptWorkerPaneResult {}),
            },
        )
        .await;

    let slot = interrupt.await.expect("interrupt task").expect("interrupt ok");
    assert_eq!(slot, 6);
}

#[tokio::test]
async fn interrupt_worker_pane_surfaces_app_error() {
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-int", 2);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let interrupt = tokio::spawn(async move { server_clone.interrupt_worker_pane("run-int").await });

    let envelope = sink.next().await.expect("EngineRequest enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::InterruptWorkerPane {
                result: Err(EngineToAppError::UnknownSlot),
            },
        )
        .await;

    let err = interrupt.await.expect("interrupt task").expect_err("expect err");
    match err {
        InterruptPaneError::App(EngineToAppError::UnknownSlot) => {}
        other => panic!("expected App(UnknownSlot), got {other:?}"),
    }
}
