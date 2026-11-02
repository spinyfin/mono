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
use boss_protocol::{CreateAttentionItemInput, WorkExecution};

use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;
use boss_execution_contracts::CubeClient;

/// Cadence for the leftover-lease sweep. Fires on boot so leases left by a
/// previous engine process return to the pool without waiting a day.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

pub use boss_engine_storage_values::attention_kinds::LEASE_UNRELEASED_ATTENTION_KIND;

pub use boss_engine_storage_values::attention_kinds::LEASE_RELEASE_FAILED_ATTENTION_KIND;

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

pub use boss_execution_contracts::Occupancy;

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

    /// True when the verdict was established on the host that owns a
    /// non-local row's workspace. A probe that only sees the engine host
    /// must not vouch for a remote workspace.
    fn probes_owning_host(&self) -> bool {
        false
    }
}

/// A verdict already established on a remote row's owning host.
pub struct OwningHostOccupancy(pub Occupancy);

impl WorkspaceOccupancy for OwningHostOccupancy {
    fn occupancy(&self, _: &Path) -> Occupancy {
        self.0
    }

    fn probes_owning_host(&self) -> bool {
        true
    }
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
    pub adapter: Option<&'a dyn boss_execution_contracts::HostAdapter>,
}

#[async_trait::async_trait]
impl BookmarkGate for DbBookmarkGate<'_> {
    async fn status(&self, execution: &WorkExecution) -> BookmarkStatus {
        let result: Result<()> = async {
            anyhow::ensure!(
                !has_open_bookmark_failure(self.db, &execution.id),
                "recovery creation or backup failed"
            );
            let record = self.db.execution_bookmark(&execution.id)?;
            let workspace = execution
                .workspace_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("workspace path unavailable for recovery confirmation"))?;
            // A lost workspace has no working copy to cover: only the
            // shared-store pointers can be confirmed.
            if self.adapter.is_none() && workspace_provably_absent(Path::new(workspace)) {
                anyhow::ensure!(
                    record.host_id == "local",
                    "recovery confirmation requires the owning host"
                );
                return boss_engine_recovery::execution_bookmark::confirm_pointers(
                    &boss_engine_recovery::execution_bookmark::LocalJj,
                    &record,
                )
                .await;
            }
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

/// An open recovery creation or backup failure for this execution. Dispatch
/// resolves it once a later attempt writes its own bookmark, so an open one
/// always belongs to the attempt whose lease is being released. Unreadable
/// attention state counts as a failure: it is never evidence of a clean run.
fn has_open_bookmark_failure(db: &WorkDb, execution_id: &str) -> bool {
    db.list_attention_items(execution_id).map_or(true, |items| {
        items.iter().any(|item| {
            item.status == "open"
                && matches!(
                    item.kind.as_str(),
                    crate::execution_bookmark_recovery::RECOVERY_FAILED
                        | crate::execution_bookmark_recovery::CREATE_FAILED
                )
        })
    })
}

/// The directory is positively known not to exist (not merely unreadable).
fn workspace_provably_absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(err) if err.kind() == std::io::ErrorKind::NotFound)
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
        Ok(row) => {
            if row.cube_lease_id.is_none() {
                resolve_unreleased_attention(db, &row.id);
            }
            return LeaseReleaseOutcome::NoLease;
        }
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
    let local = matches!(db.execution_host_id(&execution.id), Ok(None))
        || matches!(db.execution_host_id(&execution.id), Ok(Some(host)) if host == "local");
    let occupancy_status = match cause {
        TerminalCause::Inferred if !local && !occupancy.probes_owning_host() => Occupancy::Unknown,
        TerminalCause::Inferred => match execution.workspace_path.as_deref() {
            Some(path) => occupancy.occupancy(Path::new(path)),
            None => Occupancy::Unknown,
        },
        _ => Occupancy::Empty,
    };
    // Pane and occupancy verdicts come first. Confirming the bookmark
    // snapshots the working copy and may advance the recovery head in the
    // shared store, which must not race a worker that may still be running
    // jj in this workspace.
    let bookmark_status = if matches!(
        decide(cause, &BookmarkStatus::NotRequired, pane, occupancy_status),
        LeaseReleaseDecision::Release
    ) {
        bookmark.status(execution).await
    } else {
        BookmarkStatus::NotRequired
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
                    raise_attention(
                        db,
                        execution,
                        LEASE_RELEASE_FAILED_ATTENTION_KIND,
                        "Workspace lease held: cube release failed",
                        &format!(
                            "The cube workspace lease `{lease_id}` was not released: {error}. \
                             Every release gate had passed, so the lease stays recorded and the \
                             leftover-lease sweep retries it; cube's TTL remains the backstop."
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
            if result == LeaseReleaseOutcome::Released {
                if let Err(err) = db.finish_execution_workspace_release(execution) {
                    tracing::error!(error = %err, "failed to clear released workspace metadata");
                }
                resolve_unreleased_attention(db, &execution.id);
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
    adapter: &dyn boss_execution_contracts::HostAdapter,
) -> LeaseReleaseOutcome {
    apply_release(
        db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &NeverPositioned,
        },
        adapter.force_release_lease(lease_id, Some("refused occupied workspace; preserve checkout")),
    )
    .await
}

/// How far dispatch got before a pre-start failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreStartStage {
    /// The execution's recovery bookmark has not been written. The workspace
    /// holds only what cube placed there, so there is no worker work to lose.
    BeforeRecoveryBookmark,
    /// Bookmark creation was attempted or has run; its pointers must be
    /// confirmed before the workspace is returned.
    AtOrAfterRecoveryBookmark,
}

/// Only a failure that precedes any bookmark row may skip confirmation: a row
/// means pointers exist that the release must not strand, and an open
/// creation or backup failure means the bookmark write itself may have left
/// work behind that no row records. A lease retained from a previous attempt
/// therefore keeps its bookmark gate whatever stage the caller assumes.
fn never_positioned(db: &WorkDb, execution_id: &str, stage: PreStartStage) -> bool {
    stage == PreStartStage::BeforeRecoveryBookmark
        && matches!(db.execution_bookmark_optional(execution_id), Ok(None))
        && !has_open_bookmark_failure(db, execution_id)
}

struct NeverPositioned;

#[async_trait::async_trait]
impl BookmarkGate for NeverPositioned {
    async fn status(&self, _: &WorkExecution) -> BookmarkStatus {
        BookmarkStatus::NotRequired
    }
}

/// Pre-start helper: the worker never started, so occupancy is not scanned.
pub async fn release_lease_after_proven_never_started(
    work_db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    adapter: &dyn boss_execution_contracts::HostAdapter,
    stage: PreStartStage,
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
                if let Err(err) = work_db.finish_execution_workspace_release(execution) {
                    tracing::error!(
                        execution_id = %execution.id,
                        lease_id,
                        error = %format!("{err:#}"),
                        "failed to clear released workspace metadata",
                    );
                }
                resolve_unreleased_attention(work_db, &execution.id);
                LeaseReleaseOutcome::Released
            }
            Ok(false) => LeaseReleaseOutcome::NoLease,
            Err(err) => LeaseReleaseOutcome::CubeFailed {
                error: format!("{err:#}"),
            },
        };
    }
    let db_gate = DbBookmarkGate {
        db: work_db,
        adapter: Some(adapter),
    };
    let bookmark: &dyn BookmarkGate = if never_positioned(work_db, &execution.id, stage) {
        &NeverPositioned
    } else {
        &db_gate
    };
    apply_release(
        work_db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark,
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
    let snapshot = ProcessSnapshot::capture_async().await;
    apply_release(
        work_db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane,
            occupancy: &snapshot,
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
    raise_attention(db, execution, LEASE_UNRELEASED_ATTENTION_KIND, title, body);
}

fn raise_attention(db: &WorkDb, execution: &WorkExecution, kind: &str, title: &str, body: &str) {
    match db.list_attention_items(&execution.id) {
        Ok(items) if items.iter().any(|item| item.kind == kind && item.status == "open") => {
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
        kind: kind.to_owned(),
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

/// The lease these items describe is gone, so they no longer say anything true.
fn resolve_unreleased_attention(db: &WorkDb, execution_id: &str) {
    for kind in [LEASE_UNRELEASED_ATTENTION_KIND, LEASE_RELEASE_FAILED_ATTENTION_KIND] {
        if let Err(err) = db.resolve_attention_kind_for_execution(execution_id, kind) {
            tracing::warn!(
                execution_id,
                kind,
                error = %format!("{err:#}"),
                "could not resolve workspace-lease attention",
            );
        }
    }
}

fn occupancy_of(workspace_path: &Path) -> Occupancy {
    ProcessSnapshot::capture().occupancy(workspace_path)
}

const CAPTURE_ATTEMPTS: u32 = 5;
const CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(20);

struct ProcessSnapshot {
    cwds: Vec<(u32, PathBuf)>,
    complete: bool,
}

impl ProcessSnapshot {
    /// A process caught mid-fork/exec briefly refuses its cwd even to its
    /// own user, so an incomplete scan is retried a few times before it is
    /// reported as inconclusive.
    fn capture() -> Self {
        let mut snapshot = Self::capture_once();
        for _ in 1..CAPTURE_ATTEMPTS {
            if snapshot.complete {
                break;
            }
            std::thread::sleep(CAPTURE_RETRY_DELAY);
            snapshot = Self::capture_once();
        }
        snapshot
    }

    /// [`Self::capture`] for async callers: every enumeration runs on a
    /// blocking thread and the retry delay yields instead of parking a
    /// runtime worker.
    async fn capture_async() -> Self {
        let mut snapshot = Self::capture_once_blocking().await;
        for _ in 1..CAPTURE_ATTEMPTS {
            if snapshot.complete {
                break;
            }
            tokio::time::sleep(CAPTURE_RETRY_DELAY).await;
            snapshot = Self::capture_once_blocking().await;
        }
        snapshot
    }

    async fn capture_once_blocking() -> Self {
        tokio::task::spawn_blocking(Self::capture_once)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "workspace occupancy enumeration task failed");
                Self {
                    cwds: Vec::new(),
                    complete: false,
                }
            })
    }

    fn capture_once() -> Self {
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

    /// Record one process's cwd lookup. `owner_uid` is consulted only after
    /// a failed lookup: a process owned by another user is unreadable to a
    /// non-root engine and cannot be a workspace occupant, while a failure
    /// for a same-uid process (or one whose owner is unknown) is inconclusive.
    fn record(&mut self, pid: u32, cwd: std::io::Result<PathBuf>, owner_uid: impl FnOnce() -> Option<u32>) {
        match cwd {
            Ok(path) => match path.canonicalize() {
                Ok(path) => self.cwds.push((pid, path)),
                // A deleted cwd lies inside no existing workspace.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => self.complete = false,
            },
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => {}
            Err(_) => {
                // SAFETY: geteuid has no preconditions.
                let own_uid = unsafe { libc::geteuid() };
                if !owner_uid().is_some_and(|uid| uid != own_uid) {
                    self.complete = false;
                }
            }
        }
    }
}

impl WorkspaceOccupancy for ProcessSnapshot {
    fn occupancy(&self, workspace_path: &Path) -> Occupancy {
        // No process can have its cwd in a directory that does not exist.
        if workspace_provably_absent(workspace_path) {
            return Occupancy::Empty;
        }
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
        // Only the engine user's processes: the kernel refuses another
        // user's cwd to a non-root engine, and such a process cannot be a
        // workspace occupant. A failure for one of these is inconclusive.
        // SAFETY: geteuid has no preconditions.
        let own_uid = unsafe { libc::geteuid() };
        for pid in crate::libproc::list_pids_for_uid(own_uid)? {
            snapshot.record(pid as u32, crate::libproc::proc_cwd(pid), || Some(own_uid));
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
            // A zombie or exiting process has no cwd and cannot occupy anything,
            // though kill(0) still reports it alive.
            if std::fs::read_to_string(entry.path().join("status")).is_ok_and(|status| proc_state_is_gone(&status)) {
                continue;
            }
            let cwd = std::fs::read_link(entry.path().join("cwd"));
            // A failed lookup is inconclusive, unless kill(0) independently
            // confirms that the process is gone. A same-uid process that
            // refuses its cwd (non-dumpable, e.g. ssh-agent) is deliberately
            // inconclusive too: its cwd is unknown, so it is never assumed to
            // lie outside the workspace.
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
            snapshot.record(pid, cwd, || proc_uid(&entry.path()));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = snapshot;
        Err(std::io::Error::other("unsupported occupancy probe"))
    }
}

/// `State:` of `/proc/<pid>/status` is a zombie (`Z`) or dead (`X`).
#[cfg(any(target_os = "linux", test))]
fn proc_state_is_gone(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("State:"))
        .and_then(|state| state.trim().chars().next())
        .is_some_and(|state| matches!(state, 'Z' | 'X' | 'x'))
}

/// Effective uid from the `Uid:` line of `/proc/<pid>/status`.
#[cfg(target_os = "linux")]
fn proc_uid(proc_dir: &Path) -> Option<u32> {
    let status = std::fs::read_to_string(proc_dir.join("status")).ok()?;
    let line = status.lines().find_map(|line| line.strip_prefix("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn sweep_pane_status(db: &WorkDb, live: &LiveWorkerStateRegistry, execution: &WorkExecution) -> PaneStatus {
    if live.snapshot().iter().any(|state| state.run_id == execution.id) {
        return PaneStatus::Present;
    }
    // Cancelled and failed rows go through durable death evidence: a recorded
    // worker pid that is dead. A cancelled in-flight spawn (no pid recorded
    // yet) and a failed viewer abort (pid still alive) therefore stay held.
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
        // A row that never recorded a pid (a pre-start failure, or a spawn
        // that failed before its pane) is retried only after an earlier
        // release attempt got past every gate and then failed in cube. That
        // attempt ran after the spawn had settled, so it is the durable proof
        // that nothing is mid-spawn; the occupancy scan and bookmark gate
        // still run again here.
        Ok(None) if release_previously_failed_in_cube(db, &execution.id) => PaneStatus::Gone,
        _ => PaneStatus::Present,
    }
}

fn release_previously_failed_in_cube(db: &WorkDb, execution_id: &str) -> bool {
    db.list_attention_items(execution_id).is_ok_and(|items| {
        items
            .iter()
            .any(|item| item.kind == LEASE_RELEASE_FAILED_ATTENTION_KIND && item.status == "open")
    })
}

/// A row that never started a run holds only what cube placed in the
/// workspace, so an absent bookmark is not a gap; every other row must
/// confirm its recovery pointers.
fn sweep_bookmark_gate<'a>(
    db: &WorkDb,
    execution: &WorkExecution,
    confirming: &'a DbBookmarkGate<'a>,
) -> &'a dyn BookmarkGate {
    if execution.started_at.is_none() && never_positioned(db, &execution.id, PreStartStage::BeforeRecoveryBookmark) {
        &NeverPositioned
    } else {
        confirming
    }
}

/// Counts from one leftover-lease pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EndedRunLeaseSweepOutcome {
    pub released: usize,
    /// Rows without durable death evidence (no dead worker pid, or a live
    /// pane). Expected to persist across passes, so not activity.
    pub held: usize,
    /// Rows with death evidence whose release gates or cube call kept the lease.
    pub kept: usize,
}

impl crate::sweep_loop::SweepOutcome for EndedRunLeaseSweepOutcome {
    fn has_activity(&self) -> bool {
        self.released > 0
    }

    fn log(&self) {
        tracing::info!(
            released = self.released,
            kept = self.kept,
            held = self.held,
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
    run_one_pass_with_occupancy(work_db, cube_client, live_states, ProcessSnapshot::capture_async).await
}

async fn run_one_pass_with_occupancy<O: WorkspaceOccupancy, Fut: Future<Output = O>>(
    work_db: &WorkDb,
    cube_client: &dyn CubeClient,
    live_states: &LiveWorkerStateRegistry,
    capture: impl FnOnce() -> Fut,
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
            outcome.held += 1;
            continue;
        }
        if occupancy.is_none() {
            occupancy = Some(capture.take().expect("one scan per pass")().await);
        }
        let occupancy = occupancy.as_ref().expect("scanned above");
        let db_gate = DbBookmarkGate {
            db: work_db,
            adapter: None,
        };
        let result = apply_release(
            work_db,
            &execution,
            &lease_id,
            ReleaseEvidence {
                cause: TerminalCause::Inferred,
                pane,
                occupancy,
                bookmark: sweep_bookmark_gate(work_db, &execution, &db_gate),
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
