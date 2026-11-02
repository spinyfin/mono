//! Bound worker-host operations. Coordinator hosting is intentionally separate.
use crate::{driver::AgentDriver, work::WorkDb, worker_setup::WorkerKind};
use anyhow::Result;
use boss_protocol::{WorkExecution, WorkItemBinding};
use boss_tmux::Tmux;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Engine-bound configuration; values are captured once, never read from ambient sockets.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct RunnerConfig {
    pub events_socket_path: PathBuf,
    pub frontend_socket_path: Option<PathBuf>,
    pub control_token_path: Option<PathBuf>,
    pub tmux_socket_path: PathBuf,
    pub max_review_embed_diff_lines: u64,
    pub rollout_discovery_timeout: std::time::Duration,
    pub utility: Arc<dyn crate::utility_model::UtilityModel>,
    pub review_signal_checks: fn(&str) -> &'static [&'static str],
}

/// Observations of the current run, supplied by the engine's live-state owner.
pub trait DriverSignals: Send + Sync {
    fn has_driver_signal_for_run(&self, run_id: &str) -> bool;
    fn first_hook_event_for_run(&self, run_id: &str) -> Option<String>;
    fn driver_signalled_before_reap(&self, slot_id: u8, run_id: &str) -> bool;
}

pub struct GuardScripts {
    pub settings_dir: PathBuf,
    pub path: Option<PathBuf>,
    pub checkleft: Option<PathBuf>,
    pub git_store: Option<PathBuf>,
}

/// Engine lifecycle hooks used only by worker execution, never coordinator hosting.
pub trait RunnerHostFactory: Send + Sync {
    /// Retain the host for one execution without creating an ownership cycle at rest.
    fn bind(&self) -> Result<Arc<dyn RunnerHost>>;
}

/// Operations on a host retained for the duration of an execution.
#[async_trait::async_trait]
pub trait RunnerHost: Send + Sync {
    async fn start_worker(&self, input: StartWorkerInput, timeout: std::time::Duration) -> Result<StartedWorker>;
    async fn reap_worker_pane(&self, run_id: &str);
    fn verify_review_guide(&self, workspace: &Path, packet: &boss_pr_review_sources::SourcePacket) -> Result<PathBuf>;
    fn verify_previous_head(&self, workspace: &Path, sha: &str) -> Result<()>;
    fn throttle_clause(&self, db: &WorkDb) -> Result<&'static str>;
    fn driver_signals(&self) -> Option<&dyn DriverSignals>;
    fn draft_pr_mode(&self) -> bool;
    fn non_opus_auto_mode(&self) -> bool;
    fn worker_settings_path(&self, workspace: &Path) -> PathBuf;
    fn install_guard_scripts(&self, kind: &WorkerKind, execution_id: &str) -> Result<GuardScripts>;
    fn live_metadata(
        &self,
        execution: &WorkExecution,
        has_source_automation: bool,
    ) -> boss_protocol::LiveWorkerMetadata;
    fn attributed_pool_label(&self, execution: &WorkExecution, has_source_automation: bool) -> &'static str;
}

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
    /// `session_name` / `server_label` are already known to the caller at
    /// this point (the intent write above recorded them), so this passes
    /// them straight through to [`WorkDb::persist_tmux_identity_after_observation`]
    /// rather than going through the test-only None/None wrapper.
    fn record_tmux_session_created(
        &self,
        execution_id: &str,
        spawn_token: &str,
        pane_pid: i64,
        session_name: &str,
        server_label: &str,
    ) -> anyhow::Result<bool>;
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
        session_name: &str,
        server_label: &str,
    ) -> anyhow::Result<bool> {
        self.persist_tmux_identity_after_observation(
            execution_id,
            spawn_token,
            pane_pid,
            Some(session_name),
            Some(server_label),
        )
    }
}

/// Required collaborators for a local worker spawn.
#[derive(Clone)]
pub struct TmuxWorkerHost {
    tmux: Tmux,
    pub spawn_store: Arc<dyn TmuxSpawnStore>,
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

    pub fn tmux(&self) -> &boss_tmux::Tmux {
        &self.tmux
    }

    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    pub fn socket_path(&self) -> Option<&Path> {
        self.tmux.socket_path()
    }
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
    /// engine is the source of truth for slot allocation; an optional app
    /// viewer attaches to the tmux worker in this slot.
    pub slot_id: u8,
    pub workspace_path: PathBuf,
    pub events_socket_path: PathBuf,
    /// Frontend (control) socket this engine bound. Exported as
    /// `BOSS_SOCKET_PATH` so `boss` CLI verbs reach this engine even when a
    /// driver has scoped `$HOME` (Grok's process-home sandbox). `None` skips
    /// the export — tests that don't exercise CLI reachability. Must be the
    /// bound path, never re-derived from `$BOSS_SOCKET_PATH` / `$HOME`.
    pub frontend_socket_path: Option<PathBuf>,
    /// Control-token file this engine wrote. Exported as
    /// `BOSS_ENGINE_CONTROL_TOKEN_PATH` alongside [`Self::frontend_socket_path`]
    /// so CLI discovery does not derive `<socket-stem>.control-token` (a
    /// filename the engine never writes). Must be the bound path from
    /// `resolve_engine_paths`, never re-derived from the socket stem.
    /// `None` skips the export.
    pub control_token_path: Option<PathBuf>,
    pub boss_event_path: PathBuf,
    pub initial_input: String,
    /// Extra env vars to thread to the worker on top of the ones the
    /// worker settings template injects (`BOSS_EVENTS_SOCKET`,
    /// `BOSS_SOCKET_PATH`, `BOSS_ENGINE_CONTROL_TOKEN_PATH`,
    /// `BOSS_LEASE_ID`).
    #[builder(default)]
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
    #[builder(default)]
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
    #[builder(default = WorkerKind::Standard)]
    pub worker_kind: WorkerKind,
    /// Resolved agent driver for this worker. Production callers look the
    /// slug up once via [`crate::driver::DriverRegistry::require`] and pass
    /// the same `Arc` into provision, spawn, and this struct so every trait
    /// method on the run goes through one object. Tests may pass any
    /// registered (or stub) driver.
    pub driver: Arc<dyn AgentDriver>,
    /// Required tmux owner for this local worker.
    pub tmux_host: TmuxWorkerHost,
    /// Forwarded to `WorkerSetupInput` — see that field's doc. Ignored
    /// unless `worker_kind` is [`WorkerKind::Triage`].
    #[builder(default)]
    pub automation_outcome_proposals_seam_enabled: bool,
    /// Forwarded to worker rules to gate operator question guidance.
    #[builder(default)]
    pub run_done_proposals_seam_enabled: bool,
    /// Forwarded to worker rules so the Revisions `propose blocked` sentence
    /// matches the revision prompt's seam-gated blocked channel.
    #[builder(default)]
    pub worker_signal_proposals_seam_enabled: bool,
    /// Forwarded to `WorkerSetupInput` — see that field's doc. Ignored
    /// unless `worker_kind` is [`WorkerKind::Reviewer`].
    #[builder(default)]
    pub is_review_supervisor: bool,
    /// Forwarded to `WorkerSetupInput` — see that field's doc. Ignored
    /// unless `worker_kind` is [`WorkerKind::Reviewer`].
    #[builder(default)]
    pub is_post_merge_reviewer: bool,
    /// Forwarded to `WorkerSetupInput` — see that field's doc. Ignored
    /// unless `worker_kind` is [`WorkerKind::AnswerAgent`].
    #[builder(default)]
    pub checkout_positioned_on_pr_head: bool,
    /// Badge type, project, host, and start-time stamps for the resulting
    /// `LiveWorkerState`. Production dispatch resolves these from the
    /// execution; tests may leave them empty.
    #[builder(default)]
    pub live_metadata: boss_protocol::LiveWorkerMetadata,
}

#[derive(Debug)]
pub struct StartedWorker {
    pub slot_id: u8,
    pub shell_pid: i32,
    pub written_files: WrittenFiles,
}

#[derive(Debug, Clone)]
pub struct WrittenFiles {
    pub claude_md_path: PathBuf,
    /// Absolute path to the worker settings file. Lives *outside* the
    /// workspace (under the engine worker-settings directory); the runner threads it
    /// into the spawn invocation as `claude --settings <path>`.
    pub settings_path: PathBuf,
    pub gitignore_path: PathBuf,
}
