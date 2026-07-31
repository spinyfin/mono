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
    // surfaces the slot id once both land. Worker must be Idle so
    // the typed-input activity guard allows the write.
    let (server_state, _dir) = test_server_state();
    register_idle_worker(&server_state, "run-send", 7);

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
        &crate::events_socket::IncomingHookEvent::for_test(
            crate::protocol::WorkerEvent::UserPromptSubmit {
                session_id: "claude-sess-1".into(),
                prompt: "/help\n".into(),
            },
            Some("run-send".to_owned()),
            None,
        ),
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
    //
    // Activity is Idle so the typed-input guard allows the write; the
    // gap under test is verification after a successful pty write, not
    // the mid-turn refusal path (see
    // `send_input_to_worker_refuses_when_worker_not_accepting_input`).
    let (server_state, _dir) = test_server_state();
    register_idle_worker(&server_state, "run-unverified", 3);

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

    // The app accepts the pty write — but no `UserPromptSubmit` hook
    // ever follows (observability gap after a successful write).
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

/// Safety guard (ghostty-codex-pane-viability Q2 Layer D):
/// `send_input_to_worker` must refuse a mid-turn (`Working`) worker whose
/// driver cannot be resolved, so bytes are never written into a pane whose
/// foreground process may not consume stdin. `register_working_worker`
/// registers a bare run id with no execution row, so
/// `get_execution_driver_slug` resolves to `None` and the posture fails
/// closed — that unresolvable-driver path is what this test pins, *not*
/// `Working` by itself. A mid-turn worker on a driver that buffers is
/// injectable; see
/// `send_input_to_worker_writes_to_a_mid_turn_worker_on_a_buffering_driver`.
/// The refusal is a typed error — not a silent drop and not a successful
/// "unconfirmed" write.
#[tokio::test]
async fn send_input_to_worker_refuses_when_worker_not_accepting_input() {
    use boss_protocol::WorkerActivity;

    let (server_state, _dir) = test_server_state();
    register_working_worker(&server_state, "run-working", 4);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let err = server_state
        .send_input_to_worker("run-working", "dangerous inject\n".into())
        .await
        .expect_err("mid-turn inject must be refused");
    match err {
        SendInputError::NotAcceptingInput {
            activity: Some(WorkerActivity::Working),
        } => {}
        other => panic!("expected NotAcceptingInput(Working), got {other:?}"),
    }

    // No SendToPane must have been enqueued — the guard is pre-write.
    assert_eq!(
        sink.queue_stats().depth,
        0,
        "refused inject must not enqueue SendToPane"
    );
}

/// Chore-update notify path: whenever `send_input_to_worker` comes back
/// `NotAcceptingInput`, the notice must be re-queued as a non-urgent probe
/// for Stop/idle delivery — never silently discarded. As above, the refusal
/// here comes from an unresolvable driver failing closed rather than from
/// `Working` alone; the delivered-mid-turn counterpart is
/// `chore_update_notify_delivers_mid_turn_on_a_buffering_driver`.
#[tokio::test]
async fn chore_update_notify_requeues_when_worker_not_accepting_input() {
    use boss_protocol::WorkerActivity;

    let (server_state, _dir) = test_server_state();
    let run_id = "run-chore-mid-turn";
    register_working_worker(&server_state, run_id, 5);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let msg = build_chore_update_message("old", "new", "old desc", "new desc").expect("message");

    // Mirror work_items chore-update notify: attempt immediate inject,
    // requeue on NotAcceptingInput.
    match server_state.send_input_to_worker(run_id, msg.clone()).await {
        Err(SendInputError::NotAcceptingInput {
            activity: Some(WorkerActivity::Working),
        }) => {
            let probe_id = server_state.queue_probe(run_id.to_owned(), msg.clone(), /*urgent=*/ false);
            assert_eq!(
                server_state.probe_lifecycle_state(&probe_id),
                Some(ProbeDeliveryState::Queued),
            );
            assert!(
                !server_state.probe_record(&probe_id).expect("probe record").urgent,
                "chore-update requeue must not jump the run's probe queue",
            );
        }
        other => panic!("expected NotAcceptingInput(Working), got {other:?}"),
    }

    assert_eq!(sink.queue_stats().depth, 0, "mid-turn must not SendToPane");

    let queued = server_state
        .pop_pending_probe(run_id)
        .expect("chore-update notice must be re-queued for Stop delivery");
    assert_eq!(queued.text, msg);
}

/// The other half of the mid-turn decision: a `Working` worker whose driver
/// declares `MidTurnPaneInput::Buffers` (the engine default, `claude`) *is*
/// injectable. `send_input_to_worker` writes the exact text to the pane and
/// returns `Ok(slot_id)` on `PaneInjectOutcome::Buffered` — no
/// `UserPromptSubmit` is expected inside the window, because the text is
/// sitting in the agent's composer rather than having become a prompt. When
/// the agent acts on it (a fresh turn on Claude, folded into the running turn
/// on Codex's TUI) is the driver's business; this path returns without
/// waiting for either, which is what keeps it correct on both.
#[tokio::test(start_paused = true)]
async fn send_input_to_worker_writes_to_a_mid_turn_worker_on_a_buffering_driver() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 6, None);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let run_clone = run_id.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker(&run_clone, "mid-turn nudge".into())
            .await
    });

    let envelope = sink.next().await.expect("a SendToPane EngineRequest must be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::SendToPane(input) => {
            assert_eq!(input.slot_id, 6);
            assert_eq!(input.text, "mid-turn nudge", "the exact text must reach the pane");
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

    // No `UserPromptSubmit` follows — the turn is still in flight. Drive
    // past the verification window so the buffered outcome is reached
    // deterministically.
    tokio::time::advance(Duration::from_secs(10)).await;

    let slot = send
        .await
        .expect("send task")
        .expect("a mid-turn write on a buffering driver must succeed");
    assert_eq!(slot, 6);
}

/// User-visible consequence of the above for the chore-update auto-notice:
/// against a mid-turn worker on a buffering driver the notice is delivered
/// into the composer now, rather than refused and re-queued as a probe for
/// the next Stop boundary.
#[tokio::test(start_paused = true)]
async fn chore_update_notify_delivers_mid_turn_on_a_buffering_driver() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 9, None);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let msg = build_chore_update_message("old", "new", "old desc", "new desc").expect("message");

    let server_clone = server_state.clone();
    let run_clone = run_id.clone();
    let msg_clone = msg.clone();
    let send = tokio::spawn(async move { server_clone.send_input_to_worker(&run_clone, msg_clone).await });

    let envelope = sink.next().await.expect("a SendToPane EngineRequest must be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::SendToPane(input) => {
            assert_eq!(input.slot_id, 9);
            assert_eq!(input.text, msg, "the chore-update notice must reach the pane verbatim");
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
    tokio::time::advance(Duration::from_secs(10)).await;

    // Mirror the work_items notify path: only `NotAcceptingInput` re-queues.
    match send.await.expect("send task") {
        Ok(slot) => assert_eq!(slot, 9),
        other => panic!("expected Ok(9) for a buffering mid-turn driver, got {other:?}"),
    }
    assert!(
        server_state.pop_pending_probe(&run_id).is_none(),
        "a delivered mid-turn notice must not also be re-queued as a probe",
    );
}

/// Fail closed when the slot has no live-worker-state entry: unknown
/// is not "accepting typed input".
#[tokio::test]
async fn send_input_to_worker_refuses_when_live_state_missing() {
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-no-live", 8);

    let sink = make_session_sink();
    server_state.register_app_session("session-app".into(), sink).await;

    let err = server_state
        .send_input_to_worker("run-no-live", "hi\n".into())
        .await
        .expect_err("missing live state must refuse");
    match err {
        SendInputError::NotAcceptingInput { activity: None } => {}
        other => panic!("expected NotAcceptingInput(None), got {other:?}"),
    }
}

#[tokio::test]
async fn send_input_to_worker_surfaces_app_error() {
    let (server_state, _dir) = test_server_state();
    register_idle_worker(&server_state, "run-send", 2);

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
