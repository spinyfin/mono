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
//! symptom that made the failure look like a stuck queue), and its cube
//! lease strands a remote workspace (and its multi-GB clone) as
//! unreclaimable waste.
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
    pane_releaser: Option<&dyn crate::completion::WorkerPaneReleaser>,
) -> RemoteLeaseReconcileOutcome {
    let mut outcome = RemoteLeaseReconcileOutcome::default();

    // Terminal remote executions that still hold a persona or cube lease
    // (cancelled while the worker ran, or a crash between terminalization
    // and cleanup). A terminal status is not proof the process exited, so
    // release only on a positive pid-probe death verdict. Runs at startup
    // too, reclaiming leases stranded by a crash or older engine.
    match work_db.terminal_remote_cleanup_runs() {
        Ok(handles) => {
            for handle in handles {
                release_terminal_remote_resources(work_db, provider, pane_releaser, &handle).await;
            }
        }
        Err(err) => tracing::warn!(?err, "remote-lease reconcile: terminal cleanup query failed"),
    }

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

        // Only live workers (`running` / `waiting_human`) can be reaped here.
        // Park states — especially `waiting_review` / `waiting_merge` — keep
        // their lease by design after the worker finishes and exits: the
        // workspace is retained for a follow-up revision. A dead or never-
        // reported pid in those states is expected, not a zombie; force-
        // releasing the lease would hand the workspace back to cube and
        // destroy the park. Sibling sweeps (`db_fallback_death_evidence`,
        // `lost_workspace_sweep`, `dead_pane_sweep`) all gate the same way.
        //
        // Checked *before* the SSH probe so a long-parked remote execution
        // does not pay a `kill -0` round trip every reconcile pass for the
        // whole of its park (hours, across a revision cycle).
        // `list_live_remote_runs` still admits park states because the
        // startup reattach consumer shares that query.
        match work_db.get_execution(&handle.execution_id) {
            Ok(execution) if execution.status.is_live() => {}
            Ok(_) => {
                tracing::trace!(
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    "remote-lease reconcile: execution is not live (parked or settled); not probing",
                );
                continue;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %handle.execution_id,
                    ?err,
                    "remote-lease reconcile: could not load execution status before probe; skipping",
                );
                outcome.skipped += 1;
                continue;
            }
        }

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
                if reap_dead_remote_execution(
                    work_db,
                    adapter.as_ref(),
                    dispatch_events,
                    pane_releaser,
                    &handle,
                    remote_pid,
                )
                .await
                {
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
    pane_releaser: Option<&dyn crate::completion::WorkerPaneReleaser>,
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
    // Re-check after the probe: the row may have parked or settled between
    // the pre-probe gate and now. Same `is_live()` predicate as the
    // candidate loop (terminal statuses are a subset of `!is_live()`).
    if !execution.status.is_live() {
        if execution.status.is_terminal() {
            release_remote_persona(work_db, pane_releaser, &execution.id).await;
        }
        return false;
    }

    let prior_status = execution.status.as_str().to_owned();

    // Recover WHY it died before the workspace goes back to cube and takes
    // the evidence with it. A remote worker that dies before its first hook
    // leaves every engine-side surface blank — no activity, no transcript,
    // no `Stop` — and the only record of the cause is the wrapper's
    // worker log (see [`crate::host_adapter::remote_worker_log_path`]).
    // Read before the force-release below,
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
                release_remote_persona(work_db, pane_releaser, &execution.id).await;
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

    release_remote_persona(work_db, pane_releaser, &execution.id).await;

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
    // surface that has to be sought out deliberately; a dead remote worker
    // needs to surface without anyone suspecting it first. The incident
    // that motivated this sweep presented as a card sitting in Doing,
    // reading `active`, with nothing anywhere saying its worker had died
    // minutes earlier. A remote worker dying is exactly the "someone needs
    // to know" class the attention lane exists for — especially since the
    // most common cause is a host-level problem (expired agent credentials,
    // a missing toolchain) that will kill every subsequent dispatch to that
    // host the same way until it is fixed.
    let worker_log_path = execution
        .workspace_path
        .as_deref()
        .map(crate::host_adapter::remote_worker_log_path)
        .unwrap_or_else(|| "<unknown workspace>/.boss/worker.log".to_owned());
    let attention_body = format!(
        "Execution `{exec_id}` was dispatched to host `{host}` and its worker process (pid {remote_pid}) is gone \
         without ever reporting completion.\n\n\
         The execution has been reaped and its cube lease released, so the work item can be re-dispatched.\n\n\
         **Last worker output** (`{worker_log_path}` on `{host}`):\n\n```\n{log}\n```\n\n\
         If that output shows a host-level problem — expired agent credentials, a missing binary — every dispatch \
         to `{host}` will fail the same way until it is fixed.",
        exec_id = execution.id,
        host = handle.host_id,
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

/// Release a terminal remote execution's persona, live state and cube lease
/// once its worker process is provably gone. An inconclusive probe
/// (unreachable host, `Ok(None)`) is left held for a later pass. A run with
/// no recorded pid can never be probed, so only its persona is released and
/// the cube lease is left to its TTL.
async fn release_terminal_remote_resources(
    work_db: &WorkDb,
    provider: &dyn HostAdapterProvider,
    pane_releaser: Option<&dyn crate::completion::WorkerPaneReleaser>,
    handle: &RemoteRunHandle,
) {
    let Some(remote_pid) = handle.remote_pid else {
        // The pid was never persisted, so there is nothing to probe and the
        // lease cannot be proven stale. The persona has no TTL fallback
        // (the cube lease does), so free its roster name rather than
        // leaking it permanently; the cube TTL reclaims the lease.
        release_remote_persona(work_db, pane_releaser, &handle.execution_id).await;
        return;
    };
    let Ok(Some(host)) = work_db.get_host(&handle.host_id) else {
        return;
    };
    let Ok(adapter) = provider.adapter_for(&host).await else {
        return;
    };
    if !matches!(adapter.probe_remote_worker_alive(remote_pid).await, Ok(Some(false))) {
        return;
    }
    release_remote_persona(work_db, pane_releaser, &handle.execution_id).await;
    let Ok(execution) = work_db.get_execution(&handle.execution_id) else {
        return;
    };
    let Some(lease_id) = execution.cube_lease_id.as_deref() else {
        return;
    };
    match adapter
        .force_release_lease(lease_id, Some("remote-lease reconcile: worker process gone"))
        .await
    {
        Ok(()) => {
            if let Err(err) = work_db.clear_execution_workspace(&execution.id) {
                tracing::warn!(execution_id = %execution.id, ?err, "remote-lease reconcile: clearing lease columns failed");
            }
        }
        Err(err) => tracing::warn!(
            execution_id = %execution.id,
            lease_id,
            error = %format!("{err:#}"),
            "remote-lease reconcile: terminal remote lease release failed; will retry",
        ),
    }
}

async fn release_remote_persona(
    work_db: &WorkDb,
    pane_releaser: Option<&dyn crate::completion::WorkerPaneReleaser>,
    execution_id: &str,
) {
    if let Some(releaser) = pane_releaser {
        releaser.release_pane(execution_id).await;
    } else if let Err(err) = work_db.release_persona(execution_id) {
        tracing::warn!(execution_id, ?err, "remote-lease reconcile: persona release failed");
    }
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
mod tests;
