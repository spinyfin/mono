// Handler-level tests for `FrontendRequest::RecreateCoordinator`: the
// handoff wait must be detached from the connection's read loop, wired to
// the same `Notify` the handoff writer fires, and reported as a
// `recreate_coordinator:`-prefixed error on the original request id.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use boss_tmux::{CommandOutput, CommandRunner, Tmux};

use super::*;
use crate::app::{coordinator_handoff, sessions};
use boss_protocol::CoordinatorRecreateReason;

const TOKEN: &str = "token";

/// Answers the read-only probes a live coordinator would and records calls.
#[derive(Default)]
struct LiveCoordinatorRunner {
    calls: StdMutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl CommandRunner for LiveCoordinatorRunner {
    async fn run(&self, _: &Path, args: &[OsString], _: Option<&Path>) -> std::io::Result<CommandOutput> {
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let stdout = match args.get(2).map(String::as_str) {
            Some("list-sessions") => "boss-coordinator\t\n".to_owned(),
            Some("show-environment") => format!("BOSS_SPAWN_TOKEN={TOKEN}\n"),
            Some("display-message") => "0\n".to_owned(),
            _ => String::new(),
        };
        self.calls.lock().unwrap().push(args);
        Ok(CommandOutput {
            success: true,
            code: Some(0),
            stdout,
            stderr: String::new(),
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

impl LiveCoordinatorRunner {
    fn prompted(&self) -> bool {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.get(2).map(String::as_str) == Some("send-keys") && c.iter().any(|a| a == "-l"))
    }
}

async fn fixture(
    timeout: Duration,
) -> (
    Arc<ServerState>,
    Arc<SessionSink>,
    Arc<LiveCoordinatorRunner>,
    tempfile::TempDir,
) {
    let temp = tempfile::tempdir().unwrap();
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
    let runner = Arc::new(LiveCoordinatorRunner::default());
    *state.pane_delivery_tmux_override.write().unwrap() =
        Some(Tmux::with_runner_and_socket("/usr/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap());
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    (state, sink, runner, temp)
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
    let (state, sink, runner, _dir) = fixture(Duration::from_millis(300)).await;

    // The handler must return while the handoff wait is still pending.
    tokio::time::timeout(
        Duration::from_millis(200),
        sessions::handle_recreate_coordinator(dispatch(&state, &sink, "reset-1"), reset_request()),
    )
    .await
    .expect("the handler must not await the handoff wait inline");
    wait_for_prompt(&runner).await;

    // A second request on the same connection is served during the wait,
    // and a duplicate reset is refused instead of stacking another wait.
    let started = std::time::Instant::now();
    sessions::handle_recreate_coordinator(dispatch(&state, &sink, "reset-2"), reset_request()).await;
    assert!(started.elapsed() < Duration::from_millis(200));
    let (request_id, event) = next_event(&sink).await;
    assert_eq!(request_id, "reset-2");
    let FrontendEvent::Error { message } = event else {
        panic!("expected Error, got {event:?}")
    };
    assert!(
        message.starts_with("recreate_coordinator:") && message.contains("already in progress"),
        "{message}"
    );

    let (request_id, event) = next_event(&sink).await;
    assert_eq!(request_id, "reset-1");
    let FrontendEvent::Error { message } = event else {
        panic!("expected Error, got {event:?}")
    };
    assert!(message.starts_with("recreate_coordinator: "), "{message}");
    assert!(message.contains("did not write a handoff"), "{message}");

    // The in-flight guard clears so the operator can retry.
    assert!(
        !state
            .coordinator_reset_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
    );
}

#[tokio::test]
async fn handoff_write_through_the_handler_wakes_the_reset() {
    let (state, sink, runner, _dir) = fixture(Duration::from_secs(30)).await;
    sessions::handle_recreate_coordinator(dispatch(&state, &sink, "reset-1"), reset_request()).await;
    wait_for_prompt(&runner).await;
    // The wait must still be pending: nothing is reported yet.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err(),
        "no reply may precede the handoff write"
    );

    let write_sink = make_session_sink();
    coordinator_handoff::handle_set_coordinator_handoff(
        dispatch(&state, &write_sink, "write-1"),
        FrontendRequest::SetCoordinatorHandoff {
            body: "- fresh facts".to_owned(),
        },
    )
    .await;

    // The reset proceeds past the wait (far inside the 30s timeout), so the
    // writer fired the `Notify` the handler waits on. What follows depends on
    // recreating a real session, which this fake does not model.
    let (request_id, event) = next_event(&sink).await;
    assert_eq!(request_id, "reset-1");
    if let FrontendEvent::Error { message } = event {
        assert!(!message.contains("did not write a handoff"), "{message}");
    }
}
