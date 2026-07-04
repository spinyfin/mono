//! Cross-host remote-lease reconciler: reap non-terminal remote runs whose
//! worker process is provably gone, and force-release their leaked cube
//! leases. The remote analogue of [`crate::lost_workspace_sweep`].
//!
//! ## Why this exists
//!
//! `waiting_human` is the normal post-spawn park state; a remote worker
//! stays there — cube lease and workspace retained — until its `Stop` hook
//! tunnels back and the completion handler transitions it out. A remote
//! worker that dies WITHOUT a `Stop` (it launched then crashed — the
//! anaplian failure-mode B — or was killed) leaves the row stuck forever:
//!
//! - No `Stop` → the completion handler never runs.
//! - Every existing reaper is LOCAL-only. `dead_pid_sweep` /
//!   `stale_worker_sweep` probe a local pid via `libc::kill`, and
//!   `lost_workspace_sweep` probes the local filesystem and explicitly
//!   skips `host_id != "local"`. A `.exists()` or `kill(pid, 0)` on the
//!   engine host says nothing about a worker on another machine.
//! - The cube-lease heartbeat sweep can't help either: it heartbeats via
//!   the LOCAL cube, so a remote lease is never refreshed OR reaped by it.
//!
//! So a dead remote worker strands two ways: its execution row blocks the
//! redundant-spawn guard (the work item shows "queued" forever — the
//! symptom the operator saw), and its cube lease strands a remote
//! workspace (and its multi-GB clone) as unreclaimable waste.
//!
//! ## What it does
//!
//! DB-driven (so it survives restart, unlike the registry-driven reapers)
//! over [`WorkDb::list_reattachable_remote_runs`] — active runs on a
//! non-local host whose execution is still non-terminal, which is exactly
//! the set of live-looking remote workers. For each it probes the remote
//! worker pid over the host's `ControlMaster` (`kill -0`). ONLY on POSITIVE
//! evidence of death (`Ok(Some(false))`) does it finalize the execution
//! through the terminal `mark_execution_orphaned` path, force-release the
//! cube lease on the REMOTE adapter (the correct cube), and emit a
//! `remote_lease_reconcile` event. A live worker (`Ok(Some(true))`), an
//! inconclusive probe (`Err` — the host is unreachable), or a run with no
//! recorded `remote_pid` is left ALONE: a host outage must never look like
//! proof of death, or it would mass-reap every live worker on that host.
//!
//! ## Cadence
//!
//! Runs every 60s and fires once immediately on boot (same pattern as the
//! other sweeps), so a dead remote worker clears quickly and pre-existing
//! strays clear on upgrade/restart without any hand-editing of the DB.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::ExecutionKind;

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::host_adapter::HostAdapter;
use crate::host_adapter::HostAdapterProvider;
use crate::work::{RemoteRunHandle, WorkDb};

/// Cadence for the periodic pass. Fires immediately on boot, then every
/// interval.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Counts from one pass; logged at `info` when any reaping occurred.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RemoteLeaseReconcileOutcome {
    /// Remote runs whose worker was provably dead → reaped + lease released.
    pub reaped: usize,
    /// Remote runs confirmed alive (left running).
    pub alive: usize,
    /// Remote runs we could not adjudicate — no recorded `remote_pid`, an
    /// inconclusive probe (host unreachable), or a host that has since been
    /// removed / whose adapter could not be built. Left ALONE.
    pub skipped: usize,
}

impl crate::sweep_loop::SweepOutcome for RemoteLeaseReconcileOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }

    fn log(&self) {
        tracing::info!(
            reaped = self.reaped,
            alive = self.alive,
            skipped = self.skipped,
            "remote-lease reconcile: pass complete",
        );
    }
}

/// Spawn a tokio task that runs a reconcile pass forever at `interval`,
/// firing immediately on spawn so pre-existing strays clear on boot.
pub fn spawn_loop(coordinator: Arc<ExecutionCoordinator>, interval: Duration) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let coordinator = Arc::clone(&coordinator);
        async move { coordinator.reconcile_remote_leases_once().await }
    })
}

/// Run one reconcile pass over every active remote run whose execution is
/// non-terminal. Pure over the [`HostAdapterProvider`] seam so it is
/// exercised in-process against a stub provider/adapter; the coordinator
/// binding ([`ExecutionCoordinator::reconcile_remote_leases_once`]) adds
/// the scheduler `kick` when anything was reaped.
pub async fn reconcile_remote_leases(
    work_db: &WorkDb,
    provider: &dyn HostAdapterProvider,
    dispatch_events: &dyn DispatchEventSink,
) -> RemoteLeaseReconcileOutcome {
    let mut outcome = RemoteLeaseReconcileOutcome::default();

    let candidates = match work_db.list_reattachable_remote_runs() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "remote-lease reconcile: failed to list remote runs; skipping pass",
            );
            return outcome;
        }
    };
    if candidates.is_empty() {
        return outcome;
    }

    for handle in candidates {
        // A positive death verdict needs a pid to `kill -0`. Without one
        // we have no evidence either way, so we never reap.
        let Some(remote_pid) = handle.remote_pid else {
            tracing::trace!(
                execution_id = %handle.execution_id,
                host_id = %handle.host_id,
                "remote-lease reconcile: run has no recorded remote_pid; skipping",
            );
            outcome.skipped += 1;
            continue;
        };

        let host = match work_db.get_host(&handle.host_id) {
            Ok(Some(host)) => host,
            Ok(None) => {
                tracing::warn!(
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    "remote-lease reconcile: run references a host no longer in the registry; skipping",
                );
                outcome.skipped += 1;
                continue;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    ?err,
                    "remote-lease reconcile: host lookup failed; skipping run",
                );
                outcome.skipped += 1;
                continue;
            }
        };

        let adapter = match provider.adapter_for(&host).await {
            Ok(adapter) => adapter,
            Err(err) => {
                tracing::warn!(
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    error = %format!("{err:#}"),
                    "remote-lease reconcile: could not build host adapter; skipping run",
                );
                outcome.skipped += 1;
                continue;
            }
        };

        match adapter.probe_remote_worker_alive(remote_pid).await {
            Ok(Some(true)) => {
                tracing::trace!(
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    remote_pid,
                    "remote-lease reconcile: worker alive; leaving run",
                );
                outcome.alive += 1;
            }
            Ok(Some(false)) => {
                if reap_dead_remote_execution(work_db, adapter.as_ref(), dispatch_events, &handle, remote_pid).await {
                    outcome.reaped += 1;
                } else {
                    outcome.skipped += 1;
                }
            }
            Ok(None) => {
                // A remote adapter should always return a definite
                // verdict; `None` means "can't probe" (e.g. a local
                // adapter mis-resolved for a remote host). Never reap on
                // that — leave it and surface the oddity.
                tracing::warn!(
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    "remote-lease reconcile: adapter reported no liveness verdict for a remote run; skipping",
                );
                outcome.skipped += 1;
            }
            Err(err) => {
                // Inconclusive — the probe round-trip itself failed (host
                // down, ssh error). A host outage must NOT look like death.
                tracing::debug!(
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    remote_pid,
                    error = %format!("{err:#}"),
                    "remote-lease reconcile: liveness probe inconclusive; leaving run for a later pass",
                );
                outcome.skipped += 1;
            }
        }
    }

    outcome
}

/// Finalize a remote execution whose worker is provably gone: orphan the
/// row, finalize any automation-run bookkeeping, force-release the leaked
/// cube lease on the remote, and emit the reconcile event. Returns `true`
/// when the row was (or already had been) reconciled to a terminal status.
async fn reap_dead_remote_execution(
    work_db: &WorkDb,
    adapter: &dyn HostAdapter,
    dispatch_events: &dyn DispatchEventSink,
    handle: &RemoteRunHandle,
    remote_pid: i64,
) -> bool {
    // Re-read fresh: the row may have settled (Stop finally arrived, a
    // concurrent reaper) between the candidate listing and now.
    let execution = match work_db.get_execution(&handle.execution_id) {
        Ok(execution) => execution,
        Err(err) => {
            tracing::warn!(
                execution_id = %handle.execution_id,
                ?err,
                "remote-lease reconcile: could not load execution to reap; skipping",
            );
            return false;
        }
    };
    if execution.status.is_terminal() {
        return false;
    }

    let prior_status = execution.status.as_str().to_owned();
    let reason = format!(
        "remote-lease reconcile: worker pid {remote_pid} on host `{}` is gone (kill -0: no such process); \
         reaping execution and force-releasing its cube lease (prior status `{prior_status}`)",
        handle.host_id,
    );

    match work_db.mark_execution_orphaned(&execution.id, &reason) {
        Ok(_) => {}
        Err(err) => {
            // A concurrent sweep/completion may have finalized it between
            // our snapshot and now. If it is terminal now, treat as
            // reconciled; otherwise leave it for a later pass.
            let already_terminal = work_db
                .get_execution(&execution.id)
                .map(|cur| cur.status.is_terminal())
                .unwrap_or(false);
            if already_terminal {
                return true;
            }
            tracing::warn!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "remote-lease reconcile: failed to orphan execution; leaving row as-is",
            );
            return false;
        }
    }

    // Automation-run bookkeeping parity with `lost_workspace_sweep`: a
    // triage that created a task before its worker died is recorded as
    // `produced_task`, otherwise `failed_gave_up`.
    if execution.kind == ExecutionKind::AutomationTriage {
        crate::execution_liveness::finalize_dead_automation_triage_run(
            work_db,
            &execution,
            &format!(
                "its remote worker pid {remote_pid} on host `{}` is gone",
                handle.host_id
            ),
        );
    }

    // Force-release the leaked lease on the REMOTE cube (the correct one —
    // the heartbeat/lost-workspace sweeps would target the LOCAL cube).
    // Best-effort: a failure logs and is retried next pass; cube's own TTL
    // reclaims it eventually regardless.
    if let Some(lease_id) = execution.cube_lease_id.as_deref()
        && let Err(err) = adapter
            .force_release_lease(lease_id, Some("remote-lease reconcile: worker process gone"))
            .await
    {
        tracing::warn!(
            execution_id = %execution.id,
            lease_id,
            host_id = %handle.host_id,
            error = %format!("{err:#}"),
            "remote-lease reconcile: force-release of the leaked remote lease failed \
             (will retry next pass; cube TTL reclaims it otherwise)",
        );
    }

    dispatch_events
        .emit(
            DispatchEvent::new(Stage::RemoteLeaseReconcile, Outcome::Ok, &execution.id)
                .with_work_item(&execution.work_item_id)
                .with_details(serde_json::json!({
                    "reason": "remote_worker_dead",
                    "prior_status": prior_status,
                    "host_id": handle.host_id,
                    "remote_pid": remote_pid,
                    "cube_lease_id": execution.cube_lease_id,
                    "cube_workspace_id": execution.cube_workspace_id,
                    "kind": execution.kind.as_str(),
                })),
        )
        .await;

    tracing::warn!(
        execution_id = %execution.id,
        work_item_id = %execution.work_item_id,
        host_id = %handle.host_id,
        remote_pid,
        prior_status = %prior_status,
        "remote-lease reconcile: reaped remote execution whose worker is gone and force-released its lease",
    );

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
    };
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::host_registry::Host;
    use crate::runner::RunOutcome;
    use crate::test_support::*;
    use crate::work::{CreateChoreInput, WorkDb, WorkItem};
    use anyhow::{Result, bail};
    use async_trait::async_trait;
    use boss_protocol::{ExecutionStatus, RequestExecutionInput, WorkExecution};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Canned liveness verdict for the stub adapter's probe.
    #[derive(Clone, Copy)]
    enum Probe {
        Alive,
        Dead,
        /// The probe round-trip itself failed (host down).
        Error,
    }

    /// Records `force_release_lease` calls and returns a canned liveness
    /// verdict. Every other method is unused by the reconcile path.
    struct StubAdapter {
        host_id: String,
        probe: Probe,
        force_released: Mutex<Vec<(String, Option<String>)>>,
    }

    #[async_trait]
    impl HostAdapter for StubAdapter {
        fn host_id(&self) -> &str {
            &self.host_id
        }
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            unimplemented!()
        }
        async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
            self.force_released
                .lock()
                .unwrap()
                .push((lease_id.to_owned(), reason.map(str::to_owned)));
            Ok(())
        }
        async fn create_change(&self, _: &Path, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn goto_workspace(&self, _: &Path, _: u64) -> Result<()> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            unimplemented!()
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            unimplemented!()
        }
        fn command_repr(&self, _: &[&str]) -> Option<(String, String)> {
            None
        }
        async fn spawn_worker(
            &self,
            _: &str,
            _: &WorkExecution,
            _: &WorkItem,
            _: &Path,
            _: Option<&str>,
        ) -> Result<RunOutcome> {
            unimplemented!()
        }
        async fn probe_remote_worker_alive(&self, _remote_pid: i64) -> Result<Option<bool>> {
            match self.probe {
                Probe::Alive => Ok(Some(true)),
                Probe::Dead => Ok(Some(false)),
                Probe::Error => bail!("ssh probe transport failure"),
            }
        }
    }

    struct StubProvider {
        adapter: Arc<StubAdapter>,
    }

    #[async_trait]
    impl HostAdapterProvider for StubProvider {
        async fn adapter_for(&self, _host: &Host) -> Result<Arc<dyn HostAdapter>> {
            Ok(self.adapter.clone() as Arc<dyn HostAdapter>)
        }
    }

    fn open_db() -> (TempDir, Arc<WorkDb>) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, Arc::new(db))
    }

    fn create_chore(db: &WorkDb) -> String {
        let product = create_test_product_with_repo(db, "p", Some("https://github.com/test/repo")).id;
        db.create_chore(CreateChoreInput::builder().product_id(product).name("c").build())
            .unwrap()
            .id
    }

    /// Start a remote run for `work_item_id` on `host_id`, stamp its
    /// `remote_pid`, and return the execution id. The run lands `active`
    /// with the execution non-terminal — the shape the reconcile query
    /// selects (a live-looking remote worker).
    fn start_remote_run(
        db: &WorkDb,
        work_item_id: &str,
        host_id: &str,
        lease_id: &str,
        remote_pid: Option<i64>,
    ) -> String {
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap();
        db.start_execution_run_on_host(
            &execution.id,
            "worker-1",
            "repo-1",
            lease_id,
            "mono-agent-004",
            "/remote/mono-agent-004",
            host_id,
        )
        .unwrap();
        if let Some(pid) = remote_pid {
            db.set_run_remote_pid_for_execution(&execution.id, pid).unwrap();
        }
        execution.id
    }

    fn provider(host_id: &str, probe: Probe) -> (Arc<StubAdapter>, StubProvider) {
        let adapter = Arc::new(StubAdapter {
            host_id: host_id.to_owned(),
            probe,
            force_released: Mutex::new(Vec::new()),
        });
        let provider = StubProvider {
            adapter: adapter.clone(),
        };
        (adapter, provider)
    }

    #[tokio::test]
    async fn reaps_dead_remote_worker_and_force_releases_its_lease() {
        let (_d, db) = open_db();
        let chore = create_chore(&db);
        db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
        let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-XYZ", Some(4242));

        let (adapter, provider) = provider("anaplian", Probe::Dead);
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_remote_leases(&db, &provider, &sink).await;

        assert_eq!(outcome.reaped, 1);
        assert_eq!(outcome.alive, 0);
        assert_eq!(outcome.skipped, 0);

        // Execution is now terminal (orphaned) → no longer blocks the guard.
        let after = db.get_execution(&exec_id).unwrap();
        assert_eq!(after.status, ExecutionStatus::Orphaned);

        // The leaked lease was force-released on the REMOTE adapter.
        // Clone out of the guard so no MutexGuard is held across the await below.
        let released = adapter.force_released.lock().unwrap().clone();
        assert_eq!(released.len(), 1, "the dead worker's lease must be force-released");
        assert_eq!(released[0].0, "lease-XYZ");

        // A reconcile event was emitted carrying the diagnostic detail.
        let events = sink.events_for(&exec_id).await;
        let ev = events
            .iter()
            .find(|e| e.stage == "remote_lease_reconcile")
            .expect("remote_lease_reconcile event missing");
        assert_eq!(ev.details.get("remote_pid").and_then(|v| v.as_i64()), Some(4242));
        assert_eq!(ev.details.get("host_id").and_then(|v| v.as_str()), Some("anaplian"));
    }

    #[tokio::test]
    async fn leaves_live_remote_worker_untouched() {
        let (_d, db) = open_db();
        let chore = create_chore(&db);
        db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
        let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-LIVE", Some(4242));

        let (adapter, provider) = provider("anaplian", Probe::Alive);
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_remote_leases(&db, &provider, &sink).await;

        assert_eq!(outcome.alive, 1);
        assert_eq!(outcome.reaped, 0);
        // A live worker must never be reaped or have its lease released.
        assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
        assert!(adapter.force_released.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn inconclusive_probe_never_reaps() {
        // A host outage (probe Err) must NOT look like proof of death —
        // otherwise every live worker on a briefly-unreachable host would
        // be mass-reaped.
        let (_d, db) = open_db();
        let chore = create_chore(&db);
        db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
        let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-1", Some(4242));

        let (adapter, provider) = provider("anaplian", Probe::Error);
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_remote_leases(&db, &provider, &sink).await;

        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.reaped, 0);
        assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
        assert!(adapter.force_released.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_without_remote_pid_is_skipped() {
        // No pid → no positive death evidence → never reap.
        let (_d, db) = open_db();
        let chore = create_chore(&db);
        db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
        let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-1", None);

        let (adapter, provider) = provider("anaplian", Probe::Dead);
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_remote_leases(&db, &provider, &sink).await;

        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.reaped, 0);
        assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
        assert!(adapter.force_released.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn local_runs_are_not_candidates() {
        // A local run is covered by the local sweeps and must never appear
        // here (host_id = 'local' is excluded by the candidate query).
        let (_d, db) = open_db();
        let chore = create_chore(&db);
        let exec_id = start_remote_run(&db, &chore, "local", "lease-1", Some(4242));

        let (adapter, provider) = provider("anaplian", Probe::Dead);
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_remote_leases(&db, &provider, &sink).await;

        assert_eq!(outcome, RemoteLeaseReconcileOutcome::default());
        assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
        assert!(adapter.force_released.lock().unwrap().is_empty());
    }
}
