//! End-to-end-ish tests for `PaneSpawnRunner`: drive `run_execution`
//! against a stub `WorkerSpawner`, then assert on what was actually
//! sent to the app and what files were written into the workspace.
//! These tests would have caught the bugs surfaced manually:
//!   - missing prompt injection (worker idle at bash prompt),
//!   - boss-event resolved to bare relative path (hooks fail),
//!   - sanitized PATH not threaded through to the app.
//!
//! Anything reachable via `WorkerSpawner` is fair game without
//! standing up a full engine; the broadcast / coordinator side
//! lives in `coordinator.rs` tests.
use super::*;
use super::{bound_control_token_path, engine_events_socket_path};
use crate::live_worker_state::DriverSignalKind;
use crate::spawn_flow::TmuxWorkerHost;
use crate::work::WorkDb;
use anyhow::Result;
use async_trait::async_trait;
use boss_execution_runner::pane_spawn::{
    capture_shows_driver_ready, ensure_worker_bin_dir_for_engine, path_prepend_clause,
};
use boss_execution_runner::spawn_confirmation::{confirm_spawn_started, current_run_has_turn_start_evidence};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::app::SendToAppError;
use crate::driver::AgentDriver;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::protocol::{AttachWorkerPaneResult, EngineToAppRequest, EngineToAppResponse, EnvVar};
use crate::test_support::*;
use crate::work::{CreateChoreInput, CreateProjectInput, CreateTaskInput, EffortLevel, Task, WorkExecution, WorkItem};
use crate::worker_registry::WorkerRegistry;
use boss_protocol::{ExecutionKind, ExecutionStatus, TaskKind, TaskStatus};
use boss_tmux::{CommandOutput, CommandRunner, Tmux};
use std::ffi::OsString;
use std::sync::Mutex as StdMutex;
use tempfile::TempDir;

#[derive(Clone)]
struct CapturedSpawn {
    slot_id: u8,
    initial_input: String,
    env: Vec<EnvVar>,
}

/// Records the spawn request the runner sent so tests can assert
/// on env, initial_input, etc.
///
/// `#[cfg(test)]` is redundant under this file's test-module parent, but
/// checkleft's `rust/giant-structs` parser reads each file in isolation
/// and cannot see the ancestor `cfg(test)`.
#[cfg(test)]
struct CapturingSpawner {
    pending_run_id: StdMutex<String>,
    registry: WorkerRegistry,
    live_states: LiveWorkerStateRegistry,
    last: StdMutex<Option<CapturedSpawn>>,
    /// Run ids passed to `reap_worker_pane` — lets the mid-spawn
    /// cancel test assert the runner reaped the just-spawned pane.
    reaped: StdMutex<Vec<String>>,
    /// When true, `capture-pane` returns Claude agent chrome. Independent of
    /// [`Self::auto_confirm_turn`] so tests can drive composer-ready without
    /// a hook, and vice versa.
    pane_chrome: std::sync::atomic::AtomicBool,
    /// When true, `capture-pane` stamps a driver-signal so the turn-start
    /// wait can succeed in tests.
    auto_confirm_turn: std::sync::atomic::AtomicBool,
    /// When set, the history capture (`capture-pane -S -2000`) returns this
    /// text while the visible-screen capture returns `visible_text` (empty by
    /// default): the banner has scrolled off.
    history_only_text: StdMutex<Option<String>>,
    /// Text returned by the plain visible `capture-pane` when
    /// `history_only_text` is set.
    visible_text: StdMutex<String>,
    /// When true, the history capture never completes.
    history_stalls: std::sync::atomic::AtomicBool,
    /// One entry per `capture-pane`: true for a history read, false for a
    /// visible read.
    capture_log: StdMutex<Vec<bool>>,
    /// When true, `new-session` delivers a hook for every registered or
    /// pending run, i.e. before the live-state slot is registered.
    hook_before_registration: std::sync::atomic::AtomicBool,
    /// Cancel exactly when confirmation first observes the pane.
    cancel_on_capture: StdMutex<Option<(Arc<WorkDb>, String)>>,
    /// Keeps the per-run homes root pointed inside the test's workspace for
    /// as long as the spawner lives. See [`HomesPin`].
    homes_pin: StdMutex<Option<HomesPin>>,
}

/// Pins `BOSS_CODEX_HOMES_DIR` to a directory inside the test's own workspace.
///
/// Claude provisioning writes the private brief under the per-run homes root
/// and leaves `.claude/initial-prompt.txt` as a symlink to it. That root comes
/// from a process-global env var which sibling tests repoint at their own temp
/// dirs, so a spawn that lands inside such a window leaves a symlink into a
/// directory that is deleted when that sibling finishes, and the read-back
/// fails with `NotFound`. Holding the override for the whole test (on a helper
/// thread, because its guard is a blocking lock that must not be held across
/// `.await`) keeps the brief where the test can read it.
struct HomesPin {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HomesPin {
    fn new(root: PathBuf) -> Self {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _guard = crate::driver::test_support::codex_homes_override(&root);
            ready_tx.send(()).unwrap();
            let _ = released.recv();
        });
        ready.recv().unwrap();
        Self {
            release: Some(release),
            thread: Some(thread),
        }
    }
}

impl Drop for HomesPin {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl CapturingSpawner {
    fn new() -> Self {
        Self {
            pending_run_id: StdMutex::new(String::new()),
            registry: WorkerRegistry::new(),
            live_states: LiveWorkerStateRegistry::new(),
            last: StdMutex::new(None),
            reaped: StdMutex::new(Vec::new()),
            pane_chrome: std::sync::atomic::AtomicBool::new(false),
            auto_confirm_turn: std::sync::atomic::AtomicBool::new(false),
            history_only_text: StdMutex::new(None),
            visible_text: StdMutex::new(String::new()),
            history_stalls: std::sync::atomic::AtomicBool::new(false),
            capture_log: StdMutex::new(Vec::new()),
            hook_before_registration: std::sync::atomic::AtomicBool::new(false),
            cancel_on_capture: StdMutex::new(None),
            homes_pin: StdMutex::new(None),
        }
    }

    fn spawn_input(&self) -> CapturedSpawn {
        self.last
            .lock()
            .unwrap()
            .clone()
            .expect("expected tmux new-session to be sent")
    }

    fn reaped_run_ids(&self) -> Vec<String> {
        self.reaped.lock().unwrap().clone()
    }
}

#[async_trait]
impl crate::spawn_flow::WorkerSpawner for CapturingSpawner {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        _timeout: tokio::time::Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        match request {
            EngineToAppRequest::AttachWorkerPane(input) => {
                self.last
                    .lock()
                    .unwrap()
                    .as_mut()
                    .expect("tmux worker must start before viewer attachment")
                    .slot_id = input.slot_id;
                Ok(EngineToAppResponse::AttachWorkerPane {
                    result: Ok(AttachWorkerPaneResult {}),
                })
            }
            other => panic!("unexpected request kind: {other:?}"),
        }
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        &self.registry
    }

    async fn reap_worker_pane(&self, run_id: &str) {
        self.reaped.lock().unwrap().push(run_id.to_owned());
        // Mirror production teardown enough for the test: drop the
        // slot mapping so a follow-up release is a no-op.
        let _ = self.registry.take_slot_for_run(run_id);
    }

    fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
        Some(&self.live_states)
    }
}

#[async_trait]
impl CommandRunner for CapturingSpawner {
    async fn run(&self, _program: &Path, args: &[OsString], _cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let stdout = match args.get(2).map(String::as_str) {
            Some("new-session") => {
                let env = args
                    .windows(2)
                    .filter(|pair| pair[0] == "-e")
                    .map(|pair| {
                        let (key, value) = pair[1].split_once('=').unwrap();
                        EnvVar {
                            key: key.into(),
                            value: value.into(),
                        }
                    })
                    .collect();
                *self.last.lock().unwrap() = Some(CapturedSpawn {
                    slot_id: 0,
                    env,
                    initial_input: args.last().unwrap().clone(),
                });
                if self.hook_before_registration.load(std::sync::atomic::Ordering::SeqCst) {
                    let run_id = self.pending_run_id.lock().unwrap().clone();
                    self.live_states.record_hook_event_kind(&run_id, "SessionStart");
                    self.live_states
                        .record_driver_signal(&run_id, crate::live_worker_state::DriverSignalKind::HookEvent);
                }
                ""
            }
            Some("display-message") => "4242",
            Some("capture-pane") => {
                if let Some((db, execution_id)) = self.cancel_on_capture.lock().unwrap().take() {
                    db.cancel_running_execution(&execution_id).unwrap();
                }
                if self.auto_confirm_turn.load(std::sync::atomic::Ordering::SeqCst) {
                    for state in self.live_states.snapshot() {
                        self.live_states
                            .record_driver_signal(&state.run_id, crate::live_worker_state::DriverSignalKind::HookEvent);
                    }
                }
                let history_only = self.history_only_text.lock().unwrap().clone();
                // Every command carries the explicit `-S <socket>` global, so
                // history is identified by the `-S -2000` pair that follows
                // the `capture-pane` subcommand, never a bare `-S`.
                let after_subcommand = args
                    .iter()
                    .position(|arg| arg == "capture-pane")
                    .map_or(&[][..], |i| &args[i..]);
                let is_history = after_subcommand
                    .windows(2)
                    .any(|pair| pair[0] == "-S" && pair[1] == "-2000");
                self.capture_log.lock().unwrap().push(is_history);
                if let Some(text) = history_only {
                    if is_history {
                        if self.history_stalls.load(std::sync::atomic::Ordering::SeqCst) {
                            std::future::pending::<()>().await;
                        }
                        return Ok(CommandOutput {
                            success: true,
                            code: Some(0),
                            stdout: text,
                            stderr: String::new(),
                        });
                    }
                    return Ok(CommandOutput {
                        success: true,
                        code: Some(0),
                        stdout: self.visible_text.lock().unwrap().clone(),
                        stderr: String::new(),
                    });
                } else if self.pane_chrome.load(std::sync::atomic::Ordering::SeqCst) {
                    "Claude Code\n"
                } else {
                    ""
                }
            }
            Some("-V") => "tmux 3.3",
            Some("start-server" | "set-option" | "show-options") => "",
            other => panic!("unexpected tmux command {other:?}: {args:?}"),
        };
        Ok(CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }
}

impl crate::spawn_flow::TmuxSpawnStore for CapturingSpawner {
    fn record_tmux_spawn_intent(&self, execution_id: &str, _: &str, _: &str, _: &str) -> Result<bool> {
        *self.pending_run_id.lock().unwrap() = execution_id.to_owned();
        Ok(true)
    }
    fn record_tmux_session_created(&self, _: &str, _: &str, _: i64, _: &str, _: &str) -> Result<bool> {
        Ok(true)
    }
}

fn bind_runner(
    runner: &PaneSpawnRunner,
    weak: Weak<dyn crate::spawn_flow::WorkerSpawner>,
    spawner: &Arc<CapturingSpawner>,
) {
    runner.set_skip_spawn_confirm(true);
    runner.set_server_state(weak);
    let tmux = Tmux::with_runner_and_socket("/fake/tmux", spawner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap();
    runner.set_tmux_host(TmuxWorkerHost::new(tmux, spawner.clone(), "boss-test-worker".into()));
}

fn sample_execution_id(workspace: &Path) -> String {
    format!(
        "exec-test-1-{}",
        workspace.file_name().unwrap().to_str().unwrap().trim_start_matches('.')
    )
}

fn sample_execution(workspace_path: &Path) -> WorkExecution {
    // Claude now records its private input home on the execution. Keep this
    // fixture backed by a real row, as production spawn always is.
    let db = WorkDb::open(workspace_path.join("state.db")).unwrap();
    if db.get_execution(&sample_execution_id(workspace_path)).is_err() {
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "pane spawn fixture");
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at)
                 VALUES (?1, ?2, 'chore_implementation', 'running', 'git@example.com:foo.git', '1')",
                rusqlite::params![&sample_execution_id(workspace_path), chore.id],
            )
            .unwrap();
    }

    WorkExecution::builder()
        .id(sample_execution_id(workspace_path))
        .work_item_id("task-1")
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@example.com:foo.git")
        .cube_repo_id("foo")
        .cube_lease_id("lease-1")
        .cube_workspace_id("foo-agent-001")
        .workspace_path(workspace_path.display().to_string())
        .created_at("2026-05-06T20:00:00Z")
        .started_at("2026-05-06T20:00:00Z")
        .build()
}

fn sample_chore() -> WorkItem {
    WorkItem::Chore(
        Task::builder()
            .id("task-1")
            .product_id("prod-1")
            .kind(TaskKind::Chore)
            .name("Improve top header (agent card) styling")
            .description("The gray header at the top is too cramped.")
            .status(TaskStatus::Todo)
            .created_at("2026-05-06T20:00:00Z")
            .updated_at("2026-05-06T20:00:00Z")
            .build(),
    )
}

/// Keyless utility-model provider: `resolve` fails with NoCredentials so
/// pane-summary / live-status paths take the offline branch. Without this,
/// `RuntimeConfig::utility_model()` lazy-loads `ANTHROPIC_API_KEY` from the
/// process environment and every `run_execution` issues a live Messages API
/// call (~1s each) for the pane titlebar summary — the dominant cost of
/// this module when the key is set on a developer machine or in CI.
fn keyless_utility_model() -> Arc<dyn crate::utility_model::UtilityModel> {
    Arc::new(crate::utility_model::AnthropicUtilityModel::from_lookup(None, |_| None))
}

/// Build a `RuntimeConfig` for pane-spawn tests with a keyless utility
/// model pre-installed so spawn never hits the network for titles.
fn test_runtime_config(work: crate::config::WorkConfig) -> Arc<crate::config::RuntimeConfig> {
    let cfg = crate::config::RuntimeConfig::from_parts(work, None);
    cfg.set_utility_model(keyless_utility_model());
    Arc::new(cfg)
}

/// Guard: even when `ANTHROPIC_API_KEY` is set in the process environment
/// (common on developer machines and some CI profiles), the test
/// RuntimeConfig must not surface a credential for pane-summary. Without
/// this, every `run_execution` paid ~1s for a live Messages API call.
#[test]
fn test_runtime_config_installs_keyless_utility_model() {
    let cfg = test_runtime_config(
        crate::config::WorkConfig::builder()
            .cwd(PathBuf::from("/tmp"))
            .db_path(PathBuf::from("/tmp/state.db"))
            .build(),
    );
    let err = cfg
        .utility_model()
        .resolve(crate::utility_model::UtilityTask::PaneSummary)
        .expect_err("test RuntimeConfig must not resolve a credential from the environment");
    let msg = err.to_string();
    assert!(
        msg.contains("NoCredentials") || msg.to_lowercase().contains("credential") || msg.contains("no "),
        "expected a no-credentials error, got: {msg}",
    );
}

/// Build the standard worker-spawn test scaffolding from a workspace
/// tempdir: a `CapturingSpawner`, a `Weak<dyn WorkerSpawner>` the
/// runner can upgrade, a default `RuntimeConfig` pointed at the
/// workspace, and an open `WorkDb` over `state.db`. Call sites that
/// need bespoke `WorkConfig` options (e.g. custom pool sizes) build
/// these inline instead.
fn spawn_test_env(
    workspace: &TempDir,
) -> (
    Arc<CapturingSpawner>,
    Weak<dyn crate::spawn_flow::WorkerSpawner>,
    Arc<crate::config::RuntimeConfig>,
    Arc<WorkDb>,
) {
    let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
    *spawner.homes_pin.lock().unwrap() = Some(HomesPin::new(workspace.path().join("codex-homes")));
    let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
        Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
    let cfg = test_runtime_config(
        crate::config::WorkConfig::builder()
            .cwd(workspace.path().to_path_buf())
            .db_path(workspace.path().join("state.db"))
            .frontend_socket_path(workspace.path().join("engine.sock"))
            .control_token_path(workspace.path().join("engine-control.token"))
            .build(),
    );
    let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());
    (spawner, weak, cfg, work_db)
}

/// Build a runner already bound to a `CapturingSpawner` and drive a
/// run_execution against `workspace`. Returns the spawner so tests
/// can inspect the captured request.
///
/// `boss_event_path`: when `Some`, injects a known absolute path for
/// the boss-event binary so the test is independent of host
/// filesystem layout / env vars. Pass `None` for tests that don't
/// inspect the hook command.
async fn run_once(workspace: &TempDir, boss_event_path: Option<&Path>) -> Result<Arc<CapturingSpawner>> {
    let (spawner, weak, cfg, work_db) = spawn_test_env(workspace);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    if let Some(path) = boss_event_path {
        runner.set_boss_event_path(path.to_path_buf());
    }

    runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await?;

    Ok(spawner)
}

#[tokio::test]
async fn writes_initial_prompt_to_workspace_dot_claude() {
    let workspace = TempDir::new().unwrap();
    let _spawner = run_once(&workspace, None).await.unwrap();

    let prompt_path = workspace.path().join(".claude").join("initial-prompt.txt");
    assert!(prompt_path.exists(), "expected {} to exist", prompt_path.display());
    let prompt = std::fs::read_to_string(&prompt_path).unwrap();
    // Spot-check: the prompt should mention the work item title and
    // execution id so the worker actually has its task in hand.
    assert!(prompt.contains("Improve top header"), "prompt missing work item name");
    assert!(
        prompt.contains(&sample_execution_id(workspace.path())),
        "prompt missing execution id"
    );
    assert!(
        prompt.contains("## Summary"),
        "prompt missing required output section header"
    );
}

#[tokio::test]
async fn implementation_prompt_states_pr_url_acceptance_criterion() {
    // Workers that stop without producing a PR are now blocked
    // from completing — they get probed to push and open one. The
    // dispatch prompt must telegraph that up front so the worker
    // doesn't waste a round-trip discovering it from the probe.
    let workspace = TempDir::new().unwrap();
    let _spawner = run_once(&workspace, None).await.unwrap();
    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    let rules = std::fs::read_to_string(
        crate::driver::ClaudeDriver.agent_rules_destination(workspace.path(), &sample_execution_id(workspace.path())),
    )
    .unwrap();
    assert!(
        rules.contains("A task is not complete until a PR exists"),
        "implementation prompt must state the PR-URL acceptance criterion: {prompt}",
    );
    assert!(
        rules.contains("on its own line"),
        "implementation prompt must tell the worker to print the URL on its own line: {prompt}",
    );
    assert!(
        prompt.contains("gh pr create")
            || prompt.contains("gh pr view")
            || prompt.contains("cube pr create")
            || prompt.contains("$CUBE_BIN"),
        "implementation prompt must mention gh pr commands or cube pr create: {prompt}",
    );
    assert!(
        rules.contains("jj diff -r @"),
        "implementation prompt must tell the worker to verify the diff before pushing: {prompt}",
    );
}

/// AI #6 (incident 001): the prompt must name the engine-supplied
/// branch the worker is expected to push to. The detector reads
/// this same name back out of `state.db` (via
/// `completion::expected_branch_name`) and queries
/// `gh pr list --head <branch>` against it. If a worker pushes to
/// a different bookmark, the fallback returns `None` instead of
/// misbinding — but the happy path requires the worker to follow
/// the engine's name, so the prompt must state it.
#[tokio::test]
async fn implementation_prompt_dictates_engine_supplied_branch_name() {
    let workspace = TempDir::new().unwrap();
    let _spawner = run_once(&workspace, None).await.unwrap();
    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    let expected_branch = crate::completion::expected_branch_name(
        &sample_execution_id(workspace.path()),
        &boss_protocol::BranchNaming::BossExecPrefix,
        None,
    );
    assert!(
        prompt.contains(&expected_branch),
        "prompt must name the engine-supplied branch `{expected_branch}`, got: {prompt}",
    );
    assert!(
        prompt.contains("expected branch name"),
        "prompt must include the `expected branch name` context line, got: {prompt}",
    );
}

#[tokio::test]
async fn spawn_attaches_a_viewer_after_launching_tmux() {
    let workspace = TempDir::new().unwrap();
    let spawner = run_once(&workspace, None).await.unwrap();
    assert_eq!(spawner.spawn_input().slot_id, 1);
    assert_eq!(
        spawner
            .registry
            .pane_for_run(&sample_execution_id(workspace.path()))
            .unwrap()
            .tmux_session_name
            .as_deref(),
        Some("boss-test-worker")
    );
}

fn initial_input_script(workspace: &Path) -> String {
    std::fs::read_to_string(workspace.join(".boss").join("initial-input.sh")).unwrap()
}

#[tokio::test]
async fn initial_input_types_a_short_fixed_line_sourcing_the_workspace_script() {
    let workspace = TempDir::new().unwrap();
    let spawner = run_once(&workspace, None).await.unwrap();
    let input = spawner.spawn_input();

    // The pty is typed a short, fixed-length line regardless of driver,
    // permission-rule count, workspace path, or prompt size — never the
    // assembled command itself. See `MAX_CANON_LINE_BYTES`'s doc for why:
    // a canonical-mode tty line over 1024 bytes is silently dropped in
    // its entirety (not truncated), so the previous behaviour of typing
    // the whole command meant a long enough combination of the above
    // meant the worker never started, with nothing surfaced anywhere.
    assert_eq!(
        input.initial_input,
        crate::spawn_flow::WorkerPaneLaunch::from_environment().tmux_command(". .boss/initial-input.sh\n")
    );

    let script = initial_input_script(workspace.path());
    // The pane needs a `claude` invocation that picks up the rendered
    // prompt as its first user message — going through a file avoids
    // shell-quoting issues with multi-line markdown. Without this, the
    // worker just sits at the bash prompt forever (as it did before
    // #174).
    assert!(
        script.contains("python3 .claude/initial-prompt.py claude ") && !script.contains("$(cat"),
        "expected private terminal delivery without prompt expansion, got: {script:?}",
    );
    // The first shell line re-prepends BOSS_BIN_DIR to PATH (so the
    // bundled `cube`/`boss` win over any `~/bin` repobin shim the
    // login-shell init re-prepends), then the per-workspace launcher dir
    // on top of that (so `boss` is pinned to an absolute path even in dev
    // mode, where BOSS_BIN_DIR is unset and the clause is a no-op), then
    // unsets the API key and invokes claude. See the comment at the
    // construction site.
    assert!(
        script.starts_with(&format!(
            "{}{}unset ANTHROPIC_API_KEY; {}python3 .claude/initial-prompt.py claude",
            path_prepend_clause("BOSS_BIN_DIR"),
            path_prepend_clause(boss_engine_worker_bin::WORKER_BIN_DIR_ENV),
            boss_engine_worker_bin::environment::shell_environment_clause(
                &crate::worker_setup::worker_settings_dir()
                    .join("bin")
                    .join(workspace.path().file_name().unwrap()),
            ),
        )),
        "expected the initial-input script to re-prepend BOSS_BIN_DIR then the worker launcher \
             dir, unset ANTHROPIC_API_KEY, and invoke claude, got: {script:?}",
    );
    // A fresh install must never lower worker priority implicitly.
    assert!(
        !script.contains("taskpolicy"),
        "the pane must not be placed in the Darwin background tier, got: {script:?}",
    );
}

#[tokio::test]
async fn throttle_changes_apply_to_next_spawn_in_every_local_pool() {
    for (worker_id, kind) in [
        ("worker-1", ExecutionKind::ChoreImplementation),
        ("auto-worker-1", ExecutionKind::AutomationTriage),
        ("review-1", ExecutionKind::PrReview),
    ] {
        let workspace = TempDir::new().unwrap();
        let (spawner, weak, cfg, db) = spawn_test_env(&workspace);
        let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, db.clone(), flags);
        bind_runner(&runner, weak, &spawner);
        let mut execution = sample_execution(workspace.path());
        execution.kind = kind;
        for enabled in [false, true, false] {
            crate::worker_throttle::set(&db, enabled).unwrap();
            runner
                .run_execution(
                    worker_id,
                    &execution,
                    &sample_chore(),
                    workspace.path(),
                    Some("change-1"),
                )
                .await
                .unwrap();
            let script = initial_input_script(workspace.path());
            assert_eq!(
                script.contains("/usr/sbin/taskpolicy -b -p $$"),
                enabled && cfg!(target_os = "macos"),
                "pool {worker_id}, enabled {enabled}: {script}"
            );
            if !enabled {
                assert!(!script.contains("taskpolicy"));
            }
        }
    }
}

/// Build a runner driven against a real product + chore row so
/// the dispatcher's effort/model lookup hits actual SQLite rather
/// than the synthetic `sample_chore` fixture. Returns the spawner
/// and the created chore id so the caller can re-use the row.
async fn run_once_with_chore(
    workspace: &TempDir,
    chore_input: CreateChoreInput,
    product_default_model: Option<&str>,
) -> Result<(Arc<CapturingSpawner>, Task)> {
    run_once_with_chore_inner(workspace, chore_input, product_default_model, false).await
}

/// [`run_once_with_chore`], but with the row's `reasoning` cleared back to
/// NULL after creation — i.e. a row that predates the `reasoning` column.
///
/// Every create path seeds a reasoning mode, so this is the only way to
/// exercise the dispatcher's legacy resolution path (design-family kind
/// floor, then the effort-level table) from a real SQLite row. The tests
/// that use it are the requirement-6 guarantee in test form: landing the
/// capability signal must not re-model rows that were already in flight.
async fn run_once_with_unclassified_chore(
    workspace: &TempDir,
    chore_input: CreateChoreInput,
    product_default_model: Option<&str>,
) -> Result<(Arc<CapturingSpawner>, Task)> {
    run_once_with_chore_inner(workspace, chore_input, product_default_model, true).await
}

async fn run_once_with_chore_inner(
    workspace: &TempDir,
    chore_input: CreateChoreInput,
    product_default_model: Option<&str>,
    unclassify_reasoning: bool,
) -> Result<(Arc<CapturingSpawner>, Task)> {
    let (spawner, weak, cfg, work_db) = spawn_test_env(workspace);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    if let Some(model) = product_default_model {
        work_db.set_product_default_model(&product.id, Some(model)).unwrap();
    }
    let mut chore_input = chore_input;
    chore_input.product_id = product.id.clone();
    let chore = work_db.create_chore(chore_input).unwrap();
    let chore = if unclassify_reasoning {
        let cleared = work_db
            .update_work_item(
                &chore.id,
                boss_protocol::WorkItemPatch {
                    reasoning: Some(String::new()),
                    ..boss_protocol::WorkItemPatch::default()
                },
            )
            .unwrap();
        match cleared {
            WorkItem::Chore(t) | WorkItem::Task(t) => {
                assert!(
                    t.reasoning.is_none(),
                    "helper must produce a genuinely unclassified row"
                );
                t
            }
            other => panic!("expected chore/task item, got {other:?}"),
        }
    } else {
        chore
    };

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);

    let mut execution = sample_execution(workspace.path());
    execution.work_item_id = chore.id.clone();

    runner
        .run_execution(
            "worker-1",
            &execution,
            &WorkItem::Chore(chore.clone()),
            workspace.path(),
            Some("change-1"),
        )
        .await?;

    Ok((spawner, chore))
}

/// Untagged row (NULL effort_level, NULL model_override, NULL reasoning,
/// no product default) must produce the same spawn line today's
/// engine produces — minus the implicit `claude` model selection,
/// plus an explicit `--model <engine-default-slug>`. No
/// `--effort` flag, no prompt addendum. Design §Q2 / task spec
/// regression test: "byte-equivalent to today's `claude
/// "$(cat .claude/initial-prompt.txt)"` plus the explicit
/// `--model <engine-default-slug>`."
///
/// Goes through [`run_once_with_unclassified_chore`] because "untagged" now
/// has to mean untagged on *both* axes: every create path seeds a reasoning
/// mode, so only a row predating that column reaches the engine-default
/// fall-through. That is exactly the population this assertion protects.
#[tokio::test]
async fn untagged_row_spawn_matches_engine_default() {
    let workspace = TempDir::new().unwrap();
    let chore_input = CreateChoreInput::builder()
        .product_id(String::new())
        .name("Untagged chore")
        .description("plain row, no effort/model")
        .build();
    let (_spawner, _chore) = run_once_with_unclassified_chore(&workspace, chore_input, None)
        .await
        .unwrap();
    let script = initial_input_script(workspace.path());

    // The worker settings file lives outside the workspace; the
    // engine points claude at it with `--settings '<abs-path>'`,
    // positioned before the positional prompt arg.
    let settings_path = crate::worker_setup::worker_settings_path(workspace.path());
    assert_eq!(
        script,
        format!(
            "{}{}unset ANTHROPIC_API_KEY; {}python3 .claude/initial-prompt.py claude --model {} --disallowedTools=AskUserQuestion --permission-mode auto --settings '{}'\n",
            path_prepend_clause("BOSS_BIN_DIR"),
            path_prepend_clause(boss_engine_worker_bin::WORKER_BIN_DIR_ENV),
            boss_engine_worker_bin::environment::shell_environment_clause(
                &crate::worker_setup::worker_settings_dir()
                    .join("bin")
                    .join(workspace.path().file_name().unwrap()),
            ),
            crate::driver::ClaudeDriver.descriptor().model_menu.engine_default,
            settings_path.display(),
        ),
        "untagged row should re-prepend BOSS_BIN_DIR then the worker launcher dir to PATH, then spawn with the engine default model, --permission-mode auto (Opus), --settings <worker file>, and no --effort",
    );

    // No addendum prepended — the existing implementation framing
    // must be the first thing the worker sees.
    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    assert!(
        prompt.starts_with("You are a reusable worker"),
        "untagged-row prompt must start with the original framing, got: {prompt:?}",
    );
    assert!(
        !prompt.contains("Sketch a brief plan"),
        "untagged-row prompt must not carry the medium addendum",
    );
    assert!(
        !prompt.starts_with("Begin with a written plan"),
        "untagged-row prompt must not carry the large/max addendum",
    );
}

/// A `large` row explicitly classified `standard` spawns Sonnet — but keeps
/// `--effort xhigh` and the planning addendum, because it is still a big
/// job. This is the mirror of the requirement that a small investigation
/// can reach Opus: the two axes move independently, and "big" is not the
/// same claim as "hard".
#[tokio::test]
async fn large_standard_row_spawns_sonnet_but_keeps_xhigh_and_the_addendum() {
    let workspace = TempDir::new().unwrap();
    let chore_input = CreateChoreInput::builder()
        .product_id(String::new())
        .name("Mechanical rename across fifteen files")
        .description("well-specified, just large")
        .effort_level(EffortLevel::Large)
        .reasoning(boss_protocol::ReasoningMode::Standard)
        .build();
    let (_spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
    let script = initial_input_script(workspace.path());

    assert!(
        script.contains("--model sonnet"),
        "large + standard must spawn Sonnet, got: {script:?}",
    );
    assert!(
        script.contains("--effort xhigh"),
        "the size signal is untouched — still --effort xhigh, got: {script:?}",
    );
    assert!(
        script.contains("--dangerously-skip-permissions"),
        "Sonnet takes the non-auto permission branch, got: {script:?}",
    );

    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    assert!(
        prompt.starts_with("Begin with a written plan."),
        "large addendum still applies — it follows effort, not reasoning; got: {prompt:?}",
    );
}

/// A `small` investigation row gets the dedicated Fable/Astra tier while
/// preserving the driver's effort mapping. The row's work-size estimate
/// stays honest and selects medium effort independently of the model tier.
#[tokio::test]
async fn small_investigation_row_spawns_fable_with_mapped_effort() {
    let workspace = TempDir::new().unwrap();
    let chore_input = CreateChoreInput::builder()
        .product_id(String::new())
        .name("Review cards don't surface merge-conflict state")
        .description("repro and evidence supplied; diagnose why the transition never fires")
        .effort_level(EffortLevel::Small)
        .reasoning(boss_protocol::ReasoningMode::Investigation)
        .build();
    let (_spawner, chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
    let script = initial_input_script(workspace.path());

    assert_eq!(
        chore.effort_level,
        Some(EffortLevel::Small),
        "the row still says what it is: a small job",
    );
    assert!(
        script.contains("--model fable"),
        "small + investigation must spawn Fable, got: {script:?}",
    );
    assert!(
        script.contains("--effort medium"),
        "the dedicated tier must preserve the driver's small-effort mapping, got: {script:?}",
    );
    assert!(
        script.contains("--permission-mode auto"),
        "Opus takes the auto-permission branch, got: {script:?}",
    );

    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    assert!(
        !prompt.starts_with("Begin with a written plan"),
        "reasoning must not smuggle in the large-effort addendum, got: {prompt:?}",
    );
}

/// Smoke test for the design-spec acceptance criterion: a
/// `trivial` row dispatches with `--model sonnet --effort low`
/// and no prompt addendum. Per #746 ("don't use haiku") the model
/// floor is Sonnet, not Haiku, even at the trivial tier — only the
/// effort value stays `low`. See
/// [`crate::driver::ClaudeDriver`]'s `claude_default_model_for_level`.
#[tokio::test]
async fn trivial_row_spawn_uses_sonnet_at_low_effort() {
    let workspace = TempDir::new().unwrap();
    let chore_input = CreateChoreInput::builder()
        .product_id(String::new())
        .name("Apply resize-cursor fix to nav divider")
        .description("one-line CSS tweak")
        .effort_level(EffortLevel::Trivial)
        .build();
    let (_spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
    let script = initial_input_script(workspace.path());

    assert!(
        script.contains("--model sonnet"),
        "trivial row must spawn Sonnet (#746: never Haiku), got: {script:?}",
    );
    assert!(
        !script.contains("--model haiku"),
        "trivial row must NOT spawn Haiku (#746), got: {script:?}",
    );
    assert!(
        script.contains("--effort low"),
        "trivial row must pass --effort low, got: {script:?}",
    );
    assert!(
        script.contains("--dangerously-skip-permissions"),
        "trivial row (Sonnet, non-Opus) must carry --dangerously-skip-permissions, got: {script:?}",
    );
    assert!(
        !script.contains("--permission-mode"),
        "trivial row (Sonnet, non-Opus) must NOT carry --permission-mode, got: {script:?}",
    );

    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    assert!(
        !prompt.starts_with("Sketch") && !prompt.starts_with("Begin with"),
        "trivial row prompt must have no addendum prepended, got: {prompt:?}",
    );
}

/// Smoke test for the second design-spec acceptance criterion:
/// `medium` + explicit `model_override = 'opus'` spawns `--model
/// opus --effort high`, and the medium prompt addendum is
/// prepended verbatim. Verifies that `model_override` changes only
/// the model — the effort value and addendum still follow the
/// row's `effort_level` (design §Q3).
#[tokio::test]
async fn medium_with_opus_override_uses_override_model_and_medium_addendum() {
    let workspace = TempDir::new().unwrap();
    let chore_input = CreateChoreInput::builder()
        .product_id(String::new())
        .name("Add created_via provenance to chore/task creates")
        .description("multi-file edit with judgement calls")
        .effort_level(EffortLevel::Medium)
        .model_override("opus")
        .build();
    let (_spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
    let script = initial_input_script(workspace.path());

    assert!(
        script.contains("--model opus"),
        "model_override should win precedence, got: {script:?}",
    );
    assert!(
        script.contains("--effort high"),
        "medium effort_level must still produce --effort high, got: {script:?}",
    );
    assert!(
        script.contains("--permission-mode auto"),
        "model_override=opus must carry --permission-mode auto, got: {script:?}",
    );
    assert!(
        !script.contains("--dangerously-skip-permissions"),
        "model_override=opus must NOT carry --dangerously-skip-permissions, got: {script:?}",
    );

    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    assert!(
        prompt.starts_with("Sketch a brief plan before you start editing."),
        "medium addendum must be prepended verbatim, got: {prompt:?}",
    );
}

/// Large rows get Opus at `xhigh` plus the planning-heavy
/// addendum. Confirms the third level boundary the design pins.
#[tokio::test]
async fn large_row_spawn_uses_opus_at_xhigh_with_planning_addendum() {
    let workspace = TempDir::new().unwrap();
    let chore_input = CreateChoreInput::builder()
        .product_id(String::new())
        .name("Investigate isolated test instance")
        .description("multi-subsystem investigation")
        .effort_level(EffortLevel::Large)
        .build();
    // Unclassified on purpose: this pins the *legacy* effort-table path,
    // the one every row created before the `reasoning` column takes. A
    // `large` row that has since been classified `standard` resolves to
    // Sonnet instead — see
    // `large_standard_row_spawns_sonnet_but_keeps_xhigh_and_the_addendum`.
    let (_spawner, _chore) = run_once_with_unclassified_chore(&workspace, chore_input, None)
        .await
        .unwrap();
    let script = initial_input_script(workspace.path());

    assert!(
        script.contains("--model opus"),
        "large row must spawn Opus, got: {script:?}",
    );
    assert!(
        script.contains("--effort xhigh"),
        "large row must pass --effort xhigh, got: {script:?}",
    );
    assert!(
        script.contains("--permission-mode auto"),
        "large row (Opus) must carry --permission-mode auto, got: {script:?}",
    );
    assert!(
        !script.contains("--dangerously-skip-permissions"),
        "large row (Opus) must NOT carry --dangerously-skip-permissions, got: {script:?}",
    );

    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    assert!(
        prompt.starts_with("Begin with a written plan."),
        "large addendum must be prepended verbatim, got: {prompt:?}",
    );
}

/// `products.default_model` only kicks in when `model_override`,
/// `effort_level`, and `reasoning` are all unset (design §Q3
/// step 3, now step 6). With a product default in place but no effort tag
/// and no classification, the dispatch should pick the product slug rather
/// than the engine default — and still omit `--effort`.
#[tokio::test]
async fn product_default_model_fills_in_when_row_is_untagged() {
    let workspace = TempDir::new().unwrap();
    let chore_input = CreateChoreInput::builder()
        .product_id(String::new())
        .name("Untagged on Sonnet-defaulted product")
        .build();
    let (_spawner, _chore) = run_once_with_unclassified_chore(&workspace, chore_input, Some("claude-sonnet-4-6"))
        .await
        .unwrap();
    let script = initial_input_script(workspace.path());

    assert!(
        script.contains("--model claude-sonnet-4-6"),
        "product default_model should fill in, got: {script:?}",
    );
    assert!(
        !script.contains("--effort"),
        "untagged row must not pass --effort, got: {script:?}",
    );
    assert!(
        script.contains("--dangerously-skip-permissions"),
        "Sonnet (non-Opus) must carry --dangerously-skip-permissions, got: {script:?}",
    );
    assert!(
        !script.contains("--permission-mode"),
        "Sonnet (non-Opus) must NOT carry --permission-mode, got: {script:?}",
    );
}

/// The runner must return the resolved spawn config on
/// `RunOutcome.spawn_config` so the coordinator can attach it to
/// the `pane_spawned` dispatch event. Drives `run_execution`
/// directly (rather than through `run_once_with_chore`, which
/// drops the outcome) so the returned tuple is observable.
#[tokio::test]
async fn run_outcome_carries_resolved_spawn_config() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = work_db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Trivial chore")
                .effort_level(EffortLevel::Trivial)
                .build(),
        )
        .unwrap();

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);

    let mut execution = sample_execution(workspace.path());
    execution.work_item_id = chore.id.clone();

    let outcome = runner
        .run_execution(
            "worker-1",
            &execution,
            &WorkItem::Chore(chore),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .unwrap();

    let spawn = outcome
        .spawn_config
        .expect("PaneSpawnRunner should always populate spawn_config");
    assert_eq!(spawn.effort_level, Some(EffortLevel::Trivial));
    assert_eq!(spawn.effort_value, Some("low"));
    // #746: trivial floors to Sonnet, never Haiku.
    assert_eq!(spawn.model, "sonnet");
    assert_eq!(spawn.prompt_addendum, None);
}

/// Regression (mono#2673): `PaneSpawnRunner::run_execution` must return
/// `WorkerPaneAlive` — never `WaitingHuman` — for EVERY execution kind,
/// so the stored status is `running` while the agent pane is alive and
/// working.
///
/// This pins the rule at the `PaneSpawnRunner` level. Reverting
/// `run_execution` to the old kind-conditional shape (`WaitingHuman`
/// for everything but `pr_review`) fails here even if the badge-SQL
/// test in t01.rs still passes.
#[tokio::test]
async fn every_execution_kind_yields_worker_pane_alive() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore_manual(&work_db, product.id.clone(), "Some chore being reviewed");

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg.clone(), work_db.clone(), flags.clone());
    bind_runner(&runner, weak.clone(), &spawner);

    // Build a PrReview execution; no pr_url on the chore is fine —
    // the runner falls back to the generic prompt, which is irrelevant
    // to the wait_state assertion.
    let mut pr_review_exec = sample_execution(workspace.path());
    pr_review_exec.kind = ExecutionKind::PrReview;
    pr_review_exec.work_item_id = chore.id.clone();

    let outcome = runner
        .run_execution(
            "review-1",
            &pr_review_exec,
            &WorkItem::Chore(chore.clone()),
            workspace.path(),
            Some("change-pr-review"),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome.wait_state,
        RunWaitState::WorkerPaneAlive,
        "PaneSpawnRunner must return WorkerPaneAlive for PrReview executions so the \
             execution stays in running (not waiting_human) while the reviewer pane is alive"
    );

    // A non-review worker kind must yield the SAME state: its agent is
    // just as alive, and just as much not waiting for a human.
    let runner2 = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner2, weak, &spawner);
    let mut chore_exec = sample_execution(workspace.path());
    chore_exec.kind = ExecutionKind::ChoreImplementation;
    chore_exec.work_item_id = chore.id.clone();

    let outcome2 = runner2
        .run_execution(
            "worker-1",
            &chore_exec,
            &WorkItem::Chore(chore),
            workspace.path(),
            Some("change-chore"),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome2.wait_state,
        RunWaitState::WorkerPaneAlive,
        "PaneSpawnRunner must return WorkerPaneAlive for a chore worker too — the pane is \
             alive and its agent is working, so the stored status must be `running`, not the \
             spurious `waiting_human` this run used to be parked in for its whole life"
    );
    assert_eq!(
        outcome2.wait_state.execution_status(),
        ExecutionStatus::Running,
        "the stored status a chore worker parks in must be `running`"
    );
}

/// **No env vars related to effort or token caps appear on the
/// worker subprocess.** Design §Q2 §"Knobs explicitly not in v1"
/// rejects `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `MAX_THINKING_TOKENS`,
/// and any per-execution token cap explicitly — claude's
/// `--effort` is the canonical control. Locks the rule in via the
/// captured spawn env.
#[tokio::test]
async fn spawn_env_does_not_carry_effort_or_token_cap_env_vars() {
    let workspace = TempDir::new().unwrap();
    let chore_input = CreateChoreInput::builder()
        .product_id(String::new())
        .name("Any chore")
        .effort_level(EffortLevel::Large)
        .build();
    let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
    let input = spawner.spawn_input();

    // The forbidden list from design §Q2 plus the obvious
    // adjacents an over-eager future patch might add.
    for forbidden in [
        "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
        "MAX_THINKING_TOKENS",
        "ANTHROPIC_MAX_TOKENS",
        "BOSS_EFFORT_LEVEL",
        "CLAUDE_EFFORT",
    ] {
        assert!(
            !input.env.iter().any(|EnvVar { key, .. }| key == forbidden),
            "env must not carry {forbidden} (design §Q2 forbids token-cap env knobs)",
        );
    }
}

#[tokio::test]
async fn spawn_env_carries_sanitized_path_and_engine_keys() {
    let workspace = TempDir::new().unwrap();
    let spawner = run_once(&workspace, None).await.unwrap();
    let input = spawner.spawn_input();

    let path_var = input
        .env
        .iter()
        .find(|EnvVar { key, .. }| key == "PATH")
        .expect("PATH must be set on every worker spawn");
    assert!(
        !path_var.value.contains("/Users/"),
        "PATH must not contain the user home (would expose ~/bin/bossctl), got: {}",
        path_var.value
    );
    assert!(
        path_var.value.contains("/usr/bin"),
        "PATH must include system bins, got: {}",
        path_var.value
    );

    assert!(
        input.env.iter().any(|EnvVar { key, .. }| key == "BOSS_LEASE_ID"),
        "expected BOSS_LEASE_ID to be set"
    );
    assert!(
        input.env.iter().any(|EnvVar { key, .. }| key == "BOSS_EVENTS_SOCKET"),
        "expected BOSS_EVENTS_SOCKET to be set"
    );
    assert!(
        input
            .env
            .iter()
            .any(|EnvVar { key, .. }| key == crate::config::FRONTEND_SOCKET_ENV),
        "expected BOSS_SOCKET_PATH to be set so `boss` CLI verbs reach this engine from a scoped HOME"
    );
    assert!(
        input
            .env
            .iter()
            .any(|EnvVar { key, .. }| key == crate::engine_control::TOKEN_PATH_ENV),
        "expected BOSS_ENGINE_CONTROL_TOKEN_PATH so `boss engine stop` from a pane finds engine-control.token"
    );
}

/// Workers must be told about the socket the engine actually bound, which
/// lives on the config, and NOT about whatever `$BOSS_EVENTS_SOCKET`
/// happens to say in the engine's own environment.
///
/// This is the second half of the 2026-07-23 outage: `PaneSpawnRunner`
/// re-resolved the socket from the environment, so even a fixture that had
/// correctly isolated its own socket would have baked the production path
/// into every worker's `settings.json` — sending their hooks to the live
/// engine. The bazel test env pins `HOME=/tmp` (see `engine_lib_test` in
/// `BUILD.bazel`) and does not set `BOSS_EVENTS_SOCKET` at all, so
/// `engine_events_socket_path()` falls back to
/// `/tmp/Library/Application Support/Boss/events.sock` — a path that
/// cannot collide with the test's `TempDir`-rooted config path. That is
/// what makes `assert_ne!` below meaningful: a regression that re-resolved
/// the socket from the environment would still fail loudly, just because
/// the two paths live in unrelated directories rather than because the
/// env var was deliberately mismatched.
#[tokio::test]
async fn spawn_env_uses_the_bound_socket_from_config_not_the_environment() {
    let workspace = TempDir::new().unwrap();
    let bound = workspace.path().join("boss-test-fixture.events.sock");

    let (spawner, weak, _cfg, work_db) = spawn_test_env(&workspace);
    let cfg = test_runtime_config(
        crate::config::WorkConfig::builder()
            .cwd(workspace.path().to_path_buf())
            .db_path(workspace.path().join("state.db"))
            .events_socket_path(bound.clone())
            .build(),
    );
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .unwrap();

    let input = spawner.spawn_input();
    let socket = input
        .env
        .iter()
        .find(|EnvVar { key, .. }| key == "BOSS_EVENTS_SOCKET")
        .expect("BOSS_EVENTS_SOCKET must be set on every worker spawn");
    assert_eq!(
        socket.value,
        bound.display().to_string(),
        "workers must be pointed at the socket this engine bound",
    );
    assert_ne!(
        socket.value,
        engine_events_socket_path().display().to_string(),
        "the runner must not re-resolve the socket from $BOSS_EVENTS_SOCKET",
    );
}

/// Workers must inherit the frontend socket this engine bound as
/// `BOSS_SOCKET_PATH`, not a HOME-relative default. Grok scopes `$HOME` to a
/// per-run process-home; without this export `boss propose` looks for
/// `engine.sock` inside that sandbox.
#[tokio::test]
async fn spawn_env_exports_the_bound_frontend_socket_as_boss_socket_path() {
    let workspace = TempDir::new().unwrap();
    let bound = workspace.path().join("boss-test-fixture.sock");

    let (spawner, weak, _cfg, work_db) = spawn_test_env(&workspace);
    let cfg = test_runtime_config(
        crate::config::WorkConfig::builder()
            .cwd(workspace.path().to_path_buf())
            .db_path(workspace.path().join("state.db"))
            .frontend_socket_path(bound.clone())
            .control_token_path(workspace.path().join("engine-control.token"))
            .build(),
    );
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .unwrap();

    let input = spawner.spawn_input();
    let socket = input
        .env
        .iter()
        .find(|EnvVar { key, .. }| key == crate::config::FRONTEND_SOCKET_ENV)
        .expect("BOSS_SOCKET_PATH must be set on every worker spawn when the config stamped a frontend socket");
    assert_eq!(
        socket.value,
        bound.display().to_string(),
        "workers must be pointed at the frontend socket this engine bound",
    );
    let token = input
        .env
        .iter()
        .find(|EnvVar { key, .. }| key == crate::engine_control::TOKEN_PATH_ENV)
        .expect("BOSS_ENGINE_CONTROL_TOKEN_PATH must ride with BOSS_SOCKET_PATH");
    assert_eq!(
        token.value,
        workspace.path().join("engine-control.token").display().to_string(),
        "workers must inherit the bound control-token path, not a sibling of the socket",
    );
}

#[test]
fn bound_events_socket_path_prefers_the_config_over_the_environment() {
    let work = crate::config::WorkConfig::builder()
        .cwd(PathBuf::from("/tmp"))
        .db_path(PathBuf::from("/tmp/state.db"))
        .events_socket_path(PathBuf::from("/tmp/bound.events.sock"))
        .build();
    let cfg = crate::config::RuntimeConfig::from_parts(work, None);
    assert_eq!(bound_events_socket_path(&cfg), PathBuf::from("/tmp/bound.events.sock"));
}

#[test]
fn bound_frontend_socket_path_returns_the_config_stamp() {
    let work = crate::config::WorkConfig::builder()
        .cwd(PathBuf::from("/tmp"))
        .db_path(PathBuf::from("/tmp/state.db"))
        .frontend_socket_path(PathBuf::from("/tmp/bound.sock"))
        .build();
    let cfg = crate::config::RuntimeConfig::from_parts(work, None);
    assert_eq!(bound_frontend_socket_path(&cfg), Some(PathBuf::from("/tmp/bound.sock")));

    let unstamped = crate::config::WorkConfig::builder()
        .cwd(PathBuf::from("/tmp"))
        .db_path(PathBuf::from("/tmp/state.db"))
        .build();
    let cfg = crate::config::RuntimeConfig::from_parts(unstamped, None);
    assert_eq!(
        bound_frontend_socket_path(&cfg),
        None,
        "WorkConfig::builder() without a frontend_socket_path has no stamp"
    );
}

#[test]
fn bound_control_token_path_returns_the_config_stamp() {
    let work = crate::config::WorkConfig::builder()
        .cwd(PathBuf::from("/tmp"))
        .db_path(PathBuf::from("/tmp/state.db"))
        .control_token_path(PathBuf::from("/tmp/engine-control.token"))
        .build();
    let cfg = crate::config::RuntimeConfig::from_parts(work, None);
    assert_eq!(
        bound_control_token_path(&cfg),
        Some(PathBuf::from("/tmp/engine-control.token"))
    );

    let unstamped = crate::config::WorkConfig::builder()
        .cwd(PathBuf::from("/tmp"))
        .db_path(PathBuf::from("/tmp/state.db"))
        .build();
    let cfg = crate::config::RuntimeConfig::from_parts(unstamped, None);
    assert_eq!(bound_control_token_path(&cfg), None);
}

/// Only the "this engine bound no events socket" shape (in-process
/// `serve(..., None, ...)`) falls back to the environment resolver.
#[test]
fn bound_events_socket_path_falls_back_when_nothing_was_bound() {
    let work = crate::config::WorkConfig::builder()
        .cwd(PathBuf::from("/tmp"))
        .db_path(PathBuf::from("/tmp/state.db"))
        .build();
    let cfg = crate::config::RuntimeConfig::from_parts(work, None);
    assert_eq!(bound_events_socket_path(&cfg), engine_events_socket_path());
}

/// The engine is now the source of truth for which slot a
/// worker lands in. The runner derives the slot from the
/// `worker-{N}` id the coordinator passes in and forwards it on
/// `CapturedSpawn.slot_id`. The app honors that slot
/// rather than running its own allocator. This test pins down
/// that wiring so a regression that drops the slot from the
/// request (or computes it wrong) doesn't silently re-introduce
/// the dual-allocator bug.
#[tokio::test]
async fn spawn_request_includes_engine_claimed_slot() {
    let workspace = TempDir::new().unwrap();
    let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
    let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
        Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
    let cfg = test_runtime_config(
        crate::config::WorkConfig::builder()
            .cwd(workspace.path().to_path_buf())
            .db_path(workspace.path().join("state.db"))
            .worker_pool_size(8)
            .automation_pool_size(3)
            .build(),
    );
    let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);

    // Engine claimed slot 6 (i.e. handed `worker-6` to the
    // runner). The spawn request must carry slot 6 — not 1, not
    // some random pick, not the lowest free.
    runner
        .run_execution(
            "worker-6",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .unwrap();

    let input = spawner.spawn_input();
    assert_eq!(
        input.slot_id, 6,
        "engine-claimed slot must reach the app verbatim, got {}",
        input.slot_id,
    );
}

#[tokio::test]
async fn run_execution_stamps_work_item_binding_on_live_state() {
    // The bossctl coordinator joins `agents list` output back to a
    // chore via these fields — without them, asking "stop the
    // worker on chore X" forces the user to disambiguate slot
    // numbers manually.
    let workspace = TempDir::new().unwrap();
    let spawner = run_once(&workspace, None).await.unwrap();

    let state = spawner
        .live_states
        .get(1)
        .expect("expected live state for slot 1 after run_execution");
    assert_eq!(
        state.work_item_id.as_deref(),
        Some("task-1"),
        "work_item_id should match the chore the runner was driven against"
    );
    assert_eq!(
        state.work_item_name.as_deref(),
        Some("Improve top header (agent card) styling"),
        "work_item_name should be the chore's display name"
    );
    assert_eq!(
        state.execution_id.as_deref(),
        Some(sample_execution_id(workspace.path()).as_str()),
        "execution_id should match the WorkExecution row id"
    );
    assert_eq!(
        state.pool.as_deref(),
        Some("main"),
        "ordinary chore work attributes to the main pool"
    );
    assert_eq!(
        state.kind.as_deref(),
        Some("chore_implementation"),
        "execution kind should match the WorkExecution row"
    );
}

/// Regression — the mid-spawn cancel reconciliation. When the
/// execution row is cancelled while worker creation and viewer attachment
/// is in flight, `run_execution` must, on return, (i) reap the
/// just-spawned pane (the pid is now known, so the reap is no longer
/// a no-op) and (ii) report `CancelledDuringSpawn` so the coordinator
/// releases the cube lease the cancel path deliberately left held.
/// Without this the worker survives unreaped in a workspace the
/// engine believes is free, which is what produced the duplicate
/// dispatch into a shared workspace.
#[tokio::test]
async fn run_execution_reaps_and_signals_when_cancelled_mid_spawn() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore(&work_db, product.id.clone(), "Sort struct definitions");
    let ready = create_ready_chore_execution(&work_db, chore.id.clone());
    // Start the run (ready → running, lease attached) — this is the
    // exact state the row is in when the spawn round-trip is in
    // flight — then cancel it, mirroring a kanban drag-to-Backlog
    // landing inside the spawn window.
    let (execution, _run) = work_db
        .start_execution_run(
            &ready.id,
            "worker-1",
            "foo",
            "lease-1",
            "foo-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    assert!(work_db.cancel_running_execution(&execution.id).unwrap());

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
    bind_runner(&runner, weak, &spawner);

    let chore_item = work_db.get_work_item(&chore.id).unwrap();
    let outcome = runner
        .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
        .await
        .unwrap();

    assert_eq!(
        outcome.wait_state,
        RunWaitState::CancelledDuringSpawn,
        "a cancel that races the spawn window must yield CancelledDuringSpawn",
    );
    assert!(
        outcome.slot_id.is_none(),
        "the pane was reaped, so the coordinator must not keep the pool slot claimed",
    );
    assert_eq!(
        spawner.reaped_run_ids().as_slice(),
        [execution.id.as_str()],
        "the runner must reap the just-spawned pane for the cancelled execution",
    );
}

/// Any task whose `project_id` is set must surface the parent
/// project's name/description/goal in its spawn prompt — the
/// task row itself is intentionally a thin handle (the design
/// task starts with `description = ''`; ordinary `project_task`
/// rows often only carry an implementation brief that omits the
/// project's *why*). Without the spawn-time walk the worker
/// boots with no project context and has to ask, which defeats
/// the point of having a project record at all.
#[tokio::test]
async fn spawn_prompt_for_project_scoped_task_includes_parent_project_context() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

    // Stand up a real product → project → task chain so the
    // runner's `get_project` lookup hits a row with the
    // description/goal we want to assert on. `--no-autostart` on
    // the project keeps the auto-spawned design task parked so
    // it doesn't compete with our explicit run_execution call.
    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let project = work_db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Engine dispatch instrumentation".to_owned(),
            description: Some("Instrument the auto-dispatcher so every spawn decision is traceable.".to_owned()),
            goal: Some("Operators can answer 'why did this task spawn now' from logs alone.".to_owned()),
            autostart: false,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let task = work_db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Tag dispatch logs with execution kind")
                .build(),
        )
        .unwrap();

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);

    let mut execution = sample_execution(workspace.path());
    execution.kind = ExecutionKind::TaskImplementation;
    execution.work_item_id = task.id.clone();

    runner
        .run_execution(
            "worker-1",
            &execution,
            &WorkItem::Task(task),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .unwrap();

    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    assert!(
        prompt.contains("parent project: `Engine dispatch instrumentation`"),
        "prompt missing parent project name line:\n{prompt}",
    );
    assert!(
        prompt.contains("Instrument the auto-dispatcher"),
        "prompt missing parent project description:\n{prompt}",
    );
    assert!(
        prompt.contains("'why did this task spawn now'"),
        "prompt missing parent project goal:\n{prompt}",
    );
}

/// `boss project create` auto-files a `kind = 'design'` task as
/// ordinal-0 of every new project. When that task dispatches it
/// becomes a `project_design` execution. The worker prompt must
/// state up front that the deliverable is a design document — not
/// an implementation. Without this guard the worker has only the
/// project's name/goal to go on and frequently starts coding;
/// observed against worker O'Brien (exec_18aebf0caa1187e8_b).
#[tokio::test]
async fn spawn_prompt_for_auto_design_task_states_design_only_directive() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let project = work_db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Worker live-status dashboard".to_owned(),
            description: Some("Surface every running worker's live state on the kanban without polling.".to_owned()),
            goal: Some("Operators can see what every active worker is doing without opening panes.".to_owned()),
            autostart: false,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();

    // Find the design task `create_project` auto-filed for this
    // project. It sorts ordinal-0 with `kind = 'design'`.
    let design_task = work_db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap()
        .into_iter()
        .find(|t| t.kind == TaskKind::Design)
        .expect("create_project should auto-file a kind='design' task");

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);

    let mut execution = sample_execution(workspace.path());
    execution.kind = ExecutionKind::ProjectDesign;
    execution.work_item_id = design_task.id.clone();

    runner
        .run_execution(
            "worker-1",
            &execution,
            &WorkItem::Task(design_task),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .unwrap();

    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();

    // The deliverable directive must be unmistakable.
    assert!(
        prompt.contains("the deliverable is a **design document**"),
        "design prompt must state the deliverable is a design doc:\n{prompt}",
    );
    assert!(
        prompt.contains("only the design doc"),
        "design prompt must scope the PR to the design doc only:\n{prompt}",
    );
    assert!(
        prompt.contains("Do not edit code"),
        "design prompt must forbid code edits:\n{prompt}",
    );

    // Canonical path uses the project slug since no design_doc_path
    // pointer is configured on this brand-new project.
    assert!(
        prompt.contains(&format!("docs/designs/{}.md", project.slug)),
        "design prompt must include the canonical doc path derived from the project slug `{}`:\n{prompt}",
        project.slug,
    );

    // Required section shape — all five anchors must be named so
    // the worker doesn't invent its own headings.
    for heading in [
        "**Goals**",
        "**Non-goals**",
        "**Alternatives considered**",
        "**Chosen approach**",
        "**Risks / open questions**",
    ] {
        assert!(
            prompt.contains(heading),
            "design prompt missing required section `{heading}`:\n{prompt}",
        );
    }

    // The parent project's goal must come through verbatim — that
    // is the whole point of pulling project context at spawn time.
    assert!(
        prompt.contains("Operators can see what every active worker is doing without opening panes."),
        "design prompt must surface the parent project's goal verbatim:\n{prompt}",
    );

    // The PR-URL acceptance criterion still applies to design
    // runs — they produce a PR, it just contains the doc only.
    let rules = std::fs::read_to_string(
        crate::driver::ClaudeDriver.agent_rules_destination(workspace.path(), &sample_execution_id(workspace.path())),
    )
    .unwrap();
    assert!(
        rules.contains("A task is not complete until a PR exists"),
        "design worker rules must keep the PR-URL acceptance criterion: {rules}",
    );

    // Deliverable 2 — the breakdown sizing contract: the design worker
    // must be told to pre-split its breakdown to one-PR-per-session
    // granularity, so breakdowns arrive pre-decomposed and the planner
    // gate rarely fires.
    assert!(
        prompt.contains("size each entry to one reviewable PR by one worker in one session"),
        "design prompt must carry the one-PR-per-session sizing contract:\n{prompt}",
    );
    assert!(
        prompt.contains("single-subsystem and single-PR"),
        "design prompt must require single-subsystem, single-PR entries:\n{prompt}",
    );
    assert!(
        prompt.contains("sweeps and validation campaigns"),
        "design prompt must split sweeps/validation campaigns into separate dependent entries:\n{prompt}",
    );
    assert!(
        prompt.contains("unknown-format discovery"),
        "design prompt must route unknown-format discovery to its own investigation entry:\n{prompt}",
    );
}

/// When the project already has a `design_doc_path` pointer set
/// (the resumed-design-pass case — a doc was filed, then the
/// engine respawned the design task to revise it), the canonical
/// path in the worker prompt must come from that pointer verbatim
/// instead of the slug-derived default. Otherwise the worker
/// could write to two different files across runs.
#[tokio::test]
async fn spawn_prompt_for_design_task_uses_explicit_design_doc_path() {
    use crate::work::SetProjectDesignDocInput;

    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let project = work_db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Merge poller cadence tuning".to_owned(),
            description: Some("Pick the right merge-poller cadence.".to_owned()),
            goal: Some("Reduce GitHub API spend without lagging merges.".to_owned()),
            autostart: false,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();

    work_db
        .set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: None,
            design_doc_branch: None,
            design_doc_path: Some("tools/boss/docs/designs/merge-poller-cadence.md".into()),
            unset: false,
        })
        .unwrap();

    let design_task = work_db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap()
        .into_iter()
        .find(|t| t.kind == TaskKind::Design)
        .expect("create_project should auto-file a kind='design' task");

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);

    let mut execution = sample_execution(workspace.path());
    execution.kind = ExecutionKind::ProjectDesign;
    execution.work_item_id = design_task.id.clone();

    runner
        .run_execution(
            "worker-1",
            &execution,
            &WorkItem::Task(design_task),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .unwrap();

    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();

    assert!(
        prompt.contains("`tools/boss/docs/designs/merge-poller-cadence.md`"),
        "design prompt must use the project's explicit design_doc_path pointer:\n{prompt}",
    );
    // And it should NOT also fall through to the slug-derived
    // suggestion line — that would be ambiguous.
    assert!(
        !prompt.contains("`design_doc_path` pointer is not yet set"),
        "design prompt should not emit the pointer-missing fallback when the pointer is set:\n{prompt}",
    );
}

#[tokio::test]
async fn settings_json_uses_absolute_boss_event_path() {
    // Inject a fake boss-event at a known absolute temp path so this
    // test is deterministic on every agent — no host PATH lookup, no
    // BOSS_EVENT_BIN env var, no runfiles, no bazel-bin dependency.
    let fake_bin_dir = TempDir::new().unwrap();
    let fake_boss_event = fake_bin_dir.path().join("boss-event");
    std::fs::write(&fake_boss_event, b"").unwrap();

    let workspace = TempDir::new().unwrap();
    let _spawner = run_once(&workspace, Some(&fake_boss_event)).await.unwrap();

    // The settings file lives outside the workspace tree, keyed by
    // workspace name (see worker_setup); it must NOT be written into
    // the workspace `.claude/`.
    let settings_path = crate::worker_setup::worker_settings_path(workspace.path());
    assert!(
        !workspace.path().join(".claude").join("settings.json").exists(),
        "engine must not write .claude/settings.json into the workspace",
    );
    let settings = std::fs::read_to_string(&settings_path).unwrap();

    // Hooks must invoke an absolute path; the bare name
    // `boss-event` is what produced the production
    // `command not found` failures because the worker's sanitized
    // PATH doesn't include the bazel-out directory.
    let expected_path = fake_boss_event.to_str().unwrap();
    assert!(
        settings.contains(expected_path),
        "expected absolute boss-event path {} in settings file, got: {}",
        expected_path,
        settings,
    );
    assert!(
        !settings.contains("'boss-event'") && !settings.contains("\"boss-event\""),
        "settings file must not invoke `boss-event` as a bare name, got: {}",
        settings,
    );
}

/// `BOSS_EVENT_BIN` short-circuits everything else.
#[tokio::test]
async fn mismatched_guide_head_fails_attempt_without_spawning() {
    use crate::work::{PrSourceCapturePersistOutcome, PrSourceCaptureTrigger};
    use boss_engine_test_git::jj::JjRepo;

    let workspace = TempDir::new().unwrap();
    let repo = JjRepo::new(workspace.path());
    let (spawner, weak, cfg, db) = spawn_test_env(&workspace);
    let root = create_active_chore(&db, &create_product(&db), "Guide pin verification");
    let packet = review_guide_source_packet(&"b".repeat(40), &"a".repeat(40));
    let capture = db
        .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet)
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(capture) = capture else {
        panic!("capture must persist")
    };
    let attempt = db
        .create_pr_review_guide_attempt(&capture.series_id, &capture.comparison_id, "review-guide-v5")
        .unwrap();
    let execution = db
        .create_pr_review_guide_execution(&capture.comparison_id, "acme/widget")
        .unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, db.clone(), flags);
    bind_runner(&runner, weak, &spawner);
    let error = runner
        .run_execution("worker-1", &execution, &sample_chore(), &repo.worker, None)
        .await
        .expect_err("mismatched head must refuse launch");
    assert!(error.to_string().contains("does not match pinned head"), "{error:#}");
    assert!(spawner.last.lock().unwrap().is_none());
    let failed = db
        .pr_review_guide_attempt_for_execution(&execution.id)
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, "failed");
    assert!(failed.error.unwrap().contains(&packet.head_sha));
}

#[tokio::test]
async fn spawn_confirmation_timeout_reaps_the_pane_and_fails_the_spawn() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(40));

    let err = runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect_err("absent composer chrome must fail the spawn");
    let msg = err.to_string();
    assert!(msg.contains("composer never became ready"), "{msg}");
    assert!(msg.contains("failed spawn"), "{msg}");
    assert_eq!(
        spawner.reaped_run_ids(),
        vec![spawner.pending_run_id.lock().unwrap().clone()]
    );
}

#[tokio::test]
async fn spawn_confirmation_ready_chrome_without_a_hook_fails_turn_start_and_reaps() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(true, std::sync::atomic::Ordering::SeqCst);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(40));

    let err = runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect_err("ready chrome without a driver hook must fail the spawn");
    let msg = err.to_string();
    assert!(msg.contains("no driver hook or session event"), "{msg}");
    assert!(msg.contains("failed spawn"), "{msg}");
    assert_eq!(
        spawner.reaped_run_ids(),
        vec![spawner.pending_run_id.lock().unwrap().clone()]
    );
}

#[tokio::test]
async fn spawn_confirmation_passes_when_transcript_path_is_set_without_a_registry_signal() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(true, std::sync::atomic::Ordering::SeqCst);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore(&work_db, product.id.clone(), "Transcript path turn start");
    let ready = create_ready_chore_execution(&work_db, chore.id.clone());
    let (execution, _run) = work_db
        .start_execution_run(
            &ready.id,
            "worker-1",
            "foo",
            "lease-1",
            "foo-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    work_db
        .set_run_transcript_path_if_unset(&execution.id, "/tmp/session.jsonl")
        .unwrap();

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(200));

    let chore_item = work_db.get_work_item(&chore.id).unwrap();
    runner
        .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
        .await
        .expect("a persisted transcript_path must complete spawn without a registry signal");
    assert!(
        !spawner.live_states.has_driver_signal_for_run(&execution.id),
        "fixture must not record a registry driver signal"
    );
    assert!(spawner.reaped_run_ids().is_empty());
}

#[tokio::test]
async fn spawn_confirmation_passes_when_driver_chrome_and_hook_arrive() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(true, std::sync::atomic::Ordering::SeqCst);
    spawner
        .auto_confirm_turn
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(200));

    runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect("composer chrome plus a recorded hook must complete spawn");
    assert!(spawner.reaped_run_ids().is_empty());
}

#[tokio::test]
async fn spawn_confirmation_passes_when_hook_arrives_without_chrome() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(false, std::sync::atomic::Ordering::SeqCst);
    spawner
        .auto_confirm_turn
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(200));

    runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect("a recorded hook without composer chrome must complete spawn");
    assert!(spawner.reaped_run_ids().is_empty());
}

#[tokio::test]
async fn run_execution_reaps_and_signals_when_cancelled_during_confirmation() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(true, std::sync::atomic::Ordering::SeqCst);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore(&work_db, product.id.clone(), "Cancel during confirm");
    let ready = create_ready_chore_execution(&work_db, chore.id.clone());
    let (execution, _run) = work_db
        .start_execution_run(
            &ready.id,
            "worker-1",
            "foo",
            "lease-1",
            "foo-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    // Confirmation polls composer chrome and turn-start concurrently, and the
    // turn-start arm is biased. An 80ms window is shorter than the 100ms poll
    // and can expire on a loaded host after one SQLite read, before capture-pane
    // runs, so cancel_on_capture never fires. 500ms matches the passing
    // confirmation tests' order of magnitude and still stays far below production.
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(500));
    // Once capture runs, record a hook so confirmation can finish instead of
    // waiting out the turn-start timeout after the cancel has already landed.
    spawner
        .auto_confirm_turn
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // Synchronize cancellation with confirmation instead of racing a sleep
    // against its timeout on a loaded test host.
    *spawner.cancel_on_capture.lock().unwrap() = Some((work_db.clone(), execution.id.clone()));

    let chore_item = work_db.get_work_item(&chore.id).unwrap();
    let outcome = runner
        .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
        .await
        .unwrap();

    assert_eq!(
        outcome.wait_state,
        RunWaitState::CancelledDuringSpawn,
        "a cancel that lands during confirmation must yield CancelledDuringSpawn",
    );
    assert!(outcome.slot_id.is_none());
    assert!(
        spawner.cancel_on_capture.lock().unwrap().is_none(),
        "confirmation must observe the pane and trigger cancellation",
    );
    assert_eq!(spawner.reaped_run_ids().as_slice(), [execution.id.as_str()]);
}

/// Run a confirming spawn against a pane whose visible screen is empty and
/// whose scrollback holds `history`; returns the spawn result and spawner.
async fn confirm_with_history_only(history: &str) -> (Result<RunOutcome>, Arc<CapturingSpawner>) {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    *spawner.history_only_text.lock().unwrap() = Some(history.to_owned());
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(60));
    let result = runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await;
    (result, spawner)
}

#[tokio::test]
async fn spawn_confirmation_finds_banner_that_only_exists_in_scrollback() {
    // Composer readiness is satisfied by history alone; with no hook the
    // failure is therefore the turn-start one, not "composer never ready".
    let (result, spawner) = confirm_with_history_only("Claude Code 2.1.283\n").await;
    let msg = result.expect_err("no hook, so turn start must fail").to_string();
    assert!(msg.contains("no driver hook or session event"), "{msg}");
    assert_eq!(
        spawner.reaped_run_ids(),
        vec![spawner.pending_run_id.lock().unwrap().clone()]
    );
    // Banner is absent from the visible screen, so only the history read can
    // have supplied it; removing the fallback leaves composer never ready.
    let log = spawner.capture_log.lock().unwrap().clone();
    assert_eq!(&log[..2], [false, true], "visible read must miss, then history is read");
}

#[tokio::test]
async fn spawn_confirmation_scrollback_without_a_marker_still_fails() {
    let (result, spawner) = confirm_with_history_only("bash: exec: claude: not found\nlogin: \n❯ \n").await;
    let msg = result
        .expect_err("shell/error scrollback is not driver evidence")
        .to_string();
    assert!(msg.contains("composer never became ready"), "{msg}");
    assert_eq!(
        spawner.reaped_run_ids(),
        vec![spawner.pending_run_id.lock().unwrap().clone()]
    );
}

#[tokio::test]
async fn spawn_confirmation_passes_on_a_hook_delivered_before_registration() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner
        .hook_before_registration
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(60));

    // No pane marker and no hook after registration: the only evidence is
    // the one delivered while the run had no live-state entry.
    runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect("a pre-registration hook must count as driver evidence");
    assert!(spawner.reaped_run_ids().is_empty());
    assert_eq!(
        spawner
            .live_states
            .first_hook_event_for_run(&sample_execution_id(workspace.path()))
            .as_deref(),
        Some("SessionStart")
    );
}

#[tokio::test]
async fn visible_capture_snapshot_survives_a_stalled_history_capture() {
    let spawner = Arc::new(CapturingSpawner::new());
    *spawner.history_only_text.lock().unwrap() = Some("never returned".to_owned());
    *spawner.visible_text.lock().unwrap() = "bash: claude: command not found\n".to_owned();
    spawner.history_stalls.store(true, std::sync::atomic::Ordering::SeqCst);
    let tmux = Tmux::with_runner_and_socket("/fake/tmux", spawner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap();
    let snapshot = std::sync::Mutex::new(String::from("older snapshot"));
    let probe = capture_shows_driver_ready(&tmux, "boss-test-worker", None, &snapshot);
    let outcome = tokio::time::timeout(std::time::Duration::from_millis(50), probe).await;
    assert!(outcome.is_err(), "history capture stalls until the deadline");
    assert_eq!(
        *snapshot.lock().unwrap(),
        "bash: claude: command not found\n",
        "visible text must be retained as failure evidence",
    );
}

#[test]
fn probe_and_nudge_texts_avoid_internal_vocabulary() {
    use crate::completion::{
        NO_OP_DECLARATION_REFUSED_PROBE, PROBE_EMPTY_PR, PROBE_NO_PR, PROBE_STALE_PR, WORKER_WAIT_NUDGE_DIRECTIVE,
        probe_push_to_existing_pr,
    };
    let pr = "https://github.com/org/repo/pull/42";
    for (label, text) in [
        ("no PR probe", PROBE_NO_PR.to_owned()),
        ("existing PR probe", probe_push_to_existing_pr(pr)),
        ("stale PR probe", PROBE_STALE_PR.to_owned()),
        ("empty PR probe", PROBE_EMPTY_PR.to_owned()),
        ("refused no-op probe", NO_OP_DECLARATION_REFUSED_PROBE.to_owned()),
        ("wait nudge directive", WORKER_WAIT_NUDGE_DIRECTIVE.to_owned()),
        (
            "declare-done probe",
            crate::run_done_backstop::PROBE_DECLARE_DONE.to_owned(),
        ),
        (
            "conflict still present probe",
            crate::conflict_stop_gate::probe_conflict_still_present(pr, "CONFLICTING", "DIRTY"),
        ),
        (
            "conflict mergeability unknown probe",
            crate::conflict_stop_gate::probe_conflict_mergeability_unknown(pr),
        ),
    ] {
        assert!(
            crate::prose_vocabulary::forbidden_prose(&text).is_empty(),
            "{label}: {text}"
        );
    }
}

#[test]
fn current_run_turn_start_accepts_a_driver_signal_without_transcript_path() {
    let (_dir, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let chore = crate::test_support::create_test_chore(&db, product.id.clone(), "Hook only");
    let ready = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());
    let (execution, _run) = db
        .start_execution_run(&ready.id, "worker-1", "foo", "lease-1", "ws-1", "/tmp/ws-1")
        .unwrap();
    let registry = LiveWorkerStateRegistry::new();
    registry.register_spawn(1, execution.id.clone(), "opus", 42, None);
    registry.record_driver_signal(&execution.id, DriverSignalKind::HookEvent);
    assert!(db.transcript_path_for_execution(&execution.id).unwrap().is_none());
    assert!(current_run_has_turn_start_evidence(&db, Some(&registry), &execution.id));
}
#[tokio::test]
async fn turn_start_predicate_can_read_driver_signal_registry() {
    let registry = Arc::new(LiveWorkerStateRegistry::new());
    registry.register_spawn(1, "exec-hook", "opus", 42, None);
    let live = Arc::clone(&registry);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(15)).await;
        live.record_driver_signal("exec-hook", DriverSignalKind::HookEvent);
    });
    confirm_spawn_started(
        "claude",
        "exec-hook",
        Duration::from_millis(100),
        Duration::from_millis(200),
        Duration::from_millis(5),
        std::future::pending::<bool>,
        || async { registry.has_driver_signal_for_run("exec-hook") },
    )
    .await
    .expect("a recorded hook must count as turn-start evidence");
}

#[test]
fn worker_environment_probe_artifacts_use_the_bundled_dispatcher() {
    let output = PathBuf::from(std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR").unwrap());
    let workspace = output.join("probe-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("REPOBIN.toml"),
        "version = 1\n[tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n",
    )
    .unwrap();
    let test_repobin = PathBuf::from(
        std::env::var("BOSS_TEST_REPOBIN").expect("BOSS_TEST_REPOBIN must be $(rootpath //tools/repobin)"),
    );
    let engine_dir = output.join("probe-engine");
    std::fs::create_dir_all(&engine_dir).unwrap();
    let dispatcher = engine_dir.join("repobin");
    std::fs::copy(&test_repobin, &dispatcher).expect("copy rust_test-provided repobin beside the fake engine");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dispatcher).unwrap().permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&dispatcher, perms).unwrap();
    }
    let engine_path = engine_dir.join("engine");
    std::fs::write(&engine_path, b"").unwrap();
    let bin = ensure_worker_bin_dir_for_engine(&output, &workspace, &engine_path).unwrap();
    let launcher = std::fs::read_to_string(bin.join("checkleft")).unwrap();
    let quoted = format!("'{}'", dispatcher.display().to_string().replace('\'', r"'\''"));
    assert!(
        launcher.contains(&format!("exec {quoted} exec 'checkleft'")),
        "launcher must exec the rust_test-provided repobin at {dispatcher:?}: {launcher}"
    );
    let activate = format!(
        "export PATH={}:\"$PATH\"; {}\n",
        crate::ssh_transport::shell_quote(&bin.display().to_string()),
        boss_engine_worker_bin::environment::shell_environment_clause(&bin),
    );
    std::fs::write(output.join("activate-worker-environment.sh"), activate).unwrap();
}

#[tokio::test]
async fn merge_review_policy_reaches_worker_prompt_through_adapter() {
    let workspace = TempDir::new().unwrap();
    let (_dir, db) = crate::test_support::open_db();
    let product = create_product(&db);
    let task = create_test_chore(&db, product, "Observe required checks");
    let mut execution = create_ready_chore_execution(&db, task.id.clone());
    execution.repo_remote_url = "git@github.com:linkedin-multiproduct/some-repo.git".into();
    let composed = crate::runner::compose_worker_spawn(
        &db,
        "worker-1",
        &execution,
        &WorkItem::Chore(task),
        workspace.path(),
        None,
        crate::runner::WorkerSpawnOpts::default(),
    )
    .await
    .unwrap();
    for check in crate::merge_poller::review_signal_checks_for_owner("linkedin-multiproduct") {
        assert!(
            composed.prompt_text.contains(check),
            "missing merge observer check {check}"
        );
    }
    assert!(composed.prompt_text.contains("Owner Approval"));
}

#[test]
fn actual_driver_spawn_prefix_pins_boss_and_cube_before_path_decoys() {
    use crate::driver::{DriverRegistry, SpawnRequest, grok, test_support};
    use crate::protocol::EnvVar;
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let _codex = test_support::codex_auth_source_override(&root.path().join("codex"), &root.path().join("auth.json"));
    let _grok = test_support::grok_homes_override(&root.path().join("grok"));
    let bundle = root.path().join("bundle");
    let decoy = root.path().join("decoy");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::create_dir_all(&decoy).unwrap();
    for name in ["boss", "cube"] {
        for (dir, output) in [(&bundle, "bundled"), (&decoy, "decoy")] {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho {output}-{name}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    struct DeliveryEnvironment {
        prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }
    impl Drop for DeliveryEnvironment {
        fn drop(&mut self) {
            for (key, value) in &self.prior {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }
    let mut restore = DeliveryEnvironment { prior: Vec::new() };
    for key in [
        "BOSS_BIN_DIR",
        "BOSS_CLI_BIN",
        "CUBE_CLI_BIN",
        "BUILD_WORKSPACE_DIRECTORY",
    ] {
        restore.prior.push((key, std::env::var_os(key)));
        unsafe { std::env::remove_var(key) };
    }
    unsafe { std::env::set_var("BOSS_BIN_DIR", &bundle) };
    let run_id = "run-tool-prefix";
    let grok_home = grok::grok_home_for_run(run_id).unwrap();
    std::fs::create_dir_all(&grok_home).unwrap();
    std::fs::write(
        grok_home.join("boss-session-id"),
        "22222222-3333-4444-8555-666666666666\n",
    )
    .unwrap();
    std::fs::write(
        grok_home.join("boss-workspace-path"),
        format!("{}\n", root.path().display()),
    )
    .unwrap();
    let worker_bin = boss_execution_runner::pane_spawn::ensure_worker_bin_dir_for_engine(
        root.path(),
        root.path(),
        &bundle.join("engine"),
    )
    .unwrap();
    let inherited = format!("{}:/bin:/usr/bin", decoy.display());
    let mut env = vec![
        EnvVar {
            key: "PATH".into(),
            value: inherited.clone(),
        },
        EnvVar {
            key: boss_engine_worker_bin::WORKER_BIN_DIR_ENV.into(),
            value: worker_bin.display().to_string(),
        },
    ];
    crate::spawn_flow::apply_worker_tool_env(&mut env, bundle.to_str());
    let registry = DriverRegistry::default();
    for slug in registry.slugs() {
        let driver = registry.require(slug).unwrap();
        let mut plan = driver.spawn_invocation(SpawnRequest {
            model: driver.descriptor().model_menu.engine_default,
            effort: None,
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: Some(run_id),
        });
        // Keep the real driver env directives; substitute only its CLI so the
        // assembled prefix can be exercised without starting an agent.
        plan.command = format!(
            "/bin/bash -c {}",
            crate::ssh_transport::shell_quote("command -v boss; command -v cube; \"$BOSS_BIN\"; \"$CUBE_BIN\"")
        );
        let assembled = boss_execution_runner::pane_spawn::assemble_worker_command("", &worker_bin, &plan);
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &assembled])
            .env("PATH", &inherited)
            .envs(env.iter().map(|entry| (&entry.key, &entry.value)))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{slug}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = format!(
            "{}/boss\n{}/cube\nbundled-boss\nbundled-cube\n",
            worker_bin.display(),
            worker_bin.display()
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{slug}: {assembled}");
    }
}
