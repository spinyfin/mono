//! Release a cube workspace lease when a run has provably ended, driven from
//! the engine host: the leftover-lease sweep and the host process scan that
//! supplies its occupancy evidence. The release policy itself (`decide`,
//! `apply_release`, the occupancy gate and teardown preservation) lives in
//! `boss_engine_completion::workspace_lease_release` and is re-exported here.
//!
//! Resume reads preserved work from `preserved/<execution>` bookmarks
//! in the shared jj store, never from the originating working copy.

use std::future::Future;
use std::path::Path;
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
    fn cleanup<'a>(
        &'a self,
        db: &'a WorkDb,
        execution: &'a WorkExecution,
    ) -> Option<std::pin::Pin<Box<dyn Future<Output = Result<Occupancy>> + Send + 'a>>> {
        Some(Box::pin(async move {
            let workspace = execution
                .workspace_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("workspace path missing"))?;
            let mut result = boss_worker_processes::cleanup("BOSS_RUN_ID", &execution.id, workspace).await?;
            // An occupant is live only when it resolves to a non-terminal
            // execution, or a terminal one whose recorded shell is still
            // alive. Unknown owners and terminal rows with no recorded pid
            // are unowned: signal nothing, and let the 10-minute bound
            // return the lease loudly.
            let mut stale_owners = Vec::new();
            for owner in result.owners.keys().cloned().collect::<Vec<_>>() {
                if owner == execution.id {
                    continue;
                }
                if owner_still_live(db, &owner).await? {
                    continue;
                }
                stale_owners.push(owner.clone());
                if let Ok(Some(pid)) = db.latest_local_shell_pid_for_execution(&owner)
                    && let Ok(pid) = u32::try_from(pid)
                    && !boss_worker_processes::shell_alive("BOSS_RUN_ID", &owner, pid).await?
                {
                    boss_worker_processes::cleanup("BOSS_RUN_ID", &owner, workspace).await?;
                }
            }
            if !result.owners.is_empty() {
                result = boss_worker_processes::cleanup("BOSS_RUN_ID", "", workspace).await?;
            }
            let mut live_owners = std::collections::BTreeMap::new();
            for (owner, pids) in &result.owners {
                if owner_still_live(db, owner).await? {
                    live_owners.insert(owner.clone(), pids.clone());
                } else if !stale_owners.contains(owner) {
                    stale_owners.push(owner.clone());
                }
            }
            anyhow::ensure!(
                live_owners.is_empty() && result.unknown.is_empty() && result.remaining.is_empty(),
                "workspace occupants {:?} (owners {:?}); unreadable processes {:?}",
                result.occupants,
                live_owners,
                result.unknown
            );
            if result.occupants.is_empty() {
                return Ok(Occupancy::Empty);
            }
            Err(anyhow::Error::from(boss_execution_contracts::UnownedOccupants {
                occupants: result.occupants,
            })
            .context(format!("unowned run ids {stale_owners:?}")))
        }))
    }

    fn occupancy(&self, workspace_path: &Path) -> Occupancy {
        if workspace_provably_absent(workspace_path) {
            return Occupancy::Empty;
        }
        match boss_worker_processes::scan(&workspace_path.to_string_lossy()) {
            Ok(result) if !result.unknown.is_empty() => Occupancy::Unknown,
            Ok(result) if result.occupants.is_empty() => Occupancy::Empty,
            Ok(_) => Occupancy::Occupied,
            Err(err) => {
                tracing::warn!(error = %err, "workspace occupancy scan failed");
                Occupancy::Unknown
            }
        }
    }
}

/// True when `owner` is a non-terminal execution, or a terminal one whose
/// recorded shell is still the process that carried that run's identity.
async fn owner_still_live(db: &WorkDb, owner: &str) -> Result<bool> {
    let Ok(old) = db.get_execution(owner) else {
        return Ok(false);
    };
    if !old.status.is_terminal() {
        return Ok(true);
    }
    let Ok(Some(pid)) = db.latest_local_shell_pid_for_execution(owner) else {
        return Ok(false);
    };
    let Ok(pid) = u32::try_from(pid) else {
        return Ok(false);
    };
    boss_worker_processes::shell_alive("BOSS_RUN_ID", owner, pid).await
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
    let occupancy: &dyn WorkspaceOccupancy = match occupancy {
        Some(probe) => probe,
        None => &HostOccupancy,
    };
    let plain = DbPreservation::new(work_db, adapter);
    let with_cube = cube.map(|cube| DbPreservation::new(work_db, adapter).with_cube(cube));
    let preservation: &dyn Preservation = match with_cube.as_ref() {
        Some(preservation) => preservation,
        None => &plain,
    };
    apply_release(
        work_db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane,
            occupancy,
            preservation,
        },
        do_release,
    )
    .await
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
        // that nothing is mid-spawn; the occupancy scan still runs again here.
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
/// to the pane / occupancy gates.
pub async fn run_one_pass(
    work_db: &WorkDb,
    cube_client: &dyn CubeClient,
    live_states: &LiveWorkerStateRegistry,
) -> EndedRunLeaseSweepOutcome {
    run_one_pass_with_occupancy(work_db, cube_client, live_states, || async { HostOccupancy }).await
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
            let registered = live_states.snapshot().iter().any(|state| state.run_id == execution.id);
            raise_attention(
                work_db,
                &execution,
                LEASE_UNRELEASED_ATTENTION_KIND,
                "Terminal worker still holds its workspace",
                &format!(
                    "Lease {lease_id} cannot be preserved safely: recorded shell PID {:?} still belongs to a terminal row (live registration: {registered}) and death is unconfirmed. Inspect this PID on the owning host and retry recovery; the workspace has not been reset.",
                    work_db
                        .latest_local_shell_pid_for_execution(&execution.id)
                        .ok()
                        .flatten()
                ),
            );
            outcome.held += 1;
            continue;
        }
        if occupancy.is_none() {
            occupancy = Some(capture.take().expect("one scan per pass")().await);
        }
        let occupancy = occupancy.as_ref().expect("scanned above");
        let result = apply_release(
            work_db,
            &execution,
            &lease_id,
            ReleaseEvidence {
                cause: TerminalCause::Inferred,
                pane,
                occupancy,
                preservation: &DbPreservation::new(work_db, None).with_cube(cube_client),
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
