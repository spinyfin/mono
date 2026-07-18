//! Startup reap of cube workspace leases orphaned by a prior engine
//! instance's [`crate::conflict_ladder`] rung-1 attempt.
//!
//! ## Why this exists
//!
//! [`crate::ladder_lease_registry`] releases a rung-1 lease on a *clean*
//! engine shutdown (signal or RPC). It cannot help when the engine exits
//! without running that path at all — a crash, `kill -9`, or an OOM kill —
//! which is exactly what happened in the 2026-07-18 incident this closes:
//! the engine's shutdown RPC fired while a rung-1 attempt held cube lease
//! `1de73231-cde2-4cea-8a7d-19619815195a` on workspace `flunge-agent-035`,
//! and because the old process never reached
//! `try_mechanical_rungs`'s unconditional `release_workspace` call, the
//! lease sat orphaned until cube's 30-minute TTL sweep reclaimed it.
//!
//! This module runs once at the *next* engine startup and force-releases
//! any such orphan directly, instead of waiting out the TTL.
//!
//! ## Why "any rung-1 lease found at startup is orphaned" is a safe rule
//!
//! A rung-1 lease's entire lifetime — acquire, rebase, release — is one
//! in-process async call (see `conflict_ladder::try_mechanical_rungs`); it
//! is never persisted to the work-execution DB and never survives past
//! that single call within the process that created it. So a freshly
//! started engine, which by construction has not yet run any rung-1
//! attempt of its own, cannot be looking at a lease of its own making —
//! anything bearing the rung-1 task-label prefix
//! ([`crate::conflict_ladder::RUNG1_TASK_LABEL_PREFIX`]) here was left
//! behind by a *previous* engine instance that never got to release it.
//! That scoping is also what keeps this reap strictly to leases the
//! engine's own conflict-ladder code created — it never touches a worker
//! or human-held lease, both of which use their own, unrelated task
//! labels.
//!
//! As a belt-and-braces corroboration (not the sole signal — see the note
//! on `holder` below), each candidate's recorded lease holder is also
//! pid-probed via the same `kill(pid, 0)` check
//! [`crate::dead_pid_sweep`] uses; a lease whose holder still resolves to
//! a live process is left alone rather than force-released.
//!
//! Note: cube stamps `holder` from the *transient* `cube` CLI subprocess
//! that performed the lease call (`holder_identity()` in `tools/cube`),
//! not from the long-lived engine process that logically owns the lease —
//! that subprocess exits moments after the lease call returns regardless
//! of whether the owning engine is still alive. So an `Alive` holder here
//! is the genuinely rare/anomalous case (a lease call still literally in
//! flight, or a second engine instance concurrently sharing this cube
//! pool); `Dead`, `PermissionDenied`, or an unparseable/missing holder are
//! all treated as safe to reclaim.

use crate::coordinator::CubeClient;
use crate::dead_pid_sweep::{PidStatus, probe_pid};

/// Reason recorded on every force-release this sweep performs.
const REAP_REASON: &str =
    "conflict_ladder: startup reap — rung-1 lease orphaned by a prior engine instance (2026-07-18 incident)";

/// Force-release every cube workspace lease still bearing the rung-1
/// task-label prefix at engine startup (see the module doc comment for why
/// that alone is sufficient proof of orphaning). Best-effort and
/// non-fatal: a `list_workspaces` failure or an individual
/// `force_release_lease` failure is logged and does not block startup.
/// Returns the number of leases reclaimed.
pub async fn reap_orphaned_rung1_leases(cube_client: &dyn CubeClient) -> usize {
    let workspaces = match cube_client.list_workspaces().await {
        Ok(workspaces) => workspaces,
        Err(err) => {
            tracing::warn!(
                ?err,
                "conflict_ladder: startup reap failed to list cube workspaces; skipping (best-effort)",
            );
            return 0;
        }
    };

    let mut reaped = 0usize;
    for workspace in workspaces {
        let Some(task) = workspace.task.as_deref() else {
            continue;
        };
        if !task.starts_with(crate::conflict_ladder::RUNG1_TASK_LABEL_PREFIX) {
            continue;
        }
        let Some(lease_id) = workspace.lease_id.as_deref() else {
            continue;
        };

        if matches!(holder_pid_status(workspace.holder.as_deref()), PidStatus::Alive) {
            tracing::warn!(
                workspace_id = %workspace.workspace_id,
                lease_id,
                holder = ?workspace.holder,
                "conflict_ladder: startup reap found a rung-1 lease whose holder pid still looks alive; \
                 leaving it alone (possible concurrent engine instance)",
            );
            continue;
        }

        match cube_client.force_release_lease(lease_id, Some(REAP_REASON)).await {
            Ok(()) => {
                reaped += 1;
                tracing::warn!(
                    workspace_id = %workspace.workspace_id,
                    lease_id,
                    task,
                    "conflict_ladder: startup reap released a rung-1 lease orphaned by a prior engine instance",
                );
            }
            Err(err) => tracing::warn!(
                workspace_id = %workspace.workspace_id,
                lease_id,
                ?err,
                "conflict_ladder: startup reap failed to force-release an orphaned rung-1 lease (best-effort)",
            ),
        }
    }
    reaped
}

/// Parse the trailing `:<pid>` off a `holder` string of the shape
/// `"<user>@<host>:<pid>"` (see `holder_identity()` in `tools/cube`) and
/// probe it. Returns [`PidStatus::Unknown`]-equivalent handling (via the
/// caller's `matches!(..., PidStatus::Alive)` check) for a missing or
/// unparseable holder — treated as safe to reclaim, not as live.
fn holder_pid_status(holder: Option<&str>) -> PidStatus {
    let Some(holder) = holder else {
        return PidStatus::Unknown(std::io::Error::other("no holder recorded"));
    };
    let Some(pid_str) = holder.rsplit(':').next() else {
        return PidStatus::Unknown(std::io::Error::other("holder has no `:<pid>` suffix"));
    };
    match pid_str.parse::<i32>() {
        Ok(pid) => probe_pid(pid),
        Err(_) => PidStatus::Unknown(std::io::Error::other("holder pid segment did not parse as an integer")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Result;

    use super::*;
    use crate::coordinator::{CubeRepoSummary, CubeWorkspaceStatus};

    #[derive(Default)]
    struct RecordingCube {
        workspaces: Vec<CubeWorkspaceStatus>,
        force_released: Mutex<Vec<(String, Option<String>)>>,
        list_fails: bool,
    }

    crate::stub_cube_client! { RecordingCube {
        async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
            self.force_released
                .lock()
                .unwrap()
                .push((lease_id.to_owned(), reason.map(str::to_owned)));
            Ok(())
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            if self.list_fails {
                anyhow::bail!("simulated cube outage");
            }
            Ok(self.workspaces.clone())
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(vec![])
        }
    } }

    fn workspace(
        workspace_id: &str,
        lease_id: Option<&str>,
        task: Option<&str>,
        holder: Option<&str>,
    ) -> CubeWorkspaceStatus {
        CubeWorkspaceStatus::builder()
            .workspace_id(workspace_id)
            .workspace_path(std::path::PathBuf::from(format!("/tmp/{workspace_id}")))
            .state("leased")
            .maybe_lease_id(lease_id)
            .maybe_task(task)
            .maybe_holder(holder)
            .leased_at_epoch_s(1_700_000_000)
            .build()
    }

    /// A dead-pid holder (the overwhelmingly common case — see the module
    /// doc comment on why the transient `cube` CLI subprocess pid is
    /// always dead) with the rung-1 task-label prefix is reclaimed.
    #[tokio::test]
    async fn reaps_rung1_lease_with_dead_holder() {
        let cube = RecordingCube {
            workspaces: vec![workspace(
                "flunge-agent-035",
                Some("lease-1"),
                Some("conflict-ladder rung1 task_18c2e3766445e4f0_165"),
                Some(&format!("agent@host:{}", dead_pid())),
            )],
            ..Default::default()
        };

        let reaped = reap_orphaned_rung1_leases(&cube).await;

        assert_eq!(reaped, 1);
        let calls = cube.force_released.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "lease-1");
        assert!(calls[0].1.is_some());
    }

    /// A lease whose task label does not carry the rung-1 prefix (a worker
    /// or human lease) is never touched — this is the "do not reap
    /// arbitrary leases" scope guard.
    #[tokio::test]
    async fn ignores_non_ladder_lease() {
        let cube = RecordingCube {
            workspaces: vec![workspace(
                "mono-agent-001",
                Some("lease-2"),
                Some("some worker task"),
                Some(&format!("agent@host:{}", dead_pid())),
            )],
            ..Default::default()
        };

        let reaped = reap_orphaned_rung1_leases(&cube).await;

        assert_eq!(reaped, 0);
        assert!(cube.force_released.lock().unwrap().is_empty());
    }

    /// A rung-1 lease whose holder pid is still alive is left alone.
    #[tokio::test]
    async fn leaves_rung1_lease_with_live_holder() {
        let cube = RecordingCube {
            workspaces: vec![workspace(
                "flunge-agent-035",
                Some("lease-3"),
                Some("conflict-ladder rung1 task_x"),
                Some(&format!("agent@host:{}", std::process::id())),
            )],
            ..Default::default()
        };

        let reaped = reap_orphaned_rung1_leases(&cube).await;

        assert_eq!(reaped, 0);
        assert!(cube.force_released.lock().unwrap().is_empty());
    }

    /// A `list_workspaces` failure is best-effort: logged, not propagated.
    #[tokio::test]
    async fn list_failure_is_best_effort() {
        let cube = RecordingCube {
            list_fails: true,
            ..Default::default()
        };
        assert_eq!(reap_orphaned_rung1_leases(&cube).await, 0);
    }

    /// A pid guaranteed not to exist: spawn `true`, wait for it to exit,
    /// reuse its released pid. Mirrors the same trick used throughout
    /// `dead_pid_sweep`'s and `cube_lease_heartbeat`'s tests.
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let _ = child.wait();
        pid
    }
}
