use super::*;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use boss_tmux::{CommandOutput, CommandRunner, Tmux};

/// The mocked tmux facts a [`PaneDeliveryRunner`] answers with. Kept as its
/// own type (rather than four fields directly on the runner) so the runner
/// itself stays under the project's `#[derive(bon::Builder)]` field-count
/// threshold.
struct PaneMockState {
    foreground_process: String,
    session_name: String,
    /// Whether `list-sessions` reports this pane's session as present. Real
    /// death evidence — not a foreground-command mismatch — is what the
    /// tmux pane-delivery boundary must key off; see
    /// `send_input_refuses_a_dead_tmux_pane`.
    session_present: bool,
    /// Whether `#{pane_dead}` reports the pane as dead.
    pane_dead: bool,
}

struct PaneDeliveryRunner {
    calls: Mutex<Vec<Vec<String>>>,
    stdin: Mutex<Vec<Vec<u8>>>,
    started: tokio::sync::Notify,
    state: PaneMockState,
}

impl Default for PaneDeliveryRunner {
    fn default() -> Self {
        Self::alive("claude", "boss-1")
    }
}

impl PaneDeliveryRunner {
    fn new(
        foreground_process: impl Into<String>,
        session_name: impl Into<String>,
        session_present: bool,
        pane_dead: bool,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            stdin: Mutex::new(Vec::new()),
            started: tokio::sync::Notify::new(),
            state: PaneMockState {
                foreground_process: foreground_process.into(),
                session_name: session_name.into(),
                session_present,
                pane_dead,
            },
        }
    }

    /// A live pane whose session exists and is not reported dead —
    /// `foreground_process` may or may not match the run's driver binary,
    /// which by itself must never be treated as death evidence.
    fn alive(foreground_process: impl Into<String>, session_name: impl Into<String>) -> Self {
        Self::new(foreground_process, session_name, true, false)
    }

    /// A pane whose tmux session no longer exists at all.
    fn session_gone(session_name: impl Into<String>) -> Self {
        Self::new("", session_name, false, false)
    }

    /// A pane whose session exists but tmux itself reports `#{pane_dead}`.
    fn pane_reported_dead(session_name: impl Into<String>) -> Self {
        Self::new("", session_name, true, true)
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
    fn stdin(&self) -> Vec<Vec<u8>> {
        self.stdin.lock().unwrap().clone()
    }

    fn success(stdout: impl Into<String>) -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn response(&self, args: &[OsString]) -> CommandOutput {
        let args = args.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>();
        if args.iter().any(|arg| arg == "list-sessions") {
            return Self::success(if self.state.session_present {
                format!("{}\t\n", self.state.session_name)
            } else {
                String::new()
            });
        }
        if args.iter().any(|arg| arg == "#{pane_current_command}") {
            return Self::success(format!("{}\n", self.state.foreground_process));
        }
        if args.iter().any(|arg| arg == "#{pane_dead}") {
            return Self::success(if self.state.pane_dead { "1\n" } else { "0\n" });
        }
        if args.iter().any(|arg| arg == "#{pane_dead_status}") {
            return Self::success(if self.state.pane_dead { "1\n" } else { "" });
        }
        Self::success("")
    }
}

#[async_trait]
impl CommandRunner for PaneDeliveryRunner {
    async fn run(&self, _program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none());
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect());
        self.started.notify_one();
        Ok(self.response(args))
    }

    async fn run_with_stdin(
        &self,
        _program: &Path,
        args: &[OsString],
        cwd: Option<&Path>,
        stdin: &[u8],
    ) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none());
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect());
        self.stdin.lock().unwrap().push(stdin.to_vec());
        self.started.notify_one();
        Ok(self.response(args))
    }
}

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
    let run_id = register_idle_worker_with_driver(&server_state, 7, None);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let run_id_for_send = run_id.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker(&run_id_for_send, "/help\n".into())
            .await
    });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::SendToPane(input) => {
            assert_eq!(input.slot_id, 7);
            assert_eq!(input.text, "/help\n");
            assert_eq!(input.expected_driver_binary, "claude");
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
            Some(run_id),
            None,
        ),
    )
    .await;

    let slot = send.await.expect("send task").expect("send ok");
    assert_eq!(slot, 7);
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

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let run_id_for_send = run_id.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker(&run_id_for_send, "/help\n".into())
            .await
    });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::SendToPane(input) => {
            assert_eq!(
                input.expected_driver_binary, "claude",
                "the launched driver must win over the post-launch pin change",
            );
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
    dispatch_live_worker_state(
        &server_state,
        &crate::events_socket::IncomingHookEvent::for_test(
            crate::protocol::WorkerEvent::UserPromptSubmit {
                session_id: "claude-sess-1".into(),
                prompt: "/help\n".into(),
            },
            Some(run_id),
            None,
        ),
    )
    .await;

    assert_eq!(
        send.await.expect("send task").expect("send ok, not a driver mismatch"),
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
    let runner = Arc::new(PaneDeliveryRunner::alive("claude", "boss-tmux-send"));
    *server_state.pane_delivery_tmux_override.write().unwrap() =
        Some(Tmux::with_runner("/usr/bin/tmux", runner.clone()).unwrap());

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
    let calls = runner.calls();
    assert_eq!(calls.len(), 5);
    assert_eq!(
        calls[0],
        vec![
            "-L",
            "boss",
            "list-sessions",
            "-F",
            "#{session_name}\t#{@boss_spawn_token}"
        ]
    );
    assert_eq!(
        calls[1],
        vec![
            "-L",
            "boss",
            "display-message",
            "-p",
            "-t",
            "boss-tmux-send",
            "#{pane_dead}",
        ]
    );
    assert_eq!(calls[2][..3], ["-L", "boss", "load-buffer"]);
    assert_eq!(calls[2][3], "-b");
    let buffer_name = calls[2][4].clone();
    assert!(
        buffer_name.starts_with("boss-deliver-boss-tmux-send-"),
        "unexpected buffer name: {buffer_name}"
    );
    assert_eq!(calls[2][5], "-");
    assert_eq!(
        calls[3],
        vec![
            "-L",
            "boss",
            "paste-buffer",
            "-b",
            buffer_name.as_str(),
            "-p",
            "-d",
            "-t",
            "boss-tmux-send",
        ]
    );
    assert_eq!(calls[4], vec!["-L", "boss", "send-keys", "-t", "boss-tmux-send", "C-m"]);
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
    use crate::work::ExecutionStatus;

    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 1, Some("grok"));
    let pool = server_state.execution_coordinator.worker_pool();
    pool.claim_worker(&run_id, None)
        .await
        .expect("precondition: slot must be claimed");
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, 1, "boss-tmux-driver-exited");
    let runner = Arc::new(PaneDeliveryRunner::session_gone("boss-tmux-driver-exited"));
    *server_state.pane_delivery_tmux_override.write().unwrap() =
        Some(Tmux::with_runner("/usr/bin/tmux", runner.clone()).unwrap());

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
            "-L".to_owned(),
            "boss".to_owned(),
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
    use crate::work::ExecutionStatus;

    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 1, Some("grok"));
    let pool = server_state.execution_coordinator.worker_pool();
    pool.claim_worker(&run_id, None)
        .await
        .expect("precondition: slot must be claimed");
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, 1, "boss-tmux-pane-dead");
    let runner = Arc::new(PaneDeliveryRunner::pane_reported_dead("boss-tmux-pane-dead"));
    *server_state.pane_delivery_tmux_override.write().unwrap() =
        Some(Tmux::with_runner("/usr/bin/tmux", runner.clone()).unwrap());

    let err = server_state
        .send_input_to_worker(&run_id, "do not write this to a dead pane".into())
        .await
        .expect_err("a pane tmux itself reports dead must refuse pane input");
    assert!(matches!(
        err,
        SendInputError::DriverExited {
            expected_driver_binary,
            observed_process: Some(observed_process),
        } if expected_driver_binary == "grok" && observed_process == "1"
    ));
    assert_eq!(
        server_state.work_db.get_execution(&run_id).unwrap().status,
        ExecutionStatus::Orphaned,
        "confirmed death terminalizes the execution instead of leaving it idle",
    );
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
    let runner = Arc::new(PaneDeliveryRunner::alive("bazel", "boss-tmux-foreground-child"));
    *server_state.pane_delivery_tmux_override.write().unwrap() =
        Some(Tmux::with_runner("/usr/bin/tmux", runner.clone()).unwrap());

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

#[tokio::test]
async fn tmux_pane_without_session_name_surfaces_typed_errors() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 8, None);
    server_state
        .worker_registry
        .register_tmux_run_slot_without_session_for_test(&run_id, 8);

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
    let run_id = register_idle_worker_with_driver(&server_state, 3, None);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let run_id_for_send = run_id.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker(&run_id_for_send, "[chore-update] spec changed".into())
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
    let run_id = register_idle_worker_with_driver(&server_state, 2, None);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let run_id_for_send = run_id.clone();
    let send = tokio::spawn(async move { server_clone.send_input_to_worker(&run_id_for_send, "hi\n".into()).await });

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
async fn send_input_to_worker_terminalizes_when_the_app_reports_the_driver_exited() {
    use crate::work::ExecutionStatus;

    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, 1, Some("grok"));
    let pool = server_state.execution_coordinator.worker_pool();
    pool.claim_worker(&run_id, None)
        .await
        .expect("precondition: slot must be claimed");

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let run_id_for_send = run_id.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker(&run_id_for_send, "do not send this to zsh".into())
            .await
    });

    let delivery = sink.next().await.expect("SendToPane request enqueued");
    let (delivery_request_id, delivery_request) = match delivery.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    assert!(matches!(
        delivery_request,
        EngineToAppRequest::SendToPane(ref input)
            if input.slot_id == 1 && input.expected_driver_binary == "grok"
    ));
    server_state
        .deliver_app_response(
            "session-app",
            &delivery_request_id,
            EngineToAppResponse::SendToPane {
                result: Err(EngineToAppError::DriverExited {
                    expected_driver_binary: "grok".to_owned(),
                    observed_process: Some("zsh".to_owned()),
                }),
            },
        )
        .await;

    let release = sink.next().await.expect("release request enqueued after driver exit");
    let (release_request_id, release_request) = match release.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    assert!(matches!(
        release_request,
        EngineToAppRequest::ReleaseWorkerPane(ref input) if input.slot_id == 1
    ));
    server_state
        .deliver_app_response(
            "session-app",
            &release_request_id,
            EngineToAppResponse::ReleaseWorkerPane {
                result: Ok(crate::protocol::ReleaseWorkerPaneResult {}),
            },
        )
        .await;

    assert!(matches!(
        send.await.expect("send task"),
        Err(SendInputError::DriverExited {
            expected_driver_binary,
            observed_process: Some(observed_process),
        }) if expected_driver_binary == "grok" && observed_process == "zsh"
    ));
    assert_eq!(
        server_state.work_db.get_execution(&run_id).unwrap().status,
        ExecutionStatus::Orphaned,
        "a reported driver exit must terminalize the execution"
    );
    assert!(
        server_state.worker_registry.slot_for_run(&run_id).is_none(),
        "a reported driver exit must release the pane mapping"
    );
    assert!(
        server_state.live_worker_states.get(1).is_none(),
        "a reported driver exit must not leave an idle worker state"
    );
    assert!(
        !pool.claimed_execution_ids().await.contains(&run_id),
        "a reported driver exit must release the exited execution's worker-pool claim before recovery may redispatch"
    );
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
