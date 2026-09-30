//! Shared stubbed [`CommandRunner`]s for tmux tests, so teardown,
//! pane-delivery, probe-interrupt, and probe-dispatch fixtures don't each
//! hand-roll their own copy — mirroring [`crate::tmux_adoption`]'s
//! `FakeTmuxServer` pattern.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use boss_tmux::{CommandOutput, CommandRunner, Tmux};

/// Scripted `tmux` replies in exact call order. Panics on an unexpected
/// call, which is what makes "no kill-session was issued" assertable —
/// a refused teardown that nonetheless tried to kill fails the test by
/// running out of scripted replies.
#[derive(Default)]
pub(crate) struct StubRunner {
    outcomes: StdMutex<VecDeque<CommandOutput>>,
    calls: StdMutex<Vec<Vec<String>>>,
}

impl StubRunner {
    pub(crate) fn replies(replies: impl IntoIterator<Item = CommandOutput>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: StdMutex::new(replies.into_iter().collect()),
            calls: StdMutex::new(Vec::new()),
        })
    }

    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl CommandRunner for StubRunner {
    async fn run(&self, _program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none());
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect());
        Ok(self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("stub runner received an unexpected tmux command"))
    }

    async fn run_with_stdin(
        &self,
        program: &Path,
        args: &[OsString],
        cwd: Option<&Path>,
        _stdin: &[u8],
    ) -> std::io::Result<CommandOutput> {
        self.run(program, args, cwd).await
    }
}

pub(crate) fn ok(stdout: &str) -> CommandOutput {
    CommandOutput {
        success: true,
        code: Some(0),
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

pub(crate) fn failure(stderr: &str) -> CommandOutput {
    CommandOutput {
        success: false,
        code: Some(1),
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

/// Spawn token shared by live-delivery fixtures and `register_tmux_identity_for_test`.
pub(crate) const TEST_SPAWN_TOKEN: &str = "tok-test";

/// How `capture-pane` answers. Empty is the default so unconfirmed-delivery
/// tests wait out the verify window instead of matching the engine's own paste.
#[derive(Clone, Copy)]
enum CaptureBehavior {
    Empty,
    EchoLastPaste,
}

#[derive(Clone, Copy)]
enum PaneLiveness {
    Alive,
    SessionGone,
    PaneDead,
}

/// The mocked tmux facts a [`RecordingPaneRunner`] answers with. Kept as its
/// own type so the runner stays under the project's named-field threshold.
struct RecordingPaneState {
    session_name: String,
    spawn_token: String,
    liveness: PaneLiveness,
    foreground_process: String,
    capture: CaptureBehavior,
}

/// Recording tmux runner for pane-delivery tests. Answers list-sessions, token,
/// pane-dead, send-keys, and capture-pane. Capture is empty by default; call
/// [`Self::echo_last_paste`] when a test needs confirmation without waiting
/// out the verify timeout.
pub(crate) struct RecordingPaneRunner {
    state: RecordingPaneState,
    calls: StdMutex<Vec<Vec<String>>>,
    stdin: StdMutex<Vec<Vec<u8>>>,
    on_text_write: StdMutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pub started: tokio::sync::Notify,
}

impl RecordingPaneRunner {
    pub(crate) fn new(session_name: impl Into<String>) -> Self {
        Self::alive("claude", session_name)
    }

    /// A live pane whose session exists and is not reported dead.
    /// `foreground_process` may or may not match the run's driver binary,
    /// which by itself must never be treated as death evidence.
    pub(crate) fn alive(foreground_process: impl Into<String>, session_name: impl Into<String>) -> Self {
        Self::with_spawn_token(foreground_process, session_name, PaneLiveness::Alive, TEST_SPAWN_TOKEN)
    }

    /// A pane whose tmux session no longer exists at all.
    pub(crate) fn session_gone(session_name: impl Into<String>) -> Self {
        Self::with_spawn_token("", session_name, PaneLiveness::SessionGone, TEST_SPAWN_TOKEN)
    }

    /// A pane whose session exists but tmux itself reports `#{pane_dead}`.
    pub(crate) fn pane_reported_dead(session_name: impl Into<String>) -> Self {
        Self::with_spawn_token("", session_name, PaneLiveness::PaneDead, TEST_SPAWN_TOKEN)
    }

    /// A pane whose session name is present, but whose live spawn token no
    /// longer matches the run row — the session was recycled.
    pub(crate) fn spawn_token_mismatch(session_name: impl Into<String>) -> Self {
        Self::with_spawn_token("claude", session_name, PaneLiveness::Alive, "tok-other")
    }

    pub(crate) fn with_identity(session_name: impl Into<String>, spawn_token: impl Into<String>) -> Self {
        Self::with_spawn_token("claude", session_name, PaneLiveness::Alive, spawn_token)
    }

    fn with_spawn_token(
        foreground_process: impl Into<String>,
        session_name: impl Into<String>,
        liveness: PaneLiveness,
        spawn_token: impl Into<String>,
    ) -> Self {
        Self {
            state: RecordingPaneState {
                session_name: session_name.into(),
                spawn_token: spawn_token.into(),
                liveness,
                foreground_process: foreground_process.into(),
                capture: CaptureBehavior::Empty,
            },
            calls: StdMutex::new(Vec::new()),
            stdin: StdMutex::new(Vec::new()),
            on_text_write: StdMutex::new(None),
            started: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn echo_last_paste(mut self) -> Self {
        self.state.capture = CaptureBehavior::EchoLastPaste;
        self
    }

    /// Fire `hook` once, the first time a text write (`load-buffer` or
    /// literal `send-keys -l`) is issued. Used to interleave teardown with
    /// an in-flight pane write.
    pub(crate) fn set_on_text_write(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_text_write.lock().unwrap() = Some(Box::new(hook));
    }

    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) fn stdin(&self) -> Vec<Vec<u8>> {
        self.stdin.lock().unwrap().clone()
    }

    /// How many `send-keys <session> Escape` calls were issued.
    pub(crate) fn escape_presses(&self) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.contains(&"send-keys".to_owned()) && call.last().map(String::as_str) == Some("Escape"))
            .count()
    }

    /// Whether any call carried prompt text into the pane, by either route
    /// `Tmux::send_keys` uses: `load-buffer` for multi-line text, literal
    /// `send-keys -l` chunks for single-line.
    pub(crate) fn wrote_text(&self) -> bool {
        self.calls().iter().any(|call| Self::call_is_text_write(call))
    }

    fn call_is_text_write(call: &[String]) -> bool {
        call.iter().any(|arg| arg == "load-buffer")
            || (call.iter().any(|arg| arg == "send-keys") && call.iter().any(|arg| arg == "-l"))
    }

    fn maybe_fire_text_write(&self, recorded: &[String]) {
        if !Self::call_is_text_write(recorded) {
            return;
        }
        let hook = self.on_text_write.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
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
            return Self::success(match self.state.liveness {
                PaneLiveness::SessionGone => String::new(),
                PaneLiveness::Alive | PaneLiveness::PaneDead => format!("{}\t\n", self.state.session_name),
            });
        }
        if args.iter().any(|arg| arg == "show-environment") {
            return Self::success(format!("BOSS_SPAWN_TOKEN={}\n", self.state.spawn_token));
        }
        if args.iter().any(|arg| arg == "#{pane_current_command}") {
            return Self::success(format!("{}\n", self.state.foreground_process));
        }
        if args.iter().any(|arg| arg == "#{pane_dead}") {
            return Self::success(match self.state.liveness {
                PaneLiveness::PaneDead => "1\n",
                PaneLiveness::Alive | PaneLiveness::SessionGone => "0\n",
            });
        }
        if args.iter().any(|arg| arg == "#{pane_dead_status}") {
            return Self::success(match self.state.liveness {
                PaneLiveness::PaneDead => "1\n",
                PaneLiveness::Alive | PaneLiveness::SessionGone => "",
            });
        }
        if args.iter().any(|arg| arg == "capture-pane") {
            return match self.state.capture {
                CaptureBehavior::Empty => Self::success(""),
                CaptureBehavior::EchoLastPaste => {
                    let last = self.stdin.lock().unwrap().last().cloned().unwrap_or_default();
                    Self::success(String::from_utf8_lossy(&last).into_owned())
                }
            };
        }
        Self::success("")
    }
}

#[async_trait::async_trait]
impl CommandRunner for RecordingPaneRunner {
    async fn run(&self, _program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none());
        let recorded: Vec<String> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        self.calls.lock().unwrap().push(recorded.clone());
        self.maybe_fire_text_write(&recorded);
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
        let recorded: Vec<String> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        self.calls.lock().unwrap().push(recorded.clone());
        self.stdin.lock().unwrap().push(stdin.to_vec());
        self.maybe_fire_text_write(&recorded);
        self.started.notify_one();
        Ok(self.response(args))
    }
}

pub(crate) fn tmux_with_runner(runner: Arc<RecordingPaneRunner>) -> Tmux {
    Tmux::with_runner_and_socket("/usr/bin/tmux", runner, boss_tmux::TEST_SOCKET_PATH).unwrap()
}

pub(crate) fn fake_tmux(replies: impl IntoIterator<Item = CommandOutput>) -> (Tmux, Arc<StubRunner>) {
    let runner = StubRunner::replies(replies);
    (
        Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap(),
        runner,
    )
}

/// Give a teardown fixture the same durable identity a successful spawn writes.
/// The scripted server verifies the token twice before the real process reap.
pub(super) fn install_teardown(server: &super::ServerState, execution_id: &str, pane_pid: i64) {
    let db = &server.work_db;
    if db.list_runs(execution_id).unwrap().is_empty() {
        db.create_run(
            boss_protocol::CreateRunInput::builder()
                .execution_id(execution_id)
                .agent_id("worker-1")
                .build(),
        )
        .unwrap();
    }
    let token = format!("token-{execution_id}");
    assert!(
        db.record_tmux_spawn_intent_for_execution(execution_id, boss_tmux::SERVER_LABEL, "boss-test-worker", &token)
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(execution_id, &token, pane_pid)
            .unwrap()
    );
    let env = format!("BOSS_SPAWN_TOKEN={token}\n");
    let (tmux, _) = fake_tmux([ok(&env), ok("0"), ok(&env), ok("")]);
    server.set_tmux_override_for_test(tmux);
}

pub(super) fn seed_teardown(server: &super::ServerState) -> String {
    let product = crate::test_support::create_product(&server.work_db);
    let item = crate::test_support::create_active_chore(&server.work_db, &product, "tmux worker");
    let execution = crate::test_support::create_old_execution(&server.work_db, &item);
    install_teardown(server, &execution, 4_194_303);
    execution
}
