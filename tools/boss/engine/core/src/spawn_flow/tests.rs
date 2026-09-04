use super::*;
use crate::driver::test_support::{codex_homes_override, transcript_store_override};
use crate::protocol::{AttachWorkerPaneResult, EngineToAppError};
use boss_execution_contracts::SendToAppError;
use boss_protocol::WorkItemBinding;
use boss_tmux::{CommandOutput, CommandRunner, Tmux};
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use tempfile::TempDir;

struct StubSpawner {
    registry: WorkerRegistry,
    tmux_runner: Arc<RecordingTmuxRunner>,
    spawn_calls: Arc<AtomicUsize>,
    canned_response: Result<EngineToAppResponse, SendToAppError>,
    last_request: std::sync::Mutex<Option<EngineToAppRequest>>,
}

#[async_trait::async_trait]
impl WorkerSpawner for StubSpawner {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        _timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        self.spawn_calls.fetch_add(1, Ordering::SeqCst);
        *self.last_request.lock().unwrap() = Some(request);
        self.canned_response.clone().map_err(|e| match e {
            SendToAppError::NotRegistered => SendToAppError::NotRegistered,
            SendToAppError::SessionWedged => SendToAppError::SessionWedged,
            SendToAppError::AppDisconnected => SendToAppError::AppDisconnected,
            SendToAppError::Timeout => SendToAppError::Timeout,
            SendToAppError::ResponseKindMismatch(s) => SendToAppError::ResponseKindMismatch(s),
        })
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        &self.registry
    }
}

impl StubSpawner {
    fn last_spawn_env(&self) -> Vec<(String, String)> {
        let calls = self.tmux_runner.calls();
        let create = calls
            .iter()
            .find(|args| args.get(2).map(String::as_str) == Some("new-session"))
            .unwrap();
        create
            .windows(2)
            .filter(|pair| pair[0] == "-e")
            .map(|pair| {
                let (key, value) = pair[1].split_once('=').unwrap();
                (key.to_owned(), value.to_owned())
            })
            .collect()
    }
}

fn sample_input(workspace: &TempDir, runner: Arc<RecordingTmuxRunner>) -> StartWorkerInput {
    StartWorkerInput {
        run_id: "run-test".into(),
        lease_id: "lease-test".into(),
        slot_id: 3,
        workspace_path: workspace.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        frontend_socket_path: Some(PathBuf::from("/tmp/engine.sock")),
        control_token_path: Some(PathBuf::from("/tmp/engine-control.token")),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        initial_input: "claude\n".into(),
        extra_env: vec![],
        title_summary: None,
        task_title: None,
        work_item_binding: None,
        model: "claude-opus-4-7".into(),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        pool: Some("main".into()),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
        driver: crate::driver::DriverRegistry::default()
            .require(crate::effort::ENGINE_DEFAULT_DRIVER)
            .expect("engine default driver is always registered"),
        tmux_host: TmuxWorkerHost::new(
            Tmux::with_runner_and_socket("/fake/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap(),
            runner.steps.clone(),
            "boss-3-run-test".into(),
        ),
        automation_outcome_proposals_seam_enabled: false,
        run_done_proposals_seam_enabled: false,
        worker_signal_proposals_seam_enabled: false,
        is_review_supervisor: false,
        is_post_merge_reviewer: false,
        checkout_positioned_on_pr_head: false,
        live_metadata: Default::default(),
    }
}

#[derive(Default)]
struct RecordingTmuxStore {
    steps: std::sync::Mutex<Vec<&'static str>>,
    fail_intent: bool,
    missing_run: bool,
}

impl RecordingTmuxStore {
    fn steps(&self) -> Vec<&'static str> {
        self.steps.lock().unwrap().clone()
    }
}

impl TmuxSpawnStore for RecordingTmuxStore {
    fn record_tmux_spawn_intent(
        &self,
        _execution_id: &str,
        _server_label: &str,
        _session_name: &str,
        _spawn_token: &str,
    ) -> anyhow::Result<bool> {
        self.steps.lock().unwrap().push("intent");
        if self.fail_intent {
            anyhow::bail!("intent write failed");
        }
        Ok(!self.missing_run)
    }

    fn record_tmux_session_created(
        &self,
        _execution_id: &str,
        _spawn_token: &str,
        _pane_pid: i64,
        _session_name: &str,
        _server_label: &str,
    ) -> anyhow::Result<bool> {
        self.steps.lock().unwrap().push("created");
        Ok(true)
    }
}

#[derive(Default)]
struct TmuxResponseOverrides {
    fail_command: Option<String>,
    pane_pid_stdout: String,
}

#[derive(Default)]
struct RecordingTmuxRunner {
    calls: std::sync::Mutex<Vec<Vec<String>>>,
    stdin: std::sync::Mutex<Vec<Vec<u8>>>,
    steps: Arc<RecordingTmuxStore>,
    tmux_version_stdout: String,
    response: TmuxResponseOverrides,
}

impl RecordingTmuxRunner {
    fn new(steps: Arc<RecordingTmuxStore>) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            stdin: std::sync::Mutex::new(Vec::new()),
            steps,
            tmux_version_stdout: "tmux 3.6a\n".to_owned(),
            response: TmuxResponseOverrides {
                fail_command: None,
                pane_pid_stdout: "4242\n".into(),
            },
        }
    }

    /// Overrides the probed `tmux -V` output, for tests exercising the
    /// pre-3.4 `source-file -t` fallback.
    fn with_tmux_version(mut self, version_line: &str) -> Self {
        self.tmux_version_stdout = version_line.to_owned();
        self
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    fn stdin(&self) -> Vec<Vec<u8>> {
        self.stdin.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl CommandRunner for RecordingTmuxRunner {
    async fn run(&self, _program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none());
        let args = args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if self.response.fail_command.as_deref() == args.get(2).map(String::as_str) {
            self.calls.lock().unwrap().push(args);
            return Ok(CommandOutput {
                success: false,
                code: Some(1),
                stdout: String::new(),
                stderr: "injected tmux failure".into(),
            });
        }
        let (step, stdout) = match args.get(2).map(String::as_str) {
            Some("start-server") => ("server-bootstrap", ""),
            Some("-V") => ("version", self.tmux_version_stdout.as_str()),
            Some("new-session") => ("new-session", ""),
            Some("show-options") => ("presentation", ""),
            Some("set-option") if args.get(3).map(String::as_str) == Some("-g") => {
                match args.get(4).map(String::as_str) {
                    Some("history-limit") | Some("remain-on-exit") => ("server-bootstrap", ""),
                    other => panic!("unexpected tmux global set-option: {other:?}, args={args:?}"),
                }
            }
            Some("set-option") if args.get(3).map(String::as_str) == Some("-s") => {
                match args.get(4).map(String::as_str) {
                    Some("terminal-features[100]") | Some("extended-keys") | Some("focus-events") => {
                        ("presentation", "")
                    }
                    other => panic!("unexpected tmux server set-option: {other:?}, args={args:?}"),
                }
            }
            Some("set-option") => match args.get(5).map(|option| option.as_str()) {
                Some("status") | Some("remain-on-exit") => ("presentation", ""),
                Some("@boss_spawn_token") => ("label", ""),
                other => panic!("unexpected tmux set-option: {other:?}, args={args:?}"),
            },
            Some("display-message") => ("pane-pid", self.response.pane_pid_stdout.as_str()),
            other => panic!("unexpected tmux command: {other:?}, args={args:?}"),
        };
        self.steps.steps.lock().unwrap().push(step);
        self.calls.lock().unwrap().push(args);
        Ok(CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    async fn run_with_stdin(
        &self,
        _program: &Path,
        args: &[OsString],
        cwd: Option<&Path>,
        stdin: &[u8],
    ) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none());
        let args = args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args.get(2).map(String::as_str),
            Some("source-file"),
            "unexpected tmux stdin command: {args:?}"
        );
        self.steps.steps.lock().unwrap().push("source-file");
        self.stdin.lock().unwrap().push(stdin.to_vec());
        self.calls.lock().unwrap().push(args);
        Ok(CommandOutput {
            success: true,
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[tokio::test]
async fn tmux_hosted_spawn_commits_intent_before_creation_and_carries_env_with_e_flags() {
    let workspace = TempDir::new().unwrap();
    let registry = WorkerRegistry::new();
    let spawner = StubSpawner {
        registry: registry.clone(),
        spawn_calls: Arc::new(AtomicUsize::new(0)),
        tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
        last_request: std::sync::Mutex::new(None),
        canned_response: Err(SendToAppError::NotRegistered),
    };
    let store = Arc::new(RecordingTmuxStore::default());
    let runner = Arc::new(RecordingTmuxRunner::new(store.clone()));
    let tmux =
        Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap();
    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.tmux_host = TmuxWorkerHost::new(tmux, store.clone(), "boss-3-run-test".to_owned());

    let started = start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    assert_eq!(started.shell_pid, 4242);
    assert_eq!(registry.lookup_unverified(4242).as_deref(), Some("run-test"));
    assert_eq!(
        spawner.spawn_calls.load(Ordering::SeqCst),
        1,
        "tmux path must attach an app viewer surface after creating the session"
    );
    match spawner.last_request.lock().unwrap().clone() {
        Some(EngineToAppRequest::AttachWorkerPane(request)) => {
            assert_eq!(request.run_id, "run-test");
            assert_eq!(request.slot_id, 3);
            assert_eq!(request.session_name, "boss-3-run-test");
            assert_eq!(request.tmux_socket_path, boss_tmux::TEST_SOCKET_PATH);
        }
        other => panic!("expected AttachWorkerPane request, got {other:?}"),
    }
    assert_eq!(
        store.steps(),
        vec![
            "intent",
            "server-bootstrap",
            "new-session",
            "presentation",
            "presentation",
            "presentation",
            "presentation",
            "presentation",
            "presentation",
            "label",
            "pane-pid",
            "created"
        ]
    );

    let calls = runner.calls();
    assert_eq!(
        calls[0],
        vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "start-server",
            ";",
            "set-option",
            "-s",
            "exit-empty",
            "off",
            ";",
            "set-option",
            "-g",
            "history-limit",
            "2000",
            ";",
            "set-option",
            "-g",
            "remain-on-exit",
            "on"
        ]
    );
    let create = &calls[1];
    assert_eq!(
        &create[..5],
        ["-S", boss_tmux::TEST_SOCKET_PATH, "new-session", "-d", "-s"]
    );
    assert!(create.windows(2).any(|pair| pair == ["-e", "BOSS_RUN_ID=run-test"]));
    assert!(create.windows(2).any(|pair| pair == ["-e", "BOSS_SESSION_SCHEMA=1"]));
    crate::tmux_session_options::assert_color_environment(create);
    assert!(
        create
            .windows(2)
            .any(|pair| pair[0] == "-e" && pair[1].starts_with("BOSS_SPAWN_TOKEN="))
    );
    assert!(
        create
            .windows(2)
            .any(|pair| pair == ["-e", "BOSS_EVENTS_SOCKET=/tmp/events.sock"])
    );
    assert!(
        create.last().unwrap().contains(" -l -i -c "),
        "tmux must launch through WorkerPaneLaunch's interactive login shell: {create:?}"
    );
    assert_eq!(
        calls[7],
        [
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "set-option",
            "-s",
            "terminal-features[100]",
            "xterm*:extkeys",
            ";",
            "set-option",
            "-s",
            "extended-keys",
            "on",
            ";",
            "set-option",
            "-s",
            "focus-events",
            "on",
            ";",
            "set-option",
            "-t",
            "boss-3-run-test",
            "status",
            "off",
            ";",
            "set-option",
            "-t",
            "boss-3-run-test",
            "remain-on-exit",
            "on"
        ]
    );
    assert_eq!(
        &calls[8][..6],
        [
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "set-option",
            "-t",
            "boss-3-run-test",
            "@boss_spawn_token"
        ]
    );
    let pane_pid = calls
        .iter()
        .position(|call| call.get(2).map(String::as_str) == Some("display-message"))
        .expect("expected pane-pid read after options");
    let features = calls
        .iter()
        .position(|call| call.iter().any(|argument| argument == "terminal-features[100]"))
        .expect("expected terminal-features write");
    let extended = calls
        .iter()
        .position(|call| call.iter().any(|argument| argument == "extended-keys"))
        .expect("expected extended-keys write");
    let focus = calls
        .iter()
        .position(|call| call.iter().any(|argument| argument == "focus-events"))
        .expect("expected focus-events write");
    assert!(
        features < pane_pid && extended < pane_pid && focus < pane_pid,
        "server options must be set before the attach identity is returned"
    );
    assert_eq!(
        calls[9],
        vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "display-message",
            "-p",
            "-t",
            "boss-3-run-test",
            "#{pane_pid}"
        ]
    );
}

/// Codex-only tmux mouse fix (see `codex-tmux.conf` / `tmux_session_config_for`):
/// a codex tmux-hosted spawn must source the codex tmux session config
/// scoped to its own session (`source-file -t <session>`), while the
/// claude-driver test above (same tmux-hosted path) proves no such call
/// happens for a driver with no `tmux_session_config_for` entry. Written
/// as a sync test driving its own current-thread runtime — mirroring
/// `codex_spawn_registers_live_worker_state_like_claude` — because
/// `codex_homes_override`'s guard must stay held across the `await`.
#[test]
fn tmux_hosted_codex_spawn_sources_the_codex_mouse_config_scoped_to_its_session() {
    let homes = TempDir::new().unwrap();
    let _homes_env = codex_homes_override(homes.path());
    let transcripts = TempDir::new().unwrap();
    let _transcripts_env = transcript_store_override(transcripts.path());

    let workspace = TempDir::new().unwrap();
    let spawner = StubSpawner {
        registry: WorkerRegistry::new(),
        spawn_calls: Arc::new(AtomicUsize::new(0)),
        tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
        last_request: std::sync::Mutex::new(None),
        canned_response: Err(SendToAppError::NotRegistered),
    };
    let store = Arc::new(RecordingTmuxStore::default());
    let runner = Arc::new(RecordingTmuxRunner::new(store.clone()));
    let tmux =
        Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap();
    let mut input = codex_input(&workspace, "run-codex-tmux", 1, spawner.tmux_runner.clone());
    input.tmux_host = TmuxWorkerHost::new(tmux, store.clone(), "boss-1-run-codex-tmux".to_owned());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        start_worker(&spawner, input, StdDuration::from_secs(1))
            .await
            .expect("codex tmux-hosted spawn should succeed");
        spawner.stop_progress_ingress("run-codex-tmux");
    });

    assert!(
        store.steps().contains(&"source-file"),
        "codex spawn must source the codex tmux session config: {:?}",
        store.steps()
    );
    let sourced = runner.stdin();
    let content = sourced
        .last()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .expect("expected a source-file stdin payload");
    assert!(content.contains("mouse on"), "unexpected sourced config: {content:?}");
    let directive_lines: Vec<&str> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert!(
        directive_lines.iter().all(|line| !line.contains("-g")),
        "codex tmux config must be session-scoped, not global: {directive_lines:?}"
    );
    let source_file_call = runner
        .calls()
        .into_iter()
        .find(|call| call.get(2).map(String::as_str) == Some("source-file"))
        .expect("expected a source-file call");
    assert_eq!(
        source_file_call,
        vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "source-file",
            "-t",
            "boss-1-run-codex-tmux",
            "-"
        ]
    );
}

/// `source-file -t` was only added in tmux 3.4; Boss's floor is 3.2, so
/// a codex spawn against an older-but-supported server must skip the
/// session config instead of sending the unknown flag and failing the
/// whole spawn over a cosmetic scrollback fix.
#[test]
fn tmux_hosted_codex_spawn_skips_session_config_on_pre_3_4_tmux() {
    let homes = TempDir::new().unwrap();
    let _homes_env = codex_homes_override(homes.path());
    let transcripts = TempDir::new().unwrap();
    let _transcripts_env = transcript_store_override(transcripts.path());

    let workspace = TempDir::new().unwrap();
    let spawner = StubSpawner {
        registry: WorkerRegistry::new(),
        spawn_calls: Arc::new(AtomicUsize::new(0)),
        tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
        last_request: std::sync::Mutex::new(None),
        canned_response: Err(SendToAppError::NotRegistered),
    };
    let store = Arc::new(RecordingTmuxStore::default());
    let runner = Arc::new(RecordingTmuxRunner::new(store.clone()).with_tmux_version("tmux 3.3a\n"));
    let tmux =
        Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap();
    let mut input = codex_input(&workspace, "run-codex-tmux-old", 1, spawner.tmux_runner.clone());
    input.tmux_host = TmuxWorkerHost::new(tmux, store.clone(), "boss-1-run-codex-tmux-old".to_owned());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        start_worker(&spawner, input, StdDuration::from_secs(1))
            .await
            .expect("codex tmux-hosted spawn should still succeed on pre-3.4 tmux");
        spawner.stop_progress_ingress("run-codex-tmux-old");
    });

    assert!(
        !store.steps().contains(&"source-file"),
        "pre-3.4 tmux must not receive `source-file -t`: {:?}",
        store.steps()
    );
    assert!(
        store.steps().contains(&"version"),
        "expected a version probe before deciding whether to source: {:?}",
        store.steps()
    );
}

#[tokio::test]
async fn happy_path_writes_files_sends_request_and_registers_pid() {
    let workspace = TempDir::new().unwrap();
    let registry = WorkerRegistry::new();
    let spawner = StubSpawner {
        registry: registry.clone(),
        spawn_calls: Arc::new(AtomicUsize::new(0)),
        tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
        last_request: std::sync::Mutex::new(None),
        canned_response: Ok(EngineToAppResponse::AttachWorkerPane {
            result: Ok(AttachWorkerPaneResult {}),
        }),
    };

    let started = start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await
    .unwrap();

    assert_eq!(started.slot_id, 3);
    assert_eq!(started.shell_pid, 4242);
    assert!(started.written_files.claude_md_path.exists());
    assert!(started.written_files.settings_path.exists());
    assert!(started.written_files.gitignore_path.exists());
    assert_eq!(registry.lookup_unverified(4242).as_deref(), Some("run-test"));
}

/// Call-site cutover acceptance: `start_worker` must use the driver Arc
/// on `StartWorkerInput` for workspace-file wiring — a non-Claude stub
/// yields its own config_dir / preamble, not Claude's.
#[tokio::test]
async fn start_worker_uses_resolved_non_claude_driver_for_workspace_files() {
    use crate::driver::test_support::{StubDriver, stub_descriptor};
    use crate::driver::{Capability, CapabilitySet};

    let workspace = TempDir::new().unwrap();
    let spawner = StubSpawner {
        registry: WorkerRegistry::new(),
        spawn_calls: Arc::new(AtomicUsize::new(0)),
        tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
        last_request: std::sync::Mutex::new(None),
        canned_response: Ok(EngineToAppResponse::AttachWorkerPane {
            result: Ok(AttachWorkerPaneResult {}),
        }),
    };

    let mut descriptor = stub_descriptor();
    descriptor.name = "stub-codex";
    descriptor.config_dir = ".stub";
    descriptor.agent_rules_filename = "AGENTS.md";
    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.driver = Arc::new(
        StubDriver::new(descriptor, CapabilitySet::new([Capability::Spawn]))
            .with_progress_fidelity(ProgressFidelity::Rich),
    );

    let started = start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    assert_eq!(
        started.written_files.claude_md_path,
        workspace.path().join(".stub").join("AGENTS.md"),
    );
    let rules = std::fs::read_to_string(&started.written_files.claude_md_path).unwrap();
    assert!(
        rules.contains("stub-driver preamble"),
        "start_worker must render the resolved driver's preamble, got: {rules:?}",
    );
    assert!(!workspace.path().join(".claude").join("CLAUDE.md").exists());
}

/// Local dispatch refuses a driver below `Rich` progress fidelity
/// before writing workspace files or asking the app to host a pane.
#[tokio::test]
async fn local_dispatch_refuses_below_rich_progress_fidelity() {
    use crate::driver::test_support::{StubDriver, stub_descriptor};
    use crate::driver::{Capability, CapabilitySet};

    let workspace = TempDir::new().unwrap();
    let registry = WorkerRegistry::new();
    let spawner = StubSpawner {
        registry: registry.clone(),
        spawn_calls: Arc::new(AtomicUsize::new(0)),
        tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
        last_request: std::sync::Mutex::new(None),
        canned_response: Err(SendToAppError::NotRegistered),
    };

    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.driver = Arc::new(StubDriver::new(
        stub_descriptor(),
        CapabilitySet::new([Capability::Spawn]),
    ));

    let result = start_worker(&spawner, input, StdDuration::from_secs(1)).await;
    assert!(
        matches!(
            result,
            Err(StartWorkerError::ProgressFidelity(ProgressFidelity::Minimal))
        ),
        "below-Rich local dispatch must be refused; got {result:?}",
    );
    assert_eq!(
        spawner.spawn_calls.load(Ordering::SeqCst),
        0,
        "a refused spawn must not contact the app",
    );
    assert!(
        registry.slot_for_run("run-test").is_none(),
        "a refused spawn must not register a run→slot mapping",
    );
    assert!(
        !workspace.path().join(".claude").join("CLAUDE.md").exists(),
        "a refused spawn must not write worker config",
    );
}

#[tokio::test]
async fn viewer_error_does_not_fail_the_worker() {
    let workspace = TempDir::new().unwrap();
    let mut spawner = ok_spawner_capturing();
    spawner.canned_response = Ok(EngineToAppResponse::AttachWorkerPane {
        result: Err(EngineToAppError::NoAvailableSlot),
    });
    let started = start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(started.shell_pid, 4242);
    assert_eq!(spawner.registry.lookup_unverified(4242).as_deref(), Some("run-test"));
}

/// Engine-owns-slots invariant: the slot the runner claimed for
/// this worker (set on `StartWorkerInput.slot_id`) must reach
/// the app verbatim on `AttachWorkerPaneInput.slot_id`. A drop
/// here would re-allow the app's old firstIndex(where:) heuristic
/// to silently override the engine's pick.
#[tokio::test]
async fn spawn_request_carries_engine_claimed_slot_id() {
    let workspace = TempDir::new().unwrap();
    let spawner = StubSpawner {
        registry: WorkerRegistry::new(),
        spawn_calls: Arc::new(AtomicUsize::new(0)),
        tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
        last_request: std::sync::Mutex::new(None),
        canned_response: Ok(EngineToAppResponse::AttachWorkerPane {
            result: Ok(AttachWorkerPaneResult {}),
        }),
    };
    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.slot_id = 7;

    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    let last = spawner.last_request.lock().unwrap().clone().unwrap();
    match last {
        EngineToAppRequest::AttachWorkerPane(req) => {
            assert_eq!(req.slot_id, 7);
        }
        other => panic!("expected AttachWorkerPane request, got {other:?}"),
    }
}

struct AbortTrackingSpawner {
    inner: StubSpawner,
    aborted: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl WorkerSpawner for AbortTrackingSpawner {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        self.inner.send_to_app_request(request, timeout).await
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        &self.inner.registry
    }

    async fn abort_worker_spawn(&self, run_id: &str) -> Result<(), String> {
        self.aborted.lock().unwrap().push(run_id.to_owned());
        Ok(())
    }
}

#[tokio::test]
async fn busy_viewer_fails_spawn_with_the_occupant_reason() {
    let workspace = TempDir::new().unwrap();
    let mut spawner = ok_spawner_capturing();
    spawner.canned_response = Ok(EngineToAppResponse::AttachWorkerPane {
        result: Err(EngineToAppError::SlotBusy {
            occupying_run_id: Some("run-husk".into()),
        }),
    });
    let spawner = AbortTrackingSpawner {
        inner: spawner,
        aborted: Default::default(),
    };
    let result = start_worker(
        &spawner,
        sample_input(&workspace, spawner.inner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await;
    assert!(
        matches!(result, Err(StartWorkerError::ViewerRejected(EngineToAppError::SlotBusy {
        occupying_run_id: Some(ref occupant),
    })) if occupant == "run-husk")
    );
    assert_eq!(spawner.inner.registry.slot_for_run("run-test"), None);
    assert_eq!(*spawner.aborted.lock().unwrap(), ["run-test"]);
}

#[tokio::test]
async fn write_failure_does_not_send_request() {
    let workspace = TempDir::new().unwrap();
    let registry = WorkerRegistry::new();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let spawner = StubSpawner {
        registry,
        spawn_calls: spawn_calls.clone(),
        tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
        last_request: std::sync::Mutex::new(None),
        canned_response: Ok(EngineToAppResponse::AttachWorkerPane {
            result: Ok(AttachWorkerPaneResult {}),
        }),
    };

    // Point at a path that's a regular file, not a directory, so
    // create_dir_all fails inside write_workspace_files.
    let blocked = workspace.path().join("blocked");
    std::fs::write(&blocked, b"i am a file").unwrap();
    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.workspace_path = blocked;

    let result = start_worker(&spawner, input, StdDuration::from_secs(1)).await;
    assert!(matches!(result, Err(StartWorkerError::WriteFiles(_))));
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
}

/// Sink that swallows every decoded progress envelope. The codex
/// live-state tests care that the file ingress can be *prepared and
/// activated* inside `start_worker`, not what it eventually decodes.
struct NoopEventSink;

#[async_trait::async_trait]
impl crate::stdout_progress::WorkerEventSink for NoopEventSink {
    async fn dispatch_worker_event(&self, _incoming: crate::events_socket::IncomingHookEvent) {}
}

/// Checkpoint store for the spawn-ordering tests, which have no DB. What
/// they assert is the *sequence* of collaborator calls, not the durable
/// resume point — that is covered where the tail actually runs, in
/// `agent_jsonl_progress`.
struct DiscardCheckpoints;

impl crate::agent_jsonl_progress::IngressCheckpointStore for DiscardCheckpoints {
    fn store_ingress_checkpoint(
        &self,
        _run_id: &str,
        _checkpoint: &crate::agent_jsonl_progress::IngressCheckpoint,
    ) -> Result<(), String> {
        Ok(())
    }

    fn load_ingress_checkpoint(
        &self,
        _run_id: &str,
    ) -> Result<Option<crate::agent_jsonl_progress::IngressCheckpoint>, String> {
        Ok(None)
    }
}

/// One observable step of `start_worker`'s collaborator sequence,
/// recorded in call order by [`LiveStateSpawner`].
///
/// The whole point of the Codex/Claude pair below is the *ordering*
/// (prepare → pane request → register → activate), so the tests assert
/// against this sequence rather than against end state only. Each
/// variant carries the fact that would otherwise be unobservable: which
/// ingress variant the driver asked for, and whether the live-state
/// entry existed by the time activation ran.
#[derive(Debug, PartialEq, Eq)]
enum SpawnStep {
    /// `prepare_progress_ingress`, tagged with the `ProgressIngress`
    /// variant the resolved driver returned.
    Prepared(&'static str),
    /// The viewer attachment request after tmux worker creation.
    PaneRequested,
    /// `activate_progress_ingress`, tagged with whether the slot's
    /// live-state entry was already registered when it fired.
    Activated { registered: bool },
    /// `note_worker_viewer_attach_failed`: the worker started but the
    /// app did not give it a viewer, so production schedules the
    /// bounded re-attach.
    ViewerAttachFailedNoted,
}

/// Spawner that wires the two production collaborators `StubSpawner`
/// leaves at their trait defaults: a real [`LiveWorkerStateRegistry`]
/// and a real [`crate::agent_jsonl_progress::AgentJsonlProgressManager`].
///
/// Both are what a byte-stream driver actually exercises — the file
/// ingress is prepared *before* the pane request and the live-state
/// entry is stamped *after* it, so a driver whose ingress preparation
/// fails never reaches registration at all. Testing Codex against the
/// default no-op implementations would silently skip exactly the
/// ordering that matters.
///
/// Constructed via [`Self::new`] rather than the generated builder —
/// `#[derive(bon::Builder)]` is present solely to satisfy the project's
/// giant-struct convention (8 named fields).
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct LiveStateSpawner {
    registry: WorkerRegistry,
    tmux_runner: Arc<RecordingTmuxRunner>,
    live_states: LiveWorkerStateRegistry,
    jsonl: crate::agent_jsonl_progress::AgentJsonlProgressManager,
    slot_id: u8,
    spawn_calls: Arc<AtomicUsize>,
    steps: std::sync::Mutex<Vec<SpawnStep>>,
    /// When set, the fake app answers `AttachWorkerPane` with this
    /// error instead of a viewer (the worker itself still starts).
    viewer_refusal: Option<EngineToAppError>,
}

impl LiveStateSpawner {
    fn new(slot_id: u8) -> Self {
        Self {
            registry: WorkerRegistry::new(),
            tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
            live_states: LiveWorkerStateRegistry::new(),
            jsonl: crate::agent_jsonl_progress::AgentJsonlProgressManager::new(),
            slot_id,
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            steps: std::sync::Mutex::new(Vec::new()),
            viewer_refusal: None,
        }
    }

    fn record(&self, step: SpawnStep) {
        self.steps.lock().expect("step log mutex poisoned").push(step);
    }

    /// The recorded sequence, for a whole-sequence `assert_eq!`.
    fn steps(&self) -> Vec<SpawnStep> {
        std::mem::take(&mut *self.steps.lock().expect("step log mutex poisoned"))
    }
}

#[async_trait::async_trait]
impl WorkerSpawner for LiveStateSpawner {
    async fn send_to_app_request(
        &self,
        _request: EngineToAppRequest,
        _timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        self.spawn_calls.fetch_add(1, Ordering::SeqCst);
        self.record(SpawnStep::PaneRequested);
        Ok(EngineToAppResponse::AttachWorkerPane {
            result: match &self.viewer_refusal {
                Some(err) => Err(err.clone()),
                None => Ok(AttachWorkerPaneResult {}),
            },
        })
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        &self.registry
    }

    fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
        Some(&self.live_states)
    }

    async fn note_worker_viewer_attach_failed(&self, _run_id: &str) {
        self.record(SpawnStep::ViewerAttachFailedNoted);
    }

    fn prepare_progress_ingress(
        &self,
        run_id: &str,
        driver: Arc<dyn AgentDriver>,
        ingress: ProgressIngress,
    ) -> Result<(), String> {
        // Record the variant in *both* arms. Only `AgentJsonlFile`
        // has a run to prepare, but a test that asserted nothing about
        // which variant arrived would keep passing — while silently
        // exercising the no-op arm — if a driver's
        // `progress_observation_wiring` ever changed shape.
        let ingress = match ingress {
            ProgressIngress::AgentJsonlFile(ingress) => {
                self.record(SpawnStep::Prepared("AgentJsonlFile"));
                ingress
            }
            ProgressIngress::HookCallback(_) => {
                self.record(SpawnStep::Prepared("HookCallback"));
                return Ok(());
            }
            ProgressIngress::StdoutJsonl => {
                self.record(SpawnStep::Prepared("StdoutJsonl"));
                return Ok(());
            }
        };
        self.jsonl.prepare_run(
            run_id,
            driver,
            ingress,
            NoopEventSink,
            std::sync::Arc::new(DiscardCheckpoints),
        )
    }

    fn activate_progress_ingress(&self, run_id: &str) {
        self.record(SpawnStep::Activated {
            registered: self.live_states.get(self.slot_id).is_some(),
        });
        self.jsonl.activate_run(run_id);
    }

    fn stop_progress_ingress(&self, run_id: &str) {
        self.jsonl.stop_run(run_id);
    }
}

/// Build a `StartWorkerInput` for the real `CodexDriver`, plus the
/// per-run `CODEX_HOME` layout `CodexDriver::provision_workspace` leaves
/// behind in production: `sessions/` is a symlink into the durable
/// transcript store, not a real directory. Faking a real directory here
/// (as this fixture used to) would hide exactly the writer/checker
/// mismatch this test exists to catch, so it drives the real
/// `provision_durable_sessions` writer instead.
fn codex_input(workspace: &TempDir, run_id: &str, slot_id: u8, runner: Arc<RecordingTmuxRunner>) -> StartWorkerInput {
    let codex_home = crate::driver::codex::codex_home_for_run(run_id).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();
    crate::driver::transcript_store::provision_durable_sessions(&codex_home, "codex", run_id).unwrap();
    let mut input = sample_input(workspace, runner);
    input.run_id = run_id.to_owned();
    input.slot_id = slot_id;
    input.model = "gpt-6-astra".into();
    input.initial_input = "exec codex exec --json\n".into();
    input.work_item_binding = Some(WorkItemBinding {
        work_item_id: "task-codex-1".into(),
        work_item_name: "Hello world from Codex".into(),
        execution_id: run_id.to_owned(),
    });
    input.driver = crate::driver::DriverRegistry::default()
        .require("codex")
        .expect("codex driver is registered");
    input
}

/// Regression — `bossctl agents list` renders the engine's
/// [`LiveWorkerStateRegistry`] and nothing else, so a worker missing
/// from it is invisible to every operator verb keyed on that list
/// (`agents status`, `agents send`, `agents stop`, the coordinator's
/// "who is working on X" lookup).
///
/// Codex is the first driver whose progress arrives over a byte stream
/// (`ProgressIngress::AgentJsonlFile`) rather than the hook socket, and
/// that ingress is prepared *before* tmux worker creation — an unhappy
/// preparation short-circuits `start_worker` before it ever reaches the
/// registration below. This pins that a Codex spawn lands in the same
/// registry, with the same routing stamps, as a Claude spawn.
///
/// Written as a sync test driving its own current-thread runtime rather
/// than `#[tokio::test]`: `CodexDriver` resolves its homes root from the
/// process environment, so the override guard has to stay held for the
/// whole spawn — and holding a `MutexGuard` across an `.await` is the
/// real hazard `clippy::await_holding_lock` names. `block_on` keeps the
/// guard inside a single blocking call on a runtime this test owns, so
/// there is nothing to starve.
#[test]
fn codex_spawn_registers_live_worker_state_like_claude() {
    let homes = TempDir::new().unwrap();
    let _homes_env = codex_homes_override(homes.path());

    // `progress_observation_wiring` watches the durable transcript store
    // directly (see [`durable_sessions_dir`]) rather than the
    // `CODEX_HOME/sessions` symlink `provision_workspace` points at it —
    // that link is itself a symlink and fails the progress ingress's
    // real-directory precondition. Override the store root so
    // `codex_input`'s real `provision_durable_sessions` call lands
    // somewhere this test controls.
    let transcripts = TempDir::new().unwrap();
    let _transcripts_env = transcript_store_override(transcripts.path());

    let workspace = TempDir::new().unwrap();
    let spawner = LiveStateSpawner::new(4);
    let input = codex_input(&workspace, "exec-codex-1", 4, spawner.tmux_runner.clone());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    let started = runtime.block_on(async {
        let started = start_worker(&spawner, input, StdDuration::from_secs(1))
            .await
            .expect("codex spawn should succeed");
        // Cancel the file ingress inside the runtime that owns its task,
        // so the tempdir is not pulled out from under a live poller.
        spawner.stop_progress_ingress("exec-codex-1");
        started
    });

    assert_eq!(spawner.spawn_calls.load(Ordering::SeqCst), 1);
    assert_eq!(started.slot_id, 4);
    // The ordering this test exists to pin: the byte-stream ingress is
    // genuinely prepared (not the no-op arm) *before* the pane request,
    // and activated only once the slot is registered.
    assert_eq!(
        spawner.steps(),
        vec![
            SpawnStep::Prepared("AgentJsonlFile"),
            SpawnStep::PaneRequested,
            SpawnStep::Activated { registered: true },
        ],
    );

    let state = spawner
        .live_states
        .get(4)
        .expect("a Codex spawn must register a LiveWorkerState — `agents list` reads only this registry");
    assert_eq!(state.run_id, "exec-codex-1");
    assert_eq!(state.model, "gpt-6-astra");
    assert_eq!(state.shell_pid, 4242);
    assert_eq!(state.pool.as_deref(), Some("main"));
    assert_eq!(state.kind.as_deref(), Some("chore_implementation"));
    assert_eq!(state.work_item_id.as_deref(), Some("task-codex-1"));
}

/// A non-`SlotBusy` viewer refusal at spawn must not fail the worker
/// (the tmux session owns it), but it must not be silent either: the
/// reason is stamped on the live state the app renders, and the
/// spawner is told so production can schedule the bounded re-attach.
#[tokio::test]
async fn refused_spawn_time_viewer_attach_is_stamped_on_the_live_state_and_noted() {
    let workspace = TempDir::new().unwrap();
    let mut spawner = LiveStateSpawner::new(3);
    spawner.viewer_refusal = Some(EngineToAppError::Internal {
        message: "pane allocator not ready".into(),
    });

    let started = start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await
    .expect("a refused viewer must not fail the spawn");
    assert_eq!(started.slot_id, 3);

    assert_eq!(
        spawner.steps(),
        vec![
            SpawnStep::Prepared("HookCallback"),
            SpawnStep::PaneRequested,
            SpawnStep::Activated { registered: true },
            SpawnStep::ViewerAttachFailedNoted,
        ],
        "the failure is noted only after the live state exists, so the retry can find the run",
    );
    let state = spawner.live_states.get(3).expect("the worker is registered headless");
    assert_eq!(
        state.viewer_attach_error.as_deref(),
        Some("the app refused the viewer: app internal error: pane allocator not ready"),
    );
}

/// The same spawn against the Claude driver, so the assertion above is
/// pinned as a *parity* claim rather than a Codex-only snapshot: if a
/// future change makes registration conditional on the ingress kind,
/// exactly one of this pair fails and names which side regressed. The
/// step sequence asserts the complementary ingress variant, so the two
/// halves cannot silently collapse onto the same path.
#[tokio::test]
async fn claude_spawn_registers_live_worker_state() {
    let workspace = TempDir::new().unwrap();
    let spawner = LiveStateSpawner::new(3);

    start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await
    .unwrap();

    assert_eq!(
        spawner.steps(),
        vec![
            SpawnStep::Prepared("HookCallback"),
            SpawnStep::PaneRequested,
            SpawnStep::Activated { registered: true },
        ],
    );

    let state = spawner
        .live_states
        .get(3)
        .expect("a Claude spawn registers a LiveWorkerState");
    assert_eq!(state.run_id, "run-test");
    assert_eq!(
        state.viewer_attach_error, None,
        "a spawn whose viewer attached must not carry a failure reason"
    );
    assert_eq!(state.pool.as_deref(), Some("main"));
}

fn ok_spawner_capturing() -> StubSpawner {
    StubSpawner {
        registry: WorkerRegistry::new(),
        spawn_calls: Arc::new(AtomicUsize::new(0)),
        tmux_runner: Arc::new(RecordingTmuxRunner::new(Arc::default())),
        last_request: std::sync::Mutex::new(None),
        // Echo whatever sample_input claims (slot 3) so the
        // engine-side debug_assert that the app honored the
        // claimed slot doesn't fire in tests.
        canned_response: Ok(EngineToAppResponse::AttachWorkerPane {
            result: Ok(AttachWorkerPaneResult {}),
        }),
    }
}

#[tokio::test]
async fn env_includes_sanitized_path_and_engine_keys() {
    // The exact-PATH assertion below holds only while no other test has
    // `BOSS_BIN_DIR` set.
    let _ambient = BOSS_BIN_DIR_TEST_LOCK.lock().await;
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await
    .unwrap();

    let env = spawner.last_spawn_env();
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .expect("PATH should always be set on worker spawn")
        .1
        .clone();
    assert_eq!(path, WORKER_SANITIZED_PATH);
    assert!(
        !path.contains("/Users/"),
        "sanitized PATH must not contain user bin dir"
    );
    assert!(!path.contains(".cargo"), "sanitized PATH must not contain cargo bin");

    assert_eq!(
        env.iter()
            .find(|(k, _)| k == "BOSS_EVENTS_SOCKET")
            .map(|(_, v)| v.as_str()),
        Some("/tmp/events.sock"),
    );
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == crate::config::FRONTEND_SOCKET_ENV)
            .map(|(_, v)| v.as_str()),
        Some("/tmp/engine.sock"),
        "workers must inherit the bound frontend socket as BOSS_SOCKET_PATH so `boss` CLI verbs do not resolve engine.sock from a driver-scoped HOME",
    );
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == crate::engine_control::TOKEN_PATH_ENV)
            .map(|(_, v)| v.as_str()),
        Some("/tmp/engine-control.token"),
        "workers must inherit the bound control-token path so `boss engine stop` does not derive <stem>.control-token from BOSS_SOCKET_PATH",
    );
    assert_eq!(
        env.iter().find(|(k, _)| k == "BOSS_LEASE_ID").map(|(_, v)| v.as_str()),
        Some("lease-test"),
    );
}

#[tokio::test]
async fn omitted_frontend_socket_is_not_exported() {
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();
    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.frontend_socket_path = None;
    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();
    let env = spawner.last_spawn_env();
    assert!(
        !env.iter().any(|(k, _)| k == crate::config::FRONTEND_SOCKET_ENV),
        "an unstamped config must not invent a production BOSS_SOCKET_PATH"
    );
    assert!(
        !env.iter().any(|(k, _)| k == crate::engine_control::TOKEN_PATH_ENV),
        "control-token export rides with the frontend-socket export"
    );
}

#[tokio::test]
async fn omitted_control_token_is_not_exported() {
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();
    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.control_token_path = None;
    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();
    let env = spawner.last_spawn_env();
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == crate::config::FRONTEND_SOCKET_ENV)
            .map(|(_, v)| v.as_str()),
        Some("/tmp/engine.sock"),
    );
    assert!(
        !env.iter().any(|(k, _)| k == crate::engine_control::TOKEN_PATH_ENV),
        "an unstamped control-token path must not invent a sibling of the socket"
    );
}

#[tokio::test]
async fn worker_bin_dir_is_prepended_ahead_of_the_sanitized_path() {
    // The launcher dir holds a `boss` pinned to an absolute path. It
    // has to win over every other PATH entry, otherwise a bare `boss`
    // can still land on a build-from-source shim.
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.extra_env = vec![(
        boss_engine_worker_bin::WORKER_BIN_DIR_ENV.into(),
        "/tmp/boss-worker-settings/bin".into(),
    )];

    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    let env = spawner.last_spawn_env();
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .expect("PATH is always set")
        .1
        .clone();
    // Asserted positionally rather than by equality: BOSS_BIN_DIR may
    // or may not be set in the ambient env (installed vs dev mode),
    // and it inserts its own entry. What must hold either way is that
    // the launcher dir is first and the sanitized PATH still trails.
    assert_eq!(
        path.split(':').next(),
        Some("/tmp/boss-worker-settings/bin"),
        "launcher dir must be the first PATH entry, got {path}",
    );
    assert!(
        path.ends_with(WORKER_SANITIZED_PATH),
        "sanitized PATH must still be the tail, got {path}",
    );
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == boss_engine_worker_bin::WORKER_BIN_DIR_ENV)
            .map(|(_, v)| v.as_str()),
        Some("/tmp/boss-worker-settings/bin"),
        "the dir must also be exported so the pane's first shell line can re-prepend it",
    );
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == boss_engine_worker_bin::BOSS_BIN_ENV)
            .map(|(_, v)| v.as_str()),
        Some("/tmp/boss-worker-settings/bin/boss"),
        "BOSS_BIN must name the launcher, not a PATH entry",
    );
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == boss_engine_worker_bin::CUBE_BIN_ENV)
            .map(|(_, v)| v.as_str()),
        Some("/tmp/boss-worker-settings/bin/cube"),
        "CUBE_BIN must name the launcher, not a PATH entry",
    );
    assert!(
        !env.iter().any(|(k, _)| k == boss_engine_worker_bin::CHECKLEFT_BIN_ENV),
        "a repository without a REPOBIN.toml checkleft declaration must retain PATH behaviour",
    );
}

#[tokio::test]
async fn declared_checkleft_exports_the_worker_bin_launcher() {
    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("REPOBIN.toml"), "[tools.checkleft]\n").unwrap();
    let worker_bin_dir = TempDir::new().unwrap();
    // CHECKLEFT_BIN must only be exported when the launcher was actually
    // written -- create it here to model the real `ensure_worker_bin_dir`
    // success path, distinct from the write-failed case covered by
    // `declared_checkleft_without_a_written_launcher_is_not_exported`.
    std::fs::write(worker_bin_dir.path().join("checkleft"), b"#!/bin/sh\n").unwrap();
    let spawner = ok_spawner_capturing();
    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.extra_env = vec![(
        boss_engine_worker_bin::WORKER_BIN_DIR_ENV.into(),
        worker_bin_dir.path().display().to_string(),
    )];

    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    assert_eq!(
        spawner
            .last_spawn_env()
            .iter()
            .find(|(k, _)| k == boss_engine_worker_bin::CHECKLEFT_BIN_ENV)
            .map(|(_, v)| v.as_str()),
        Some(worker_bin_dir.path().join("checkleft").display().to_string().as_str()),
        "declared checkleft must name the worker-bin launcher",
    );
}

#[tokio::test]
async fn declared_checkleft_without_a_written_launcher_is_not_exported() {
    // REPOBIN.toml declares checkleft, but no launcher file exists at
    // the worker-bin dir. CHECKLEFT_BIN must not be exported
    // naming a file that is not there -- that would both fail the
    // worker's `"$CHECKLEFT_BIN" run` with ENOENT and, because the
    // launch guard derives WATCH_CHECKLEFT from CHECKLEFT_BIN being
    // set, block a bare `checkleft` at the same time.
    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("REPOBIN.toml"), "[tools.checkleft]\n").unwrap();
    let worker_bin_dir = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();
    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.extra_env = vec![(
        boss_engine_worker_bin::WORKER_BIN_DIR_ENV.into(),
        worker_bin_dir.path().display().to_string(),
    )];

    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    assert!(
        !spawner
            .last_spawn_env()
            .iter()
            .any(|(k, _)| k == boss_engine_worker_bin::CHECKLEFT_BIN_ENV),
        "must not export CHECKLEFT_BIN when the launcher file was never written",
    );
}

#[tokio::test]
async fn path_is_untouched_when_no_worker_bin_dir_is_supplied() {
    // The launcher dir is best-effort: a temp-dir failure must not
    // corrupt PATH with an empty leading entry (which `sh` reads as
    // the current directory).
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await
    .unwrap();

    let env = spawner.last_spawn_env();
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .expect("PATH is always set")
        .1
        .clone();
    assert!(
        path.ends_with(WORKER_SANITIZED_PATH),
        "sanitized PATH must be intact, got {path}",
    );
    assert!(!path.starts_with(':'), "PATH must not gain an empty leading entry");
    assert!(
        !path.contains("::"),
        "PATH must not gain an empty entry (`::` is the current directory to sh), got {path}",
    );
    assert!(
        !env.iter().any(|(k, _)| k == boss_engine_worker_bin::WORKER_BIN_DIR_ENV),
        "no launcher dir supplied means no launcher dir exported",
    );
}

/// Pure unit of the installed-mode env wiring: given a bundle bin
/// dir, workers get `BOSS_BIN_DIR`, `BOSS_BIN`, and a PATH prepend —
/// never a silent no-op, never a user-bin fallback.
#[test]
fn apply_boss_bin_dir_exports_boss_bin_and_prepends_path() {
    let mut env = vec![EnvVar {
        key: "PATH".into(),
        value: WORKER_SANITIZED_PATH.into(),
    }];
    let fake = "/tmp/fake-boss-app/Contents/Resources/bin";
    apply_boss_bin_dir_to_worker_env(&mut env, Some(fake));

    assert_eq!(
        env.iter().find(|e| e.key == "BOSS_BIN_DIR").map(|e| e.value.as_str()),
        Some(fake),
    );
    assert_eq!(
        env.iter().find(|e| e.key == "BOSS_BIN").map(|e| e.value.as_str()),
        Some("/tmp/fake-boss-app/Contents/Resources/bin/boss"),
    );
    assert_eq!(
        env.iter().find(|e| e.key == "CUBE_BIN").map(|e| e.value.as_str()),
        Some("/tmp/fake-boss-app/Contents/Resources/bin/cube"),
        "a bundle-derived CUBE_BIN must exist even if the per-workspace launcher dir write \
         later fails, so a rendered prompt never names an unset variable",
    );
    let path = env.iter().find(|e| e.key == "PATH").expect("PATH").value.as_str();
    assert_eq!(
        path,
        format!("{fake}:{WORKER_SANITIZED_PATH}"),
        "bundle bin dir must lead PATH so bare boss wins",
    );

    // Empty / missing must not invent empty PATH entries.
    let mut untouched = vec![EnvVar {
        key: "PATH".into(),
        value: WORKER_SANITIZED_PATH.into(),
    }];
    apply_boss_bin_dir_to_worker_env(&mut untouched, None);
    apply_boss_bin_dir_to_worker_env(&mut untouched, Some(""));
    assert_eq!(untouched.len(), 1);
    assert_eq!(untouched[0].value, WORKER_SANITIZED_PATH);
    assert!(!untouched.iter().any(|e| e.key == "BOSS_BIN" || e.key == "BOSS_BIN_DIR"));
}

/// `set_env_var` must overwrite an existing entry in place rather than
/// pushing a duplicate: a `.find(|e| e.key == ..)` consumer sees the
/// FIRST match, so a stale duplicate would silently win over a later,
/// more-specific value (the per-workspace launcher overwriting the
/// bundle path from `apply_boss_bin_dir_to_worker_env`).
#[test]
fn set_env_var_overwrites_rather_than_duplicates() {
    let mut env = vec![EnvVar {
        key: "PATH".into(),
        value: WORKER_SANITIZED_PATH.into(),
    }];
    apply_boss_bin_dir_to_worker_env(&mut env, Some("/tmp/fake-boss-app/Contents/Resources/bin"));
    assert_eq!(env.iter().filter(|e| e.key == "BOSS_BIN").count(), 1);
    assert_eq!(env.iter().filter(|e| e.key == "CUBE_BIN").count(), 1);

    set_env_var(&mut env, "BOSS_BIN", "/tmp/boss-worker-settings/bin/boss".into());
    set_env_var(&mut env, "CUBE_BIN", "/tmp/boss-worker-settings/bin/cube".into());

    assert_eq!(
        env.iter().filter(|e| e.key == "BOSS_BIN").count(),
        1,
        "must overwrite, not duplicate, BOSS_BIN"
    );
    assert_eq!(
        env.iter().filter(|e| e.key == "CUBE_BIN").count(),
        1,
        "must overwrite, not duplicate, CUBE_BIN"
    );
    assert_eq!(
        env.iter().find(|e| e.key == "BOSS_BIN").map(|e| e.value.as_str()),
        Some("/tmp/boss-worker-settings/bin/boss"),
        "the per-workspace launcher must win over the bundle path"
    );
    assert_eq!(
        env.iter().find(|e| e.key == "CUBE_BIN").map(|e| e.value.as_str()),
        Some("/tmp/boss-worker-settings/bin/cube"),
    );
}

#[tokio::test]
async fn env_forces_editor_vars_to_noop() {
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await
    .unwrap();

    let env = spawner.last_spawn_env();
    // Every editor-resolution env that git/jj/$EDITOR-aware tools
    // consult must be forced to a non-zero-exit no-op. `false`
    // causes the editor invocation to fail fast so the worker
    // notices and re-runs with `-m`.
    for key in ["EDITOR", "VISUAL", "GIT_EDITOR", "JJ_EDITOR"] {
        let value = env
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("expected env key {key} on worker spawn"))
            .1
            .as_str();
        assert_eq!(
            value, WORKER_EDITOR_NOOP,
            "{key} should be forced to {WORKER_EDITOR_NOOP}, got {value}",
        );
    }
}

#[tokio::test]
async fn env_forces_xai_api_key_to_block_interactive_grok_auth() {
    // Belt-and-suspenders against the "unsolicited browser auth"
    // incident: every worker pane gets a deliberately-invalid
    // XAI_API_KEY so any Grok invocation inside the pane — not just
    // the driver's own precheck-guarded spawn — fails loudly on a
    // missing/expired session token instead of falling through to
    // interactive device-code/browser OAuth. A real, already
    // provisioned GROK_HOME/auth.json still wins (Grok's documented
    // auth precedence puts an active session token ahead of
    // XAI_API_KEY), so this changes nothing for a correctly
    // provisioned Grok pane.
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await
    .unwrap();

    let env = spawner.last_spawn_env();
    assert_eq!(
        env.iter().find(|(k, _)| k == "XAI_API_KEY").map(|(_, v)| v.as_str()),
        Some(WORKER_XAI_API_KEY_NO_INTERACTIVE_AUTH),
    );
}

#[tokio::test]
async fn xai_api_key_cannot_be_overridden_via_extra_env() {
    // XAI_API_KEY is not on WORKER_EXTRA_ENV_ALLOWLIST, so a caller
    // cannot smuggle a different value in through extra_env and
    // reintroduce the interactive-auth fallback path.
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.extra_env = vec![("XAI_API_KEY".into(), "xai-real-looking-key".into())];

    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    let env = spawner.last_spawn_env();
    let values: Vec<&str> = env
        .iter()
        .filter(|(k, _)| k == "XAI_API_KEY")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(
        values,
        vec![WORKER_XAI_API_KEY_NO_INTERACTIVE_AUTH],
        "extra_env must not be able to override the safety-net XAI_API_KEY",
    );
}

#[tokio::test]
async fn extra_env_allowlist_keeps_known_keys() {
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.extra_env = vec![
        ("BOSS_TASK_ID".into(), "T-42".into()),
        ("CUBE_LEASE_ID".into(), "lease-cube".into()),
        ("CUBE_REPO".into(), "mono".into()),
    ];

    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    let env = spawner.last_spawn_env();
    assert_eq!(
        env.iter().find(|(k, _)| k == "BOSS_TASK_ID").map(|(_, v)| v.as_str()),
        Some("T-42"),
    );
    assert_eq!(
        env.iter().find(|(k, _)| k == "CUBE_LEASE_ID").map(|(_, v)| v.as_str()),
        Some("lease-cube"),
    );
    assert_eq!(
        env.iter().find(|(k, _)| k == "CUBE_REPO").map(|(_, v)| v.as_str()),
        Some("mono"),
    );
}

#[tokio::test]
async fn title_summary_is_forwarded_to_spawn_request() {
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.title_summary = Some("Pane Titlebar Summary".to_owned());

    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    match spawner.last_request.lock().unwrap().clone() {
        Some(EngineToAppRequest::AttachWorkerPane(input)) => {
            assert_eq!(input.summary.as_deref(), Some("Pane Titlebar Summary"));
        }
        other => panic!("expected AttachWorkerPane, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_title_summary_does_not_attach_one() {
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await
    .unwrap();

    match spawner.last_request.lock().unwrap().clone() {
        Some(EngineToAppRequest::AttachWorkerPane(input)) => {
            assert!(input.summary.is_none());
        }
        other => panic!("expected AttachWorkerPane, got {other:?}"),
    }
}

#[tokio::test]
async fn extra_env_drops_non_allowlisted_keys() {
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();

    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    // Mix of clearly-dangerous keys and a fake one to confirm
    // both get filtered. `BOSS_CONTROL_SOCKET` is the canonical
    // example: even if some upstream caller tried to set it, the
    // worker must never see it.
    input.extra_env = vec![
        ("BOSS_CONTROL_SOCKET".into(), "/tmp/should-not-leak".into()),
        ("AWS_SESSION_TOKEN".into(), "secret".into()),
        ("RANDOM_KEY".into(), "v".into()),
        ("BOSS_TASK_ID".into(), "T-keep".into()),
    ];

    start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

    let env = spawner.last_spawn_env();
    let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert!(!keys.contains(&"BOSS_CONTROL_SOCKET"));
    assert!(!keys.contains(&"AWS_SESSION_TOKEN"));
    assert!(!keys.contains(&"RANDOM_KEY"));
    // Allowlisted key still made it through.
    assert!(keys.contains(&"BOSS_TASK_ID"));
}

/// Delegates to [`StubSpawner`] but also exposes a work database, the way
/// the production spawner does.
struct DbSpawner {
    inner: StubSpawner,
    db: Arc<crate::work::WorkDb>,
}

#[async_trait::async_trait]
impl WorkerSpawner for DbSpawner {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        self.inner.send_to_app_request(request, timeout).await
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        self.inner.worker_registry()
    }

    fn work_db(&self) -> Option<&crate::work::WorkDb> {
        Some(&self.db)
    }
}

/// A freshly spawned progression pane is registered at the confined tier and
/// its neighbourhood is captured before the registration; an ordinary worker
/// is not.
async fn assert_progression_spawn(driver_name: &str) {
    use crate::test_support::{create_test_chore_manual, create_test_product_named, open_db_arc};
    let (_dir, db) = open_db_arc();
    let product = create_test_product_named(&db, "p");
    let chore = create_test_chore_manual(&db, product.id.clone(), "stuck");
    let execution = db
        .create_execution(
            boss_protocol::CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(boss_protocol::ExecutionKind::Progression)
                .status(boss_protocol::ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    for (kind, expected_tier) in [(WorkerKind::Progression, true), (WorkerKind::Standard, false)] {
        let workspace = TempDir::new().unwrap();
        let spawner = DbSpawner {
            inner: ok_spawner_capturing(),
            db: db.clone(),
        };
        let mut input = sample_input(&workspace, spawner.inner.tmux_runner.clone());
        if driver_name == "codex" {
            input = codex_input(&workspace, &execution.id, 3, spawner.inner.tmux_runner.clone());
        }
        input.driver = crate::driver::DriverRegistry::default().require(driver_name).unwrap();
        input.run_id = execution.id.clone();
        if kind == WorkerKind::Progression {
            assert!(execution.cube_lease_id.is_none());
            input.execution_kind = execution.kind.as_str().to_owned();
            input.lease_id.clear();
        }
        input.worker_kind = kind;
        start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();
        assert_eq!(spawner.inner.registry.registered_tier(4242), Some(expected_tier));
        let env = spawner.inner.last_spawn_env();
        assert!(env.iter().all(|(_, value)| !value.is_empty()), "{driver_name}: {env:?}");
        assert_eq!(
            env.iter()
                .find(|(key, _)| key == "BOSS_LEASE_ID")
                .map(|(_, v)| v.as_str()),
            if expected_tier { None } else { Some("lease-test") },
        );
        assert!(
            env.iter()
                .any(|(key, value)| key == "BOSS_RUN_ID" && value == &execution.id)
        );
        let calls = spawner.inner.tmux_runner.calls();
        let create = calls
            .iter()
            .find(|args| args.get(2).map(String::as_str) == Some("new-session"))
            .unwrap();
        assert!(
            create
                .windows(2)
                .any(|pair| pair[0] == "-c" && Path::new(&pair[1]) == workspace.path())
        );
    }
    db.check_progression_scope(&execution.id, &chore.id)
        .expect("the progression spawn captured its neighbourhood");
}

#[tokio::test]
async fn claude_progression_spawn_without_a_cube_lease() {
    assert_progression_spawn("claude").await;
}

#[test]
fn codex_progression_spawn_without_a_cube_lease() {
    let homes = TempDir::new().unwrap();
    let _homes_env = codex_homes_override(homes.path());
    let transcripts = TempDir::new().unwrap();
    let _transcripts_env = transcript_store_override(transcripts.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(assert_progression_spawn("codex"));
}

#[tokio::test]
async fn an_empty_lease_on_a_standard_worker_still_fails_spawn() {
    let workspace = TempDir::new().unwrap();
    let spawner = ok_spawner_capturing();
    let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
    input.lease_id.clear();
    let error = start_worker(&spawner, input, StdDuration::from_secs(1))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("tmux environment value cannot be empty"));
    assert!(
        !spawner
            .tmux_runner
            .calls()
            .iter()
            .any(|args| args.get(2).map(String::as_str) == Some("new-session"))
    );
    assert_eq!(spawner.spawn_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn absent_or_unresponsive_viewer_keeps_the_worker_registered() {
    for response in [SendToAppError::Timeout, SendToAppError::NotRegistered] {
        let workspace = TempDir::new().unwrap();
        let mut spawner = ok_spawner_capturing();
        spawner.canned_response = Err(response);
        let started = start_worker(
            &spawner,
            sample_input(&workspace, spawner.tmux_runner.clone()),
            StdDuration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(started.shell_pid, 4242);
        assert_eq!(spawner.registry.lookup_unverified(4242).as_deref(), Some("run-test"));
        assert_eq!(spawner.registry.slot_for_run("run-test"), Some(3));
    }
}
#[tokio::test]
async fn local_spawn_requires_successfully_persisted_identity() {
    for fail_intent in [false, true] {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();
        let store = Arc::new(RecordingTmuxStore {
            fail_intent,
            missing_run: !fail_intent,
            ..Default::default()
        });
        let mut input = sample_input(&workspace, spawner.tmux_runner.clone());
        input.tmux_host.spawn_store = store.clone();

        let result = start_worker(&spawner, input, StdDuration::from_secs(1)).await;

        assert!(matches!(result, Err(StartWorkerError::Tmux(_))), "{result:?}");
        assert_eq!(store.steps(), ["intent"]);
        assert!(
            spawner.tmux_runner.calls().is_empty(),
            "no process before durable identity"
        );
        assert_eq!(spawner.spawn_calls.load(Ordering::SeqCst), 0);
        assert!(spawner.registry.slot_for_run("run-test").is_none());
    }
}

/// Pause the intent write so the test can orphan the execution first.
struct IntentWriteGate {
    started: (Mutex<bool>, Condvar),
    released: (Mutex<bool>, Condvar),
}

impl IntentWriteGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: (Mutex::new(false), Condvar::new()),
            released: (Mutex::new(false), Condvar::new()),
        })
    }

    fn wait_to_write(&self) {
        *self.started.0.lock().unwrap() = true;
        self.started.1.notify_all();
        let mut released = self.released.0.lock().unwrap();
        while !*released {
            released = self.released.1.wait(released).unwrap();
        }
    }

    fn wait_until_paused(&self) {
        let mut started = self.started.0.lock().unwrap();
        while !*started {
            started = self.started.1.wait(started).unwrap();
        }
    }

    fn release(&self) {
        *self.released.0.lock().unwrap() = true;
        self.released.1.notify_all();
    }
}

struct GatedWorkDbStore {
    db: crate::work::WorkDb,
    gate: Arc<IntentWriteGate>,
}

impl TmuxSpawnStore for GatedWorkDbStore {
    fn record_tmux_spawn_intent(
        &self,
        execution_id: &str,
        server_label: &str,
        session_name: &str,
        spawn_token: &str,
    ) -> anyhow::Result<bool> {
        self.gate.wait_to_write();
        self.db
            .record_tmux_spawn_intent_for_execution(execution_id, server_label, session_name, spawn_token)
    }

    fn record_tmux_session_created(
        &self,
        execution_id: &str,
        spawn_token: &str,
        pane_pid: i64,
        session_name: &str,
        server_label: &str,
    ) -> anyhow::Result<bool> {
        self.db.persist_tmux_identity_after_observation(
            execution_id,
            spawn_token,
            pane_pid,
            Some(session_name),
            Some(server_label),
        )
    }
}

#[test]
fn a_late_spawn_does_not_create_a_session_after_the_execution_is_orphaned() {
    let workspace = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = crate::work::WorkDb::open(db_dir.path().join("boss.db")).unwrap();
    let product = crate::test_support::create_test_product(&db);
    let chore = crate::test_support::create_test_chore(&db, product.id.clone(), "Late spawn");
    let execution_id = crate::test_support::create_old_execution(&db, &chore.id);
    db.start_execution_run(
        &execution_id,
        "worker-1",
        "repo",
        "lease",
        "workspace",
        workspace.path().to_str().unwrap(),
    )
    .unwrap();

    let gate = IntentWriteGate::new();
    let store = Arc::new(GatedWorkDbStore { db, gate: gate.clone() });
    let spawner = ok_spawner_capturing();
    let tmux_runner = spawner.tmux_runner.clone();
    let mut input = sample_input(&workspace, tmux_runner.clone());
    input.run_id = execution_id.clone();
    input.tmux_host.spawn_store = store.clone();

    let spawn_thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(start_worker(&spawner, input, StdDuration::from_secs(1)))
    });
    gate.wait_until_paused();
    store
        .db
        .mark_execution_orphaned(&execution_id, "pane never attached")
        .unwrap();
    gate.release();
    let result = spawn_thread.join().expect("spawn thread");
    assert!(matches!(result, Err(StartWorkerError::Tmux(_))), "{result:?}");
    assert!(
        !tmux_runner
            .calls()
            .iter()
            .any(|args| args.get(2).map(String::as_str) == Some("new-session")),
        "tmux must never create a session after the execution is orphaned: {:?}",
        tmux_runner.calls()
    );
    assert!(store.db.tmux_identity_for_execution(&execution_id).unwrap().is_none());
}

#[tokio::test]
async fn tmux_failure_is_an_explicit_local_failure_without_app_fallback() {
    let workspace = TempDir::new().unwrap();
    let mut spawner = ok_spawner_capturing();
    Arc::get_mut(&mut spawner.tmux_runner).unwrap().response.fail_command = Some("new-session".into());
    let result = start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await;
    assert!(matches!(result, Err(StartWorkerError::Tmux(_))), "{result:?}");
    assert_eq!(
        spawner.spawn_calls.load(Ordering::SeqCst),
        0,
        "tmux failure must never call the app"
    );
    assert!(spawner.registry.slot_for_run("run-test").is_none());
    let steps = spawner.tmux_runner.steps.steps();
    assert_eq!(steps.first(), Some(&"intent"));
    assert!(!steps.contains(&"created"));
}

#[tokio::test]
async fn invalid_tmux_pid_is_a_failure_instead_of_provisional_registration() {
    let workspace = TempDir::new().unwrap();
    let mut spawner = ok_spawner_capturing();
    Arc::get_mut(&mut spawner.tmux_runner).unwrap().response.pane_pid_stdout = "0".into();
    let result = start_worker(
        &spawner,
        sample_input(&workspace, spawner.tmux_runner.clone()),
        StdDuration::from_secs(1),
    )
    .await;
    assert!(matches!(result, Err(StartWorkerError::Tmux(_))), "{result:?}");
    assert_eq!(spawner.spawn_calls.load(Ordering::SeqCst), 0);
    assert!(spawner.registry.slot_for_run("run-test").is_none());
}
