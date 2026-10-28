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
    /// Operator/engine stop; the pane has been (or is being) reaped.
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

/// Recovery-bookmark confirmation. Production reads the engine record.
pub trait BookmarkGate: Send + Sync {
    fn status(&self, execution: &WorkExecution) -> BookmarkStatus;
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

/// Map a terminal execution's status onto a [`TerminalCause`].
pub fn cause_for(execution: &WorkExecution) -> TerminalCause {
    match execution.status {
        ExecutionStatus::Completed => TerminalCause::ProvenCompleted,
        ExecutionStatus::Failed if execution.started_at.is_none() => TerminalCause::ProvenNeverStarted,
        ExecutionStatus::Failed | ExecutionStatus::Cancelled => TerminalCause::ProvenStopped,
        ExecutionStatus::Abandoned | ExecutionStatus::Orphaned => TerminalCause::Inferred,
        _ => TerminalCause::Inferred,
    }
}

/// Confirm the engine-created recovery bookmark was recorded. A missing
/// record means this run never preserved work (pre-start, or a fixture);
/// resume does not read the originating workspace either way.
pub struct DbBookmarkGate<'a> {
    pub db: &'a WorkDb,
}

impl BookmarkGate for DbBookmarkGate<'_> {
    fn status(&self, execution: &WorkExecution) -> BookmarkStatus {
        match self.db.execution_bookmark_optional(&execution.id) {
            Ok(Some(_)) => BookmarkStatus::Confirmed,
            Ok(None) => BookmarkStatus::NotRequired,
            Err(err) => BookmarkStatus::Unconfirmed(format!("{err:#}")),
        }
    }
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
/// called before the DB columns are cleared; a cube error keeps the
/// columns (restoring them if a caller already nulled them) and files
/// attention. Errors are never swallowed.
#[allow(clippy::too_many_arguments)] // policy tuple: row, lease, cause, pane, occupancy, bookmark, cube future
pub async fn apply_release(
    db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    cause: TerminalCause,
    pane: PaneStatus,
    occupancy: &dyn WorkspaceOccupancy,
    bookmark: &dyn BookmarkGate,
    do_release: impl Future<Output = Result<()>>,
) -> LeaseReleaseOutcome {
    let bookmark_status = bookmark.status(execution);
    let occupancy_status = match cause {
        TerminalCause::Inferred => match execution.workspace_path.as_deref() {
            Some(path) => occupancy.occupancy(Path::new(path)),
            None => Occupancy::Unknown,
        },
        _ => Occupancy::Empty,
    };
    match decide(cause, &bookmark_status, pane, occupancy_status) {
        LeaseReleaseDecision::Keep(reason) => {
            // A completion path may already have nulled the columns. Put them
            // back so the leftover-lease sweep can retry rather than waiting
            // on cube's TTL with no recorded lease.
            if let Err(restore_err) = db.restore_execution_workspace(
                &execution.id,
                lease_id,
                execution.cube_workspace_id.as_deref(),
                execution.workspace_path.as_deref(),
            ) {
                tracing::error!(
                    execution_id = %execution.id,
                    lease_id,
                    error = %format!("{restore_err:#}"),
                    "failed to restore lease columns after keeping the lease",
                );
            }
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
        LeaseReleaseDecision::Release => match do_release.await {
            Ok(()) => {
                if let Err(err) = db.clear_execution_workspace(&execution.id) {
                    tracing::error!(
                        execution_id = %execution.id,
                        lease_id,
                        error = %format!("{err:#}"),
                        "cube released the workspace but clearing lease columns failed",
                    );
                }
                tracing::info!(
                    execution_id = %execution.id,
                    lease_id,
                    cause = cause.label(),
                    "workspace lease released",
                );
                LeaseReleaseOutcome::Released
            }
            Err(err) => {
                let error = format!("{err:#}");
                if let Err(restore_err) = db.restore_execution_workspace(
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
        },
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

/// Pre-start helper: the worker never started, so occupancy is not scanned.
pub async fn release_lease_after_proven_never_started(
    work_db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    do_release: impl Future<Output = Result<()>>,
) -> LeaseReleaseOutcome {
    apply_release(
        work_db,
        execution,
        lease_id,
        TerminalCause::ProvenNeverStarted,
        PaneStatus::Gone,
        &EmptyOccupancy,
        &DbBookmarkGate { db: work_db },
        do_release,
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
        TerminalCause::Inferred,
        pane,
        &HostOccupancy,
        &DbBookmarkGate { db: work_db },
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
    let root = match workspace_path.canonicalize() {
        Ok(root) => root,
        Err(_) => return Occupancy::Empty,
    };
    match process_cwds() {
        Ok(cwds) => {
            let self_pid = std::process::id();
            if cwds
                .iter()
                .any(|(pid, cwd)| *pid != self_pid && path_is_inside(cwd, &root))
            {
                Occupancy::Occupied
            } else {
                Occupancy::Empty
            }
        }
        Err(err) => {
            tracing::warn!(
                workspace = %workspace_path.display(),
                error = %format!("{err:#}"),
                "workspace occupancy probe failed; treating occupancy as unknown",
            );
            Occupancy::Unknown
        }
    }
}

fn path_is_inside(cwd: &Path, workspace: &Path) -> bool {
    cwd == workspace || cwd.starts_with(workspace)
}

fn process_cwds() -> std::io::Result<Vec<(u32, PathBuf)>> {
    #[cfg(target_os = "macos")]
    {
        let mut out = Vec::new();
        for pid in crate::libproc::list_pids()? {
            match crate::libproc::proc_cwd(pid) {
                Ok(cwd) => {
                    if let Ok(canonical) = cwd.canonicalize() {
                        out.push((pid as u32, canonical));
                    }
                }
                Err(_) => {
                    // ESRCH, EPERM on another user's process, zombies: skip.
                    // Workers in this workspace run as the engine's user.
                }
            }
        }
        Ok(out)
    }
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        let proc = std::fs::read_dir("/proc")?;
        for entry in proc {
            let entry = entry?;
            let pid = match entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) {
                Some(pid) => pid,
                None => continue,
            };
            match std::fs::read_link(entry.path().join("cwd")) {
                Ok(cwd) => {
                    if let Ok(canonical) = cwd.canonicalize() {
                        out.push((pid, canonical));
                    }
                }
                Err(_) => {}
            }
        }
        Ok(out)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(std::io::Error::other(
            "workspace occupancy probe is not supported on this OS",
        ))
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
    let mut outcome = EndedRunLeaseSweepOutcome::default();
    let candidates = match work_db.list_terminal_executions_holding_lease() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "ended-run lease sweep: candidate query failed");
            return outcome;
        }
    };
    for execution in candidates {
        let Some(lease_id) = execution.cube_lease_id.clone() else {
            continue;
        };
        let pane = if live_states.is_run_live(&execution.id) {
            PaneStatus::Present
        } else {
            PaneStatus::Gone
        };
        let result = apply_release(
            work_db,
            &execution,
            &lease_id,
            cause_for(&execution),
            pane,
            &HostOccupancy,
            &DbBookmarkGate { db: work_db },
            cube_client.release_workspace_with_reset(&lease_id),
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
mod tests {
    use super::*;
    use crate::test_support::*;
    use boss_engine_recovery::execution_bookmark::ExecutionBookmark;
    use boss_protocol::{CreateExecutionInput, ExecutionKind};
    use std::sync::Mutex;

    struct SweepCube;
    crate::stub_cube_client! { SweepCube {
        async fn release_workspace(&self, _: &str) -> Result<()> { Ok(()) }
        async fn release_workspace_with_reset(&self, _: &str) -> Result<()> { Ok(()) }
    } }

    struct FixedOccupancy(Occupancy);
    impl WorkspaceOccupancy for FixedOccupancy {
        fn occupancy(&self, _: &Path) -> Occupancy {
            self.0
        }
    }

    struct FixedBookmark(BookmarkStatus);
    impl BookmarkGate for FixedBookmark {
        fn status(&self, _: &WorkExecution) -> BookmarkStatus {
            self.0.clone()
        }
    }

    fn decide_release(cause: TerminalCause) -> LeaseReleaseDecision {
        decide(cause, &BookmarkStatus::NotRequired, PaneStatus::Gone, Occupancy::Empty)
    }

    #[test]
    fn proven_completed_releases() {
        assert_eq!(
            decide_release(TerminalCause::ProvenCompleted),
            LeaseReleaseDecision::Release
        );
    }

    #[test]
    fn proven_never_started_releases() {
        assert_eq!(
            decide_release(TerminalCause::ProvenNeverStarted),
            LeaseReleaseDecision::Release
        );
    }

    #[test]
    fn proven_stopped_releases_when_pane_gone() {
        assert_eq!(
            decide_release(TerminalCause::ProvenStopped),
            LeaseReleaseDecision::Release
        );
    }

    #[test]
    fn proven_stopped_keeps_when_pane_present() {
        assert_eq!(
            decide(
                TerminalCause::ProvenStopped,
                &BookmarkStatus::NotRequired,
                PaneStatus::Present,
                Occupancy::Empty,
            ),
            LeaseReleaseDecision::Keep(KeepReason::PaneStillPresent)
        );
    }

    #[test]
    fn inferred_releases_when_pane_gone_and_empty() {
        assert_eq!(decide_release(TerminalCause::Inferred), LeaseReleaseDecision::Release);
    }

    #[test]
    fn inferred_keeps_when_a_process_is_in_the_workspace() {
        assert_eq!(
            decide(
                TerminalCause::Inferred,
                &BookmarkStatus::NotRequired,
                PaneStatus::Gone,
                Occupancy::Occupied,
            ),
            LeaseReleaseDecision::Keep(KeepReason::ProcessInWorkspace)
        );
    }

    #[test]
    fn inferred_keeps_when_occupancy_is_unknown() {
        assert_eq!(
            decide(
                TerminalCause::Inferred,
                &BookmarkStatus::NotRequired,
                PaneStatus::Gone,
                Occupancy::Unknown,
            ),
            LeaseReleaseDecision::Keep(KeepReason::OccupancyUnconfirmed)
        );
    }

    #[test]
    fn bookmark_unconfirmed_keeps_even_for_proven_completion() {
        assert_eq!(
            decide(
                TerminalCause::ProvenCompleted,
                &BookmarkStatus::Unconfirmed("jj bookmark missing".into()),
                PaneStatus::Gone,
                Occupancy::Empty,
            ),
            LeaseReleaseDecision::Keep(KeepReason::BookmarkUnconfirmed)
        );
    }

    fn seed_terminal(db: &WorkDb, status: ExecutionStatus, lease: &str) -> WorkExecution {
        let product = create_test_product(db);
        let chore = create_test_chore(db, product.id.clone(), "lease-release");
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(status)
                .cube_lease_id(lease)
                .cube_workspace_id("ws-1")
                .workspace_path("/tmp/ws-lease-release")
                .build(),
        )
        .unwrap()
    }

    async fn apply_with(
        db: &WorkDb,
        execution: &WorkExecution,
        cause: TerminalCause,
        occupancy: Occupancy,
        bookmark: BookmarkStatus,
        release: Result<()>,
    ) -> LeaseReleaseOutcome {
        let lease = execution.cube_lease_id.clone().unwrap();
        apply_release(
            db,
            execution,
            &lease,
            cause,
            PaneStatus::Gone,
            &FixedOccupancy(occupancy),
            &FixedBookmark(bookmark),
            async { release },
        )
        .await
    }

    #[tokio::test]
    async fn apply_releases_completed_and_clears_columns() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-done");
        let outcome = apply_with(
            &db,
            &execution,
            TerminalCause::ProvenCompleted,
            Occupancy::Empty,
            BookmarkStatus::Confirmed,
            Ok(()),
        )
        .await;
        assert_eq!(outcome, LeaseReleaseOutcome::Released);
        let after = db.get_execution(&execution.id).unwrap();
        assert!(after.cube_lease_id.is_none());
        assert!(after.workspace_path.is_none());
    }

    #[tokio::test]
    async fn apply_releases_never_started_failure() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Failed, "lease-prestart");
        let outcome = apply_with(
            &db,
            &execution,
            TerminalCause::ProvenNeverStarted,
            Occupancy::Occupied,
            BookmarkStatus::NotRequired,
            Ok(()),
        )
        .await;
        assert_eq!(outcome, LeaseReleaseOutcome::Released);
        assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
    }

    #[tokio::test]
    async fn apply_releases_cancelled_when_pane_gone() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Cancelled, "lease-stop");
        let outcome = apply_with(
            &db,
            &execution,
            TerminalCause::ProvenStopped,
            Occupancy::Empty,
            BookmarkStatus::NotRequired,
            Ok(()),
        )
        .await;
        assert_eq!(outcome, LeaseReleaseOutcome::Released);
    }

    #[tokio::test]
    async fn apply_keeps_inferred_when_a_process_is_in_the_workspace() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Orphaned, "lease-orphan");
        let outcome = apply_with(
            &db,
            &execution,
            TerminalCause::Inferred,
            Occupancy::Occupied,
            BookmarkStatus::NotRequired,
            Ok(()),
        )
        .await;
        assert_eq!(
            outcome,
            LeaseReleaseOutcome::Kept {
                reason: KeepReason::ProcessInWorkspace
            }
        );
        assert_eq!(
            db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
            Some("lease-orphan")
        );
    }

    #[tokio::test]
    async fn apply_keeps_and_files_attention_when_bookmark_is_unconfirmed() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-bm");
        let released = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&released);
        let outcome = apply_release(
            &db,
            &execution,
            "lease-bm",
            TerminalCause::ProvenCompleted,
            PaneStatus::Gone,
            &FixedOccupancy(Occupancy::Empty),
            &FixedBookmark(BookmarkStatus::Unconfirmed("bookmark write failed".into())),
            async move {
                *flag.lock().unwrap() = true;
                Ok(())
            },
        )
        .await;
        assert_eq!(
            outcome,
            LeaseReleaseOutcome::Kept {
                reason: KeepReason::BookmarkUnconfirmed
            }
        );
        assert!(
            !*released.lock().unwrap(),
            "cube must not be called when the bookmark is unconfirmed"
        );
        assert_eq!(
            db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
            Some("lease-bm")
        );
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(
            items
                .iter()
                .any(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND && item.status == "open")
        );
    }

    #[tokio::test]
    async fn apply_keeps_lease_and_files_attention_when_cube_release_fails() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-fail");
        let outcome = apply_with(
            &db,
            &execution,
            TerminalCause::ProvenCompleted,
            Occupancy::Empty,
            BookmarkStatus::NotRequired,
            Err(anyhow::anyhow!("cube unavailable")),
        )
        .await;
        assert!(matches!(outcome, LeaseReleaseOutcome::CubeFailed { .. }));
        assert_eq!(
            db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
            Some("lease-fail")
        );
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(items.iter().any(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND));
    }

    #[tokio::test]
    async fn apply_keep_restores_columns_when_bookmark_is_unconfirmed_after_clear() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-keep-restore");
        db.clear_execution_workspace(&execution.id).unwrap();
        let cleared = db.get_execution(&execution.id).unwrap();
        let outcome = apply_release(
            &db,
            &cleared,
            "lease-keep-restore",
            TerminalCause::ProvenCompleted,
            PaneStatus::Gone,
            &FixedOccupancy(Occupancy::Empty),
            &FixedBookmark(BookmarkStatus::Unconfirmed("bookmark write failed".into())),
            async { Ok(()) },
        )
        .await;
        assert_eq!(
            outcome,
            LeaseReleaseOutcome::Kept {
                reason: KeepReason::BookmarkUnconfirmed
            }
        );
        assert_eq!(
            db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
            Some("lease-keep-restore")
        );
    }

    #[tokio::test]
    async fn apply_restores_columns_when_caller_had_already_cleared_them() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-restore");
        db.clear_execution_workspace(&execution.id).unwrap();
        let cleared = db.get_execution(&execution.id).unwrap();
        assert!(cleared.cube_lease_id.is_none());
        let outcome = apply_release(
            &db,
            &cleared,
            "lease-restore",
            TerminalCause::ProvenCompleted,
            PaneStatus::Gone,
            &FixedOccupancy(Occupancy::Empty),
            &FixedBookmark(BookmarkStatus::NotRequired),
            async { Err(anyhow::anyhow!("cube timeout")) },
        )
        .await;
        assert!(matches!(outcome, LeaseReleaseOutcome::CubeFailed { .. }));
        assert_eq!(
            db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
            Some("lease-restore")
        );
    }

    #[tokio::test]
    async fn sweep_releases_a_completed_row_still_holding_a_lease() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-sweep");
        let live = LiveWorkerStateRegistry::new();
        let outcome = run_one_pass(&db, &SweepCube, &live).await;
        assert_eq!(outcome.released, 1);
        assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
    }

    #[tokio::test]
    async fn sweep_keeps_orphaned_row_when_occupancy_is_occupied() {
        let (_dir, db) = open_db();
        let dir = tempfile::TempDir::new().unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "occupied-orphan");
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Orphaned)
                    .cube_lease_id("lease-occ")
                    .workspace_path(dir.path().display().to_string())
                    .build(),
            )
            .unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        // HostOccupancy is what the sweep uses; give the child a moment to
        // exist so the cwd probe can see it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let outcome = run_one_pass(&db, &SweepCube, &LiveWorkerStateRegistry::new()).await;
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(outcome.released, 0);
        assert_eq!(
            db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
            Some("lease-occ")
        );
    }

    #[test]
    fn db_bookmark_gate_treats_a_recorded_bookmark_as_confirmed() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-rec");
        db.record_execution_bookmark(&ExecutionBookmark {
            execution_id: execution.id.clone(),
            repo_path: PathBuf::from("/tmp/repo"),
            host_id: "local".to_owned(),
            base_commit: None,
        })
        .unwrap();
        assert_eq!(DbBookmarkGate { db: &db }.status(&execution), BookmarkStatus::Confirmed);
    }

    #[test]
    fn db_bookmark_gate_treats_a_missing_record_as_not_required() {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Failed, "lease-none");
        assert_eq!(
            DbBookmarkGate { db: &db }.status(&execution),
            BookmarkStatus::NotRequired
        );
    }

    #[test]
    fn cause_for_maps_terminal_statuses() {
        let (_dir, db) = open_db();
        assert_eq!(
            cause_for(&seed_terminal(&db, ExecutionStatus::Completed, "a")),
            TerminalCause::ProvenCompleted
        );
        assert_eq!(
            cause_for(&seed_terminal(&db, ExecutionStatus::Failed, "b")),
            TerminalCause::ProvenNeverStarted
        );
        assert_eq!(
            cause_for(&seed_terminal(&db, ExecutionStatus::Cancelled, "c")),
            TerminalCause::ProvenStopped
        );
        assert_eq!(
            cause_for(&seed_terminal(&db, ExecutionStatus::Orphaned, "d")),
            TerminalCause::Inferred
        );
        assert_eq!(
            cause_for(&seed_terminal(&db, ExecutionStatus::Abandoned, "e")),
            TerminalCause::Inferred
        );
    }

    #[test]
    fn host_occupancy_sees_a_child_with_cwd_in_the_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let seen = HostOccupancy.occupancy(dir.path());
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(seen, Occupancy::Occupied);
    }

    #[test]
    fn host_occupancy_of_an_empty_unique_dir_is_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(HostOccupancy.occupancy(dir.path()), Occupancy::Empty);
    }
}
