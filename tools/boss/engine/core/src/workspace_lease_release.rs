//! Release a cube workspace lease when a run has provably ended, driven from
//! the engine host: the leftover-lease sweep and the host process scan that
//! supplies its occupancy evidence. The release policy itself (`decide`,
//! `apply_release` and the bookmark/occupancy gates) lives in
//! `boss_engine_completion::workspace_lease_release` and is re-exported here.
//!
//! Resume reads preserved work from `preserved/<execution>` bookmarks
//! in the shared jj store, never from the originating working copy.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use boss_protocol::WorkExecution;

use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;
use boss_execution_contracts::CubeClient;

pub use boss_engine_completion::workspace_lease_release::*;

/// Cadence for the leftover-lease sweep. Fires on boot so leases left by a
/// previous engine process return to the pool without waiting a day.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Scan local processes for a cwd inside the workspace.
pub struct HostOccupancy;

impl WorkspaceOccupancy for HostOccupancy {
    fn occupancy(&self, workspace_path: &Path) -> Occupancy {
        occupancy_of(workspace_path)
    }
}

/// Inferred-death helper used by pane/pid/lost-workspace reconcilers.
/// Occupancy is scanned on the host; a live pane still mapped in
/// `live_states` is treated as present. Remote rows go through
/// [`release_remote_lease`] when `adapter` is the owning host; without an
/// adapter the lease stays held.
pub async fn release_lease_after_inferred_death(
    work_db: &WorkDb,
    execution: &WorkExecution,
    pane: PaneStatus,
    adapter: Option<&dyn boss_execution_contracts::HostAdapter>,
    cube: Option<&dyn CubeClient>,
    do_release: impl Future<Output = Result<()>>,
) -> LeaseReleaseOutcome {
    release_lease_after_inferred_death_with(work_db, execution, pane, adapter, None, cube, do_release).await
}

/// [`release_lease_after_inferred_death`] with an explicit occupancy probe.
/// `None` scans the host's process table; a caller that has to be
/// deterministic injects its own probe.
pub async fn release_lease_after_inferred_death_with(
    work_db: &WorkDb,
    execution: &WorkExecution,
    pane: PaneStatus,
    adapter: Option<&dyn boss_execution_contracts::HostAdapter>,
    occupancy: Option<&dyn WorkspaceOccupancy>,
    cube: Option<&dyn CubeClient>,
    do_release: impl Future<Output = Result<()>>,
) -> LeaseReleaseOutcome {
    let Some(lease_id) = execution.cube_lease_id.as_deref() else {
        return LeaseReleaseOutcome::NoLease;
    };
    let local = matches!(work_db.execution_host_id(&execution.id), Ok(None))
        || matches!(work_db.execution_host_id(&execution.id), Ok(Some(host)) if host == "local");
    if !local {
        return match adapter {
            Some(host) => release_remote_lease(work_db, host, &execution.id, pane, |_| do_release).await,
            None => LeaseReleaseOutcome::Kept {
                reason: KeepReason::OccupancyUnconfirmed,
            },
        };
    }
    let snapshot;
    let occupancy: &dyn WorkspaceOccupancy = match occupancy {
        Some(probe) => probe,
        None => {
            snapshot = ProcessSnapshot::capture_async().await;
            &snapshot
        }
    };
    let db_gate = match cube {
        Some(cube) => DbBookmarkGate::new(work_db, adapter).with_cube(cube),
        None => DbBookmarkGate::new(work_db, adapter),
    };
    let bookmark = sweep_bookmark_gate(work_db, execution, &db_gate);
    apply_release(
        work_db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane,
            occupancy,
            bookmark,
        },
        do_release,
    )
    .await
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
pub(crate) fn sweep_bookmark_gate<'a>(
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
        let db_gate = DbBookmarkGate::new(work_db, None).with_cube(cube_client);
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
