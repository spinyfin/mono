//! The hub's host adapter for workers that live on a spoke.
//!
//! [`SpokeHostAdapter`] implements the same [`HostAdapter`] the SSH and local
//! adapters do, but every operation is a typed command on the host's one
//! persistent stream instead of a shell line. Three rules shape it
//! (design §§2.1, 2.5, 3.2 step 4):
//!
//! * **Ownership is recorded first.** [`SpokeHostAdapterProvider::adapter_for_execution`]
//!   reserves `(execution, run, generation, host, spawn token)` in the work
//!   database before it returns an adapter, and the command outbox refuses any
//!   command naming a run the database does not hold for that host. An
//!   unresolved assignment is never handed to a second host.
//! * **Uncertainty is preserved.** A timeout or a disconnect after a command was
//!   stored is reported as uncertain, never as failure. A retry of a lease or a
//!   spawn reuses the stored command, so nothing is leased or started twice, and
//!   a late result is answered from the stored result.
//! * **Runs keep their transport.** An execution with a spoke assignment is a
//!   spoke run whatever its host's transport says now, and an operation that
//!   names no spoke assignment goes to the legacy adapter, never the other way
//!   round. A spoke-owned lease or workspace is never sent over SSH.
//!
//! Nothing here mounts a listener or composes a worker prompt: the stream
//! manager is handed in, and prompt/settings composition is the injected
//! [`SpawnComposer`]. The adapter ships inert until the engine installs the
//! provider and a host's transport is set to `spoke_ready`.
mod outbox;
mod spawn;
mod workspace;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use boss_engine_recovery::execution_bookmark::{
    ExecutionBookmark, PreserveOutcome, PreservedTip, PruneOutcome, RestoreReport, Voucher, WorkspaceTipSource,
};
use boss_engine_structured_output::StructuredOutputKind;
use boss_execution_contracts::{
    CollectOutcome, ControlOutcome, CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
    CubeWorkspaceStatus, HostAdapter, HostAdapterProvider, Occupancy, RunOutcome, WorkerInputKind, WorkerPane,
};
use boss_protocol::{WorkExecution, WorkItem};
use boss_protocol_spoke::v1::{Command, CommandResult, ResultCode, Run, WorkspaceKind, command};
use boss_spoke_stream::{CommandOutcome, StreamManager};
use boss_work_db::{Host, HostTransport, SpokeAssignment, WorkDb};
use prost::Message;

pub use outbox::WorkDbOutbox;
pub use spawn::{ComposedSpawn, SpawnComposer, SpawnInput};
use workspace::ReleaseMode;

/// Deadlines and lifetimes for commands the adapter sends.
#[derive(Debug, Clone, Copy, bon::Builder)]
pub struct SpokeAdapterConfig {
    /// How long a caller waits for an ordinary command's result.
    #[builder(default = Duration::from_secs(60))]
    pub command_deadline: Duration,
    /// How long a caller waits for a spawn: the supervisor waits for the
    /// initial brief to reach the driver before it answers.
    #[builder(default = Duration::from_secs(120))]
    pub spawn_deadline: Duration,
    /// How long a stored command stays executable. A spoke that was offline
    /// longer rejects it, durably and in order, rather than acting late.
    #[builder(default = Duration::from_secs(600))]
    pub command_ttl: Duration,
}

/// Why a command did not complete. Carried in the returned error so callers
/// can tell "nothing happened" from "unknown" without parsing text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostCallError {
    /// The command was not submitted: the host is not connected, its queue is
    /// full or the command is invalid. Safe to retry.
    #[error("host {host_id} could not take {op}; nothing was sent: {reason}")]
    NotSent {
        host_id: String,
        op: &'static str,
        reason: String,
    },
    /// The command was stored and may have run. Reconcile with the same
    /// command id; do not issue a replacement.
    #[error("outcome of {op} ({command_id}) on host {host_id} is uncertain: {reason}")]
    Uncertain {
        host_id: String,
        op: &'static str,
        command_id: String,
        reason: String,
    },
    /// The host refused before any side effect.
    #[error("host {host_id} rejected {op}: {reason}")]
    Rejected {
        host_id: String,
        op: &'static str,
        reason: String,
    },
}

/// The services every adapter for every host shares.
#[derive(bon::Builder)]
pub(crate) struct Hub {
    pub(crate) streams: StreamManager,
    pub(crate) db: Arc<WorkDb>,
    pub(crate) outbox: WorkDbOutbox,
    pub(crate) composer: Arc<dyn SpawnComposer>,
    pub(crate) config: SpokeAdapterConfig,
    #[builder(skip)]
    nonce: AtomicU64,
}

impl Hub {
    pub(crate) fn unique_id(&self, prefix: &str, run_id: &str) -> String {
        format!(
            "{prefix}-{run_id}-{}-{:x}",
            boss_work_db::next_id("c"),
            self.nonce.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A command for `run`, valid for the configured lifetime.
    pub(crate) fn new_command(&self, command_id: &str, run: Option<Run>, body: command::Body) -> Command {
        let not_after = SystemTime::now() + self.config.command_ttl;
        Command {
            sequence: 0,
            command_id: command_id.to_owned(),
            run,
            not_after_unix_ms: not_after.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64),
            body: Some(body),
        }
    }

    pub(crate) async fn send(
        &self,
        host_id: &str,
        op: &'static str,
        command_id: String,
        run: Option<Run>,
        body: command::Body,
        deadline: Duration,
    ) -> Result<CommandResult, HostCallError> {
        let command = self.new_command(&command_id, run, body);
        self.send_command(host_id, op, command, deadline).await
    }

    /// Send the command stored under `command_id`, byte for byte, or build and
    /// send a new one when nothing is stored. A retry must present the original
    /// command (same body, same expiry) or the outbox refuses it as a different
    /// request under a known id; the stored sequence is the outbox's to assign,
    /// so it goes out as zero. A settled command answers from its stored result.
    pub(crate) async fn send_stable(
        &self,
        host_id: &str,
        op: &'static str,
        command_id: String,
        run: Option<Run>,
        body: command::Body,
        deadline: Duration,
    ) -> Result<CommandResult, HostCallError> {
        let (db_host, id) = (host_id.to_owned(), command_id.clone());
        let db = Arc::clone(&self.db);
        let stored = tokio::task::spawn_blocking(move || db.host_command_wire(&db_host, &id))
            .await
            .map_err(|error| HostCallError::NotSent {
                host_id: host_id.to_owned(),
                op,
                reason: format!("work database task failed: {error}"),
            })?
            .map_err(|error| HostCallError::NotSent {
                host_id: host_id.to_owned(),
                op,
                reason: format!("reading the stored command failed: {error:#}"),
            })?;
        let command = match stored {
            Some(wire) => {
                let mut command = Command::decode(wire.as_slice()).map_err(|error| HostCallError::Uncertain {
                    host_id: host_id.to_owned(),
                    op,
                    command_id: command_id.clone(),
                    reason: format!("the stored command cannot be decoded: {error}"),
                })?;
                command.sequence = 0;
                command
            }
            None => self.new_command(&command_id, run, body),
        };
        self.send_command(host_id, op, command, deadline).await
    }

    /// Send an already-built command, for a retry that must reuse the stored one.
    pub(crate) async fn send_command(
        &self,
        host_id: &str,
        op: &'static str,
        command: Command,
        deadline: Duration,
    ) -> Result<CommandResult, HostCallError> {
        let command_id = command.command_id.clone();
        match self.streams.command(host_id, command, deadline).await {
            Ok(result) => Ok(result),
            Err(CommandOutcome::OutcomeUncertain) => Err(HostCallError::Uncertain {
                host_id: host_id.to_owned(),
                op,
                command_id,
                reason: "no result before the deadline; the host may still run it".to_owned(),
            }),
            Err(other) => Err(HostCallError::NotSent {
                host_id: host_id.to_owned(),
                op,
                reason: other.to_string(),
            }),
        }
    }
}

/// A stored command's result, read as a verdict on whether the effect happened.
pub(crate) fn expect_ok(
    host_id: &str,
    op: &'static str,
    result: CommandResult,
) -> std::result::Result<CommandResult, HostCallError> {
    match ResultCode::try_from(result.code) {
        Ok(ResultCode::Ok) => Ok(result),
        Ok(ResultCode::Rejected | ResultCode::Absent) => Err(HostCallError::Rejected {
            host_id: host_id.to_owned(),
            op,
            reason: result.reason,
        }),
        _ => Err(HostCallError::Uncertain {
            host_id: host_id.to_owned(),
            op,
            command_id: result.command_id,
            reason: if result.reason.is_empty() {
                "the host reported an uncertain outcome".to_owned()
            } else {
                result.reason
            },
        }),
    }
}

/// The run identity a command names on the wire.
pub(crate) fn run_of(assignment: &SpokeAssignment) -> Run {
    Run {
        execution_id: assignment.execution_id.clone(),
        run_id: assignment.run_id.clone(),
        assignment_generation: assignment.assignment_generation,
        spawn_token: assignment.spawn_token.clone(),
    }
}

/// A stable 64-bit digest of a request, so a retry of the same request reuses
/// its command id while a changed request gets a new one.
pub(crate) fn request_digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

/// Where an operation goes: to the host's spoke, or to the legacy transport the
/// run started on.
pub(crate) enum Route {
    Spoke(Box<SpokeAssignment>),
    Legacy(Arc<dyn HostAdapter>),
}

pub struct SpokeHostAdapter {
    host: Host,
    hub: Arc<Hub>,
    /// The assignment this adapter was created for, when it dispatches one.
    bound: Option<SpokeAssignment>,
    /// The provider of the transport older runs on this host still use.
    legacy: Option<Arc<dyn HostAdapterProvider>>,
}

impl SpokeHostAdapter {
    pub(crate) fn host_name(&self) -> &str {
        &self.host.id
    }

    /// Run blocking database work off the async executor.
    pub(crate) async fn db<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&WorkDb) -> Result<T> + Send + 'static,
    {
        let db = Arc::clone(&self.hub.db);
        tokio::task::spawn_blocking(move || work(&db))
            .await
            .context("work database task failed")?
    }

    /// A pid no spoke assignment names belongs to the legacy transport, or to
    /// no one: with no legacy transport the answer is "cannot probe", which no
    /// caller treats as death.
    pub(crate) async fn legacy_probe_pid(&self, pid: i64) -> Result<Option<bool>> {
        match &self.legacy {
            Some(provider) => {
                provider
                    .adapter_for(&self.host)
                    .await?
                    .probe_remote_worker_alive(pid)
                    .await
            }
            None => Ok(None),
        }
    }

    async fn legacy_adapter(&self, what: &str) -> Result<Arc<dyn HostAdapter>> {
        let provider = self.legacy.as_ref().with_context(|| {
            format!(
                "{what} on host {} names no spoke assignment and this adapter has no legacy transport; \
                 refusing to guess one",
                self.host.id
            )
        })?;
        provider.adapter_for(&self.host).await
    }

    /// The bound assignment as the database holds it now. It must still be
    /// open: a resolved attempt cannot start new work.
    pub(crate) async fn bound_open(&self) -> Result<SpokeAssignment> {
        let bound = self
            .bound
            .as_ref()
            .context("this operation needs an execution-bound spoke adapter")?;
        let (execution_id, generation) = (bound.execution_id.clone(), bound.assignment_generation);
        let current = self
            .db(move |db| db.spoke_assignment_for_execution(&execution_id))
            .await?
            .filter(|current| current.assignment_generation == generation && current.host_id == self.host.id)
            .context("the execution's spoke assignment is gone")?;
        if current.resolved {
            bail!(
                "assignment {}#{} is already resolved; reserve a new one",
                current.execution_id,
                current.assignment_generation
            );
        }
        Ok(current)
    }

    pub(crate) async fn route_lease(&self, lease_id: &str) -> Result<Route> {
        let (host, lease) = (self.host.id.clone(), lease_id.to_owned());
        match self.db(move |db| db.spoke_assignment_for_lease(&host, &lease)).await? {
            Some(assignment) => Ok(Route::Spoke(Box::new(assignment))),
            None => Ok(Route::Legacy(self.legacy_adapter("a lease operation").await?)),
        }
    }

    /// Cube names a workspace's directory after its id, so the last path
    /// component addresses it. The path is display-only; the host resolves the
    /// id in its own lease registry.
    pub(crate) async fn route_path(&self, path: &Path) -> Result<Route> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("workspace path {} names no workspace", path.display()))?
            .to_owned();
        let host = self.host.id.clone();
        match self
            .db(move |db| db.open_spoke_assignment_for_workspace(&host, &name))
            .await?
        {
            Some(assignment) => Ok(Route::Spoke(Box::new(assignment))),
            None => Ok(Route::Legacy(self.legacy_adapter("a workspace operation").await?)),
        }
    }

    /// Recovery operations that need no workspace stay addressable after the
    /// lease is released, so they resolve the execution's latest assignment.
    pub(crate) async fn route_execution(&self, execution_id: &str) -> Result<Route> {
        let (host, execution) = (self.host.id.clone(), execution_id.to_owned());
        let found = self
            .db(move |db| db.spoke_assignment_for_execution(&execution))
            .await?
            .filter(|assignment| assignment.host_id == host);
        match found {
            Some(assignment) => Ok(Route::Spoke(Box::new(assignment))),
            None => Ok(Route::Legacy(self.legacy_adapter("a recovery operation").await?)),
        }
    }

    /// Whether host-scoped operations (repo ensure, listings) use the spoke.
    /// Read at call time, not captured when the adapter was made: the host's
    /// transport can change under a long-lived adapter. The spoke serves them
    /// when new work goes there, and also while any spoke assignment is still
    /// open on the host, because those workspaces exist only in the spoke's
    /// registry and an SSH listing would not see them.
    async fn host_scoped_uses_spoke(&self) -> Result<bool> {
        if self.bound.is_some() {
            return Ok(true);
        }
        let host = self.host.id.clone();
        self.db(move |db| {
            Ok(db.host_transport(&host)? == HostTransport::SpokeReady || db.host_has_open_spoke_assignment(&host)?)
        })
        .await
    }

    /// The assignment an execution-addressed control request acts on.
    pub(crate) async fn control_target(&self, execution_id: &str) -> Result<Option<SpokeAssignment>> {
        let (host, execution) = (self.host.id.clone(), execution_id.to_owned());
        Ok(self
            .db(move |db| db.spoke_assignment_for_execution(&execution))
            .await?
            .filter(|assignment| assignment.host_id == host && !assignment.resolved))
    }
}

#[async_trait]
impl HostAdapter for SpokeHostAdapter {
    fn host_id(&self) -> &str {
        &self.host.id
    }

    /// No local subprocess runs on the hub for a spoke, which is what this
    /// diagnostic names. (The legacy transport's own command line is reported by
    /// its own adapter, which callers reach only when routed to it.)
    fn command_repr(&self, _args: &[&str]) -> Option<(String, String)> {
        None
    }

    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        if !self.host_scoped_uses_spoke().await? {
            return self.legacy_adapter("a repo ensure").await?.ensure_repo(origin).await;
        }
        self.ws_ensure_repo(origin).await
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        if !self.host_scoped_uses_spoke().await? {
            return self.legacy_adapter("a repo listing").await?.list_repos().await;
        }
        self.ws_list_repos().await
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        if !self.host_scoped_uses_spoke().await? {
            return self
                .legacy_adapter("a workspace listing")
                .await?
                .list_workspaces()
                .await;
        }
        self.ws_list_workspaces().await
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        // A lease names no existing assignment, so only an execution-bound
        // adapter may acquire one; an unbound adapter never falls back to SSH.
        self.ws_lease(repo_id, task, prefer_workspace_id, allow_dirty, exclude_workspace_ids)
            .await
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        match self.route_lease(lease_id).await? {
            Route::Spoke(assignment) => self.ws_release(&assignment, lease_id, ReleaseMode::Normal).await,
            Route::Legacy(legacy) => legacy.release_workspace(lease_id).await,
        }
    }

    async fn release_workspace_with_reset(&self, lease_id: &str) -> Result<()> {
        match self.route_lease(lease_id).await? {
            // Cube's release resets the working copy on the host; the wire has
            // one release kind.
            Route::Spoke(assignment) => self.ws_release(&assignment, lease_id, ReleaseMode::Normal).await,
            Route::Legacy(legacy) => legacy.release_workspace_with_reset(lease_id).await,
        }
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        match self.route_lease(lease_id).await? {
            // The spoke heartbeats its own leases locally; the hub's sweep only
            // confirms the lease is still held by the assignment.
            Route::Spoke(assignment) => self.ws_confirm_lease(&assignment, lease_id).await,
            Route::Legacy(legacy) => legacy.heartbeat_lease(lease_id, ttl_seconds).await,
        }
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        match self.route_lease(lease_id).await? {
            Route::Spoke(assignment) => {
                self.ws_release(
                    &assignment,
                    lease_id,
                    ReleaseMode::Force(reason.unwrap_or_default().to_owned()),
                )
                .await
            }
            Route::Legacy(legacy) => legacy.force_release_lease(lease_id, reason).await,
        }
    }

    async fn create_change(&self, workspace_path: &Path, title: &str) -> Result<CubeChangeHandle> {
        match self.route_path(workspace_path).await? {
            Route::Spoke(assignment) => self.ws_create_change(&assignment, title).await,
            Route::Legacy(legacy) => legacy.create_change(workspace_path, title).await,
        }
    }

    async fn goto_workspace(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        match self.route_path(workspace_path).await? {
            Route::Spoke(assignment) => self.ws_goto(&assignment, Some(pr), "").await,
            Route::Legacy(legacy) => legacy.goto_workspace(workspace_path, pr).await,
        }
    }

    async fn goto_workspace_revision(&self, workspace_path: &Path, sha: &str) -> Result<()> {
        match self.route_path(workspace_path).await? {
            Route::Spoke(assignment) => self.ws_goto(&assignment, None, sha).await,
            Route::Legacy(legacy) => legacy.goto_workspace_revision(workspace_path, sha).await,
        }
    }

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        match self.route_path(workspace_path).await? {
            Route::Spoke(assignment) => self.ws_status(&assignment).await,
            Route::Legacy(legacy) => legacy.workspace_status(workspace_path).await,
        }
    }

    // ── Recovery ────────────────────────────────────────────────────────────

    async fn create_execution_bookmark(
        &self,
        workspace: &Path,
        execution_id: &str,
        predecessor: Option<&ExecutionBookmark>,
        inherited_base: Option<&str>,
    ) -> Result<ExecutionBookmark> {
        match self.route_path(workspace).await? {
            Route::Spoke(assignment) => {
                self.rec_create(&assignment, execution_id, predecessor, inherited_base)
                    .await
            }
            Route::Legacy(legacy) => {
                legacy
                    .create_execution_bookmark(workspace, execution_id, predecessor, inherited_base)
                    .await
            }
        }
    }

    async fn confirm_workspace_recovery(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        recorded: Option<&PreservedTip>,
        since_unix: Option<i64>,
    ) -> Result<PreserveOutcome> {
        self.confirm_workspace_recovery_from(record, workspace, recorded, since_unix, WorkspaceTipSource::Live)
            .await
    }

    async fn confirm_workspace_recovery_from(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        recorded: Option<&PreservedTip>,
        since_unix: Option<i64>,
        tip_source: WorkspaceTipSource,
    ) -> Result<PreserveOutcome> {
        match self.route_path(workspace).await? {
            Route::Spoke(assignment) => {
                self.rec_confirm(&assignment, record, recorded, since_unix, tip_source)
                    .await
            }
            Route::Legacy(legacy) => {
                legacy
                    .confirm_workspace_recovery_from(record, workspace, recorded, since_unix, tip_source)
                    .await
            }
        }
    }

    async fn restore_preserved_work(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        recorded: Option<&PreservedTip>,
    ) -> Result<Option<RestoreReport>> {
        match self.route_path(workspace).await? {
            Route::Spoke(assignment) => self.rec_restore_preserved(&assignment, record, recorded).await,
            Route::Legacy(legacy) => legacy.restore_preserved_work(record, workspace, recorded).await,
        }
    }

    async fn execution_bookmark_diff(&self, record: &ExecutionBookmark) -> Result<String> {
        match self.route_execution(&record.execution_id).await? {
            Route::Spoke(assignment) => self.rec_diff(&assignment, record).await,
            Route::Legacy(legacy) => legacy.execution_bookmark_diff(record).await,
        }
    }

    async fn restore_execution_bookmark(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        self_retry: bool,
    ) -> Result<bool> {
        match self.route_path(workspace).await? {
            Route::Spoke(assignment) => self.rec_restore(&assignment, record, self_retry).await,
            Route::Legacy(legacy) => legacy.restore_execution_bookmark(record, workspace, self_retry).await,
        }
    }

    async fn discard_orphaned_execution_baseline(&self, record: &ExecutionBookmark) -> Result<()> {
        match self.route_execution(&record.execution_id).await? {
            Route::Spoke(assignment) => self.rec_discard(&assignment, record).await,
            Route::Legacy(legacy) => legacy.discard_orphaned_execution_baseline(record).await,
        }
    }

    async fn restore_rebased_execution_bookmark(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        pr_bookmark: Option<&str>,
        base_branch: &str,
    ) -> Result<RestoreReport> {
        match self.route_path(workspace).await? {
            Route::Spoke(assignment) => {
                self.rec_restore_rebased(&assignment, record, pr_bookmark, base_branch)
                    .await
            }
            Route::Legacy(legacy) => {
                legacy
                    .restore_rebased_execution_bookmark(record, workspace, pr_bookmark, base_branch)
                    .await
            }
        }
    }

    async fn prepare_recovery_pr(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        repo_id: &str,
        pr: u64,
    ) -> Result<bool> {
        match self.route_path(workspace).await? {
            Route::Spoke(assignment) => {
                // Whether the PR is still open is hub-side GitHub policy; only
                // the pointer-preserving positioning crosses the wire.
                if !self.recovery_pr_is_open(repo_id, pr).await? {
                    return Ok(false);
                }
                self.rec_prepare_pr(&assignment, record, pr).await?;
                Ok(true)
            }
            Route::Legacy(legacy) => legacy.prepare_recovery_pr(record, workspace, repo_id, pr).await,
        }
    }

    async fn start_fresh_workspace(&self, workspace: &Path) -> Result<String> {
        match self.route_path(workspace).await? {
            Route::Spoke(assignment) => {
                self.rec_commit(&assignment, WorkspaceKind::StartFresh, "start_fresh")
                    .await
            }
            Route::Legacy(legacy) => legacy.start_fresh_workspace(workspace).await,
        }
    }

    async fn positioned_parent(&self, workspace: &Path) -> Result<String> {
        match self.route_path(workspace).await? {
            Route::Spoke(assignment) => {
                self.rec_commit(&assignment, WorkspaceKind::PositionedParent, "positioned_parent")
                    .await
            }
            Route::Legacy(legacy) => legacy.positioned_parent(workspace).await,
        }
    }

    async fn release_execution_bookmark_heads(
        &self,
        record: &ExecutionBookmark,
        release_names: bool,
    ) -> Result<Vec<(String, String)>> {
        match self.route_execution(&record.execution_id).await? {
            Route::Spoke(assignment) => self.rec_release_heads(&assignment, record, release_names).await,
            Route::Legacy(legacy) => legacy.release_execution_bookmark_heads(record, release_names).await,
        }
    }

    // ── Spawn, observation and control ──────────────────────────────────────

    async fn spawn_worker(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        self.spawn(worker_id, execution, work_item, workspace_path, cube_change_id)
            .await
    }

    async fn read_transcript_tail_bytes(&self, path: &str, max_bytes: u64) -> Result<Option<String>> {
        // A spoke run's transcript is materialized on the hub by run identity;
        // its recorded path is a path on the host and is never opened here.
        if self.bound.is_some() {
            bail!(
                "the transcript of a spoke worker on host {} is read from hub custody, not by path",
                self.host.id
            );
        }
        self.legacy_adapter("a transcript read")
            .await?
            .read_transcript_tail_bytes(path, max_bytes)
            .await
    }

    async fn probe_remote_worker_alive(&self, remote_pid: i64) -> Result<Option<bool>> {
        self.probe_pid_alive(remote_pid).await
    }

    // ── Operations the spoke has no wire form for ───────────────────────────
    //
    // Each routes by the same key as the rest: a run with a spoke assignment is
    // answered for the spoke, anything else is the legacy transport's to
    // answer. None falls back to the trait default, which would answer "not
    // applicable" for a run the legacy transport could in fact serve.

    async fn preserved_work_unpublished(&self, record: &ExecutionBookmark, recorded: &PreservedTip) -> Result<bool> {
        match self.route_execution(&record.execution_id).await? {
            Route::Spoke(assignment) => self.rec_preserved_unpublished(&assignment, record, recorded).await,
            Route::Legacy(legacy) => legacy.preserved_work_unpublished(record, recorded).await,
        }
    }

    async fn prune_execution_bookmark(
        &self,
        record: &ExecutionBookmark,
        vouchers: &[Voucher],
        trunk: &str,
    ) -> Result<PruneOutcome> {
        match self.route_execution(&record.execution_id).await? {
            Route::Spoke(assignment) => self.rec_prune(&assignment, record, vouchers, trunk).await,
            Route::Legacy(legacy) => legacy.prune_execution_bookmark(record, vouchers, trunk).await,
        }
    }

    async fn probe_initial_input_alive(&self, workspace: &str, run_id: &str) -> Result<Option<bool>> {
        match self.route_path(Path::new(workspace)).await? {
            // The spoke owns its workers' launch input; the hub has no evidence
            // of its own, and silence is never proof of death.
            Route::Spoke(_) => Ok(None),
            Route::Legacy(legacy) => legacy.probe_initial_input_alive(workspace, run_id).await,
        }
    }

    async fn cleanup_initial_input(&self, workspace: &str, run_id: &str) -> Result<()> {
        match self.route_path(Path::new(workspace)).await? {
            Route::Spoke(_) => Ok(()),
            Route::Legacy(legacy) => legacy.cleanup_initial_input(workspace, run_id).await,
        }
    }

    async fn reattach_events_forward(&self, run_id: &str, engine_events_socket: &str) -> Result<bool> {
        match self.route_execution(run_id).await? {
            // A spoke dials the hub itself; there is no forward to re-open.
            Route::Spoke(_) => Ok(false),
            Route::Legacy(legacy) => legacy.reattach_events_forward(run_id, engine_events_socket).await,
        }
    }

    async fn probe_workspace_occupancy(&self, workspace: &Path) -> Result<Occupancy> {
        match self.route_path(workspace).await? {
            // Unknown keeps the lease; the hub cannot see another machine's processes.
            Route::Spoke(_) => Ok(Occupancy::Unknown),
            Route::Legacy(legacy) => legacy.probe_workspace_occupancy(workspace).await,
        }
    }

    async fn read_worker_log_tail(&self, workspace_path: &str, max_bytes: u64) -> Result<Option<String>> {
        match self.route_path(Path::new(workspace_path)).await? {
            Route::Spoke(_) => Ok(None),
            Route::Legacy(legacy) => legacy.read_worker_log_tail(workspace_path, max_bytes).await,
        }
    }

    async fn collect_structured_output(
        &self,
        execution_id: &str,
        kind: StructuredOutputKind,
        destination: &Path,
    ) -> Result<CollectOutcome> {
        match self.route_execution(execution_id).await? {
            // A spoke run's artifacts reach the hub through its journal.
            Route::Spoke(_) => Ok(CollectOutcome::NotAvailable),
            Route::Legacy(legacy) => legacy.collect_structured_output(execution_id, kind, destination).await,
        }
    }

    async fn deliver_worker_input(
        &self,
        execution_id: &str,
        kind: WorkerInputKind,
        text: &str,
    ) -> Result<ControlOutcome> {
        self.ctl_deliver(execution_id, kind, text).await
    }

    async fn interrupt_worker(&self, execution_id: &str) -> Result<ControlOutcome> {
        self.ctl_interrupt(execution_id).await
    }

    async fn stop_worker(&self, execution_id: &str, grace: Duration) -> Result<ControlOutcome> {
        self.ctl_stop(execution_id, grace).await
    }

    async fn observe_worker(&self, execution_id: &str) -> Result<Option<WorkerPane>> {
        self.ctl_observe(execution_id).await
    }
}

/// Routes hosts whose transport is the spoke to [`SpokeHostAdapter`] and every
/// other host to the provider it wraps.
///
/// New work goes to the spoke only when the host's transport is `spoke_ready`;
/// runs that already hold a spoke assignment stay on it regardless. The `local`
/// host is never routed here: migrating it is a separate step.
pub struct SpokeHostAdapterProvider {
    inner: Arc<dyn HostAdapterProvider>,
    hub: Arc<Hub>,
}

impl SpokeHostAdapterProvider {
    pub fn new(
        inner: Arc<dyn HostAdapterProvider>,
        streams: StreamManager,
        db: Arc<WorkDb>,
        composer: Arc<dyn SpawnComposer>,
        config: SpokeAdapterConfig,
    ) -> Self {
        Self {
            inner,
            hub: Arc::new(
                Hub::builder()
                    .streams(streams)
                    .outbox(WorkDbOutbox::new(Arc::clone(&db)))
                    .db(db)
                    .composer(composer)
                    .config(config)
                    .build(),
            ),
        }
    }

    /// The command outbox to give the stream manager, so the manager and the
    /// adapter store commands in the same database.
    pub fn outbox(&self) -> WorkDbOutbox {
        self.hub.outbox.clone()
    }

    fn adapter(&self, host: &Host, bound: Option<SpokeAssignment>, legacy: bool) -> Arc<dyn HostAdapter> {
        Arc::new(SpokeHostAdapter {
            host: host.clone(),
            hub: Arc::clone(&self.hub),
            bound,
            legacy: legacy.then(|| Arc::clone(&self.inner)),
        })
    }

    async fn transport(&self, host: &Host) -> Result<HostTransport> {
        let db = Arc::clone(&self.hub.db);
        let id = host.id.clone();
        tokio::task::spawn_blocking(move || db.host_transport(&id))
            .await
            .context("work database task failed")?
    }
}

#[async_trait]
impl HostAdapterProvider for SpokeHostAdapterProvider {
    async fn adapter_for(&self, host: &Host) -> Result<Arc<dyn HostAdapter>> {
        if host.id == boss_protocol::LOCAL_HOST_ID {
            return self.inner.adapter_for(host).await;
        }
        match self.transport(host).await? {
            HostTransport::LegacySsh => self.inner.adapter_for(host).await,
            // Runs started over either transport can address this host, and
            // each operation finds its own by the lease or workspace it names.
            HostTransport::SpokePending | HostTransport::SpokeReady => Ok(self.adapter(host, None, true)),
        }
    }

    async fn adapter_for_execution(&self, host: &Host, execution_id: &str) -> Result<Arc<dyn HostAdapter>> {
        let db = Arc::clone(&self.hub.db);
        let (host_id, execution) = (host.id.clone(), execution_id.to_owned());
        let transport = self.transport(host).await?;
        let local = host.id == boss_protocol::LOCAL_HOST_ID;
        let reserved = tokio::task::spawn_blocking(move || -> Result<Option<SpokeAssignment>> {
            // Ownership is settled before the destination is chosen, whatever
            // the destination is: an unresolved assignment on another host pins
            // the execution there, so the local host, an SSH host and a host
            // whose spoke is still pending all refuse it, not just a ready spoke.
            db.ensure_spoke_ownership(&execution, &host_id)?;
            if local {
                return Ok(None);
            }
            if transport == HostTransport::SpokeReady {
                return db.reserve_spoke_assignment(&execution, &host_id).map(Some);
            }
            // Not ready for new spoke work: only a run that already holds an
            // assignment here keeps using it.
            Ok(db
                .spoke_assignment_for_execution(&execution)?
                .filter(|assignment| assignment.host_id == host_id && !assignment.resolved))
        })
        .await
        .context("work database task failed")??;
        match reserved {
            Some(assignment) => Ok(self.adapter(host, Some(assignment), false)),
            None => self.inner.adapter_for_execution(host, execution_id).await,
        }
    }
}
