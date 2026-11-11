//! The hub's host adapter for workers that live on a spoke.
//!
//! [`SpokeHostAdapter`] implements the same [`HostAdapter`] the SSH and local
//! adapters do, but every operation is a typed command on the host's one
//! persistent stream instead of a shell line. Three rules shape it:
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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use boss_engine_recovery::execution_bookmark::{
    ExecutionBookmark, PreserveOutcome, PreservedScope, PreservedTip, RestoreReport, WorkspaceTipSource,
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
    #[builder(default = Duration::from_secs(360))]
    pub spawn_deadline: Duration,
    /// Bound for turn evidence after the host reports delivery.
    #[builder(default = Duration::from_secs(180))]
    pub prompt_timeout: Duration,
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
    pub(crate) signals: Arc<dyn boss_execution_contracts::DriverSignals>,
    pub(crate) config: SpokeAdapterConfig,
    #[builder(skip)]
    nonce: AtomicU64,
    /// One lock per `(host, command id)` that [`Hub::send_stable`] is working
    /// on, so two overlapping calls for the same stable request cannot both
    /// decide nothing was stored.
    #[builder(skip)]
    stable_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Hub {
    pub(crate) fn stable_callers(&self) -> usize {
        self.stable_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            // The map's own handle is not a caller.
            .map(|lock| Arc::strong_count(lock) - 1)
            .sum()
    }

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
    ///
    /// Calls for the same `(host, command id)` are serialized from the read of
    /// the stored state through the send, so a second overlapping call sees what
    /// the first stored (or its settled result) instead of building a command of
    /// its own that the outbox would refuse as a duplicate.
    pub(crate) async fn send_stable(
        &self,
        host_id: &str,
        op: &'static str,
        command_id: String,
        run: Option<Run>,
        body: command::Body,
        deadline: Duration,
    ) -> Result<CommandResult, HostCallError> {
        let key = format!("{host_id}\0{command_id}");
        let lock = Arc::clone(
            self.stable_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(key.clone())
                .or_default(),
        );
        let guard = lock.lock().await;
        let outcome = self
            .send_stable_locked(host_id, op, command_id, run, body, deadline)
            .await;
        drop(guard);
        let mut locks = self
            .stable_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Only the map and this call hold it: nobody is waiting, so forget it.
        if Arc::strong_count(&lock) == 2 {
            locks.remove(&key);
        }
        outcome
    }

    /// What an earlier attempt left behind for `command_id`: the stored command
    /// and, once it settled, its result. Failing to read either is not evidence
    /// that nothing was stored, so it is reported as uncertain, never as unsent.
    async fn stored_state(
        &self,
        host_id: &str,
        op: &'static str,
        command_id: &str,
    ) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>), HostCallError> {
        let uncertain = |reason: String| HostCallError::Uncertain {
            host_id: host_id.to_owned(),
            op,
            command_id: command_id.to_owned(),
            reason,
        };
        let (db_host, id) = (host_id.to_owned(), command_id.to_owned());
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || -> Result<_> {
            let wire = db.host_command_wire(&db_host, &id)?;
            let result = match wire {
                Some(_) => db.host_command_result(&db_host, &id)?,
                None => None,
            };
            Ok((wire, result))
        })
        .await
        .map_err(|error| uncertain(format!("work database task failed: {error}")))?
        .map_err(|error| uncertain(format!("reading the stored command failed: {error:#}")))
    }

    async fn send_stable_locked(
        &self,
        host_id: &str,
        op: &'static str,
        command_id: String,
        run: Option<Run>,
        body: command::Body,
        deadline: Duration,
    ) -> Result<CommandResult, HostCallError> {
        let uncertain = |reason: String| HostCallError::Uncertain {
            host_id: host_id.to_owned(),
            op,
            command_id: command_id.clone(),
            reason,
        };
        let settled_result = |settled: Vec<u8>| {
            CommandResult::decode(settled.as_slice())
                .map_err(|error| uncertain(format!("the stored result cannot be decoded: {error}")))
        };
        let (command, existed) = match self.stored_state(host_id, op, &command_id).await? {
            // A settled command answers from its stored result even with no
            // live stream.
            (Some(_), Some(settled)) => return settled_result(settled),
            (Some(wire), None) => {
                let mut command = Command::decode(wire.as_slice())
                    .map_err(|error| uncertain(format!("the stored command cannot be decoded: {error}")))?;
                command.sequence = 0;
                (command, true)
            }
            (None, _) => (self.new_command(&command_id, run, body), false),
        };
        match self.send_command(host_id, op, command, deadline).await {
            // An earlier connection may already have executed the stored
            // command, so a retry that cannot reach the host does not make it
            // "never sent".
            Err(HostCallError::NotSent { reason, .. }) if existed => Err(uncertain(format!(
                "an earlier attempt stored this command and it may have run; this retry could not be sent: {reason}"
            ))),
            // A refusal of a command believed new is only believable if nothing
            // was stored meanwhile (another hub task, or a stream that stored it
            // before failing): look again, and treat a failed look as uncertain.
            Err(HostCallError::NotSent { reason, .. }) => match self.stored_state(host_id, op, &command_id).await? {
                (None, _) => Err(HostCallError::NotSent {
                    host_id: host_id.to_owned(),
                    op,
                    reason,
                }),
                (Some(_), Some(settled)) => settled_result(settled),
                (Some(_), None) => Err(uncertain(format!(
                    "this command was stored while it was being sent and may have run: {reason}"
                ))),
            },
            other => other,
        }
    }

    /// Serialize selection through completion so a concurrent sweep cannot
    /// prune a result while another caller is about to retry that attempt.
    pub(crate) async fn send_probe(
        &self,
        host_id: &str,
        op: &'static str,
        base: String,
        run: Run,
        body: command::Body,
        deadline: Duration,
    ) -> Result<CommandResult> {
        // Separate from the command-id lock, including the first (base) id.
        let key = format!("probe\0{host_id}\0{base}");
        let lock = Arc::clone(
            self.stable_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(key.clone())
                .or_default(),
        );
        let guard = lock.lock().await;
        let outcome = async {
            let db = Arc::clone(&self.db);
            let host = host_id.to_owned();
            let id = tokio::task::spawn_blocking(move || db.refresh_host_probe(&host, &base))
                .await
                .context("work database task failed")??;
            Ok(self.send_stable(host_id, op, id, Some(run), body, deadline).await?)
        }
        .await;
        drop(guard);
        let mut locks = self
            .stable_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::strong_count(&lock) == 2 {
            locks.remove(&key);
        }
        outcome
    }

    /// The id of the current attempt at a repeatable request. Attempts are
    /// `base`, `base-a1`, `base-a2`, ... and the current one is the first not
    /// settled in a way `superseded` calls final: an unsettled attempt is reused
    /// (its late result still answers it), a settled-usable one is returned as
    /// stored, and anything else moves on to a new attempt. The chain is read
    /// from the stored commands themselves, so it survives a restart.
    pub(crate) async fn current_attempt(
        &self,
        host_id: &str,
        base: &str,
        superseded: fn(&CommandResult) -> bool,
    ) -> Result<String> {
        let db = Arc::clone(&self.db);
        let (host, base) = (host_id.to_owned(), base.to_owned());
        tokio::task::spawn_blocking(move || -> Result<String> {
            let mut attempt = 0_u32;
            loop {
                let id = if attempt == 0 {
                    base.clone()
                } else {
                    format!("{base}-a{attempt}")
                };
                if let Some(bytes) = db.host_command_result(&host, &id)?
                    && superseded(&CommandResult::decode(bytes.as_slice())?)
                {
                    attempt += 1;
                    continue;
                }
                return Ok(id);
            }
        })
        .await
        .context("work database task failed")?
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
        match self
            .streams
            .command(host_id, outbox::command_admission(&command), command, deadline)
            .await
        {
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

    /// Workspace ids are local to each user's cube store. Match the recorded
    /// full display path so a legacy user's identically named workspace cannot
    /// address a spoke lease.
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
            .filter(|assignment| assignment.display_path.as_deref().map(Path::new) == Some(path))
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

    /// Whether an execution still using the legacy transport holds a lease on
    /// this host. An adapter without a legacy transport has none to cover.
    async fn legacy_leases_remain(&self) -> Result<bool> {
        if self.legacy.is_none() {
            return Ok(false);
        }
        let host = self.host.id.clone();
        self.db(move |db| db.host_has_legacy_lease(&host)).await
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
        let mut repos = self.ws_list_repos().await?;
        if self.legacy_leases_remain().await? {
            for repo in self.legacy_adapter("a repo listing").await?.list_repos().await? {
                if !repos.iter().any(|known| known.repo_id == repo.repo_id) {
                    repos.push(repo);
                }
            }
        }
        Ok(repos)
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        if !self.host_scoped_uses_spoke().await? {
            return self
                .legacy_adapter("a workspace listing")
                .await?
                .list_workspaces()
                .await;
        }
        let mut workspaces = self.ws_list_workspaces().await?;
        // A legacy run's lease lives in the SSH user's store. Callers read a
        // lease missing from this listing as already released, so while any
        // remain the listing covers both transports.
        if self.legacy_leases_remain().await? {
            workspaces.extend(
                self.legacy_adapter("a workspace listing")
                    .await?
                    .list_workspaces()
                    .await?,
            );
        }
        Ok(workspaces)
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
            Route::Spoke(assignment) => self.ws_release(&assignment, lease_id, ReleaseMode::Reset).await,
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
        // its recorded path is a path on the host and is never opened here, or
        // sent over SSH. Only a transcript a legacy run on this host recorded is
        // the legacy transport's to read, whatever layout its driver used.
        let (host, owned) = (self.host.id.clone(), path.to_owned());
        if !self.db(move |db| db.transcript_is_legacy(&host, &owned)).await? {
            // `Ok(None)` would tell the caller the transcript is engine-local
            // and to open the path on the hub; a host-side path must not be.
            bail!(
                "the transcript at {path} belongs to a spoke run on host {}; it is not readable from the hub by path",
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

    async fn forget_preserved_references(
        &self,
        record: &ExecutionBookmark,
        scope: PreservedScope,
    ) -> Result<Vec<String>> {
        match self.route_execution(&record.execution_id).await? {
            Route::Spoke(assignment) => self.rec_forget_preserved(&assignment, record, scope).await,
            Route::Legacy(legacy) => legacy.forget_preserved_references(record, scope).await,
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

    async fn cleanup_execution_processes(&self, execution_id: &str, workspace: &Path) -> Result<Occupancy> {
        match self.route_execution(execution_id).await? {
            Route::Legacy(legacy) => legacy.cleanup_execution_processes(execution_id, workspace).await,
            Route::Spoke(_) => match self.ctl_stop(execution_id, Duration::ZERO).await? {
                ControlOutcome::Delivered(_) => Ok(Occupancy::Empty),
                outcome => {
                    let reason = format!("{outcome:?}");
                    // Only unowned processes remain: let the hub bound the wait.
                    if let Some(unowned) = boss_execution_contracts::UnownedOccupants::from_message(&reason) {
                        return Err(unowned.into());
                    }
                    anyhow::bail!("owning host could not confirm process cleanup: {reason}")
                }
            },
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
        signals: Arc<dyn boss_execution_contracts::DriverSignals>,
    ) -> Self {
        Self {
            inner,
            hub: Arc::new(
                Hub::builder()
                    .streams(streams)
                    .outbox(WorkDbOutbox::new(Arc::clone(&db)))
                    .db(db)
                    .composer(composer)
                    .signals(signals)
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

    /// How many stable-request calls are in flight, counting each one that is
    /// working on a request and each one waiting for it. Lets a test observe an
    /// overlap instead of sleeping for one.
    #[doc(hidden)]
    pub fn stable_callers(&self) -> usize {
        self.hub.stable_callers()
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
            // A host that once ran spokes keeps the ownership-aware adapter: the
            // recovery pointers of its resolved spoke runs live with the spoke,
            // not in the SSH user's store.
            HostTransport::LegacySsh => {
                let (db, id) = (Arc::clone(&self.hub.db), host.id.clone());
                let history = tokio::task::spawn_blocking(move || db.host_has_spoke_history(&id))
                    .await
                    .context("work database task failed")??;
                if history {
                    Ok(self.adapter(host, None, true))
                } else {
                    self.inner.adapter_for(host).await
                }
            }
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
            // Only a ready spoke takes new work, and never the local host. Every
            // other destination (local, SSH, a spoke still pending) reserves
            // nothing, but still takes ownership atomically: it records a
            // legacy claim in the transaction that retires an unused
            // reservation on any host (fencing that host's bound adapter out of
            // command admission), and one that may have started is refused, so
            // two hosts can never both run the execution.
            //
            // An execution a legacy destination already holds (a claim, or a
            // cube lease no spoke recorded) stays on that destination: another
            // host is refused, and the same host is not given a spoke
            // assignment that would run it a second time.
            if local || transport != HostTransport::SpokeReady || db.legacy_destination_host(&execution)?.is_some() {
                return db.claim_legacy_destination(&execution, &host_id, transport);
            }
            db.reserve_spoke_assignment(&execution, &host_id).map(Some)
        })
        .await
        .context("work database task failed")??;
        match reserved {
            Some(assignment) => Ok(self.adapter(host, Some(assignment), false)),
            None => self.inner.adapter_for_execution(host, execution_id).await,
        }
    }
}
