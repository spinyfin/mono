// Tests for `FrontendRequest::RecreateCoordinator`: the handoff wait must be
// detached from the connection's read loop, wired to the same `Notify` the
// handoff writer fires, and report a missing handoff on the original request
// id before replacing the session when the grace period expires.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use boss_tmux::{CommandOutput, CommandRunner, Tmux};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::app::{coordinator_handoff, sessions};
use boss_protocol::CoordinatorRecreateReason;

const TOKEN: &str = "token";

/// Models a live coordinator tmux server: answers the read-only probes,
/// records every call, and adopts the spawn token of a `new-session` so a
/// replacement is observable the way a real server would show it.
struct LiveCoordinatorRunner {
    token: StdMutex<String>,
    options: StdMutex<BTreeMap<String, String>>,
    calls: StdMutex<Vec<Vec<String>>>,
}

impl Default for LiveCoordinatorRunner {
    fn default() -> Self {
        Self {
            token: StdMutex::new(TOKEN.to_owned()),
            options: StdMutex::default(),
            calls: StdMutex::default(),
        }
    }
}

#[async_trait::async_trait]
impl CommandRunner for LiveCoordinatorRunner {
    async fn run(&self, _: &Path, args: &[OsString], _: Option<&Path>) -> std::io::Result<CommandOutput> {
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let mut success = true;
        let mut stderr = String::new();
        let stdout = match args.get(2).map(String::as_str) {
            Some("list-sessions") => "boss-coordinator\t\n".to_owned(),
            Some("show-environment") => format!("BOSS_SPAWN_TOKEN={}\n", self.token.lock().unwrap()),
            Some("display-message") => match args.last().map(String::as_str) {
                Some("#{pane_id}") => "%7\n".to_owned(),
                _ => "0\n".to_owned(),
            },
            Some("new-session") => {
                if let Some(token) = args.windows(2).find_map(|pair| {
                    pair[0]
                        .eq("-e")
                        .then(|| pair[1].strip_prefix("BOSS_SPAWN_TOKEN="))
                        .flatten()
                }) {
                    *self.token.lock().unwrap() = token.to_owned();
                }
                String::new()
            }
            Some("show-options") => {
                let key = match args.get(3).map(String::as_str) {
                    Some("-s") => format!("server:{}", args[5]),
                    Some("-v") => format!("session:{}:{}", args[5], args[6]),
                    other => panic!("unexpected show-options scope: {other:?}, args={args:?}"),
                };
                match self.options.lock().unwrap().get(&key) {
                    Some(value) => format!("{value}\n"),
                    None => {
                        success = false;
                        stderr = "invalid option".to_owned();
                        String::new()
                    }
                }
            }
            Some("set-option") => {
                let mut index = 2;
                let mut options = self.options.lock().unwrap();
                while index < args.len() {
                    let (key, value_index) = match args.get(index + 1).map(String::as_str) {
                        Some("-s") => (format!("server:{}", args[index + 2]), index + 3),
                        Some("-t") => (format!("session:{}:{}", args[index + 2], args[index + 3]), index + 4),
                        Some("-g") => (format!("global:{}", args[index + 2]), index + 3),
                        other => panic!("unexpected set-option scope: {other:?}, args={args:?}"),
                    };
                    options.insert(key, args[value_index].clone());
                    index = value_index + 1;
                    if index < args.len() {
                        index += 1;
                    }
                }
                String::new()
            }
            _ => String::new(),
        };
        self.calls.lock().unwrap().push(args);
        Ok(CommandOutput {
            success,
            code: Some(if success { 0 } else { 1 }),
            stdout,
            stderr,
        })
    }

    async fn run_with_stdin(
        &self,
        program: &Path,
        args: &[OsString],
        cwd: Option<&Path>,
        _: &[u8],
    ) -> std::io::Result<CommandOutput> {
        self.run(program, args, cwd).await
    }
}

#[async_trait::async_trait]
impl crate::coordinator_tmux::inbox::HandoffDelivery for LiveCoordinatorRunner {
    async fn deliver(&self, _: &Tmux, _: &crate::work::CoordinatorTmuxRecord, prompt: &str) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(vec!["inbox".into(), prompt.into(), "delivered".into()]);
        Ok(())
    }
}

impl LiveCoordinatorRunner {
    fn prompted(&self) -> bool {
        self.calls()
            .iter()
            .any(|c| c.first().map(String::as_str) == Some("inbox"))
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    fn position(&self, verb: &str) -> Option<usize> {
        self.calls()
            .iter()
            .position(|c| c.get(2).map(String::as_str) == Some(verb))
    }
}

struct Fixture {
    state: Arc<ServerState>,
    sink: Arc<SessionSink>,
    runner: Arc<LiveCoordinatorRunner>,
    session_dir: PathBuf,
    _dir: tempfile::TempDir,
}

async fn fixture(timeout: Duration) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let session_dir = temp.path().join("boss-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let runner = Arc::new(LiveCoordinatorRunner::default());
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build(),
        None,
    ));
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(
        cfg,
        None,
        None,
        ServerStateOverrides::builder()
            .coordinator_handoff_timeout(timeout)
            .coordinator_handoff_delivery(runner.clone())
            .coordinator_tmux(
                Tmux::with_runner_and_socket("/usr/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap(),
            )
            .coordinator_working_directory(session_dir.clone())
            .build(),
    )
    .unwrap();
    state
        .work_db
        .record_coordinator_tmux_spawn_intent("boss-coordinator", TOKEN, "opus", None)
        .unwrap();
    state.work_db.record_coordinator_tmux_session_created(TOKEN).unwrap();
    *state.tmux_preflight.write().unwrap() = crate::tmux_preflight::TmuxPreflight::Ready {
        program: "/nonexistent/tmux".into(),
        version: boss_tmux::MINIMUM_VERSION,
    };
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    Fixture {
        state,
        sink,
        runner,
        session_dir,
        _dir: temp,
    }
}

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>, request_id: &str) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-app")
        .request_id(request_id)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

fn reset_request() -> FrontendRequest {
    FrontendRequest::RecreateCoordinator {
        expected_spawn_token: TOKEN.to_owned(),
        reason: CoordinatorRecreateReason::OperatorReset,
        force_without_handoff: false,
    }
}

async fn next_event(sink: &SessionSink) -> (String, FrontendEvent) {
    let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
        .await
        .expect("an event must arrive")
        .expect("sink must stay open");
    (envelope.request_id.unwrap_or_default(), envelope.payload)
}

async fn wait_for_prompt(runner: &LiveCoordinatorRunner) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !runner.prompted() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the reset must send its handoff prompt");
}

#[tokio::test]
async fn timeout_is_reported_on_the_original_request_without_blocking_the_handler() {
    let Fixture {
        state,
        sink,
        runner,
        _dir,
        ..
    } = fixture(Duration::from_millis(300)).await;

    // The handler must return while the handoff wait is still pending.
    tokio::time::timeout(
        Duration::from_millis(200),
        sessions::handle_recreate_coordinator(dispatch(&state, &sink, "reset-1"), reset_request()),
    )
    .await
    .expect("the handler must not await the handoff wait inline");
    wait_for_prompt(&runner).await;

    // A duplicate reset is refused instead of stacking another wait.
    sessions::handle_recreate_coordinator(dispatch(&state, &sink, "reset-2"), reset_request()).await;
    let (request_id, event) = next_event(&sink).await;
    assert_eq!(request_id, "reset-2");
    let FrontendEvent::Error { message } = event else {
        panic!("expected Error, got {event:?}")
    };
    assert!(
        message.starts_with("recreate_coordinator:") && message.contains("already in progress"),
        "{message}"
    );

    // Engine requests have priority in SessionSink, so the attachment may
    // overtake the warning. Both must be delivered, with their own identities.
    let first = next_event(&sink).await;
    let second = next_event(&sink).await;
    let (warning, attachment) = if matches!(first.1, FrontendEvent::Error { .. }) {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(warning.0, "reset-1");
    let FrontendEvent::Error { message } = warning.1 else {
        panic!("missing timeout warning");
    };
    assert!(message.contains("no fresh handoff was written"), "{message}");
    let FrontendEvent::EngineRequest {
        request_id,
        request: EngineToAppRequest::AttachCoordinatorPane(attach),
    } = attachment.1
    else {
        panic!("expected replacement attachment");
    };
    assert_ne!(attach.spawn_token, TOKEN);
    assert!(runner.position("kill-session").unwrap() < runner.position("new-session").unwrap());
    assert!(
        runner.position("send-keys").is_none(),
        "the composer must stay untouched"
    );
    state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::AttachCoordinatorPane {
                result: Ok(boss_protocol::AttachCoordinatorPaneResult {}),
            },
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state
            .coordinator_reset_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("reset must finish after attachment");
}

#[tokio::test]
async fn handoff_write_through_the_handler_replaces_the_coordinator() {
    let Fixture {
        state,
        sink,
        runner,
        session_dir,
        _dir,
    } = fixture(Duration::from_secs(30)).await;
    sessions::handle_recreate_coordinator(dispatch(&state, &sink, "reset-1"), reset_request()).await;
    wait_for_prompt(&runner).await;
    // The wait must still be pending: nothing is reported yet.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err(),
        "no reply may precede the handoff write"
    );
    assert!(runner.position("new-session").is_none());

    let write_sink = make_session_sink();
    coordinator_handoff::handle_set_coordinator_handoff(
        dispatch(&state, &write_sink, "write-1"),
        FrontendRequest::SetCoordinatorHandoff {
            body: "- fresh facts".to_owned(),
        },
    )
    .await;

    // The replacement is attached through the app: the only frame the app sees
    // is the attach request for the new session, never an Error.
    // The reset also gets a correlated completion reply (so the app settles its
    // retry ledger); its order relative to the attach request is not fixed.
    let mut completion_seen = false;
    let (request_id, request) = loop {
        let (reply_id, event) = next_event(&sink).await;
        match event {
            FrontendEvent::CoordinatorRecreated => {
                assert_eq!(reply_id, "reset-1", "completion must echo the request id");
                completion_seen = true;
            }
            FrontendEvent::EngineRequest { request_id, request } => break (request_id, request),
            other => panic!("expected the coordinator attach request, got {other:?}"),
        }
    };
    let EngineToAppRequest::AttachCoordinatorPane(attach) = request else {
        panic!("expected AttachCoordinatorPane, got {request:?}")
    };
    assert_ne!(
        attach.spawn_token, TOKEN,
        "the replacement must mint a fresh spawn token"
    );
    let record = state.work_db.coordinator_tmux_record().unwrap().unwrap();
    assert_eq!(record.spawn_token, attach.spawn_token);
    assert_eq!(record.spawn_state, "created");
    assert_eq!(attach.session_name, "boss-coordinator");

    // Old session killed, then the replacement created.
    let kill = runner.position("kill-session").expect("the old session is killed");
    let new = runner.position("new-session").expect("a replacement is created");
    assert!(
        kill < new,
        "the old session must be killed before the replacement exists"
    );

    // The replacement's start brief carries the handoff that just landed.
    let brief = std::fs::read_to_string(
        session_dir
            .join(".claude")
            .join(crate::coordinator_handoff::START_BRIEF_FILENAME),
    )
    .unwrap();
    assert!(brief.contains("- fresh facts"), "{brief}");
    assert!(
        runner.calls().iter().any(|c| c
            .iter()
            .any(|a| a.contains(crate::coordinator_handoff::START_BRIEF_FILENAME))),
        "the new session must be launched with the brief as its initial prompt"
    );

    // The app acknowledging the viewer completes the reset, with no error.
    state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::AttachCoordinatorPane {
                result: Ok(boss_protocol::AttachCoordinatorPaneResult {}),
            },
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.coordinator_attached_spawn_token.lock().unwrap().as_deref() != Some(attach.spawn_token.as_str()) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the new session must be recorded as attached");
    if !completion_seen {
        let (reply_id, event) = next_event(&sink).await;
        assert!(matches!(event, FrontendEvent::CoordinatorRecreated), "got {event:?}");
        assert_eq!(reply_id, "reset-1", "completion must echo the request id");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err(),
        "a successful reset sends no further frame after its completion reply"
    );
}

/// Reads frames until `matches` accepts one, within five seconds.
async fn read_frame_where(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    matches: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0, "connection closed");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if matches(&frame) {
                return frame;
            }
        }
    })
    .await
    .expect("expected frame must arrive")
}

async fn send_frame(writer: &mut tokio::net::unix::OwnedWriteHalf, request_id: &str, payload: serde_json::Value) {
    let line = serde_json::json!({ "request_id": request_id, "payload": payload }).to_string();
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
}

/// A pending reset must not stall the connection's read loop: an independent
/// request is answered and the app's reply to an engine-to-app request is
/// routed while the handoff wait is still running.
#[tokio::test]
async fn pending_reset_does_not_stall_the_connection_read_loop() {
    let Fixture {
        state, runner, _dir, ..
    } = fixture(Duration::from_millis(1500)).await;
    let (engine_side, app_side) = tokio::net::UnixStream::pair().unwrap();
    let conn = tokio::spawn(handle_frontend_connection(engine_side, state.clone(), None));
    let (read_half, mut writer) = app_side.into_split();
    let mut reader = BufReader::new(read_half);

    send_frame(
        &mut writer,
        "register",
        serde_json::json!({"type": "register_app_session"}),
    )
    .await;
    read_frame_where(&mut reader, |f| f["payload"]["type"] == "app_session_registered").await;

    // An engine-to-app request is outstanding on this connection.
    let pending = tokio::spawn({
        let state = state.clone();
        async move {
            state
                .send_to_app(
                    EngineToAppRequest::DetachWorkerPane(crate::protocol::DetachWorkerPaneInput {
                        run_id: "run-test".into(),
                        slot_id: 1,
                    }),
                    Duration::from_secs(10),
                )
                .await
        }
    });
    let outstanding = read_frame_where(&mut reader, |f| {
        f["payload"]["type"] == "engine_request" && f["payload"]["request"]["kind"] == "detach_worker_pane"
    })
    .await;
    let outstanding_id = outstanding["payload"]["request_id"].as_str().unwrap().to_owned();

    let started = std::time::Instant::now();
    send_frame(&mut writer, "reset-1", serde_json::to_value(reset_request()).unwrap()).await;
    wait_for_prompt(&runner).await;
    eprintln!("isolated reset RPC to prompt: {}ms", started.elapsed().as_millis());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        state
            .coordinator_reset_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
    );

    // A second, independent request is served while the reset waits.
    send_frame(
        &mut writer,
        "version-check",
        serde_json::json!({"type": "get_engine_version"}),
    )
    .await;
    read_frame_where(&mut reader, |f| {
        f["request_id"] == "version-check" && f["payload"]["type"] == "engine_version_result"
    })
    .await;

    // The app's reply to the outstanding engine request is routed too.
    send_frame(
        &mut writer,
        "app-reply",
        serde_json::to_value(FrontendRequest::EngineResponse {
            request_id: outstanding_id,
            response: EngineToAppResponse::DetachWorkerPane {
                result: Ok(crate::protocol::DetachWorkerPaneResult {}),
            },
        })
        .unwrap(),
    )
    .await;
    let response = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("the engine response must reach the waiting send_to_app")
        .unwrap();
    assert!(response.is_ok(), "{response:?}");

    // All of that happened before the reset gave up.
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    assert!(
        state
            .coordinator_reset_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
    );

    // The reset still reports on its own request id once the wait times out.
    let reply = read_frame_where(&mut reader, |f| f["request_id"] == "reset-1").await;
    assert_eq!(reply["payload"]["type"], "error");
    let message = reply["payload"]["message"].as_str().unwrap();
    assert!(message.contains("no fresh handoff was written"), "{message}");

    drop(writer);
    let _ = tokio::time::timeout(Duration::from_secs(2), conn).await;
}
