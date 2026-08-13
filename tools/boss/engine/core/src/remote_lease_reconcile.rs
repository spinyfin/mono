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
//! - The cube-lease heartbeat sweep does not reap either. It now routes to
//!   the owning host's cube (see [`crate::cube_lease_heartbeat`]'s "Which
//!   cube gets the beat"), so a remote lease is genuinely *refreshed* — but
//!   its auto-reap fires only on sustained heartbeat FAILURE, and a lease
//!   whose worker has died goes on heartbeating perfectly well. Proving the
//!   worker itself is gone needs a pid probe on the remote host, which is
//!   this sweep.
//!
//! So a dead remote worker strands two ways: its execution row blocks the
//! redundant-spawn guard (the work item shows "queued" forever — the
//! symptom the operator saw), and its cube lease strands a remote
//! workspace (and its multi-GB clone) as unreclaimable waste.
//!
//! ## What it does
//!
//! DB-driven (so it survives restart, unlike the registry-driven reapers)
//! over [`WorkDb::list_live_remote_runs`] — the latest run of every
//! non-terminal execution on a non-local host, which is exactly the set of
//! live-looking remote workers. That query judges liveness on the
//! EXECUTION, never on `work_runs.status`: the dispatch path completes the
//! run row within milliseconds of a successful spawn, so requiring an
//! `active` run row (as it originally did) made this whole sweep a no-op in
//! production while its unit tests kept passing. For each it probes the remote
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

use boss_protocol::{CreateAttentionItemInput, ExecutionKind};

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::host_adapter::HostAdapter;
use crate::host_adapter::HostAdapterProvider;
use crate::work::{RemoteRunHandle, WorkDb};

/// `work_attention_items.kind` for a remote worker the reconciler found
/// dead. Declared in [`crate::attention_lifecycle::ATTENTION_LIFECYCLES`].
pub const REMOTE_WORKER_DIED_ATTENTION_KIND: &str = "remote_worker_died";

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

    let candidates = match work_db.list_live_remote_runs() {
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

    // Recover WHY it died before the workspace goes back to cube and takes
    // the evidence with it. A remote worker that dies before its first hook
    // leaves every engine-side surface blank — no activity, no transcript,
    // no `Stop` — and the only record of the cause is the wrapper's
    // `<workspace>/.boss/worker.log`. Read before the force-release below,
    // because releasing the lease is what makes that file unreachable.
    // Best-effort by construction: the reap proceeds identically whether or
    // not the log can be read, and an unreadable log is reported as
    // unavailable rather than being allowed to read as "no output".
    let worker_log = read_worker_log_tail(adapter, execution.workspace_path.as_deref()).await;
    let log_clause = match worker_log.as_deref() {
        Some(tail) if !tail.trim().is_empty() => format!("; last worker output: {}", tail.trim()),
        _ => String::new(),
    };

    let reason = format!(
        "remote-lease reconcile: worker pid {remote_pid} on host `{}` is gone (kill -0: no such process); \
         reaping execution and force-releasing its cube lease (prior status `{prior_status}`){log_clause}",
        handle.host_id,
    );

    match work_db.mark_execution_orphaned(&execution.id, &reason) {
        Ok(_) => {
            // Reap termination path (remote-lease reconcile): tear down
            // any driver-owned state outside the workspace.
            // `mark_execution_orphaned` preserves `workspace_path`, so the
            // pre-call `execution` snapshot is still current.
            crate::driver_teardown::teardown_driver_workspace(
                work_db,
                &execution.id,
                execution.workspace_path.as_deref().map(std::path::Path::new),
                crate::driver_teardown::TeardownReason::RemoteLeaseReconcile,
            )
            .await;
        }
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

    // Raise an attention item. A dispatch event alone is a forensic
    // surface an operator has to already suspect something to go looking
    // at; the incident that motivated this sweep presented as a card
    // sitting in Doing, reading `active`, with nothing anywhere saying its
    // worker had died minutes earlier. A remote worker dying is exactly the
    // "someone needs to know" class the attention lane exists for —
    // especially since the most common cause is a host-level problem
    // (expired agent credentials, a missing toolchain) that will kill every
    // subsequent dispatch to that host the same way until a human fixes it.
    let attention_body = format!(
        "Execution `{exec_id}` was dispatched to host `{host}` and its worker process (pid {remote_pid}) is gone \
         without ever reporting completion.\n\n\
         The execution has been reaped and its cube lease released, so the work item can be re-dispatched.\n\n\
         **Last worker output** (`{workspace}/.boss/worker.log` on `{host}`):\n\n```\n{log}\n```\n\n\
         If that output shows a host-level problem — expired agent credentials, a missing binary — every dispatch \
         to `{host}` will fail the same way until it is fixed.",
        exec_id = execution.id,
        host = handle.host_id,
        workspace = execution.workspace_path.as_deref().unwrap_or("<unknown workspace>"),
        log = match worker_log.as_deref() {
            Some(tail) if !tail.trim().is_empty() => tail.trim().to_owned(),
            Some(_) => "(the worker log exists but is empty — the worker produced no output at all)".to_owned(),
            None => "(the worker log could not be read from the host)".to_owned(),
        },
    );
    if let Err(err) = work_db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(execution.id.clone()),
        work_item_id: None,
        kind: REMOTE_WORKER_DIED_ATTENTION_KIND.to_owned(),
        status: None,
        title: format!("Remote worker died on host {}", handle.host_id),
        body_markdown: attention_body,
        resolved_at: None,
    }) {
        tracing::warn!(
            execution_id = %execution.id,
            error = %format!("{err:#}"),
            "remote-lease reconcile: failed to raise the remote-worker-died attention item; the reap itself stands",
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
                    "worker_log_tail": worker_log,
                })),
        )
        .await;

    tracing::warn!(
        execution_id = %execution.id,
        work_item_id = %execution.work_item_id,
        host_id = %handle.host_id,
        remote_pid,
        prior_status = %prior_status,
        worker_log_tail = worker_log.as_deref().unwrap_or("<unreadable>"),
        "remote-lease reconcile: reaped remote execution whose worker is gone and force-released its lease",
    );

    true
}

/// Pull the tail of the dead worker's `worker.log` from `adapter`'s host.
///
/// `None` means the tail is genuinely unavailable — no workspace path
/// recorded, a local adapter (which has no such log), or a failed
/// round-trip. That is deliberately distinct from `Some("")`, which means
/// the log was read and the worker really did produce no output: "we could
/// not look" and "there was nothing to see" lead to different next steps
/// for whoever reads the attention item.
async fn read_worker_log_tail(adapter: &dyn HostAdapter, workspace_path: Option<&str>) -> Option<String> {
    let workspace_path = workspace_path?;
    match adapter
        .read_worker_log_tail(workspace_path, crate::host_adapter::WORKER_LOG_TAIL_BYTES)
        .await
    {
        Ok(tail) => tail,
        Err(err) => {
            tracing::debug!(
                host_id = adapter.host_id(),
                workspace_path,
                error = %format!("{err:#}"),
                "remote-lease reconcile: could not read the dead worker's log tail; reaping without it",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::host_registry::Host;
    use crate::test_support::*;
    use crate::work::{CreateChoreInput, WorkDb};
    use anyhow::{Result, bail};
    use async_trait::async_trait;
    use boss_protocol::{ExecutionStatus, FinishExecutionRunInput, RequestExecutionInput};
    use std::sync::Arc;
    use std::sync::Mutex;

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
        /// Canned `worker.log` tail. `Ok(None)` models an adapter with no
        /// such log; `Err` models a failed round-trip.
        worker_log: Mutex<Option<String>>,
        worker_log_fails: bool,
        /// Workspace paths `read_worker_log_tail` was asked about, so a
        /// test can prove the read happened BEFORE the lease was released.
        worker_log_reads: Mutex<Vec<String>>,
    }

    crate::stub_host_adapter! { StubAdapter {
        fn host_id(&self) -> &str {
            &self.host_id
        }
        async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
            self.force_released
                .lock()
                .unwrap()
                .push((lease_id.to_owned(), reason.map(str::to_owned)));
            Ok(())
        }
        async fn probe_remote_worker_alive(&self, _remote_pid: i64) -> Result<Option<bool>> {
            match self.probe {
                Probe::Alive => Ok(Some(true)),
                Probe::Dead => Ok(Some(false)),
                Probe::Error => bail!("ssh probe transport failure"),
            }
        }
        async fn read_worker_log_tail(&self, workspace_path: &str, _max_bytes: u64) -> Result<Option<String>> {
            self.worker_log_reads.lock().unwrap().push(workspace_path.to_owned());
            if self.worker_log_fails {
                bail!("ssh transport failure reading worker.log");
            }
            Ok(self.worker_log.lock().unwrap().clone())
        }
    } }

    struct StubProvider {
        adapter: Arc<StubAdapter>,
    }

    #[async_trait]
    impl HostAdapterProvider for StubProvider {
        async fn adapter_for(&self, _host: &Host) -> Result<Arc<dyn HostAdapter>> {
            Ok(self.adapter.clone() as Arc<dyn HostAdapter>)
        }
    }

    fn create_chore(db: &WorkDb) -> String {
        let product = create_test_product_with_repo(db, "p", Some("https://github.com/test/repo")).id;
        db.create_chore(CreateChoreInput::builder().product_id(product).name("c").build())
            .unwrap()
            .id
    }

    /// Start a remote run for `work_item_id` on `host_id`, stamp its
    /// `remote_pid`, and return the execution id.
    ///
    /// Deliberately reproduces the FULL production sequence, including the
    /// `finish_execution_run` the dispatch path performs within
    /// milliseconds of a successful spawn (`run_status = "completed"` while
    /// the execution parks live on `running` — see
    /// `coordinator::record_run_completion`). The original fixture stopped
    /// at `start_execution_run_on_host`, leaving the run row `active` in a
    /// way it never is in production; that is precisely why every test here
    /// passed while the sweep selected nothing at all on a real engine.
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
        let (_execution, run) = db
            .start_execution_run_on_host(
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
        // The dispatch action completes; the worker keeps running.
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&execution.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::Running)
                .run_status("completed")
                .clear_workspace_lease(false)
                .build(),
        )
        .unwrap();
        execution.id
    }

    fn provider(host_id: &str, probe: Probe) -> (Arc<StubAdapter>, StubProvider) {
        provider_with_log(host_id, probe, Some(String::new()), false)
    }

    fn provider_with_log(
        host_id: &str,
        probe: Probe,
        worker_log: Option<String>,
        worker_log_fails: bool,
    ) -> (Arc<StubAdapter>, StubProvider) {
        let adapter = Arc::new(StubAdapter {
            host_id: host_id.to_owned(),
            probe,
            force_released: Mutex::new(Vec::new()),
            worker_log: Mutex::new(worker_log),
            worker_log_fails,
            worker_log_reads: Mutex::new(Vec::new()),
        });
        let provider = StubProvider {
            adapter: adapter.clone(),
        };
        (adapter, provider)
    }

    #[tokio::test]
    async fn reaps_dead_remote_worker_and_force_releases_its_lease() {
        let (_d, db) = open_db_arc();
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

    /// The 2026-08-12 anaplian incident's *diagnosability* half.
    ///
    /// Both remote workers died seconds after launch with `Failed to
    /// authenticate. API Error: 401 OAuth access token has expired.` in
    /// `<workspace>/.boss/worker.log`. Every engine-side surface was blank
    /// and no attention item was raised, so the card sat in Doing reading
    /// `active` with nothing anywhere naming the cause. The reap must carry
    /// that line out with it — into the orphan reason, the dispatch event,
    /// and an attention item — because the workspace (and the log) goes back
    /// to cube moments later.
    #[tokio::test]
    async fn reap_carries_the_dead_workers_log_into_the_reason_event_and_attention() {
        let (_d, db) = open_db_arc();
        let chore = create_chore(&db);
        db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
        let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-AUTH", Some(4242));

        const OAUTH_ERROR: &str = "Failed to authenticate. API Error: 401 OAuth access token has expired.";
        let (adapter, provider) = provider_with_log("anaplian", Probe::Dead, Some(OAUTH_ERROR.to_owned()), false);
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_remote_leases(&db, &provider, &sink).await;
        assert_eq!(outcome.reaped, 1);

        // The log was read against the run's recorded workspace path...
        assert_eq!(
            adapter.worker_log_reads.lock().unwrap().clone(),
            vec!["/remote/mono-agent-004".to_owned()],
        );
        // ...and its content reached the durable orphan reason. The run row
        // was already closed by the dispatch path, so `mark_execution_orphaned`
        // records the reason on `work_runs.error_text` (its `result_summary` /
        // `finished_at` guards are already spent by then).
        assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Orphaned);
        let reason = db
            .list_runs(&exec_id)
            .unwrap()
            .into_iter()
            .filter_map(|run| run.error_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            reason.contains(OAUTH_ERROR),
            "the orphan reason must name why the worker died, got: {reason}",
        );

        // ...the dispatch event...
        let events = sink.events_for(&exec_id).await;
        let ev = events
            .iter()
            .find(|e| e.stage == "remote_lease_reconcile")
            .expect("remote_lease_reconcile event missing");
        assert_eq!(
            ev.details.get("worker_log_tail").and_then(|v| v.as_str()),
            Some(OAUTH_ERROR),
        );

        // ...and an attention item, so the failure is not silent. This is
        // the surface whose absence made the incident invisible.
        let attentions = db.list_attention_items(&exec_id).unwrap();
        let item = attentions
            .iter()
            .find(|a| a.kind == REMOTE_WORKER_DIED_ATTENTION_KIND)
            .expect("a dead remote worker must raise an attention item");
        assert!(
            item.title.contains("anaplian"),
            "title should name the host: {}",
            item.title
        );
        assert!(
            item.body_markdown.contains(OAUTH_ERROR),
            "the attention body must carry the worker's own output",
        );
    }

    /// An unreadable log must never masquerade as "the worker said
    /// nothing" — the two lead a reader to different next steps. The reap
    /// itself proceeds either way; losing the log is not a reason to leave
    /// a dead worker's lease stranded.
    #[tokio::test]
    async fn unreadable_worker_log_still_reaps_and_says_it_was_unreadable() {
        let (_d, db) = open_db_arc();
        let chore = create_chore(&db);
        db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
        let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-NOLOG", Some(4242));

        let (adapter, provider) = provider_with_log("anaplian", Probe::Dead, None, true);
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_remote_leases(&db, &provider, &sink).await;

        assert_eq!(outcome.reaped, 1, "a failed log read must not block the reap");
        assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Orphaned);
        assert_eq!(
            adapter.force_released.lock().unwrap().len(),
            1,
            "the lease must still be released",
        );

        let attentions = db.list_attention_items(&exec_id).unwrap();
        let item = attentions
            .iter()
            .find(|a| a.kind == REMOTE_WORKER_DIED_ATTENTION_KIND)
            .expect("attention item still filed");
        assert!(
            item.body_markdown.contains("could not be read"),
            "an unreadable log must say so rather than read as empty output: {}",
            item.body_markdown,
        );
    }

    #[tokio::test]
    async fn leaves_live_remote_worker_untouched() {
        let (_d, db) = open_db_arc();
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
        let (_d, db) = open_db_arc();
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
        let (_d, db) = open_db_arc();
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
        let (_d, db) = open_db_arc();
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
