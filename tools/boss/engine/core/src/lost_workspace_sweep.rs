//! Reconciler for non-terminal executions whose cube workspace directory
//! has vanished from disk — the 2026-06-14 "waiting_human zombie".
//!
//! ## Why this exists
//!
//! `waiting_human` is the *normal post-spawn park state*: the runner returns
//! it the instant a worker pane spawns, and the row stays there — lease and
//! workspace retained, `finished_at` NULL — until the worker's `Stop` hook
//! fires and the completion handler transitions it out. A worker that dies
//! WITHOUT a `Stop` (killed, crashed, or — as in the incident — had its cube
//! workspace relocated out from under it by the workspace-root migration
//! `~/Documents/dev/workspaces` → `~/.local/share/cube/workspaces`) leaves
//! the row stuck in `waiting_human` forever:
//!
//! - No `Stop` → the completion handler never runs.
//! - `waiting_human` is `is_live()` and not `can_reconcile()`, so every
//!   other sweep deliberately skips it.
//! - The one reaper that could catch a dead worker (`dead_pid_sweep`) is
//!   driven by the in-memory `LiveWorkerStateRegistry`, which is EMPTY after
//!   an engine restart — so a pre-restart zombie is invisible to it.
//! - The startup reconciler (`run_reconcile`) probes cube, but the zombie's
//!   old-root workspace is absent from the cube pool → verdict `Unknown` →
//!   treated as Live → never reaped. This is why engine restarts don't clear
//!   it.
//!
//! With the row stuck `waiting_human`, the redundant-spawn guard
//! (`schedule_execution`) — which blocks on `is_live()` — refuses every
//! subsequent spawn for that work item with `redundant_spawn`. For the three
//! automation triage zombies that meant automations were 100% wedged for 17
//! days.
//!
//! ## What it does
//!
//! It is DB-driven (survives restart, unlike the registry-driven reapers)
//! and keys on *positive* evidence of death: a non-terminal execution whose
//! recorded LOCAL `workspace_path` no longer exists on disk. Such a row is
//! finalized through the proper terminal path (`mark_execution_orphaned`,
//! which stamps `finished_at` and orphans its runs), its automation-run
//! bookkeeping is finalized (a triage that created a task before dying is
//! recorded as `produced_task` with `produced_task_id`; otherwise
//! `failed_gave_up`), and a `lost_workspace_reconcile` trace event is
//! emitted carrying the exec id, prior status, and reason.
//!
//! The same [`reconcile_if_workspace_lost`] routine is called inline by the
//! redundant-spawn guard so a zombie never blocks a spawn even between sweep
//! passes.
//!
//! ## Host safety
//!
//! A local `.exists()` probe is meaningless for a remote (SSH-host) worker
//! whose `workspace_path` lives on another machine, so the reconciler acts
//! ONLY on executions whose latest run ran on `host_id == "local"`. Remote
//! workers are never reaped here.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same pattern as
//! the other sweeps), so the zombies clear on upgrade/restart without any
//! hand-editing of the DB.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::WorkExecution;

use crate::coordinator::{CubeClient, ExecutionCoordinator};
use crate::dispatch_events::{DispatchEventSink, Stage};
use crate::execution_liveness::execution_workspace_dir_missing;
use crate::work::WorkDb;

/// Cadence for the periodic pass. Fires immediately on boot, then every
/// interval — fast enough that a zombie formed mid-run is cleared long
/// before the next 15-minute automation fire.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Counts from one pass; logged at `info` when any reaping occurred.
#[derive(Debug, Default)]
pub struct LostWorkspaceSweepOutcome {
    pub reaped: usize,
}

impl crate::sweep_loop::SweepOutcome for LostWorkspaceSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }

    fn log(&self) {
        tracing::info!(reaped = self.reaped, "lost-workspace sweep: pass complete");
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`,
/// firing immediately on spawn so pre-restart zombies clear on boot.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    cube_client: Arc<dyn CubeClient>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let cube_client = Arc::clone(&cube_client);
        let dispatch_events = Arc::clone(&dispatch_events);
        async move {
            run_one_pass(
                work_db.as_ref(),
                Arc::clone(&coordinator),
                cube_client.as_ref(),
                dispatch_events.as_ref(),
            )
            .await
        }
    })
}

/// Run a single lost-workspace reconciliation pass over every non-terminal
/// execution that recorded a workspace path.
pub async fn run_one_pass(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    cube_client: &dyn CubeClient,
    dispatch_events: &dyn DispatchEventSink,
) -> LostWorkspaceSweepOutcome {
    let mut outcome = LostWorkspaceSweepOutcome::default();

    let candidates = match work_db.list_non_terminal_executions_with_workspace() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "lost-workspace sweep: failed to list candidate executions; skipping pass",
            );
            return outcome;
        }
    };

    for execution in candidates {
        if reconcile_if_workspace_lost(work_db, dispatch_events, &execution).await {
            outcome.reaped += 1;
            // `mark_execution_orphaned` deliberately leaves the cube lease
            // columns intact (a live workspace may hold in-flight commits a
            // resume should reclaim). Here we KNOW the workspace directory
            // is gone, so best-effort force-release the dead lease to stop
            // cube tracking a workspace that no longer exists — the source
            // of the incident's 17 days of lease-heartbeat errors. Failure
            // is benign: the lease is very likely already gone.
            if let Some(lease_id) = execution.cube_lease_id.as_deref()
                && let Err(err) = cube_client
                    .force_release_lease(lease_id, Some("lost-workspace reconcile: workspace directory gone"))
                    .await
            {
                tracing::debug!(
                    execution_id = %execution.id,
                    lease_id,
                    error = %format!("{err:#}"),
                    "lost-workspace sweep: best-effort lease force-release failed (likely already released)",
                );
            }
        }
    }

    if outcome.reaped > 0 {
        // A cleared zombie unblocks the redundant-spawn guard for its work
        // item; kick the scheduler so any ready execution that was queued
        // behind the wedge dispatches immediately.
        coordinator.kick();
    }

    outcome
}

/// Finalize `execution` iff it is a non-terminal LOCAL execution whose
/// recorded workspace directory has vanished from disk. Returns `true` when
/// the row was (or already had been) reconciled to a terminal status;
/// `false` when it is NOT a lost-workspace zombie and callers should keep
/// treating it as live.
///
/// DB-only plus a trace event — no cube/coordinator dependency — so it can
/// be called both from the periodic [`run_one_pass`] and inline from the
/// coordinator's redundant-spawn guard.
pub async fn reconcile_if_workspace_lost(
    work_db: &WorkDb,
    dispatch_events: &dyn DispatchEventSink,
    execution: &WorkExecution,
) -> bool {
    // Already settled — nothing to reconcile.
    if execution.status.is_terminal() {
        return false;
    }

    // Host safety: a local filesystem probe only means anything for a local
    // worker. Only reconcile when the latest run ran on `host_id == "local"`.
    // Anything else (remote host, or no run recorded to judge from) is left
    // alone so a live remote worker is never falsely reaped.
    match work_db.latest_run_host_for_execution(&execution.id) {
        Ok(Some(host)) if host == "local" => {}
        Ok(other) => {
            if other.is_some() {
                tracing::trace!(
                    execution_id = %execution.id,
                    host = ?other,
                    "lost-workspace reconcile: skipping non-local execution",
                );
            }
            return false;
        }
        Err(err) => {
            tracing::debug!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "lost-workspace reconcile: could not resolve run host; skipping conservatively",
            );
            return false;
        }
    }

    // The authoritative liveness signal: the worker's cwd is gone.
    if !execution_workspace_dir_missing(execution) {
        return false;
    }

    let prior_status = execution.status.as_str();
    let workspace_path = execution.workspace_path.clone().unwrap_or_default();
    let reason = format!(
        "lost-workspace reconcile: cube workspace directory `{workspace_path}` no longer exists on disk; \
         worker pane is gone (prior status `{prior_status}`)"
    );

    // Funnel the orphan → triage-bookkeeping → dispatch-event flow through the
    // shared reconciler finalize (see `execution_liveness::finalize_gone_execution`),
    // the single place that flow lives for both this sweep and `dead_pane_sweep`.
    let reconciled = crate::execution_liveness::finalize_gone_execution(
        work_db,
        dispatch_events,
        execution,
        &reason,
        &format!("its cube workspace `{workspace_path}` is gone"),
        Stage::LostWorkspaceReconcile,
        serde_json::json!({
            "reason": "workspace_dir_missing",
            "prior_status": prior_status,
            "workspace_path": workspace_path,
            "kind": execution.kind.as_str(),
        }),
    )
    .await;

    if reconciled {
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            prior_status,
            workspace_path = %workspace_path,
            "lost-workspace reconcile: finalized execution whose workspace directory is gone",
        );
    }

    reconciled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use tempfile::TempDir;

    use crate::dispatch_events::NoopDispatchEventSink;
    use crate::work::{AutomationFireRecord, WorkDb};
    use boss_protocol::{
        AUTOMATION_OUTCOME_FAILED_GAVE_UP, AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AUTOMATION_OUTCOME_PRODUCED_TASK,
        ExecutionStatus, FinishExecutionRunInput,
    };

    fn create_automation(db: &WorkDb, product_id: &str) -> String {
        seed_daily_automation(db, product_id).id
    }

    /// Create a triage execution, start its run (stamping `host_id` +
    /// `workspace_path`), then park it in `waiting_human` — reproducing the
    /// exact production shape of a just-spawned triage worker.
    fn parked_triage_execution(db: &WorkDb, automation_id: &str, workspace_path: &str, host: &str) -> WorkExecution {
        let exec = db
            .create_automation_triage_execution(automation_id, "https://github.com/test/repo")
            .unwrap();
        let (_exec, run) = db
            .start_execution_run_on_host(
                &exec.id,
                "auto-worker-1",
                "repo-1",
                "lease-1",
                "mono-agent-028",
                workspace_path,
                host,
            )
            .unwrap();
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&exec.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::WaitingHuman)
                .run_status("completed")
                .build(),
        )
        .unwrap();
        db.get_execution(&exec.id).unwrap()
    }

    /// Seed the pessimistic dispatch-time run row the scheduler writes.
    fn seed_dispatch_run(db: &WorkDb, automation_id: &str, triage_execution_id: &str, scheduled_for: i64) {
        db.record_automation_run_and_advance(
            AutomationFireRecord::builder()
                .automation_id(automation_id.to_owned())
                .scheduled_for(scheduled_for)
                .started_at(scheduled_for)
                .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                .detail("dispatched; awaiting triage worker decision (Stop not yet received)")
                .triage_execution_id(triage_execution_id.to_owned())
                .build(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn reconciles_waiting_human_zombie_whose_workspace_is_gone() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        // A path that does not exist — the old cube root removed by the migration.
        let exec = parked_triage_execution(&db, &automation, "/nonexistent/old-root/mono-agent-028", "local");
        seed_dispatch_run(&db, &automation, &exec.id, 1_700_000_000);
        assert_eq!(exec.status, ExecutionStatus::WaitingHuman);

        let sink = NoopDispatchEventSink;
        let reconciled = reconcile_if_workspace_lost(&db, &sink, &exec).await;
        assert!(
            reconciled,
            "a waiting_human zombie with a missing workspace must be reconciled"
        );

        // Execution is now terminal (orphaned) — it no longer blocks the guard.
        let after = db.get_execution(&exec.id).unwrap();
        assert!(after.status.is_terminal(), "expected terminal, got {}", after.status);
        assert_eq!(after.status, ExecutionStatus::Orphaned);

        // The false "dispatched; awaiting …" detail is overwritten with the truth.
        let runs = db.list_automation_runs(&automation).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP);
        assert!(runs[0].finished_at.is_some(), "reconciled run must be finalized");
        assert!(
            !runs[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("awaiting triage worker decision"),
            "the pessimistic placeholder must be replaced, got {:?}",
            runs[0].detail
        );
    }

    #[tokio::test]
    async fn records_produced_task_when_triage_made_a_task_before_dying() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/nonexistent/old-root/mono-agent-035", "local");
        seed_dispatch_run(&db, &automation, &exec.id, 1_700_000_000);

        // The triage worker created a task before its pane died.
        let task_id = create_test_chore_manual(&db, product.as_str(), "produced by triage").id;
        db.stamp_task_source_automation_for_test(&task_id, &automation, "todo")
            .unwrap();

        let sink = NoopDispatchEventSink;
        assert!(reconcile_if_workspace_lost(&db, &sink, &exec).await);

        let runs = db.list_automation_runs(&automation).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].outcome, AUTOMATION_OUTCOME_PRODUCED_TASK,
            "a triage that created a task before dying must be recorded as produced_task"
        );
        assert_eq!(
            runs[0].produced_task_id.as_deref(),
            Some(task_id.as_str()),
            "produced_task_id must be linked (the historical bookkeeping gap)"
        );
    }

    #[tokio::test]
    async fn leaves_live_execution_whose_workspace_exists() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        // Point workspace_path at a directory that really exists.
        let real_dir = TempDir::new().unwrap();
        let exec = parked_triage_execution(&db, &automation, real_dir.path().to_str().unwrap(), "local");

        let sink = NoopDispatchEventSink;
        let reconciled = reconcile_if_workspace_lost(&db, &sink, &exec).await;
        assert!(
            !reconciled,
            "a live execution whose workspace exists must NOT be reconciled"
        );
        let after = db.get_execution(&exec.id).unwrap();
        assert_eq!(after.status, ExecutionStatus::WaitingHuman, "row must be left live");
    }

    #[tokio::test]
    async fn never_reaps_remote_execution_on_local_probe() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        // Remote host: the missing local path must NOT trigger a reap.
        let exec = parked_triage_execution(&db, &automation, "/remote/only/path/mono-agent-036", "remote-1");

        let sink = NoopDispatchEventSink;
        let reconciled = reconcile_if_workspace_lost(&db, &sink, &exec).await;
        assert!(
            !reconciled,
            "a remote worker must never be reaped by a local filesystem probe"
        );
        let after = db.get_execution(&exec.id).unwrap();
        assert_eq!(after.status, ExecutionStatus::WaitingHuman);
    }
}
