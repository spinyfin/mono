//! Shared stubbed [`CommandRunner`] for tmux-teardown tests, so
//! `tmux_teardown.rs`, `worker_process_reaping.rs`, and
//! `worker_pane_lifecycle.rs` don't each hand-roll their own copy —
//! mirroring [`crate::tmux_adoption`]'s `FakeTmuxServer` pattern.

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

/// Always-alive tmux pane for delivery tests. Answers list-sessions, token,
/// pane-dead, send-keys, and capture-pane (echoing the last paste so
/// confirmation does not wait out the verify timeout).
pub(crate) struct AlivePaneRunner {
    session_name: String,
    spawn_token: String,
    session_present: bool,
    calls: StdMutex<Vec<Vec<String>>>,
    stdin: StdMutex<Vec<Vec<u8>>>,
}

impl AlivePaneRunner {
    pub(crate) fn new(session_name: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            session_name: session_name.into(),
            spawn_token: TEST_SPAWN_TOKEN.to_owned(),
            session_present: true,
            calls: StdMutex::new(Vec::new()),
            stdin: StdMutex::new(Vec::new()),
        })
    }

    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) fn stdin(&self) -> Vec<Vec<u8>> {
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
            return Self::success(if self.session_present {
                format!("{}\t\n", self.session_name)
            } else {
                String::new()
            });
        }
        if args.iter().any(|arg| arg == "show-environment") {
            return Self::success(format!("BOSS_SPAWN_TOKEN={}\n", self.spawn_token));
        }
        if args.iter().any(|arg| arg == "#{pane_dead}") {
            return Self::success("0\n");
        }
        if args.iter().any(|arg| arg == "capture-pane") {
            let last = self.stdin.lock().unwrap().last().cloned().unwrap_or_default();
            return Self::success(String::from_utf8_lossy(&last).into_owned());
        }
        Self::success("")
    }
}

#[async_trait::async_trait]
impl CommandRunner for AlivePaneRunner {
    async fn run(&self, _program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none());
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect());
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
        Ok(self.response(args))
    }
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
