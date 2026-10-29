//! Release a cube workspace lease when a run has provably ended.
//!
//! Cube's 24-hour lease TTL is the backstop for two cases this module must
//! still honour: a recovery bookmark that did not write, and an inferred
//! death whose pane or process occupancy cannot be confirmed. Every other
//! terminal path releases the lease as soon as those gates pass, so a
//! finished run does not hold a 10–20 GiB workspace until TTL.
//!
//! Resume reads preserved work from `boss-recovery/<execution>` bookmarks
//! in the shared jj store, never from the originating working copy.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use boss_protocol::{CreateAttentionItemInput, ExecutionStatus, WorkExecution};

use crate::coordinator::CubeClient;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// Cadence for the leftover-lease sweep. Fires on boot so leases left by a
/// previous engine process return to the pool without waiting a day.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Attention kind filed when a lease is kept because a gate failed.
pub const LEASE_UNRELEASED_ATTENTION_KIND: &str = "workspace_lease_unreleased";

/// Why the run ended. Proven causes may release without an occupancy scan;
/// inferred death may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCause {
    /// Worker finished its turn and the completion path tore the pane down.
    ProvenCompleted,
    /// Pre-start failure: lease was acquired, no worker ever started.
    ProvenNeverStarted,
    /// Explicit stop; the pane and worker process have been confirmed torn down.
    ProvenStopped,
    /// Abandoned, orphaned, idle reap, or liveness timeout — death inferred.
    Inferred,
}

/// Whether a live worker pane still maps to this execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    Gone,
    Present,
}

/// Whether any process has its cwd inside the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    Empty,
    Occupied,
    /// Probe failed; inferred death must not release.
    Unknown,
}

/// Whether the recovery bookmark is confirmed written, when the run had work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookmarkStatus {
    NotRequired,
    Confirmed,
    Unconfirmed(String),
}

/// Why [`decide`] kept the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    BookmarkUnconfirmed,
    PaneStillPresent,
    ProcessInWorkspace,
    OccupancyUnconfirmed,
}

/// Policy result: release now, or keep the lease (cube TTL remains the backstop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseReleaseDecision {
    Release,
    Keep(KeepReason),
}

/// Outcome of attempting a cube release after [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseReleaseOutcome {
    Released,
    NoLease,
    Kept { reason: KeepReason },
    CubeFailed { error: String },
}

/// Workspace occupancy probe. Production walks process cwds; tests inject.
pub trait WorkspaceOccupancy: Send + Sync {
    fn occupancy(&self, workspace_path: &Path) -> Occupancy;
}

/// Recovery-bookmark confirmation on the execution's owning host.
#[async_trait::async_trait]
pub trait BookmarkGate: Send + Sync {
    async fn status(&self, execution: &WorkExecution) -> BookmarkStatus;
}

/// Pure policy. Callers still have to perform the cube release and DB clear.
pub fn decide(
    cause: TerminalCause,
    bookmark: &BookmarkStatus,
    pane: PaneStatus,
    occupancy: Occupancy,
) -> LeaseReleaseDecision {
    if matches!(bookmark, BookmarkStatus::Unconfirmed(_)) {
        return LeaseReleaseDecision::Keep(KeepReason::BookmarkUnconfirmed);
    }
    if pane == PaneStatus::Present {
        return LeaseReleaseDecision::Keep(KeepReason::PaneStillPresent);
    }
    match cause {
        TerminalCause::ProvenCompleted | TerminalCause::ProvenNeverStarted | TerminalCause::ProvenStopped => {
            LeaseReleaseDecision::Release
        }
        TerminalCause::Inferred => match occupancy {
            Occupancy::Empty => LeaseReleaseDecision::Release,
            Occupancy::Occupied => LeaseReleaseDecision::Keep(KeepReason::ProcessInWorkspace),
            Occupancy::Unknown => LeaseReleaseDecision::Keep(KeepReason::OccupancyUnconfirmed),
        },
    }
}

/// Confirm preserved work through jj on the owning host. A missing record
/// is never evidence of an empty workspace.
pub struct DbBookmarkGate<'a> {
    pub db: &'a WorkDb,
    pub adapter: Option<&'a dyn crate::host_adapter::HostAdapter>,
}

#[async_trait::async_trait]
impl BookmarkGate for DbBookmarkGate<'_> {
    async fn status(&self, execution: &WorkExecution) -> BookmarkStatus {
        let result: Result<()> = async {
            let failures = self.db.list_attention_items(&execution.id)?;
            anyhow::ensure!(
                !failures.iter().any(|item| item.status == "open"
                    && matches!(
                        item.kind.as_str(),
                        crate::execution_bookmark_recovery::RECOVERY_FAILED
                            | crate::execution_bookmark_recovery::CREATE_FAILED
                    )),
                "recovery creation or backup failed"
            );
            let record = self.db.execution_bookmark(&execution.id)?;
            let workspace = execution
                .workspace_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("workspace path unavailable for recovery confirmation"))?;
            if let Some(adapter) = self.adapter {
                adapter.confirm_workspace_recovery(&record, Path::new(workspace)).await
            } else {
                anyhow::ensure!(
                    record.host_id == "local",
                    "recovery confirmation requires the owning host"
                );
                boss_engine_recovery::execution_bookmark::confirm_workspace(
                    &boss_engine_recovery::execution_bookmark::LocalJj,
                    &record,
                    Path::new(workspace),
                )
                .await
            }
        }
        .await;
        match result {
            Ok(()) => BookmarkStatus::Confirmed,
            Err(err) => BookmarkStatus::Unconfirmed(format!("{err:#}")),
        }
    }
}

/// Evidence supplied by the caller, separately from lease identity.
pub struct ReleaseEvidence<'a> {
    pub cause: TerminalCause,
    pub pane: PaneStatus,
    pub occupancy: &'a dyn WorkspaceOccupancy,
    pub bookmark: &'a dyn BookmarkGate,
}

/// Scan local processes for a cwd inside the workspace.
pub struct HostOccupancy;

impl WorkspaceOccupancy for HostOccupancy {
    fn occupancy(&self, workspace_path: &Path) -> Occupancy {
        occupancy_of(workspace_path)
    }
}

/// Always-empty occupancy, for proven-death paths that do not scan.
pub struct EmptyOccupancy;

impl WorkspaceOccupancy for EmptyOccupancy {
    fn occupancy(&self, _: &Path) -> Occupancy {
        Occupancy::Empty
    }
}

/// Release `lease_id` for `execution` when [`decide`] says so. Cube is
/// claimed with a compare-and-clear before cube is called. Only the claim
/// owner may restore the lease after a genuine release failure.
pub async fn apply_release(
    db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    evidence: ReleaseEvidence<'_>,
    do_release: impl Future<Output = Result<()>>,
) -> LeaseReleaseOutcome {
    let current = match db.get_execution(&execution.id) {
        Ok(row) if row.cube_lease_id.as_deref() == Some(lease_id) => row,
        Ok(_) => return LeaseReleaseOutcome::NoLease,
        Err(err) => {
            return LeaseReleaseOutcome::CubeFailed {
                error: format!("{err:#}"),
            };
        }
    };
    let execution = &current;
    let ReleaseEvidence {
        cause,
        pane,
        occupancy,
        bookmark,
    } = evidence;
    let bookmark_status = bookmark.status(execution).await;
    let local = matches!(db.execution_host_id(&execution.id), Ok(None))
        || matches!(db.execution_host_id(&execution.id), Ok(Some(host)) if host == "local");
    let occupancy_status = match cause {
        TerminalCause::Inferred if !local => Occupancy::Unknown,
        TerminalCause::Inferred => match execution.workspace_path.as_deref() {
            Some(path) => occupancy.occupancy(Path::new(path)),
            None => Occupancy::Unknown,
        },
        _ => Occupancy::Empty,
    };
    match decide(cause, &bookmark_status, pane, occupancy_status) {
        LeaseReleaseDecision::Keep(reason) => {
            if matches!(reason, KeepReason::BookmarkUnconfirmed) {
                let detail = match &bookmark_status {
                    BookmarkStatus::Unconfirmed(msg) => msg.clone(),
                    _ => reason.label().to_owned(),
                };
                raise_unreleased_attention(
                    db,
                    execution,
                    "Workspace lease held: recovery bookmark unconfirmed",
                    &format!(
                        "The cube workspace lease `{lease_id}` was not released because the recovery bookmark \
                         for this run could not be confirmed. The lease stays held until cube's TTL. {detail}"
                    ),
                );
            }
            tracing::info!(
                execution_id = %execution.id,
                lease_id,
                reason = reason.label(),
                "workspace lease kept",
            );
            LeaseReleaseOutcome::Kept { reason }
        }
        LeaseReleaseDecision::Release => {
            match db.claim_execution_workspace_release(&execution.id, lease_id) {
                Ok(true) => {}
                Ok(false) => return LeaseReleaseOutcome::NoLease,
                Err(err) => {
                    return LeaseReleaseOutcome::CubeFailed {
                        error: format!("{err:#}"),
                    };
                }
            }
            let result = match do_release.await {
                Ok(()) => {
                    tracing::info!(
                        execution_id = %execution.id,
                        lease_id,
                        cause = cause.label(),
                        "workspace lease released",
                    );
                    LeaseReleaseOutcome::Released
                }
                Err(err)
                    if err.chain().any(|source| {
                        source
                            .downcast_ref::<crate::cube_commands::CubeCliError>()
                            .is_some_and(|error| error.exit_code == Some(5))
                    }) =>
                {
                    LeaseReleaseOutcome::Released
                }
                Err(err) => {
                    let error = format!("{err:#}");
                    if let Err(restore_err) = db.restore_claimed_execution_workspace(
                        &execution.id,
                        lease_id,
                        execution.cube_workspace_id.as_deref(),
                        execution.workspace_path.as_deref(),
                    ) {
                        tracing::error!(
                            execution_id = %execution.id,
                            lease_id,
                            error = %format!("{restore_err:#}"),
                            "failed to restore lease columns after cube release failure",
                        );
                    }
                    raise_unreleased_attention(
                        db,
                        execution,
                        "Workspace lease held: cube release failed",
                        &format!(
                            "The cube workspace lease `{lease_id}` was not released: {error}. \
                         The lease stays recorded so a later pass can retry; cube's TTL remains the backstop."
                        ),
                    );
                    tracing::error!(
                        execution_id = %execution.id,
                        lease_id,
                        error = %error,
                        "workspace lease release failed; keeping the lease",
                    );
                    LeaseReleaseOutcome::CubeFailed { error }
                }
            };
            if result == LeaseReleaseOutcome::Released
                && let Err(err) = db.finish_execution_workspace_release(execution)
            {
                tracing::error!(error = %err, "failed to clear released workspace metadata");
            }
            result
        }
    }
}

impl KeepReason {
    fn label(self) -> &'static str {
        match self {
            Self::BookmarkUnconfirmed => "bookmark_unconfirmed",
            Self::PaneStillPresent => "pane_still_present",
            Self::ProcessInWorkspace => "process_in_workspace",
            Self::OccupancyUnconfirmed => "occupancy_unconfirmed",
        }
    }
}

impl TerminalCause {
    fn label(self) -> &'static str {
        match self {
            Self::ProvenCompleted => "proven_completed",
            Self::ProvenNeverStarted => "proven_never_started",
            Self::ProvenStopped => "proven_stopped",
            Self::Inferred => "inferred",
        }
    }
}

/// The refused lease has not been positioned by this execution. Registry-only
/// release preserves all of the occupant's files and recovery pointers.
pub async fn release_refused_workspace(
    db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    adapter: &dyn crate::host_adapter::HostAdapter,
) -> LeaseReleaseOutcome {
    struct Unpositioned;
    #[async_trait::async_trait]
    impl BookmarkGate for Unpositioned {
        async fn status(&self, _: &WorkExecution) -> BookmarkStatus {
            BookmarkStatus::NotRequired
        }
    }
    apply_release(
        db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &Unpositioned,
        },
        adapter.force_release_lease(lease_id, Some("refused occupied workspace; preserve checkout")),
    )
    .await
}

/// Pre-start helper: the worker never started, so occupancy is not scanned.
pub async fn release_lease_after_proven_never_started(
    work_db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    adapter: &dyn crate::host_adapter::HostAdapter,
) -> LeaseReleaseOutcome {
    let current = match work_db.get_execution(&execution.id) {
        Ok(row) if row.cube_lease_id.as_deref() == Some(lease_id) => row,
        Ok(_) => return LeaseReleaseOutcome::NoLease,
        Err(err) => {
            return LeaseReleaseOutcome::CubeFailed {
                error: format!("{err:#}"),
            };
        }
    };
    let execution = &current;
    // A previous attempt's lease may have expired or completed release after
    // a timeout. Registry absence requires no destructive workspace operation.
    if let Ok(workspaces) = adapter.list_workspaces().await
        && !workspaces.iter().any(|ws| ws.lease_id.as_deref() == Some(lease_id))
    {
        return match work_db.claim_execution_workspace_release(&execution.id, lease_id) {
            Ok(true) => {
                let _ = work_db.finish_execution_workspace_release(execution);
                LeaseReleaseOutcome::Released
            }
            Ok(false) => LeaseReleaseOutcome::NoLease,
            Err(err) => LeaseReleaseOutcome::CubeFailed {
                error: format!("{err:#}"),
            },
        };
    }
    apply_release(
        work_db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate {
                db: work_db,
                adapter: Some(adapter),
            },
        },
        adapter.release_workspace(lease_id),
    )
    .await
}

/// Inferred-death helper used by pane/pid/lost-workspace reconcilers.
/// Occupancy is scanned on the host; a live pane still mapped in
/// `live_states` is treated as present.
pub async fn release_lease_after_inferred_death(
    work_db: &WorkDb,
    execution: &WorkExecution,
    pane: PaneStatus,
    do_release: impl Future<Output = Result<()>>,
) -> LeaseReleaseOutcome {
    let Some(lease_id) = execution.cube_lease_id.as_deref() else {
        return LeaseReleaseOutcome::NoLease;
    };
    apply_release(
        work_db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane,
            occupancy: &HostOccupancy,
            bookmark: &DbBookmarkGate {
                db: work_db,
                adapter: None,
            },
        },
        do_release,
    )
    .await
}

fn raise_unreleased_attention(db: &WorkDb, execution: &WorkExecution, title: &str, body: &str) {
    match db.list_attention_items(&execution.id) {
        Ok(items)
            if items
                .iter()
                .any(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND && item.status == "open") =>
        {
            return;
        }
        Ok(_) => {}
        Err(err) => {
            tracing::error!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "could not inspect workspace-lease attention",
            );
            return;
        }
    }
    if let Err(err) = db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(execution.id.clone()),
        work_item_id: None,
        kind: LEASE_UNRELEASED_ATTENTION_KIND.to_owned(),
        status: None,
        title: title.to_owned(),
        body_markdown: body.to_owned(),
        resolved_at: None,
    }) {
        tracing::error!(
            execution_id = %execution.id,
            error = %format!("{err:#}"),
            "could not file workspace-lease attention",
        );
    }
}

fn occupancy_of(workspace_path: &Path) -> Occupancy {
    ProcessSnapshot::capture().occupancy(workspace_path)
}

struct ProcessSnapshot {
    cwds: Vec<(u32, PathBuf)>,
    complete: bool,
}

impl ProcessSnapshot {
    fn capture() -> Self {
        let mut snapshot = Self {
            cwds: Vec::new(),
            complete: true,
        };
        if let Err(err) = process_cwds(&mut snapshot) {
            tracing::warn!(error = %err, "workspace occupancy enumeration failed");
            snapshot.complete = false;
        }
        snapshot
    }

    fn record(&mut self, pid: u32, cwd: std::io::Result<PathBuf>) {
        match cwd.and_then(|path| path.canonicalize()) {
            Ok(path) => self.cwds.push((pid, path)),
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => {}
            Err(_) => self.complete = false,
        }
    }
}

impl WorkspaceOccupancy for ProcessSnapshot {
    fn occupancy(&self, workspace_path: &Path) -> Occupancy {
        let root = match workspace_path.canonicalize() {
            Ok(root) => root,
            Err(_) => return Occupancy::Unknown,
        };
        if self
            .cwds
            .iter()
            .any(|(pid, cwd)| *pid != std::process::id() && path_is_inside(cwd, &root))
        {
            Occupancy::Occupied
        } else if self.complete {
            Occupancy::Empty
        } else {
            Occupancy::Unknown
        }
    }
}

fn path_is_inside(cwd: &Path, workspace: &Path) -> bool {
    cwd == workspace || cwd.starts_with(workspace)
}

fn process_cwds(snapshot: &mut ProcessSnapshot) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        for pid in crate::libproc::list_pids()? {
            snapshot.record(pid as u32, crate::libproc::proc_cwd(pid));
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
                continue;
            };
            let cwd = std::fs::read_link(entry.path().join("cwd"));
            // ENOENT is inconclusive (e.g. a zombie or an inaccessible cwd),
            // unless kill(0) independently confirms that the process is gone.
            if cwd.is_err()
                && i32::try_from(pid).ok().is_some_and(|pid| {
                    matches!(
                        crate::dead_pid_sweep::probe_pid(pid),
                        crate::dead_pid_sweep::PidStatus::Dead
                    )
                })
            {
                continue;
            }
            snapshot.record(pid, cwd);
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = snapshot;
        Err(std::io::Error::other("unsupported occupancy probe"))
    }
}

fn sweep_pane_status(db: &WorkDb, live: &LiveWorkerStateRegistry, execution: &WorkExecution) -> PaneStatus {
    if live.snapshot().iter().any(|state| state.run_id == execution.id) {
        return PaneStatus::Present;
    }
    // Cancel and failed-start teardown owns these leases. In particular, a
    // cancelled in-flight spawn and a failed viewer abort have no death proof.
    if matches!(execution.status, ExecutionStatus::Cancelled | ExecutionStatus::Failed) {
        return PaneStatus::Present;
    }
    match db.latest_local_shell_pid_for_execution(&execution.id) {
        Ok(Some(pid))
            if i32::try_from(pid).ok().is_some_and(|pid| {
                matches!(
                    crate::dead_pid_sweep::probe_pid(pid),
                    crate::dead_pid_sweep::PidStatus::Dead
                )
            }) =>
        {
            PaneStatus::Gone
        }
        _ => PaneStatus::Present,
    }
}

/// Counts from one leftover-lease pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EndedRunLeaseSweepOutcome {
    pub released: usize,
    pub kept: usize,
}

impl crate::sweep_loop::SweepOutcome for EndedRunLeaseSweepOutcome {
    fn has_activity(&self) -> bool {
        self.released > 0 || self.kept > 0
    }

    fn log(&self) {
        tracing::info!(
            released = self.released,
            kept = self.kept,
            "ended-run lease sweep: pass complete",
        );
    }
}

/// Spawn the leftover-lease sweep. Fires immediately on boot.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    cube_client: Arc<dyn CubeClient>,
    live_states: Arc<LiveWorkerStateRegistry>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let cube_client = Arc::clone(&cube_client);
        let live_states = Arc::clone(&live_states);
        async move { run_one_pass(work_db.as_ref(), cube_client.as_ref(), live_states.as_ref()).await }
    })
}

/// Release every terminal execution that still holds a cube lease, subject
/// to the bookmark / pane / occupancy gates.
pub async fn run_one_pass(
    work_db: &WorkDb,
    cube_client: &dyn CubeClient,
    live_states: &LiveWorkerStateRegistry,
) -> EndedRunLeaseSweepOutcome {
    run_one_pass_with_occupancy(work_db, cube_client, live_states, ProcessSnapshot::capture).await
}

async fn run_one_pass_with_occupancy<O: WorkspaceOccupancy>(
    work_db: &WorkDb,
    cube_client: &dyn CubeClient,
    live_states: &LiveWorkerStateRegistry,
    capture: impl FnOnce() -> O,
) -> EndedRunLeaseSweepOutcome {
    let mut outcome = EndedRunLeaseSweepOutcome::default();
    let candidates = match work_db.list_terminal_executions_holding_lease() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "ended-run lease sweep: candidate query failed");
            return outcome;
        }
    };
    let mut occupancy = None;
    let mut capture = Some(capture);
    for execution in candidates {
        let Some(lease_id) = execution.cube_lease_id.clone() else {
            continue;
        };
        // A terminal registry entry may still own a pane; absence from the
        // registry is not proof after a restart. Require durable pid death.
        let pane = sweep_pane_status(work_db, live_states, &execution);
        if pane == PaneStatus::Present {
            outcome.kept += 1;
            continue;
        }
        let occupancy = occupancy.get_or_insert_with(|| capture.take().expect("one scan per pass")());
        let result = apply_release(
            work_db,
            &execution,
            &lease_id,
            ReleaseEvidence {
                cause: TerminalCause::Inferred,
                pane,
                occupancy,
                bookmark: &DbBookmarkGate {
                    db: work_db,
                    adapter: None,
                },
            },
            cube_client.release_workspace(&lease_id),
        )
        .await;
        match result {
            LeaseReleaseOutcome::Released => outcome.released += 1,
            LeaseReleaseOutcome::Kept { .. } | LeaseReleaseOutcome::CubeFailed { .. } => outcome.kept += 1,
            LeaseReleaseOutcome::NoLease => {}
        }
    }
    outcome
}

#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;
