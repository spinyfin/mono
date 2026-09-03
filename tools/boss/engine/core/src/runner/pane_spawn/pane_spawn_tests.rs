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
use super::super::engine_events_socket_path;
use super::*;
use crate::app::SendToAppError;
use crate::driver::AgentDriver;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::protocol::{EngineToAppRequest, EngineToAppResponse, EnvVar, SpawnWorkerPaneInput, SpawnWorkerPaneResult};
use crate::test_support::*;
use crate::work::{CreateChoreInput, CreateProjectInput, CreateTaskInput, EffortLevel, Task, WorkExecution, WorkItem};
use crate::worker_registry::WorkerRegistry;
use boss_protocol::{ExecutionKind, ExecutionStatus, TaskKind, TaskStatus};
use std::sync::Mutex as StdMutex;
use tempfile::TempDir;

/// Records the spawn request the runner sent so tests can assert
/// on env, initial_input, etc.
struct CapturingSpawner {
    registry: WorkerRegistry,
    live_states: LiveWorkerStateRegistry,
    last: StdMutex<Option<SpawnWorkerPaneInput>>,
    /// Run ids passed to `reap_worker_pane` — lets the mid-spawn
    /// cancel test assert the runner reaped the just-spawned pane.
    reaped: StdMutex<Vec<String>>,
}

impl CapturingSpawner {
    fn new() -> Self {
        Self {
            registry: WorkerRegistry::new(),
            live_states: LiveWorkerStateRegistry::new(),
            last: StdMutex::new(None),
            reaped: StdMutex::new(Vec::new()),
        }
    }

    fn spawn_input(&self) -> SpawnWorkerPaneInput {
        self.last
            .lock()
            .unwrap()
            .clone()
            .expect("expected SpawnWorkerPane to be sent")
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
            EngineToAppRequest::SpawnWorkerPane(input) => {
                // Echo the slot the engine claimed; the
                // engine-owns-slots refactor makes the response
                // slot a confirmation echo rather than an
                // independent allocator pick.
                let slot_id = input.slot_id;
                *self.last.lock().unwrap() = Some(input);
                Ok(EngineToAppResponse::SpawnWorkerPane {
                    result: Ok(SpawnWorkerPaneResult { slot_id, shell_pid: 0 }),
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

fn sample_execution(workspace_path: &Path) -> WorkExecution {
    WorkExecution::builder()
        .id("exec-test-1")
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
    let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
        Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
    let cfg = test_runtime_config(
        crate::config::WorkConfig::builder()
            .cwd(workspace.path().to_path_buf())
            .db_path(workspace.path().join("state.db"))
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
    runner.set_server_state(weak);
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
    assert!(prompt.contains("exec-test-1"), "prompt missing execution id");
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
    assert!(
        prompt.contains("the deliverable is a PR URL"),
        "implementation prompt must state the PR-URL acceptance criterion: {prompt}",
    );
    assert!(
        prompt.contains("on its own line"),
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
        prompt.contains("jj diff -r @"),
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
    let expected_branch =
        crate::completion::expected_branch_name("exec-test-1", &boss_protocol::BranchNaming::BossExecPrefix, None);
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
async fn spawn_request_carries_claude_pane_monitor_spec() {
    // Claude is the default driver for these fixtures; the app's
    // pre-hook status pill must receive Claude's historical markers
    // on the wire rather than relying only on the app-side default.
    let workspace = TempDir::new().unwrap();
    let spawner = run_once(&workspace, None).await.unwrap();
    let input = spawner.spawn_input();
    let spec = input.pane_monitor.expect("Claude spawn must populate pane_monitor");
    assert_eq!(spec.agent_markers, vec!["Claude Code", "auto mode on", "/effort"]);
    assert_eq!(spec.busy_markers, vec!["esc to interrupt"]);
    assert_eq!(
        spec.starting_markers,
        vec!["Accessing workspace:", "Quick safety check:"]
    );
    assert_eq!(spec.prompt_prefixes, vec!["❯"]);
    assert_eq!(spec.idle_debounce_polls, 2);
}

/// Read back the full assembled command `initial_input` now sources —
/// see `write_initial_input_script`. Tests that assert on the composed
/// command (model/effort/permission flags, prompt-file read, PATH
/// prepends) read this instead of `SpawnWorkerPaneInput::initial_input`,
/// which after the MAX_CANON fix is always a short fixed line.
fn initial_input_script(workspace: &Path) -> String {
    std::fs::read_to_string(workspace.join(".boss").join("initial-input.sh"))
        .expect("initial-input script must exist in the workspace after a successful spawn")
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
    assert_eq!(input.initial_input, ". .boss/initial-input.sh\n");

    let script = initial_input_script(workspace.path());
    // The pane needs a `claude` invocation that picks up the rendered
    // prompt as its first user message — going through a file avoids
    // shell-quoting issues with multi-line markdown. Without this, the
    // worker just sits at the bash prompt forever (as it did before
    // #174).
    assert!(
        script.contains(".claude/initial-prompt.txt"),
        "expected the initial-input script to read from the prompt file, got: {script:?}",
    );
    // The first shell line marks the pane background priority (so the
    // worker's build/test tool calls yield to the coordinator's
    // interactive pane under contention), then re-prepends BOSS_BIN_DIR
    // to PATH (so the bundled `cube`/`boss` win over any `~/bin`
    // repobin shim the login-shell init re-prepends), then the
    // per-workspace launcher dir on top of that (so `boss` is pinned to
    // an absolute path even in dev mode, where BOSS_BIN_DIR is unset
    // and the clause is a no-op), then unsets the API key and invokes
    // claude. See the comment at the construction site.
    assert!(
        script.starts_with(
            "/usr/bin/taskpolicy -b -p $$ >/dev/null 2>&1; \
                 [ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; \
                 [ -n \"$BOSS_WORKER_BIN_DIR\" ] && export PATH=\"$BOSS_WORKER_BIN_DIR:$PATH\"; \
                 unset ANTHROPIC_API_KEY; claude"
        ),
        "expected the initial-input script to mark itself background priority, re-prepend \
             BOSS_BIN_DIR then the worker launcher dir, unset ANTHROPIC_API_KEY, and invoke \
             claude, got: {script:?}",
    );
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
    runner.set_server_state(weak);

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
            "/usr/bin/taskpolicy -b -p $$ >/dev/null 2>&1; \
                 [ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; \
                 [ -n \"$BOSS_WORKER_BIN_DIR\" ] && export PATH=\"$BOSS_WORKER_BIN_DIR:$PATH\"; \
                 unset ANTHROPIC_API_KEY; claude --model {} --permission-mode auto --settings '{}' \"$(cat .claude/initial-prompt.txt)\"\n",
            crate::driver::ClaudeDriver.descriptor().model_menu.engine_default,
            settings_path.display(),
        ),
        "untagged row should re-prepend BOSS_BIN_DIR then the worker launcher dir to PATH, then spawn with the engine default model, --permission-mode auto (Opus), --settings <worker file>, and no --effort",
    );

    // No addendum prepended — the existing implementation framing
    // must be the first thing the worker sees.
    let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
    assert!(
        prompt.starts_with("You are a reusable Boss worker"),
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

/// A `small` investigation row gets the dedicated Fable/Sol tier while
/// preserving the driver's default effort. The row's work-size estimate
/// stays honest and does not imply a driver effort override.
#[tokio::test]
async fn small_investigation_row_spawns_fable_at_default_effort() {
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
        !script.contains("--effort "),
        "the dedicated tier must preserve the driver's default effort, got: {script:?}",
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
    let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

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
    runner.set_server_state(weak);

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
    let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore_manual(&work_db, product.id.clone(), "Some chore being reviewed");

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg.clone(), work_db.clone(), flags.clone());
    runner.set_server_state(weak.clone());

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
    runner2.set_server_state(weak);
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
    runner.set_server_state(weak);
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
/// `SpawnWorkerPaneInput.slot_id`. The app honors that slot
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
    runner.set_server_state(weak);

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
        Some("exec-test-1"),
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
/// execution row is cancelled while the `SpawnWorkerPane` round-trip
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
    runner.set_server_state(weak);

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
    let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

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
    runner.set_server_state(weak);

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
    let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

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
    runner.set_server_state(weak);

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
    assert!(
        prompt.contains("the deliverable is a PR URL"),
        "design prompt must keep the PR-URL acceptance criterion:\n{prompt}",
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
    let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

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
    runner.set_server_state(weak);

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
#[test]
fn resolve_boss_event_prefers_env_override() {
    let dir = TempDir::new().unwrap();
    let engine = dir.path().join("engine");
    std::fs::write(&engine, b"").unwrap();
    let override_path = PathBuf::from("/opt/whatever/boss-event");
    let resolved = resolve_boss_event_binary(&engine, None, Some(&override_path), None, None);
    assert_eq!(resolved, Some(override_path));
}

/// `BOSS_BIN_DIR` is the installed-mode path; it wins over the
/// dev-mode runfiles and workspace-bazel-bin candidates so a
/// deployed Boss.app never silently falls through to a workspace clone.
#[test]
fn resolve_boss_event_prefers_boss_bin_dir_over_runfiles() {
    let dir = TempDir::new().unwrap();
    let engine = dir.path().join("engine");
    std::fs::write(&engine, b"").unwrap();

    // Synthesize the bundle bin/ directory (installed mode).
    let bundle_bin = dir.path().join("bundle-bin");
    std::fs::create_dir_all(&bundle_bin).unwrap();
    let bundle_shim = bundle_bin.join("boss-event");
    std::fs::write(&bundle_shim, b"").unwrap();

    // Also synthesize runfiles (dev mode) — must NOT be picked.
    let runfiles = dir.path().join("engine.runfiles/_main/tools/boss/event-shim");
    std::fs::create_dir_all(&runfiles).unwrap();
    std::fs::write(runfiles.join("boss-event"), b"").unwrap();

    let resolved = resolve_boss_event_binary(&engine, None, None, Some(&bundle_bin), None);
    assert_eq!(resolved, Some(bundle_shim));
}

/// When the engine binary has runfiles at the bazel-conventional
/// path, the resolver must pick that up — this is the production
/// path under `bazel run //tools/boss/engine/core:engine` once the
/// engine `rust_binary` has the `data` dep on
/// `//tools/boss/event-shim:boss-event`. The original #174 fix
/// only covered the BOSS_EVENT_BIN branch; this test covers the
/// branch that actually fires in real launches.
#[test]
fn resolve_boss_event_uses_runfiles_when_present() {
    let dir = TempDir::new().unwrap();
    let engine = dir.path().join("engine");
    std::fs::write(&engine, b"").unwrap();

    // Synthesize the bazel runfiles tree the data dep produces.
    let runfiles = dir.path().join("engine.runfiles/_main/tools/boss/event-shim");
    std::fs::create_dir_all(&runfiles).unwrap();
    let shim = runfiles.join("boss-event");
    std::fs::write(&shim, b"").unwrap();

    let resolved = resolve_boss_event_binary(&engine, None, None, None, None);
    assert_eq!(resolved, Some(shim));
}

/// Workspace `bazel-bin` symlink path is the secondary candidate
/// — covers `bazel build` + non-`bazel run` scenarios where the
/// engine binary is invoked directly but `BUILD_WORKSPACE_DIRECTORY`
/// is set.
#[test]
fn resolve_boss_event_falls_back_to_workspace_bazel_bin() {
    let dir = TempDir::new().unwrap();
    let engine = dir.path().join("engine");
    std::fs::write(&engine, b"").unwrap();

    let workspace = dir.path().join("workspace");
    let bazel_bin = workspace.join("bazel-bin/tools/boss/event-shim");
    std::fs::create_dir_all(&bazel_bin).unwrap();
    let shim = bazel_bin.join("boss-event");
    std::fs::write(&shim, b"").unwrap();

    let resolved = resolve_boss_event_binary(&engine, Some(&workspace), None, None, None);
    assert_eq!(resolved, Some(shim));
}

/// The `PATH`-prepend clauses compose so that the clause emitted
/// LAST ends up FIRST on `PATH`. The worker launcher dir has to be
/// last for that reason, and each clause must no-op on an unset var.
#[test]
fn path_prepend_clauses_compose_with_the_last_one_winning() {
    let line = format!(
        "{}{}",
        path_prepend_clause("BOSS_BIN_DIR"),
        path_prepend_clause(boss_engine_worker_bin::WORKER_BIN_DIR_ENV),
    );
    assert_eq!(
        line,
        "[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; \
             [ -n \"$BOSS_WORKER_BIN_DIR\" ] && export PATH=\"$BOSS_WORKER_BIN_DIR:$PATH\"; "
    );

    // Both vars set: the launcher dir must be the first PATH entry.
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{line}printf '%s' \"$PATH\""))
        .env("PATH", "/usr/bin")
        .env("BOSS_BIN_DIR", "/bundle/bin")
        .env("BOSS_WORKER_BIN_DIR", "/worker/bin")
        .output()
        .expect("sh must be available");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/worker/bin:/bundle/bin:/usr/bin");

    // Neither set (dev mode, launcher write failed): PATH unchanged,
    // and in particular no empty leading entry.
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{line}printf '%s' \"$PATH\""))
        .env("PATH", "/usr/bin")
        .env_remove("BOSS_BIN_DIR")
        .env_remove("BOSS_WORKER_BIN_DIR")
        .output()
        .expect("sh must be available");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/usr/bin");
}

/// `ensure_worker_bin_dir` always leaves a usable `boss` (and `cube`)
/// behind, even when nothing resolves — an unresolved launcher that
/// fails loudly beats letting the worker PATH-resolve a build-from-source
/// shim. `bossctl` stays Boss-tier. The dir is keyed by workspace name
/// so concurrent spawns do not share a path.
#[test]
fn ensure_worker_bin_dir_writes_boss_and_cube_never_bossctl() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("workspaces/mono-agent-007");
    std::fs::create_dir_all(&workspace).unwrap();
    let bin_dir = ensure_worker_bin_dir(dir.path(), &workspace).expect("launcher dir must be written");
    assert_eq!(bin_dir, dir.path().join("bin").join("mono-agent-007"));

    let mut entries: Vec<String> = std::fs::read_dir(&bin_dir)
        .expect("readdir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["boss".to_owned(), "cube".to_owned()],
        "workers get `boss` and `cube` launchers; `bossctl` is Boss-tier",
    );
}

/// A non-derived spawn must still call `write_cube_launcher` so a stale
/// derived-PR compose wrapper left by a PREVIOUS worker in this same
/// workspace cannot linger and silently run for the new worker. Simulate
/// that stale state directly (bypassing `ensure_worker_bin_dir`, which
/// would itself already overwrite it) and assert the overwrite lands.
#[test]
fn ensure_worker_bin_dir_clears_a_stale_compose_wrapper() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("workspaces/mono-agent-008");
    std::fs::create_dir_all(&workspace).unwrap();
    let bin_dir = dir.path().join("bin").join("mono-agent-008");
    boss_engine_worker_bin::write_cube_pr_body_compose_launcher(&bin_dir, "## Prior worker PR body header\n")
        .expect("seed a stale compose wrapper");
    let stale = std::fs::read_to_string(bin_dir.join("cube")).unwrap();
    assert!(
        stale.contains("Prior worker PR body header"),
        "test setup must actually seed the compose wrapper"
    );

    let out = ensure_worker_bin_dir(dir.path(), &workspace).expect("launcher dir must be written");
    assert_eq!(out, bin_dir);
    let after = std::fs::read_to_string(bin_dir.join("cube")).unwrap();
    assert!(
        !after.contains("Prior worker PR body header"),
        "a non-derived spawn must overwrite a stale compose wrapper from a previous worker, got:\n{after}",
    );
}

/// Distinct workspaces get distinct launcher dirs under the shared
/// settings root — never a single host-wide `bin/boss`.
#[test]
fn ensure_worker_bin_dir_is_per_workspace() {
    let dir = TempDir::new().unwrap();
    let ws_a = dir.path().join("workspaces/mono-agent-a");
    let ws_b = dir.path().join("workspaces/mono-agent-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();

    let a = ensure_worker_bin_dir(dir.path(), &ws_a).expect("a");
    let b = ensure_worker_bin_dir(dir.path(), &ws_b).expect("b");
    assert_ne!(a, b);
    assert!(a.ends_with("bin/mono-agent-a"));
    assert!(b.ends_with("bin/mono-agent-b"));
    assert!(a.join("boss").exists());
    assert!(b.join("boss").exists());
}

/// When nothing resolves the function returns `None` — the caller
/// (`boss_event_binary`) turns this into a hard panic rather than
/// silently baking a bare `boss-event` into hook commands (which
/// causes `command not found` in the worker's sanitized PATH).
#[test]
fn resolve_boss_event_returns_none_when_nothing_resolves() {
    let dir = TempDir::new().unwrap();
    let engine = dir.path().join("engine");
    std::fs::write(&engine, b"").unwrap();
    let resolved = resolve_boss_event_binary(&engine, None, None, None, None);
    assert_eq!(resolved, None);
}

/// The stable bin dir (installed by the engine at startup) is
/// preferred over bazel runfiles and bazel-bin so a `bazel clean`
/// doesn't break hook paths already baked into worker settings.json.
#[test]
fn resolve_boss_event_prefers_stable_bin_dir_over_runfiles() {
    let dir = TempDir::new().unwrap();
    let engine = dir.path().join("engine");
    std::fs::write(&engine, b"").unwrap();

    // Synthesize the stable bin dir (engine startup installs it here).
    let stable_bin = dir.path().join("stable-bin");
    std::fs::create_dir_all(&stable_bin).unwrap();
    let stable_shim = stable_bin.join("boss-event");
    std::fs::write(&stable_shim, b"stable").unwrap();

    // Also synthesize runfiles — must NOT be picked when stable exists.
    let runfiles = dir.path().join("engine.runfiles/_main/tools/boss/event-shim");
    std::fs::create_dir_all(&runfiles).unwrap();
    std::fs::write(runfiles.join("boss-event"), b"runfiles").unwrap();

    let resolved = resolve_boss_event_binary(&engine, None, None, None, Some(&stable_bin));
    assert_eq!(resolved, Some(stable_shim));
}

/// `install_boss_event_to_stable_bin` copies the shim and marks it
/// executable so workers can invoke it directly.
#[test]
fn install_boss_event_to_stable_bin_copies_and_makes_executable() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("boss-event-source");
    std::fs::write(&source, b"#!/bin/sh\necho ok\n").unwrap();

    let stable_bin = dir.path().join("stable/bin");
    let result = install_boss_event_to_stable_bin(&source, &stable_bin);
    assert!(result.is_ok(), "install should succeed: {result:?}");
    let stable = result.unwrap();
    assert_eq!(stable, stable_bin.join("boss-event"));
    assert!(stable.exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&stable).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "boss-event must be executable after install");
    }
}

/// Installing when src == dst is a no-op (doesn't fail or corrupt the file).
#[test]
fn install_boss_event_to_stable_bin_no_op_when_already_stable() {
    let dir = TempDir::new().unwrap();
    let stable_bin = dir.path().join("bin");
    std::fs::create_dir_all(&stable_bin).unwrap();
    let stable = stable_bin.join("boss-event");
    std::fs::write(&stable, b"#!/bin/sh\n").unwrap();

    let result = install_boss_event_to_stable_bin(&stable, &stable_bin);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), stable);
}
