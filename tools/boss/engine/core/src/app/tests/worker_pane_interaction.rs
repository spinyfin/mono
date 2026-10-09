use super::*;
use std::sync::Arc;

use super::tmux_stub::{RecordingPaneRunner, TEST_SPAWN_TOKEN, tmux_with_runner};

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

/// A pin change (`tasks.driver`) applied to a task after its worker has
/// already launched must not retroactively change which process the
/// pane-input boundary expects to see in that worker's PTY —
/// `expected_driver_binary` reads the *launched* driver
/// (`work_executions.driver`, frozen at spawn), not a live re-resolution of
/// the pin. Without this, changing the pin mid-run would make the still-
/// correctly-running worker look like a driver mismatch and terminalize it.
#[tokio::test]
async fn send_input_is_unaffected_by_a_driver_pin_change_after_launch() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 7, None);
    assert_eq!(
        server_state.work_db.get_execution(&run_id).unwrap().driver.as_deref(),
        Some("claude"),
        "precondition: the worker launched on the engine default driver",
    );
    let task_id = server_state.work_db.get_execution(&run_id).unwrap().work_item_id;
    server_state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET driver = ?2 WHERE id = ?1",
            rusqlite::params![task_id, "grok"],
        )
        .unwrap();

    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, 7, "boss-7");
    register_tmux_identity_for_test(&server_state, &run_id, "boss-7", TEST_SPAWN_TOKEN);
    let runner = Arc::new(RecordingPaneRunner::new("boss-7").echo_last_paste());
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));
    assert_eq!(
        server_state
            .send_input_to_worker(&run_id, "/help\n".into())
            .await
            .expect("send ok, not a driver mismatch"),
        7
    );
}

#[tokio::test]
async fn send_input_to_tmux_worker_pastes_multiline_text_and_confirms_delivery() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 7, None);
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, 7, "boss-tmux-send");
    register_tmux_identity_for_test(&server_state, &run_id, "boss-tmux-send", TEST_SPAWN_TOKEN);
    let runner = Arc::new(RecordingPaneRunner::alive("claude", "boss-tmux-send"));
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));

    // No app session is registered. The runner notification proves the
    // waiter has been registered and the direct tmux path was selected before
    // we emit the hook that makes this a confirmed (not merely unconfirmed)
    // delivery.
    let command_started = runner.started.notified();
    let server_clone = server_state.clone();
    let run_id_for_send = run_id.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker(&run_id_for_send, "first line\nsecond line\n".into())
            .await
    });
    command_started.await;
    dispatch_live_worker_state(
        &server_state,
        &crate::events_socket::IncomingHookEvent::for_test(
            crate::protocol::WorkerEvent::UserPromptSubmit {
                session_id: "tmux-sess-1".into(),
                prompt: "first line\nsecond line".into(),
            },
            Some(run_id),
            None,
        ),
    )
    .await;

    assert_eq!(send.await.expect("send task").expect("tmux send succeeds"), 7);
    assert_eq!(runner.escape_presses(), 0, "an idle worker must not be interrupted");
    let calls = runner.calls();
    assert!(
        calls.len() >= 6,
        "session, spawn-token, and pane verification plus paste delivery issue six tmux calls; extra capture-pane reads are the pane-text confirmation signal racing the UserPromptSubmit hook: {calls:?}"
    );
    assert_eq!(
        calls[0],
        vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "list-sessions",
            "-F",
            "#{session_name}\t#{@boss_spawn_token}"
        ]
    );
    assert_eq!(
        calls[1],
        vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "show-environment",
            "-t",
            "boss-tmux-send",
            "BOSS_SPAWN_TOKEN"
        ]
    );
    assert_eq!(
        calls[2],
        vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "display-message",
            "-p",
            "-t",
            "boss-tmux-send",
            "#{pane_dead}",
        ]
    );
    assert_eq!(calls[3][..3], ["-S", boss_tmux::TEST_SOCKET_PATH, "load-buffer"]);
    assert_eq!(calls[3][3], "-b");
    let buffer_name = calls[3][4].clone();
    assert!(
        buffer_name.starts_with("boss-deliver-boss-tmux-send-"),
        "unexpected buffer name: {buffer_name}"
    );
    assert_eq!(calls[3][5], "-");
    assert_eq!(
        calls[4],
        vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "paste-buffer",
            "-b",
            buffer_name.as_str(),
            "-p",
            "-d",
            "-t",
            "boss-tmux-send",
        ]
    );
    assert_eq!(
        calls[5],
        vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "send-keys",
            "-t",
            "boss-tmux-send",
            "C-m"
        ]
    );
    for extra in &calls[6..] {
        assert!(
            extra.contains(&"capture-pane".to_owned()),
            "commands after the paste must be pane-text confirmation, not another write: {extra:?}"
        );
    }
    assert_eq!(runner.stdin(), vec![b"first line\nsecond line".to_vec()]);
}

/// Real death evidence (the tmux session no longer exists) must still
/// refuse the write and terminalize the run. A foreground-command mismatch
/// alone must NOT — see
/// `mid_turn_probe_to_a_tmux_pane_running_a_foreground_child_is_not_orphaned`
/// below, which pins the opposite: an agent running a foreground child
/// (e.g. `bazel build`) is alive and must still receive its write.
#[tokio::test]
async fn send_input_refuses_a_dead_tmux_pane() {
    use crate::work::{ExecutionStatus, TmuxPaneObservationKind};

    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 1, Some("grok"));
    let pool = server_state.execution_coordinator.worker_pool();
    pool.claim_worker(&run_id, None)
        .await
        .expect("precondition: slot must be claimed");
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, 1, "boss-tmux-driver-exited");
    register_tmux_identity_for_test(&server_state, &run_id, "boss-tmux-driver-exited", TEST_SPAWN_TOKEN);
    let runner = Arc::new(RecordingPaneRunner::session_gone("boss-tmux-driver-exited"));
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));

    let (teardown, _) = super::tmux_stub::fake_tmux([super::tmux_stub::failure("session not found")]);
    server_state.set_tmux_override_for_test(teardown);

    let err = server_state
        .send_input_to_worker(&run_id, "do not write this to a dead pane".into())
        .await
        .expect_err("a session that no longer exists must refuse pane input");
    assert!(matches!(
        err,
        SendInputError::DriverExited {
            expected_driver_binary,
            observed_process: None,
        } if expected_driver_binary == "grok"
    ));

    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S".to_owned(),
            boss_tmux::TEST_SOCKET_PATH.to_owned(),
            "list-sessions".to_owned(),
            "-F".to_owned(),
            "#{session_name}\t#{@boss_spawn_token}".to_owned(),
        ]],
        "session-absence is established by list-sessions alone; no text may reach send-keys",
    );
    assert_eq!(
        server_state.work_db.get_execution(&run_id).unwrap().status,
        ExecutionStatus::Orphaned,
        "confirmed death terminalizes the execution instead of leaving it idle",
    );
    let observation = server_state
        .work_db
        .tmux_pane_observation_for_execution(&run_id)
        .unwrap()
        .expect("a session-missing pane delivery check must persist its observation");
    assert_eq!(observation.kind, TmuxPaneObservationKind::SessionMissing);
    assert_eq!(observation.pane_dead, None);
    assert!(
        server_state.worker_registry.slot_for_run(&run_id).is_none(),
        "the terminalized execution must no longer own a pane slot",
    );
    assert!(
        server_state.live_worker_states.get(1).is_none(),
        "the terminalized execution must no longer appear as an idle worker",
    );
    assert_eq!(
        pool.idle_count().await,
        pool.capacity().await,
        "the dead driver's worker-pool claim must be released",
    );
}

/// The second form of real death evidence: the session still exists but
/// tmux itself reports the pane dead (`#{pane_dead}`) — e.g. the driver
/// crashed and tmux has not yet reaped the session.
#[tokio::test]
async fn send_input_refuses_a_tmux_pane_reported_dead() {
    use crate::work::{ExecutionStatus, TmuxPaneObservationKind};

    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 1, Some("grok"));
    let pool = server_state.execution_coordinator.worker_pool();
    pool.claim_worker(&run_id, None)
        .await
        .expect("precondition: slot must be claimed");
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, 1, "boss-tmux-pane-dead");
    register_tmux_identity_for_test(&server_state, &run_id, "boss-tmux-pane-dead", TEST_SPAWN_TOKEN);
    let runner = Arc::new(RecordingPaneRunner::pane_reported_dead("boss-tmux-pane-dead"));
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));

    let err = server_state
        .send_input_to_worker(&run_id, "do not write this to a dead pane".into())
        .await
        .expect_err("a pane tmux itself reports dead must refuse pane input");
    assert!(matches!(
        err,
        SendInputError::DriverExited {
            expected_driver_binary,
            observed_process: Some(observed_process),
        } if expected_driver_binary == "grok" && observed_process == "pane_dead_status=1"
    ));
    assert_eq!(
        server_state.work_db.get_execution(&run_id).unwrap().status,
        ExecutionStatus::Orphaned,
        "confirmed death terminalizes the execution instead of leaving it idle",
    );
    let observation = server_state
        .work_db
        .tmux_pane_observation_for_execution(&run_id)
        .unwrap()
        .expect("a pane-dead delivery check must persist its observation");
    assert_eq!(observation.kind, TmuxPaneObservationKind::Dead);
    assert_eq!(observation.pane_dead, Some(true));
    assert_eq!(observation.pane_dead_status.as_deref(), Some("1"));
}

/// The failure mode that made the naive foreground-command check dangerous:
/// a live, working worker whose pane's foreground command is a child
/// process (e.g. `bazel build`), not the driver binary itself. This must
/// NOT be treated as death — the session is present and tmux does not
/// report the pane dead, so the mid-turn probe write must still land, and
/// the execution, pane mapping and live worker state must all survive.
#[tokio::test(start_paused = true)]
async fn mid_turn_probe_to_a_tmux_pane_running_a_foreground_child_is_not_orphaned() {
    use crate::events_socket::IncomingHookEvent;
    use crate::work::ExecutionStatus;

    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 2, None);
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, 2, "boss-tmux-foreground-child");
    register_tmux_identity_for_test(&server_state, &run_id, "boss-tmux-foreground-child", TEST_SPAWN_TOKEN);
    let runner = Arc::new(RecordingPaneRunner::alive("bazel", "boss-tmux-foreground-child"));
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));

    let probe_id = server_state.queue_probe(run_id.clone(), "status update".into(), false);
    let post_tool_use = IncomingHookEvent::for_test(
        crate::protocol::WorkerEvent::PostToolUse {
            session_id: "tmux-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
        Some(run_id.clone()),
        None,
    );

    let outcome = dispatch_probe_on_post_tool_use(&server_state, &post_tool_use).await;

    assert_eq!(
        outcome,
        ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Buffered),
        "a live worker running a foreground child (not the driver) must still receive the write",
    );
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Buffered)
    );
    assert_ne!(
        server_state.work_db.get_execution(&run_id).unwrap().status,
        ExecutionStatus::Orphaned,
        "an uncorroborated foreground-command mismatch must not terminalize the run",
    );
    assert!(
        server_state.worker_registry.slot_for_run(&run_id).is_some(),
        "the pane mapping must survive an uncorroborated foreground mismatch",
    );
    assert!(
        server_state.live_worker_states.get(2).is_some(),
        "the live worker state must survive an uncorroborated foreground mismatch",
    );
}

/// A session bearing the run's expected *name* can belong to a different,
/// later-spawned session if tmux recycled the name (e.g. after a crash and
/// respawn). `list-sessions` alone cannot tell the two apart; only the
/// durable `@boss_spawn_token` distinguishes them. A mismatched token must
/// refuse the write and terminalize the run exactly like session-absence or
/// `#{pane_dead}` — writing into it would land the prompt in a foreign
/// agent's PTY.
#[tokio::test]
async fn send_input_refuses_a_tmux_pane_whose_spawn_token_no_longer_matches() {
    use crate::work::ExecutionStatus;

    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 1, Some("grok"));
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, 1, "boss-tmux-recycled");
    register_tmux_identity_for_test(&server_state, &run_id, "boss-tmux-recycled", TEST_SPAWN_TOKEN);
    let runner = Arc::new(RecordingPaneRunner::spawn_token_mismatch("boss-tmux-recycled"));
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));

    let err = server_state
        .send_input_to_worker(&run_id, "must not reach a foreign agent's pane".into())
        .await
        .expect_err("a session whose spawn token no longer matches the run row must refuse pane input");
    assert!(matches!(
        err,
        SendInputError::DriverExited {
            expected_driver_binary,
            observed_process: Some(observed_process),
        } if expected_driver_binary == "grok" && observed_process == "spawn_token_mismatch"
    ));
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}\t#{@boss_spawn_token}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "show-environment".to_owned(),
                "-t".to_owned(),
                "boss-tmux-recycled".to_owned(),
                "BOSS_SPAWN_TOKEN".to_owned(),
            ],
        ],
        "the token mismatch is established right after list-sessions; no text may reach send-keys",
    );
    assert_eq!(
        server_state.work_db.get_execution(&run_id).unwrap().status,
        ExecutionStatus::Orphaned,
        "confirmed death terminalizes the execution instead of leaving it idle",
    );
}

#[tokio::test]
async fn tmux_pane_without_session_name_surfaces_typed_errors() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 8, None);
    server_state.worker_registry.register_tmux_run_slot(&run_id, 8, "");

    assert!(matches!(
        server_state.send_input_to_worker(&run_id, "hello".into()).await,
        Err(SendInputError::Tmux(_))
    ));
    assert!(matches!(
        server_state.interrupt_worker_pane(&run_id).await,
        Err(InterruptPaneError::Tmux(_))
    ));
}

#[tokio::test]
async fn unavailable_tmux_preflight_surfaces_typed_errors() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 9, None);
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, 9, "boss-tmux-unavailable");

    assert!(matches!(
        server_state.send_input_to_worker(&run_id, "hello".into()).await,
        Err(SendInputError::Tmux(_))
    ));
    assert!(matches!(
        server_state.interrupt_worker_pane(&run_id).await,
        Err(InterruptPaneError::Tmux(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn send_input_to_worker_records_unconfirmed_without_probe_fallback() {
    // Regression test, corrected understanding (2026-07-13): the
    // chore-update auto-notice (routed through `send_input_to_worker`)
    // originally looked like it silently vanished — the pane write
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
    let run_id = register_idle_worker_with_driver(&server_state, 3, None);
    let _tmux = install_live_tmux_delivery(&server_state, &run_id, 3, "boss-3");

    let slot = server_state
        .send_input_to_worker(&run_id, "[chore-update] spec changed".into())
        .await
        .expect("delivery must still return Ok — the pane write itself succeeded");
    assert_eq!(slot, 3);

    assert!(
        server_state.pop_pending_probe(&run_id).is_none(),
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
/// `send_input_to_worker_interrupts_a_long_tool_call_before_submitting`.
/// The refusal is a typed error — not a silent drop and not a successful
/// "unconfirmed" write.
#[tokio::test]
async fn send_input_to_worker_refuses_when_worker_not_accepting_input() {
    use boss_protocol::WorkerActivity;

    let (server_state, _dir) = test_server_state();
    register_working_worker(&server_state, "run-working", 4);

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
}

/// Chore-update notify path: whenever `send_input_to_worker` comes back
/// `NotAcceptingInput`, the notice must be re-queued as a non-urgent probe
/// for Stop/idle delivery — never silently discarded. As above, the refusal
/// here comes from an unresolvable driver failing closed rather than from
/// `Working` alone; the delivered-mid-turn counterpart is
/// `chore_update_notify_interrupts_before_delivering`.
#[tokio::test]
async fn chore_update_notify_requeues_when_worker_not_accepting_input() {
    use boss_protocol::WorkerActivity;

    let (server_state, _dir) = test_server_state();
    let run_id = "run-chore-mid-turn";
    register_working_worker(&server_state, run_id, 5);

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

    let queued = server_state
        .pop_pending_probe(run_id)
        .expect("chore-update notice must be re-queued for Stop delivery");
    assert_eq!(queued.text, msg);
}

/// A long tool call emits no boundary until Escape arrives. Sending a nudge
/// must create that boundary and submit exactly once before the tool finishes.
#[tokio::test(start_paused = true)]
async fn send_input_to_worker_interrupts_a_long_tool_call_before_submitting() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 6, None);
    let runner = install_live_tmux_delivery(&server_state, &run_id, 6, "boss-6");
    let parker = super::probe_interrupt::park_once_interrupted(&server_state, &runner, 6);
    let confirmer = super::probe_interrupt::confirm_write_when_it_lands(&server_state, &runner, &run_id);
    let started = tokio::time::Instant::now();

    let (slot, receipt) = server_state
        .send_input_to_worker_with_receipt(&run_id, "mid-turn nudge".into())
        .await
        .unwrap();
    parker.await.unwrap();
    confirmer.await.unwrap();
    assert_eq!(slot, 6);
    let record = server_state
        .probe_record(&receipt.expect("busy send must return a receipt"))
        .unwrap();
    assert_eq!(record.state, ProbeDeliveryState::Consumed);
    assert!(record.detail.unwrap().contains("resumed=confirmed"));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(runner.escape_presses(), 1);
    let calls = runner.calls();
    let escape = calls
        .iter()
        .position(|c| c.last().is_some_and(|a| a == "Escape"))
        .unwrap();
    let paste = calls.iter().position(|c| c.iter().any(|a| a == "load-buffer")).unwrap();
    assert!(escape < paste, "must interrupt before pasting: {calls:?}");
    assert_eq!(calls.iter().filter(|c| c.iter().any(|a| a == "load-buffer")).count(), 1);
    let pasted = String::from_utf8(runner.stdin().first().unwrap().clone()).unwrap();
    assert!(pasted.ends_with("mid-turn nudge"));
    assert!(server_state.pop_pending_probe(&run_id).is_none());
}

/// Brief updates use the same interrupting path, including the reconciliation
/// notice for any partial edit/build that the interrupt cancelled.
#[tokio::test(start_paused = true)]
async fn chore_update_notify_interrupts_before_delivering() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 9, None);
    let runner = install_live_tmux_delivery(&server_state, &run_id, 9, "boss-9");
    let parker = super::probe_interrupt::park_once_interrupted(&server_state, &runner, 9);
    let confirmer = super::probe_interrupt::confirm_write_when_it_lands(&server_state, &runner, &run_id);
    let msg = build_chore_update_message("old", "new", "old desc", "new desc").expect("message");
    assert_eq!(
        server_state.send_input_to_worker(&run_id, msg.clone()).await.unwrap(),
        9
    );
    parker.await.unwrap();
    confirmer.await.unwrap();
    let pasted = String::from_utf8(runner.stdin().last().cloned().unwrap_or_default()).unwrap();
    // Pane submission strips trailing line endings before pressing Enter.
    assert!(pasted.ends_with(msg.trim_end_matches(['\r', '\n'])));
    assert!(pasted.contains(super::super::probe_interrupt::INTERRUPT_NOTICE));
    assert_eq!(runner.escape_presses(), 1);
    assert_eq!(runner.stdin().len(), 1);
    assert!(server_state.pop_pending_probe(&run_id).is_none());
}

/// Fail closed when the slot has no live-worker-state entry: unknown
/// is not "accepting typed input".
#[tokio::test]
async fn send_input_to_worker_refuses_when_live_state_missing() {
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-no-live", 8);

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
async fn interrupt_tmux_worker_does_not_require_an_app_session() {
    let (server_state, _dir) = test_server_state();
    server_state
        .worker_registry
        .register_tmux_run_slot("run-tmux-interrupt", 6, "boss-tmux-interrupt");
    *server_state.tmux_preflight.write().unwrap() = crate::tmux_preflight::TmuxPreflight::Ready {
        program: std::path::PathBuf::from("/usr/bin/true"),
        version: boss_tmux::MINIMUM_VERSION,
    };

    assert_eq!(
        server_state
            .interrupt_worker_pane("run-tmux-interrupt")
            .await
            .expect("tmux interrupt succeeds"),
        6
    );
}

#[tokio::test]
async fn interrupt_worker_pane_without_tmux_identity_fails_closed() {
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-int", 2);
    let err = server_state
        .interrupt_worker_pane("run-int")
        .await
        .expect_err("local missing tmux identity must fail closed");
    assert!(matches!(err, InterruptPaneError::Tmux(_)));
}

/// The completion adapter must interrupt its own nudge, leaving an older
/// explicitly queued probe untouched.
#[tokio::test(start_paused = true)]
async fn engine_nudge_interrupts_without_stealing_an_older_probe() {
    use crate::completion::ProbeQueuer;
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 6, None);
    let runner = install_live_tmux_delivery(&server_state, &run_id, 6, "boss-6");
    let older = server_state.queue_probe(run_id.clone(), "wait for a boundary".into(), false);
    let parker = super::probe_interrupt::park_once_interrupted(&server_state, &runner, 6);
    let confirmer = super::probe_interrupt::confirm_write_when_it_lands(&server_state, &runner, &run_id);
    let queuer = crate::app::probes::ServerStateProbeQueuer::default();
    queuer.set_server_state(Arc::downgrade(&server_state));
    queuer.queue_probe(&run_id, "act now");
    queuer.deliver_queued_probes_now(&run_id);
    parker.await.unwrap();
    confirmer.await.unwrap();
    assert_eq!(runner.escape_presses(), 1);
    let pasted = String::from_utf8(runner.stdin().first().unwrap().clone()).unwrap();
    assert!(pasted.contains("act now"));
    assert!(!pasted.contains("wait for a boundary"));
    assert_eq!(
        server_state.probe_lifecycle_state(&older),
        Some(ProbeDeliveryState::Queued)
    );
}

/// Failure to interrupt must never be reported as a successful agents send.
#[tokio::test(start_paused = true)]
async fn send_input_fails_visibly_when_the_long_tool_call_does_not_stop() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 6, None);
    let runner = install_live_tmux_delivery(&server_state, &run_id, 6, "boss-6");
    let err = server_state
        .send_input_to_worker(&run_id, "act now".into())
        .await
        .unwrap_err();
    let SendInputError::NudgeDelivery { probe_id, state, .. } = err else {
        panic!("expected a queryable failed nudge, got {err:?}");
    };
    assert_eq!(state, "interrupt_failed");
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::InterruptFailed)
    );
    assert_eq!(runner.escape_presses(), 2);
    assert!(!runner.wrote_text());
    assert!(server_state.pop_pending_probe(&run_id).is_none());
}

/// Put a parked worker back mid-turn, the way a long tool call does: a
/// `PreToolUse` with no balancing `Stop`.
fn resume_into_long_tool_call(server_state: &ServerState, slot_id: u8) {
    use boss_protocol::{WorkerActivity, WorkerEvent};
    server_state.live_worker_states.apply_event(
        slot_id,
        &WorkerEvent::PreToolUse {
            session_id: "test-sess".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
        },
    );
    assert_eq!(
        server_state.live_worker_states.get(slot_id).unwrap().activity,
        WorkerActivity::Working,
        "precondition: the slot must be mid-turn",
    );
}

/// Park `slot_id` (as the driver's turn-boundary signal would) once at least
/// `escapes` Escape presses have been sent — unlike
/// `park_once_interrupted`, which fires on the first one.
fn park_after_escapes(
    server_state: &Arc<ServerState>,
    runner: &Arc<super::tmux_stub::RecordingPaneRunner>,
    slot_id: u8,
    escapes: usize,
) -> tokio::task::JoinHandle<()> {
    use boss_protocol::WorkerEvent;
    let server_state = server_state.clone();
    let runner = runner.clone();
    tokio::spawn(async move {
        for _ in 0..2000 {
            if runner.escape_presses() >= escapes {
                server_state.live_worker_states.apply_event(
                    slot_id,
                    &WorkerEvent::Stop {
                        session_id: "test-sess".into(),
                        stop_hook_active: false,
                        stop_reason: crate::protocol::StopReason::Interrupted,
                    },
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("engine never sent Escape press #{escapes}");
    })
}

/// Confirm the `index`th text write the way the worker's CLI does, by firing
/// `UserPromptSubmit` with what actually reached tmux.
fn confirm_nth_write(
    server_state: &Arc<ServerState>,
    runner: &Arc<super::tmux_stub::RecordingPaneRunner>,
    run_id: &str,
    index: usize,
) -> tokio::task::JoinHandle<()> {
    use boss_protocol::WorkerEvent;
    let server_state = server_state.clone();
    let runner = runner.clone();
    let run_id = run_id.to_owned();
    tokio::spawn(async move {
        for _ in 0..4000 {
            if let Some(written) = runner.stdin().get(index) {
                let prompt = String::from_utf8_lossy(written).into_owned();
                dispatch_live_worker_state(
                    &server_state,
                    &crate::events_socket::IncomingHookEvent::for_test(
                        WorkerEvent::UserPromptSubmit {
                            session_id: "test-sess".into(),
                            prompt,
                        },
                        Some(run_id.clone()),
                        None,
                    ),
                )
                .await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("engine never wrote text write #{index} into the pane");
    })
}

/// A submitted nudge keeps the run's in-flight slot until its reply boundary.
/// If it resumes the worker into *another* long tool call there is no boundary
/// coming, so the next nudge must still interrupt — and be submitted exactly
/// once — rather than queueing behind the slot and reporting a failure that
/// invites a duplicate retry.
#[tokio::test(start_paused = true)]
async fn second_nudge_interrupts_a_worker_that_resumed_into_another_long_tool_call() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 6, None);
    let runner = install_live_tmux_delivery(&server_state, &run_id, 6, "boss-6");

    let parker = super::probe_interrupt::park_once_interrupted(&server_state, &runner, 6);
    let confirmer = super::probe_interrupt::confirm_write_when_it_lands(&server_state, &runner, &run_id);
    let (_, first) = server_state
        .send_input_to_worker_with_receipt(&run_id, "first nudge".into())
        .await
        .unwrap();
    parker.await.unwrap();
    confirmer.await.unwrap();
    let first = first.expect("busy send must return a receipt");
    assert_eq!(server_state.probe_lifecycle_state(&first), Some(ProbeDeliveryState::Consumed));
    assert_eq!(runner.escape_presses(), 1);
    assert_eq!(
        server_state.in_flight_probe_id(&run_id).as_deref(),
        Some(first.as_str()),
        "precondition: the submitted probe still holds the slot awaiting its reply boundary",
    );

    // The first nudge sent the worker into another long tool call: no Stop.
    resume_into_long_tool_call(&server_state, 6);
    let parker = park_after_escapes(&server_state, &runner, 6, 2);
    let confirmer = confirm_nth_write(&server_state, &runner, &run_id, 1);
    let (_, second) = server_state
        .send_input_to_worker_with_receipt(&run_id, "second nudge".into())
        .await
        .expect("the second nudge must interrupt, not fail behind the first one's slot");
    parker.await.unwrap();
    confirmer.await.unwrap();
    let second = second.expect("busy send must return a receipt");

    assert_ne!(first, second);
    assert_eq!(runner.escape_presses(), 2, "the second nudge must send its own Escape");
    assert_eq!(runner.stdin().len(), 2, "each nudge is submitted exactly once");
    let pasted = String::from_utf8(runner.stdin()[1].clone()).unwrap();
    assert!(pasted.ends_with("second nudge"));
    assert_eq!(
        server_state.probe_lifecycle_state(&second),
        Some(ProbeDeliveryState::Consumed)
    );
    assert_eq!(
        server_state.probe_lifecycle_state(&first),
        Some(ProbeDeliveryState::Consumed),
        "the superseded probe was already delivered; only its reply slot moved on",
    );
    assert!(server_state.pop_pending_probe(&run_id).is_none());
}

/// Duplicate-delivery protection: a sibling probe whose write is still in
/// progress holds the slot, so a nudge must not interrupt or write. It is
/// reported as a `NudgeDelivery` error carrying the probe's real (still
/// queued) state instead of being claimed delivered.
#[tokio::test(start_paused = true)]
async fn nudge_reports_its_state_when_a_sibling_probe_holds_the_slot() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 6, None);
    let runner = install_live_tmux_delivery(&server_state, &run_id, 6, "boss-6");
    let sibling = server_state.queue_probe(run_id.clone(), "sibling".into(), false);
    let claimed = server_state
        .try_reserve_probe_for_delivery(&run_id, None, 0)
        .expect("sibling claims the slot");
    assert_eq!(claimed.probe_id, sibling);
    server_state.set_probe_lifecycle(&sibling, ProbeDeliveryState::Injected);

    let err = server_state
        .send_input_to_worker_with_receipt(&run_id, "nudge".into())
        .await
        .unwrap_err();
    let SendInputError::NudgeDelivery {
        probe_id,
        state,
        detail,
    } = err
    else {
        panic!("expected NudgeDelivery, got {err:?}");
    };
    assert_eq!(state, "queued");
    assert!(detail.contains("another delivery path"), "detail: {detail}");
    assert_eq!(server_state.probe_lifecycle_state(&probe_id), Some(ProbeDeliveryState::Queued));
    assert_eq!(runner.escape_presses(), 0, "must not interrupt behind a write in progress");
    assert!(!runner.wrote_text());
    assert_eq!(
        server_state.in_flight_probe_id(&run_id).as_deref(),
        Some(sibling.as_str()),
        "the sibling keeps its claim",
    );
}

/// A driver that reads no mid-turn input (Grok) is unchanged by the
/// interrupting send: the send is refused with the typed error, with no
/// Escape, no text, and no probe left queued or claimed as a side effect.
#[tokio::test(start_paused = true)]
async fn send_to_a_rejecting_driver_is_refused_without_side_effects() {
    use boss_protocol::WorkerActivity;
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 4, Some("grok"));
    let runner = install_live_tmux_delivery(&server_state, &run_id, 4, "boss-4");

    let err = server_state
        .send_input_to_worker_with_receipt(&run_id, "nudge".into())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            SendInputError::NotAcceptingInput {
                activity: Some(WorkerActivity::Working)
            }
        ),
        "expected NotAcceptingInput(Working), got {err:?}",
    );
    assert_eq!(runner.escape_presses(), 0);
    assert!(!runner.wrote_text());
    assert_eq!(server_state.pending_probe_count(&run_id), 0);
    assert!(!server_state.has_in_flight_probe(&run_id));
}

/// The engine-nudge adapter on a rejecting driver keeps its queued-boundary
/// behaviour: the nudge waits for the worker's own boundary, with no
/// interrupt, no composer write, and no claim on the in-flight slot.
#[tokio::test(start_paused = true)]
async fn engine_nudge_to_a_rejecting_driver_waits_for_its_boundary() {
    use crate::completion::ProbeQueuer;
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 4, Some("grok"));
    let runner = install_live_tmux_delivery(&server_state, &run_id, 4, "boss-4");
    let queuer = crate::app::probes::ServerStateProbeQueuer::default();
    queuer.set_server_state(Arc::downgrade(&server_state));

    queuer.queue_probe(&run_id, "act now");
    queuer.deliver_queued_probes_now(&run_id);
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert_eq!(runner.escape_presses(), 0);
    assert!(!runner.wrote_text());
    assert_eq!(server_state.pending_probe_count(&run_id), 1, "still queued for its boundary");
    assert!(!server_state.has_in_flight_probe(&run_id));
}

/// A nudge queued while the worker was parked must still interrupt if the
/// worker has started a turn by the time the sweep delivery runs, rather than
/// being skipped on the assumption that an interrupt was scheduled.
#[tokio::test(start_paused = true)]
async fn engine_nudge_queued_while_parked_interrupts_if_the_worker_went_busy() {
    use crate::completion::ProbeQueuer;
    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 6, None);
    let runner = install_live_tmux_delivery(&server_state, &run_id, 6, "boss-6");
    let queuer = crate::app::probes::ServerStateProbeQueuer::default();
    queuer.set_server_state(Arc::downgrade(&server_state));

    queuer.queue_probe(&run_id, "act now");
    assert_eq!(runner.escape_presses(), 0, "parked at queue time: nothing to interrupt yet");

    // The worker starts a long tool call before the delivery task runs.
    resume_into_long_tool_call(&server_state, 6);
    let parker = super::probe_interrupt::park_once_interrupted(&server_state, &runner, 6);
    let confirmer = super::probe_interrupt::confirm_write_when_it_lands(&server_state, &runner, &run_id);
    queuer.deliver_queued_probes_now(&run_id);
    parker.await.unwrap();
    confirmer.await.unwrap();

    assert_eq!(runner.escape_presses(), 1);
    let pasted = String::from_utf8(runner.stdin().first().unwrap().clone()).unwrap();
    assert!(pasted.contains("act now"));
    assert_eq!(server_state.pending_probe_count(&run_id), 0);
}
