//! End-to-end worker spawn helper.
//!
//! Combines the pieces that need to fire when the engine starts a
//! pane-hosted worker for a run:
//!
//! 1. Render and write `<workspace>/.claude/CLAUDE.md` (and a
//!    self-excluding `.gitignore`) plus the worker settings file —
//!    which lives *outside* the workspace tree — from the templates in
//!    [`crate::worker_setup`].
//! 2. Send `SpawnWorkerPane` (legacy) or `AttachWorkerPane` (tmux-hosted)
//!    to the registered app session via the engine→app dispatch on
//!    `ServerState`.
//! 3. Register the returned shell pid in the
//!    [`crate::worker_registry::WorkerRegistry`] so subsequent hook
//!    events from the boss-event shim can be correlated back to the
//!    run via the registry's ancestor walk.
//!
//! This module is just the helper; the pane-driven runner that drives
//! it lives in `runner::PaneSpawnRunner`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use anyhow::{Context, anyhow};
use boss_protocol::WorkItemBinding;
use boss_tmux::{DisplayField, NewSession, SERVER_LABEL, Tmux};
use thiserror::Error;
use tokio::time::Duration;

use std::sync::Arc;

use crate::driver::{AgentDriver, Capability, ProgressIngress, ProgressObservationConfig};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::protocol::{
    AttachWorkerPaneInput, EngineToAppError, EngineToAppRequest, EngineToAppResponse, EnvVar, SpawnWorkerPaneInput,
    SpawnWorkerPaneResult,
};
use crate::work::WorkDb;
use crate::worker_registry::WorkerRegistry;
use crate::worker_setup::{WorkerKind, WorkerSetupInput, WrittenFiles, write_workspace_files};

/// Sanitized PATH for worker panes. Excludes `~/bin` (where the
/// `bossctl` symlink lives in this user's setup) and any other
/// per-user bin dir, so a worker that tries to invoke `bossctl`
/// directly fails with a PATH miss. Per `v2-design-risks.md` R3.
///
/// Order: Homebrew first (modern Apple-silicon paths), then the
/// system bins. `/usr/local/bin` is included for legacy x86 brew
/// installs but Apple-silicon machines ignore it.
const WORKER_SANITIZED_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

/// Env keys allowed to flow from the runner's `extra_env` into the
/// worker pane. Anything outside this set is dropped with a warning;
/// the goal is to prevent ambient env (e.g., a stray
/// `BOSS_CONTROL_SOCKET` left over from an interactive run, or
/// arbitrary tokens carried from the user's shell) from reaching
/// workers. Standard env (HOME, USER, SHELL, TERM, LANG, locale)
/// inherits naturally from the app process and is not in this list
/// because we never set it explicitly here.
const WORKER_EXTRA_ENV_ALLOWLIST: &[&str] = &[
    "BOSS_TASK_ID",
    // Absolute paths of the engine-owned structured-output artifacts the
    // worker writes: its designated payload (review findings / triage decision
    // / followups) and the PR URL. See `crate::structured_output`.
    "BOSS_PR_URL_OUTPUT",
    "BOSS_STRUCTURED_OUTPUT",
    // Engine-owned directory holding this workspace's `boss` launcher
    // (and nothing else — notably not `bossctl`). Prepended to the
    // worker's PATH so a bare `boss` runs the CLI shipped with this
    // engine rather than a build-from-source shim in the user's `~/bin`.
    // See `boss_engine_worker_bin`.
    boss_engine_worker_bin::WORKER_BIN_DIR_ENV,
    "CUBE_LEASE_ID",
    "CUBE_REPO",
];

/// Value forced into `EDITOR`/`VISUAL`/`GIT_EDITOR`/`JJ_EDITOR` for
/// worker panes. `false` exits non-zero immediately, so any tool that
/// falls through to `$EDITOR` aborts loudly instead of silently popping
/// the user's vim/VS Code window. The CLAUDE.md template tells workers
/// to always pass `-m` inline; this is the safety net that turns a
/// forgotten `-m` into a fast, recoverable error.
const WORKER_EDITOR_NOOP: &str = "false";

/// Value forced into `XAI_API_KEY` for every worker pane, same belt-and-
/// suspenders rationale as `WORKER_EDITOR_NOOP` above: a caller-side
/// precheck (`grok::home::provision_grok_home`) already refuses to spawn
/// the driver's own Grok pane when the real `GROK_HOME/auth.json` is
/// missing, but that guard only covers the one path that calls it. Any
/// other Grok invocation inside a worker pane — an ad hoc `grok agent
/// leader ...` a worker runs directly via Bash, a probe script, a future
/// driver path — inherits no such precheck, and the bundled Grok CLI's
/// own default behavior on a missing/expired session token is to
/// proactively call its `authenticate` extension method and escalate
/// through device-code/browser OAuth (confirmed live: `xai_grok_pager`
/// logs `auto-triggering login at startup` and, when the account's
/// device-flow endpoint is unavailable, falls through to loopback OAuth,
/// which shells out to open a real browser window — the incident this
/// guards against).
///
/// Per Grok's own documented auth precedence
/// (`~/.grok/docs/user-guide/02-authentication.md#auth-precedence`), an
/// active session token always wins over `XAI_API_KEY`, so a real,
/// already-provisioned `GROK_HOME/auth.json` is unaffected by this and
/// nothing changes for a correctly-provisioned Grok pane. Only when no
/// session token is available does Grok fall back to `XAI_API_KEY` —
/// and forcing that fallback to an intentionally-invalid value makes the
/// missing-credential case fail loudly and immediately (a 400 from
/// `api.x.ai`: "Incorrect API key provided") instead of escalating to an
/// interactive browser flow. Verified empirically: with no `auth.json`
/// present, the real pane invocation shape returns this exact API error
/// in under a second and never calls the platform's URL-open command,
/// versus opening a browser (or hanging on a device-code prompt) with
/// this var absent.
const WORKER_XAI_API_KEY_NO_INTERACTIVE_AUTH: &str = "xai-boss-worker-no-interactive-auth-fallback";

/// Current environment contract for Boss-owned tmux sessions.
/// [`crate::tmux_adoption`] imports this directly (rather than
/// redeclaring it, unlike the env var names below) because it is a fact
/// about *this* engine build's own contract, not an echo of something a
/// different process generation wrote — importing means a version bump
/// here is automatically enforced there with no second edit to keep in
/// sync. That module rejects adopting a session whose schema is missing,
/// unparseable, or newer than this value.
pub(crate) const TMUX_SESSION_SCHEMA: &str = "1";
/// Name of the environment variable [`TMUX_SESSION_SCHEMA`] is carried in.
/// Redeclared in [`crate::tmux_adoption`] rather than imported, matching
/// [`TMUX_SPAWN_TOKEN_ENV`]'s rationale: adoption reads what this constant
/// names from a different process generation's live session, not from
/// this module's own state. Private (not `pub(crate)`) because nothing
/// outside this module reads it — only [`TMUX_SESSION_SCHEMA`] needs the
/// wider visibility, for the reason given above.
const TMUX_SESSION_SCHEMA_ENV: &str = "BOSS_SESSION_SCHEMA";
const TMUX_SPAWN_TOKEN_ENV: &str = "BOSS_SPAWN_TOKEN";
const TMUX_SPAWN_TOKEN_OPTION: &str = "@boss_spawn_token";

/// Durable writes surrounding a tmux session creation. Kept as a narrow
/// seam so the spawn-ordering test can exercise the real tmux command shape
/// without requiring a SQLite fixture; [`WorkDb`] is the production store.
pub trait TmuxSpawnStore: Send + Sync {
    /// Persist the session identity before tmux receives any command.
    fn record_tmux_spawn_intent(
        &self,
        execution_id: &str,
        server_label: &str,
        session_name: &str,
        spawn_token: &str,
    ) -> anyhow::Result<bool>;

    /// Mark the pre-recorded session as created after its pane pid is known.
    fn record_tmux_session_created(&self, execution_id: &str, spawn_token: &str, pane_pid: i64)
    -> anyhow::Result<bool>;
}

impl TmuxSpawnStore for WorkDb {
    fn record_tmux_spawn_intent(
        &self,
        execution_id: &str,
        server_label: &str,
        session_name: &str,
        spawn_token: &str,
    ) -> anyhow::Result<bool> {
        self.record_tmux_spawn_intent_for_execution(execution_id, server_label, session_name, spawn_token)
    }

    fn record_tmux_session_created(
        &self,
        execution_id: &str,
        spawn_token: &str,
        pane_pid: i64,
    ) -> anyhow::Result<bool> {
        self.record_tmux_session_created_for_execution(execution_id, spawn_token, pane_pid)
    }
}

/// The collaborators for the tmux-hosted branch of a single worker spawn.
/// `None` on [`StartWorkerInput::tmux_host`] leaves the legacy app RPC
/// entirely unchanged.
#[derive(Clone)]
pub struct TmuxWorkerHost {
    tmux: Tmux,
    spawn_store: Arc<dyn TmuxSpawnStore>,
    session_name: String,
}

impl TmuxWorkerHost {
    pub fn new(tmux: Tmux, spawn_store: Arc<dyn TmuxSpawnStore>, session_name: String) -> Self {
        Self {
            tmux,
            spawn_store,
            session_name,
        }
    }

    fn session_name(&self) -> &str {
        &self.session_name
    }
}

/// Create one detached tmux session using the durable write ordering required
/// for restart adoption. Once the intent write succeeds, every later failure
/// deliberately leaves that `intended` record in place: it is the durable
/// evidence a future reconciler needs to distinguish a never-created session
/// from a session created just before a crash.
async fn start_tmux_worker(
    host: &TmuxWorkerHost,
    execution_id: &str,
    workspace_path: &Path,
    command: &str,
    env: &[EnvVar],
) -> Result<i32, StartWorkerError> {
    let spawn_token = crate::engine_control::generate_token();
    let intent_recorded = host
        .spawn_store
        .record_tmux_spawn_intent(execution_id, SERVER_LABEL, &host.session_name, &spawn_token)
        .context("recording tmux spawn intent")
        .map_err(StartWorkerError::Tmux)?;
    if !intent_recorded {
        return Err(StartWorkerError::Tmux(anyhow!(
            "no active run accepted tmux spawn intent for execution {execution_id}"
        )));
    }

    let mut environment = env
        .iter()
        .map(|EnvVar { key, value }| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    environment.insert(TMUX_SPAWN_TOKEN_ENV.to_owned(), spawn_token.clone());
    environment.insert(TMUX_SESSION_SCHEMA_ENV.to_owned(), TMUX_SESSION_SCHEMA.to_owned());
    // `BOSS_RUN_ID` is normally already in `env`; insert again here to make
    // the session identity contract explicit and prevent a future env
    // refactor from accidentally dropping it from the atomic `-e` set.
    environment.insert("BOSS_RUN_ID".to_owned(), execution_id.to_owned());

    host.tmux
        .new_session(&NewSession {
            name: host.session_name.clone(),
            environment,
            working_directory: workspace_path.to_path_buf(),
            command: command.to_owned(),
        })
        .await
        .context("creating detached tmux session")
        .map_err(StartWorkerError::Tmux)?;
    host.tmux
        .set_option(&host.session_name, TMUX_SPAWN_TOKEN_OPTION, &spawn_token)
        .await
        .context("mirroring tmux spawn token in session option")
        .map_err(StartWorkerError::Tmux)?;
    let pane_pid = host
        .tmux
        .display_message(&host.session_name, DisplayField::PanePid)
        .await
        .context("reading tmux pane pid")
        .map_err(StartWorkerError::Tmux)?
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| StartWorkerError::Tmux(anyhow!("tmux returned an invalid pane pid for {execution_id}")))?;
    let created_recorded = host
        .spawn_store
        .record_tmux_session_created(execution_id, &spawn_token, i64::from(pane_pid))
        .context("recording tmux session creation")
        .map_err(StartWorkerError::Tmux)?;
    if !created_recorded {
        return Err(StartWorkerError::Tmux(anyhow!(
            "tmux session was created but no intent row accepted its confirmation for execution {execution_id}"
        )));
    }
    Ok(pane_pid)
}

// No `Debug` derive: `Arc<dyn AgentDriver>` is not `Debug`, and nothing
// logs or asserts on a full `StartWorkerInput` dump.
#[derive(Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct StartWorkerInput {
    pub run_id: String,
    pub lease_id: String,
    /// Slot the engine has already claimed for this worker (1-indexed,
    /// matches the app's WorkersWorkspaceModel slot numbering). The
    /// engine is the source of truth for slot allocation; the app's
    /// job is to host the pane in this exact slot or fail with
    /// `EngineToAppError::SlotBusy`.
    pub slot_id: u8,
    pub workspace_path: PathBuf,
    pub events_socket_path: PathBuf,
    pub boss_event_path: PathBuf,
    pub initial_input: String,
    /// Extra env vars to thread to the worker on top of the ones the
    /// worker settings template injects (`BOSS_EVENTS_SOCKET`,
    /// `BOSS_LEASE_ID`).
    pub extra_env: Vec<(String, String)>,
    /// Optional 2–4 word gerund summary to display in the pane
    /// titlebar (e.g. `"fixing the fencer scraper"`). Set only when
    /// the engine successfully called Claude to generate the phrase.
    /// When absent, `task_title` is used for the fallback display.
    pub title_summary: Option<String>,
    /// Raw work-item title passed alongside `title_summary` so the
    /// app can render `"<AgentName>: <task_title>"` when no gerund
    /// summary is available (no API key or generation failed).
    pub task_title: Option<String>,
    /// Work-item linkage stamped onto the resulting `LiveWorkerState`
    /// so `bossctl agents list` / `agents status` can resolve "the
    /// worker on chore X" without prompting for a slot. `None` from
    /// callers that don't have a work item (tests).
    pub work_item_binding: Option<WorkItemBinding>,
    /// The model that was actually passed to the claude CLI via `--model`.
    /// Stamped onto `LiveWorkerState` at spawn so `bossctl agents list`
    /// reports the real dispatched model instead of a hardcoded default.
    pub model: String,
    /// When `true`, the engine-injected CLAUDE.md includes a directive
    /// to pass `--draft` to `gh pr create` by default. Sourced from
    /// the `default_pr_draft_mode` per-installation setting.
    pub draft_pr_mode: bool,
    /// Execution kind (e.g. `"chore_implementation"`, `"revision_implementation"`).
    /// Forwarded to `WorkerSetupInput` so the worker settings file can
    /// install kind-specific hook guards. Also stamped onto
    /// `LiveWorkerState.kind` at registration so `bossctl agents list`
    /// can render it without joining the execution table.
    pub execution_kind: String,
    /// Attributed worker pool for this run (`"main"`, `"automation"`, or
    /// `"review"`). Stamped onto `LiveWorkerState.pool` at registration.
    /// Production dispatch always sets this; tests may leave it `None`.
    pub pool: Option<String>,
    /// Task kind from the underlying work item (e.g. `"revision"`, `"chore"`).
    /// `None` for non-task work items (products, projects).
    /// Forwarded to `WorkerSetupInput` for defense-in-depth guard checks.
    pub task_kind: Option<String>,
    /// Worker kind — forwarded to `WorkerSetupInput` to select the per-kind
    /// tool denylist. Defaults to [`WorkerKind::Standard`] for all
    /// current callers; set to [`WorkerKind::Reviewer`] when spawning a
    /// reviewer worker.
    pub worker_kind: WorkerKind,
    /// Resolved agent driver for this worker. Production callers look the
    /// slug up once via [`crate::driver::DriverRegistry::require`] and pass
    /// the same `Arc` into provision, spawn, and this struct so every trait
    /// method on the run goes through one object. Tests may pass any
    /// registered (or stub) driver.
    pub driver: Arc<dyn AgentDriver>,
    /// Enables the direct tmux-hosted branch for this run. `None` preserves
    /// the app-hosted `SpawnWorkerPane` flow byte-for-byte.
    pub tmux_host: Option<TmuxWorkerHost>,
}

#[derive(Debug)]
pub struct StartedWorker {
    pub slot_id: u8,
    pub shell_pid: i32,
    pub written_files: WrittenFiles,
    /// `true` when the `SpawnWorkerPane` RPC never acked within the
    /// spawn window and the worker was registered *provisionally*: the
    /// app may or may not have hosted the pane, so the slot is tracked
    /// with `shell_pid = 0` and the spawn-ack sweep is left to confirm
    /// liveness (a hook/pid arrives) or reap it (total silence past the
    /// grace window). Callers use this only to log/annotate — the
    /// tracked-vs-failure decision has already been made here. See the
    /// ack-timeout branch in [`start_worker`].
    pub ack_timed_out: bool,
}

#[derive(Debug, Error)]
pub enum StartWorkerError {
    #[error("writing worker config: {0}")]
    WriteFiles(std::io::Error),
    #[error("sending SpawnWorkerPane to app: {0}")]
    Send(#[from] crate::app::SendToAppError),
    // Use Display (not Debug) so SlotBusy's "desync, not capacity"
    // wording reaches dispatch.jsonl / attention bodies rather than the
    // opaque `SlotBusy { occupying_run_id: ... }` debug dump.
    #[error("app reported spawn error: {0}")]
    AppError(EngineToAppError),
    #[error("app responded with unexpected response variant")]
    ResponseKindMismatch,
    #[error("preparing progress ingress: {0}")]
    ProgressIngress(String),
    #[error("tmux-hosted worker spawn: {0:#}")]
    Tmux(#[source] anyhow::Error),
}

impl StartWorkerError {
    /// Stable, machine-readable name for *which spawn step* refused.
    ///
    /// Carried on the `spawn_failed` dispatch event so a diagnose
    /// signature can key on the failing step rather than substring-matching
    /// a prose error, and so two different causes (an unwritable settings
    /// dir vs an ingress precondition) never collapse into one generic
    /// "spawn failed" bucket. The strings are wire values — treat them the
    /// same as `Stage::as_str()` and do not rename them casually.
    pub fn class(&self) -> &'static str {
        match self {
            Self::WriteFiles(_) => "write_files",
            Self::Send(_) => "send_spawn_request",
            Self::AppError(_) => "app_rejected",
            Self::ResponseKindMismatch => "response_kind_mismatch",
            Self::ProgressIngress(_) => "progress_ingress",
            Self::Tmux(_) => "tmux_host",
        }
    }

    /// The specific thing this step rejected, with the generic wrapper
    /// stripped.
    ///
    /// For [`Self::ProgressIngress`] that is the precondition text the
    /// ingress preparer returned verbatim (e.g.
    /// `/…/sessions is not a real directory`) — the actionable half of the
    /// message, and the part a recovery hint has to be able to name. Every
    /// other variant has no inner detail worth separating from its own
    /// `Display`, so it returns that.
    pub fn cause_detail(&self) -> String {
        match self {
            Self::ProgressIngress(detail) => detail.clone(),
            other => other.to_string(),
        }
    }
}

/// Public API for callers that want to wire pane-spawning into the
/// coordinator (or a test). The trait is implemented by
/// [`crate::app::ServerState`]; users should typically call through
/// `ServerState` directly, but the trait makes the dependency
/// explicit and lets stub implementations stand in for unit tests.
#[async_trait::async_trait]
pub trait WorkerSpawner: Send + Sync {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        timeout: Duration,
    ) -> Result<EngineToAppResponse, crate::app::SendToAppError>;

    fn worker_registry(&self) -> &WorkerRegistry;

    /// Engine's live per-slot state registry. Implementations return
    /// `None` from in-process tests that don't care about the live
    /// state surface; the spawn flow then skips the registration
    /// step. Production `ServerState` always returns `Some`.
    fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
        None
    }

    /// Hook called after `LiveWorkerStateRegistry` is updated so the
    /// caller can broadcast the snapshot on the worker live-state
    /// topic. Default no-op for tests.
    async fn publish_live_worker_states(&self) {}

    /// Hook called after a slot has been registered so the engine can
    /// spawn the per-slot live-status summarizer task. Default no-op
    /// for tests; production `ServerState` starts the task via its
    /// `LiveStatusManager`. The task tears itself down when
    /// `release_worker_pane` runs.
    fn start_live_status_slot(&self, _slot_id: u8, _run_id: &str, _driver: Arc<dyn AgentDriver>) {}

    /// Prepare a run-correlated progress source before pane spawn. File
    /// ingress implementations snapshot pre-existing candidates here but do
    /// not dispatch until [`Self::activate_progress_ingress`].
    fn prepare_progress_ingress(
        &self,
        _run_id: &str,
        _driver: Arc<dyn AgentDriver>,
        _ingress: ProgressIngress,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Activate the prepared source after live-state registration.
    fn activate_progress_ingress(&self, _run_id: &str) {}

    /// Cancel a prepared or active source after spawn failure/teardown.
    fn stop_progress_ingress(&self, _run_id: &str) {}

    /// Whether the `default_pr_draft_mode` setting is enabled. When
    /// `true`, the worker's CLAUDE.md gets a directive to pass
    /// `--draft` to `gh pr create`. Default `false` for tests.
    fn draft_pr_mode(&self) -> bool {
        false
    }

    /// Whether Sonnet/Haiku workers should use `--permission-mode auto`
    /// instead of `--dangerously-skip-permissions`. Controlled by the
    /// `workers.non_opus_permission_mode` setting. Default `false` (skip
    /// permissions) for tests; corp users set it to `true`.
    fn non_opus_auto_mode(&self) -> bool {
        false
    }

    /// Whether the attributed pool should create this worker in a detached
    /// tmux session. Defaults off so existing test spawners and production
    /// installations retain the app-hosted path until an operator enables a
    /// pool explicitly.
    fn tmux_hosting_enabled_for(&self, _pool: &str) -> bool {
        false
    }

    /// Tear down the libghostty pane and reap the OS process tree for
    /// `run_id` (the execution id). Used by the spawn flow to reap a
    /// worker that was cancelled *during* its spawn window: at cancel
    /// time the pid had not yet materialized, so the cancel path could
    /// not reap it and deliberately left the cube lease held. Once the
    /// spawn returns and the pid is registered, the runner calls this to
    /// kill the just-spawned worker before the coordinator releases the
    /// deferred lease. Default no-op for test spawners that don't host
    /// real panes; production `ServerState` delegates to
    /// [`crate::app::ServerState::release_worker_pane`].
    async fn reap_worker_pane(&self, _run_id: &str) {}
}

/// Render the worker-config files, ask the app to spawn a pane,
/// register the resulting shell pid for hook-event correlation, and
/// return the slot id + pid for the caller to record.
pub async fn start_worker<S: WorkerSpawner + ?Sized>(
    spawner: &S,
    input: StartWorkerInput,
    spawn_timeout: StdDuration,
) -> Result<StartedWorker, StartWorkerError> {
    let progress_ingress = input.driver.progress_observation_wiring(&ProgressObservationConfig {
        events_socket_path: input.events_socket_path.clone(),
        lease_id: input.lease_id.clone(),
        run_id: input.run_id.clone(),
        workspace_path: input.workspace_path.clone(),
        forwarder_binary: input.boss_event_path.clone(),
    });

    // 1. Write CLAUDE.md + .gitignore into the workspace and the worker
    //    settings file outside it (see worker_setup module docs).
    let setup = WorkerSetupInput {
        run_id: input.run_id.clone(),
        lease_id: input.lease_id.clone(),
        workspace_path: input.workspace_path.clone(),
        events_socket_path: input.events_socket_path.clone(),
        boss_event_path: input.boss_event_path.clone(),
        draft_pr_mode: input.draft_pr_mode,
        execution_kind: input.execution_kind.clone(),
        task_kind: input.task_kind.clone(),
        worker_kind: input.worker_kind.clone(),
    };
    let written = write_workspace_files(&setup, input.driver.as_ref()).map_err(StartWorkerError::WriteFiles)?;
    spawner
        .prepare_progress_ingress(&input.run_id, input.driver.clone(), progress_ingress)
        .map_err(StartWorkerError::ProgressIngress)?;

    // 2. Build the SpawnWorkerPane request. Workers get a strict env
    //    allowlist (per `v2-design-risks.md` R3): a sanitized PATH
    //    (no `bossctl`), the engine-injected `BOSS_EVENTS_SOCKET` and
    //    `BOSS_LEASE_ID`, and any caller-provided `extra_env` keys
    //    that survive the allowlist filter. Anything else is dropped.
    //
    //    Editor env vars are forced to `false` so a worker that runs
    //    `git commit` / `jj describe` without `-m` doesn't pop the
    //    user's vim/VS Code window — the command exits non-zero and
    //    the worker corrects course by passing `-m` inline. The
    //    matching CLAUDE.md guidance tells the worker the rule; this
    //    is the belt that catches it when the suspenders slip.
    let mut env = vec![
        EnvVar {
            key: "PATH".into(),
            value: WORKER_SANITIZED_PATH.into(),
        },
        EnvVar {
            key: "BOSS_EVENTS_SOCKET".into(),
            value: input.events_socket_path.display().to_string(),
        },
        EnvVar {
            key: "BOSS_LEASE_ID".into(),
            value: input.lease_id.clone(),
        },
        EnvVar {
            // Read by `boss-event` and embedded in every hook payload
            // as `_boss_run_id`. The engine uses this to correlate
            // hook events to runs without depending on a working
            // shell-pid lookup. `proc_listpids` in the app side is
            // still a TODO, and without it `WorkerRegistry`'s pid
            // map stays empty, `lookup_with_ancestor_walk` returns
            // None, and `dispatch_live_worker_state` silently skips
            // every event — that's the bug that pinned every worker's
            // activity at `Spawning` regardless of what the worker
            // was actually doing.
            key: "BOSS_RUN_ID".into(),
            value: input.run_id.clone(),
        },
        EnvVar {
            key: "EDITOR".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "VISUAL".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "GIT_EDITOR".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "JJ_EDITOR".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "XAI_API_KEY".into(),
            value: WORKER_XAI_API_KEY_NO_INTERACTIVE_AUTH.into(),
        },
    ];
    for (k, v) in input.extra_env {
        if WORKER_EXTRA_ENV_ALLOWLIST.contains(&k.as_str()) {
            env.push(EnvVar { key: k, value: v });
        } else {
            tracing::warn!(
                key = %k,
                "spawn_flow: dropping non-allowlisted env key from worker spawn",
            );
        }
    }

    // Installed mode: propagate BOSS_BIN_DIR (set by the app when
    // launching the engine from Boss.app/Contents/Resources/bin/).
    // Workers prepend this directory to PATH so `boss` / `boss-event`
    // callbacks resolve the bundled copies. Unset in dev mode (no bundle
    // bin/ directory) — workers fall back to PATH as today.
    if let Ok(boss_bin_dir) = std::env::var("BOSS_BIN_DIR")
        && !boss_bin_dir.is_empty()
    {
        env.push(EnvVar {
            key: "BOSS_BIN_DIR".into(),
            value: boss_bin_dir.clone(),
        });
        if let Some(path_entry) = env.iter_mut().find(|e| e.key == "PATH") {
            path_entry.value = format!("{boss_bin_dir}:{}", path_entry.value);
        }
    }

    // The per-workspace launcher dir goes ahead of everything, including
    // BOSS_BIN_DIR: it holds a `boss` pinned to an absolute path, so it
    // is the one entry that stays correct in dev mode (no bundle) as
    // well as installed mode. Applied after the BOSS_BIN_DIR prepend
    // above so it lands in front of it, mirroring the ordering of the
    // pane's first shell line in `pane_spawn`.
    let worker_bin_dir = env
        .iter()
        .find(|e| e.key == boss_engine_worker_bin::WORKER_BIN_DIR_ENV)
        .map(|e| e.value.clone());
    if let Some(worker_bin_dir) = worker_bin_dir.filter(|dir| !dir.is_empty())
        && let Some(path_entry) = env.iter_mut().find(|e| e.key == "PATH")
    {
        path_entry.value = format!("{worker_bin_dir}:{}", path_entry.value);
    }

    let claimed_slot = input.slot_id;
    let tmux_hosted = input.tmux_host.is_some();
    let (slot_id, shell_pid, ack_timed_out) = if let Some(tmux_host) = input.tmux_host.as_ref() {
        let shell_pid = match start_tmux_worker(
            tmux_host,
            &input.run_id,
            &input.workspace_path,
            &input.initial_input,
            &env,
        )
        .await
        {
            Ok(shell_pid) => shell_pid,
            Err(err) => {
                spawner.stop_progress_ingress(&input.run_id);
                return Err(err);
            }
        };
        // The detached tmux session is now the worker's owner. Attaching a
        // Ghostty surface is best-effort presentation only: an app restart or
        // a missing app session must not turn a successfully-created worker
        // into a failed spawn.
        match spawner
            .send_to_app_request(
                EngineToAppRequest::AttachWorkerPane(AttachWorkerPaneInput {
                    run_id: input.run_id.clone(),
                    slot_id: claimed_slot,
                    session_name: tmux_host.session_name().to_owned(),
                    summary: input.title_summary.clone(),
                    task_title: input.task_title.clone(),
                }),
                spawn_timeout,
            )
            .await
        {
            Ok(EngineToAppResponse::AttachWorkerPane { result: Ok(_) }) => {
                tracing::info!(
                    run_id = %input.run_id,
                    slot_id = claimed_slot,
                    session_name = tmux_host.session_name(),
                    "attached tmux-hosted worker pane",
                );
            }
            Ok(response) => {
                tracing::warn!(
                    run_id = %input.run_id,
                    slot_id = claimed_slot,
                    ?response,
                    "tmux worker started but app did not attach its viewer surface",
                );
            }
            Err(err) => {
                tracing::debug!(
                    run_id = %input.run_id,
                    slot_id = claimed_slot,
                    ?err,
                    "tmux worker started without an app viewer surface",
                );
            }
        }
        // Unlike the app RPC, `tmux new-session -d` returns a definite local
        // outcome and the synchronous `#{pane_pid}` read is already the
        // worker's real process identity. No provisional-ack state exists.
        (claimed_slot, shell_pid, false)
    } else {
        let send_outcome = spawner
            .send_to_app_request(
                EngineToAppRequest::SpawnWorkerPane(SpawnWorkerPaneInput {
                    run_id: input.run_id.clone(),
                    workspace_path: input.workspace_path.display().to_string(),
                    slot_id: claimed_slot,
                    initial_input: input.initial_input,
                    env,
                    summary: input.title_summary,
                    task_title: input.task_title,
                    // Driver-supplied screen-scrape markers for the app's
                    // pre-hook fallback status pill. None keeps Claude
                    // literals on the app side (older drivers / stubs).
                    // Boxed so the optional payload does not bloat the
                    // EngineToAppRequest enum when absent.
                    pane_monitor: input.driver.pane_monitor_spec().map(Box::new),
                }),
                Duration::from_secs(spawn_timeout.as_secs()),
            )
            .await;

        // Resolve the app-hosted spawn outcome into
        // `(slot_id, shell_pid, ack_timed_out)`. The timeout branch is
        // deliberately legacy-only: a tmux creation is synchronous and
        // therefore never has an unknown app-RPC outcome.
        match send_outcome {
            Ok(EngineToAppResponse::SpawnWorkerPane { result }) => match result {
                Ok(SpawnWorkerPaneResult { slot_id, shell_pid }) => (slot_id, shell_pid, false),
                Err(err) => {
                    spawner.stop_progress_ingress(&input.run_id);
                    return Err(StartWorkerError::AppError(err));
                }
            },
            Ok(
                EngineToAppResponse::ReleaseWorkerPane { .. }
                | EngineToAppResponse::AttachWorkerPane { .. }
                | EngineToAppResponse::DetachWorkerPane { .. }
                | EngineToAppResponse::SendToPane { .. }
                | EngineToAppResponse::FocusWorkerPane { .. }
                | EngineToAppResponse::InterruptWorkerPane { .. }
                | EngineToAppResponse::RevealWorkItem { .. }
                | EngineToAppResponse::OpenDocument { .. }
                | EngineToAppResponse::ListHostedPanes { .. },
            ) => {
                spawner.stop_progress_ingress(&input.run_id);
                return Err(StartWorkerError::ResponseKindMismatch);
            }
            Err(crate::app::SendToAppError::Timeout) => {
                tracing::warn!(
                    run_id = %input.run_id,
                    slot_id = claimed_slot,
                    timeout_secs = spawn_timeout.as_secs(),
                    "spawn_flow: SpawnWorkerPane ack timed out — outcome is UNKNOWN (the app may have \
                     hosted the pane anyway, e.g. a slow post-sleep RPC drain). Registering the slot \
                     provisionally with shell_pid 0 and leaving the spawn-ack sweep to confirm liveness \
                     (a hook/pid arrives) or reap on total silence. NOT failing the execution or \
                     releasing the workspace lease, which would strand a live pane and duplicate dispatch.",
                );
                (claimed_slot, 0, true)
            }
            Err(err) => {
                spawner.stop_progress_ingress(&input.run_id);
                return Err(StartWorkerError::Send(err));
            }
        }
    };

    // The engine dictates the slot; the app's response slot is just a
    // confirmation echo. A mismatch means the app picked a different
    // slot than we asked for, which would re-introduce the dual
    // allocator the engine-owns-slots refactor exists to remove. On an
    // ack timeout there is no echo, so `slot_id` is the engine-claimed
    // slot by construction and this holds trivially.
    debug_assert_eq!(
        slot_id, claimed_slot,
        "app honored a different slot ({slot_id}) than the engine claimed ({claimed_slot})"
    );

    // 3. Register the shell pid against the run id so the events
    //    socket can correlate hook events from descendants of the
    //    spawned shell back to this run, and remember the slot id so
    //    follow-up `SendToPane` requests (e.g., probe injection) can
    //    route by run id.
    if tmux_hosted {
        spawner
            .worker_registry()
            .register_tmux_run_slot(input.run_id.clone(), slot_id);
    } else {
        spawner
            .worker_registry()
            .register_run_slot(input.run_id.clone(), slot_id);
    }
    if shell_pid > 0 {
        spawner.worker_registry().register(shell_pid, input.run_id.clone());
    } else {
        tracing::info!(
            slot_id,
            run_id = %input.run_id,
            "spawn returned shell_pid 0; awaiting update_worker_shell_pid from app once surface initializes",
        );
    }

    // 4. Stamp the initial LiveWorkerState so bossctl/UI immediately
    //    see "Spawning" with the launch-default model — no more
    //    "Claude Unknown" while we wait for SessionStart to fire.
    if let Some(live_states) = spawner.live_worker_state_registry() {
        // Ask the resolved driver, rather than assume: this derives the
        // capability from the actual driver's declared capabilities and
        // passes it straight into registration, so there is no window
        // between spawn and a follow-up setter call where a concurrently
        // delivered hook event would be evaluated against a stale default.
        live_states.register_spawn_with_capabilities(
            slot_id,
            input.run_id.clone(),
            input.model,
            shell_pid,
            input.work_item_binding,
            input.driver.capabilities().provides(Capability::AwaitingInputSignal),
            // Always stamp the execution kind; production callers pass a
            // non-empty snake_case kind, tests that leave the field at a
            // placeholder still surface it on the wire. Pool may be
            // `None` for tests that never set `StartWorkerInput.pool`.
            crate::live_worker_state::LiveSpawnRouting {
                pool: input.pool,
                kind: Some(input.execution_kind),
            },
        );
        // Declare this slot's driver-reported progress fidelity so
        // `stale_worker_sweep` judges cadence-based staleness against the
        // driver that is actually running, not an assumed Claude rhythm
        // (see `ProgressFidelity::stale_threshold_secs`).
        live_states.set_progress_fidelity(slot_id, input.driver.progress_fidelity());
        spawner.publish_live_worker_states().await;
        // 5. Spin up the live-status summarizer for this slot. The
        //    manager owns the task lifecycle and will be torn down
        //    on `release_worker_pane`.
        spawner.start_live_status_slot(slot_id, &input.run_id, input.driver.clone());
    }
    spawner.activate_progress_ingress(&input.run_id);

    Ok(StartedWorker {
        slot_id,
        shell_pid,
        written_files: written,
        ack_timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SendToAppError;
    use crate::driver::test_support::{codex_homes_override, transcript_store_override};
    use boss_tmux::{CommandOutput, CommandRunner};
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    struct StubSpawner {
        registry: WorkerRegistry,
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
            match self.last_request.lock().unwrap().clone() {
                Some(EngineToAppRequest::SpawnWorkerPane(input)) => input
                    .env
                    .into_iter()
                    .map(|EnvVar { key, value }| (key, value))
                    .collect(),
                _ => panic!("last request was not SpawnWorkerPane"),
            }
        }
    }

    fn sample_input(workspace: &TempDir) -> StartWorkerInput {
        StartWorkerInput {
            run_id: "run-test".into(),
            lease_id: "lease-test".into(),
            slot_id: 3,
            workspace_path: workspace.path().to_path_buf(),
            events_socket_path: PathBuf::from("/tmp/events.sock"),
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
            tmux_host: None,
        }
    }

    #[derive(Default)]
    struct RecordingTmuxStore {
        steps: std::sync::Mutex<Vec<&'static str>>,
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
            Ok(true)
        }

        fn record_tmux_session_created(
            &self,
            _execution_id: &str,
            _spawn_token: &str,
            _pane_pid: i64,
        ) -> anyhow::Result<bool> {
            self.steps.lock().unwrap().push("created");
            Ok(true)
        }
    }

    #[derive(Default)]
    struct RecordingTmuxRunner {
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        steps: Arc<RecordingTmuxStore>,
    }

    impl RecordingTmuxRunner {
        fn new(steps: Arc<RecordingTmuxStore>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                steps,
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
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
            let (step, stdout) = match args.get(2).map(String::as_str) {
                Some("new-session") => ("new-session", ""),
                Some("set-option") => ("label", ""),
                Some("display-message") => ("pane-pid", "4242\n"),
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
    }

    #[tokio::test]
    async fn tmux_hosted_spawn_commits_intent_before_creation_and_carries_env_with_e_flags() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Err(SendToAppError::NotRegistered),
        };
        let store = Arc::new(RecordingTmuxStore::default());
        let runner = Arc::new(RecordingTmuxRunner::new(store.clone()));
        let tmux = Tmux::with_runner("/opt/homebrew/bin/tmux", runner.clone()).unwrap();
        let mut input = sample_input(&workspace);
        input.tmux_host = Some(TmuxWorkerHost::new(tmux, store.clone(), "boss-3-run-test".to_owned()));

        let started = start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

        assert_eq!(started.shell_pid, 4242);
        assert!(!started.ack_timed_out);
        assert_eq!(registry.lookup(4242).as_deref(), Some("run-test"));
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
            }
            other => panic!("expected AttachWorkerPane request, got {other:?}"),
        }
        assert_eq!(
            store.steps(),
            vec!["intent", "new-session", "label", "pane-pid", "created"]
        );

        let calls = runner.calls();
        let create = &calls[0];
        assert_eq!(&create[..5], ["-L", "boss", "new-session", "-d", "-s"]);
        assert!(create.windows(2).any(|pair| pair == ["-e", "BOSS_RUN_ID=run-test"]));
        assert!(create.windows(2).any(|pair| pair == ["-e", "BOSS_SESSION_SCHEMA=1"]));
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
        assert_eq!(
            &calls[1][..6],
            ["-L", "boss", "set-option", "-t", "boss-3-run-test", "@boss_spawn_token"]
        );
        assert_eq!(
            calls[2],
            vec![
                "-L",
                "boss",
                "display-message",
                "-p",
                "-t",
                "boss-3-run-test",
                "#{pane_pid}"
            ]
        );
    }

    #[tokio::test]
    async fn happy_path_writes_files_sends_request_and_registers_pid() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 3,
                    shell_pid: 42_111,
                }),
            }),
        };

        let started = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(started.slot_id, 3);
        assert_eq!(started.shell_pid, 42_111);
        assert!(started.written_files.claude_md_path.exists());
        assert!(started.written_files.settings_path.exists());
        assert!(started.written_files.gitignore_path.exists());
        assert_eq!(registry.lookup(42_111).as_deref(), Some("run-test"));
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
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 3,
                    shell_pid: 99,
                }),
            }),
        };

        let mut descriptor = stub_descriptor();
        descriptor.name = "stub-codex";
        descriptor.config_dir = ".stub";
        descriptor.agent_rules_filename = "AGENTS.md";
        let mut input = sample_input(&workspace);
        input.driver = Arc::new(StubDriver::new(descriptor, CapabilitySet::new([Capability::Spawn])));

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

    #[tokio::test]
    async fn shell_pid_zero_skips_registration_with_warning() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 3,
                    shell_pid: 0,
                }),
            }),
        };

        let started = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(started.shell_pid, 0);
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn app_error_propagates() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Err(EngineToAppError::NoAvailableSlot),
            }),
        };

        let result = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1)).await;
        assert!(matches!(
            result,
            Err(StartWorkerError::AppError(EngineToAppError::NoAvailableSlot))
        ));
        assert!(registry.is_empty());
    }

    /// Engine-owns-slots invariant: the slot the runner claimed for
    /// this worker (set on `StartWorkerInput.slot_id`) must reach
    /// the app verbatim on `SpawnWorkerPaneInput.slot_id`. A drop
    /// here would re-allow the app's old firstIndex(where:) heuristic
    /// to silently override the engine's pick.
    #[tokio::test]
    async fn spawn_request_carries_engine_claimed_slot_id() {
        let workspace = TempDir::new().unwrap();
        let spawner = StubSpawner {
            registry: WorkerRegistry::new(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 7,
                    shell_pid: 99,
                }),
            }),
        };
        let mut input = sample_input(&workspace);
        input.slot_id = 7;

        start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

        let last = spawner.last_request.lock().unwrap().clone().unwrap();
        match last {
            EngineToAppRequest::SpawnWorkerPane(req) => {
                assert_eq!(req.slot_id, 7);
            }
            other => panic!("expected SpawnWorkerPane request, got {other:?}"),
        }
    }

    /// If the app and the engine disagree about which slot is free
    /// (engine asks for slot N; app already hosts a session there),
    /// the app returns `SlotBusy` and the spawn flow surfaces it as
    /// `StartWorkerError::AppError(SlotBusy)` without registering a
    /// pid for the run. The coordinator can then handle the
    /// disagreement explicitly instead of the app silently picking a
    /// different slot.
    #[tokio::test]
    async fn slot_busy_error_propagates_without_registering_pid() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Err(EngineToAppError::SlotBusy {
                    occupying_run_id: Some("run-husk".into()),
                }),
            }),
        };

        let result = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1)).await;
        assert!(
            matches!(
                result,
                Err(StartWorkerError::AppError(EngineToAppError::SlotBusy { .. }))
            ),
            "expected SlotBusy app error, got {result:?}",
        );
        assert!(
            registry.is_empty(),
            "registry must be empty when the app rejects the spawn — no pid to track",
        );
    }

    #[tokio::test]
    async fn write_failure_does_not_send_request() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let spawner = StubSpawner {
            registry,
            spawn_calls: spawn_calls.clone(),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 1,
                    shell_pid: 1,
                }),
            }),
        };

        // Point at a path that's a regular file, not a directory, so
        // create_dir_all fails inside write_workspace_files.
        let blocked = workspace.path().join("blocked");
        std::fs::write(&blocked, b"i am a file").unwrap();
        let mut input = sample_input(&workspace);
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
        /// The `SpawnWorkerPane` request to the app.
        PaneRequested,
        /// `activate_progress_ingress`, tagged with whether the slot's
        /// live-state entry was already registered when it fired.
        Activated { registered: bool },
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
    struct LiveStateSpawner {
        registry: WorkerRegistry,
        live_states: LiveWorkerStateRegistry,
        jsonl: crate::agent_jsonl_progress::AgentJsonlProgressManager,
        slot_id: u8,
        shell_pid: i32,
        spawn_calls: Arc<AtomicUsize>,
        steps: std::sync::Mutex<Vec<SpawnStep>>,
    }

    impl LiveStateSpawner {
        fn new(slot_id: u8, shell_pid: i32) -> Self {
            Self {
                registry: WorkerRegistry::new(),
                live_states: LiveWorkerStateRegistry::new(),
                jsonl: crate::agent_jsonl_progress::AgentJsonlProgressManager::new(),
                slot_id,
                shell_pid,
                spawn_calls: Arc::new(AtomicUsize::new(0)),
                steps: std::sync::Mutex::new(Vec::new()),
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
            Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: self.slot_id,
                    shell_pid: self.shell_pid,
                }),
            })
        }

        fn worker_registry(&self) -> &WorkerRegistry {
            &self.registry
        }

        fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
            Some(&self.live_states)
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
    fn codex_input(workspace: &TempDir, run_id: &str, slot_id: u8) -> StartWorkerInput {
        let codex_home = crate::driver::codex::codex_home_for_run(run_id).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        crate::driver::transcript_store::provision_durable_sessions(&codex_home, "codex", run_id).unwrap();
        let mut input = sample_input(workspace);
        input.run_id = run_id.to_owned();
        input.slot_id = slot_id;
        input.model = "gpt-5.6-sol".into();
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
    /// that ingress is prepared *before* `SpawnWorkerPane` — an unhappy
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
        let spawner = LiveStateSpawner::new(4, 4242);
        let input = codex_input(&workspace, "exec-codex-1", 4);

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
        assert_eq!(state.model, "gpt-5.6-sol");
        assert_eq!(state.shell_pid, 4242);
        assert_eq!(state.pool.as_deref(), Some("main"));
        assert_eq!(state.kind.as_deref(), Some("chore_implementation"));
        assert_eq!(state.work_item_id.as_deref(), Some("task-codex-1"));
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
        let spawner = LiveStateSpawner::new(3, 77);

        start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
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
        assert_eq!(state.pool.as_deref(), Some("main"));
    }

    fn ok_spawner_capturing() -> StubSpawner {
        StubSpawner {
            registry: WorkerRegistry::new(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            // Echo whatever sample_input claims (slot 3) so the
            // engine-side debug_assert that the app honored the
            // claimed slot doesn't fire in tests.
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 3,
                    shell_pid: 1,
                }),
            }),
        }
    }

    #[tokio::test]
    async fn env_includes_sanitized_path_and_engine_keys() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
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
            env.iter().find(|(k, _)| k == "BOSS_LEASE_ID").map(|(_, v)| v.as_str()),
            Some("lease-test"),
        );
    }

    #[tokio::test]
    async fn worker_bin_dir_is_prepended_ahead_of_the_sanitized_path() {
        // The launcher dir holds a `boss` pinned to an absolute path. It
        // has to win over every other PATH entry, otherwise a bare `boss`
        // can still land on a build-from-source shim.
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        let mut input = sample_input(&workspace);
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
    }

    #[tokio::test]
    async fn path_is_untouched_when_no_worker_bin_dir_is_supplied() {
        // The launcher dir is best-effort: a temp-dir failure must not
        // corrupt PATH with an empty leading entry (which `sh` reads as
        // the current directory).
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
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

    #[tokio::test]
    async fn env_forces_editor_vars_to_noop() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
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

        start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
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

        let mut input = sample_input(&workspace);
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

        let mut input = sample_input(&workspace);
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

        let mut input = sample_input(&workspace);
        input.title_summary = Some("Pane Titlebar Summary".to_owned());

        start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

        match spawner.last_request.lock().unwrap().clone() {
            Some(EngineToAppRequest::SpawnWorkerPane(input)) => {
                assert_eq!(input.summary.as_deref(), Some("Pane Titlebar Summary"));
            }
            other => panic!("expected SpawnWorkerPane, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_title_summary_does_not_attach_one() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await
            .unwrap();

        match spawner.last_request.lock().unwrap().clone() {
            Some(EngineToAppRequest::SpawnWorkerPane(input)) => {
                assert!(input.summary.is_none());
            }
            other => panic!("expected SpawnWorkerPane, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn extra_env_drops_non_allowlisted_keys() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        let mut input = sample_input(&workspace);
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

    /// Regression test: a `SpawnWorkerPane` ack timeout must NOT surface
    /// as a spawn failure. The app may have hosted the pane anyway (seen
    /// previously: a slow post-sleep RPC drain made the ack time out while
    /// the `claude` process had already started). `start_worker` returns
    /// `Ok` with `ack_timed_out` set, `shell_pid = 0`, and the
    /// engine-claimed slot — and it registers the run→slot mapping so the
    /// (possibly-live) pane's hook events correlate back to this run and
    /// the spawn-ack sweep can confirm liveness or reap. Because it is
    /// `Ok`, the coordinator never takes the failure path that releases
    /// the lease and duplicate-dispatches the work item.
    #[tokio::test]
    async fn ack_timeout_registers_provisional_worker_instead_of_failing() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Err(SendToAppError::Timeout),
        };

        let started = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await
            .expect("ack timeout must be a provisional success, not an error");

        assert!(started.ack_timed_out, "ack timeout must be flagged as provisional");
        assert_eq!(started.shell_pid, 0, "a provisional spawn has no reported shell pid");
        assert_eq!(
            started.slot_id, 3,
            "a provisional spawn tracks the engine-claimed slot (no app echo on timeout)",
        );
        // The run→slot mapping is the correlation key: without it,
        // dispatch_live_worker_state would drop every hook the live pane
        // emits, and the spawn-ack sweep would never see the slot.
        assert_eq!(
            registry.slot_for_run("run-test"),
            Some(3),
            "provisional spawn must register the run→slot mapping so hooks correlate",
        );
    }

    /// Only an ambiguous ack *timeout* is treated as provisional. A
    /// `NotRegistered` send error means the request was never delivered
    /// (no app session) — the pane definitively did not spawn — so it
    /// stays a hard failure and registers nothing. The coordinator's
    /// failure path (release lease + mark failed) is correct for this
    /// case.
    #[tokio::test]
    async fn not_registered_send_error_stays_a_hard_failure() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Err(SendToAppError::NotRegistered),
        };

        let result = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1)).await;
        assert!(
            matches!(result, Err(StartWorkerError::Send(SendToAppError::NotRegistered))),
            "a never-delivered spawn must remain a hard failure; got {result:?}",
        );
        assert!(
            registry.slot_for_run("run-test").is_none(),
            "a hard-failure spawn must not register a run→slot mapping",
        );
    }
}
