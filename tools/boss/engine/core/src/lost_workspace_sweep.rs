//! Restart-robust reconciler for non-terminal LOCAL executions whose worker
//! is provably gone. Named for its original signal (a vanished cube workspace
//! directory — the 2026-06-14 "waiting_human zombie"), it now reaps on any of
//! three durable death signals; see [`reconcile_if_execution_dead`]:
//!
//! 1. the cube workspace directory has vanished from disk (original signal);
//! 2. the recorded pane pid (`work_runs.worker_shell_pid`) is dead; or
//! 3. no pane pid was ever reported and the run has been live past the
//!    pane-attach deadline (a spawn that stalled before `pane_spawned`).
//!
//! Signals 2 and 3 close the 2026-07-03 gap where a pane died on an app
//! restart while its workspace dir survived and its cube lease stayed alive
//! (kept beating by the engine's own DB-fallback heartbeat), so no existing
//! reconciler reaped it.
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
//! The same [`reconcile_if_execution_dead`] routine is called inline by the
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
use crate::execution_liveness::{PaneLivenessVerdict, classify_pane_liveness, execution_workspace_dir_missing};
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
        if reconcile_if_execution_dead(work_db, dispatch_events, &execution).await {
            outcome.reaped += 1;
            // `mark_execution_orphaned` deliberately leaves the cube lease
            // columns intact (a live workspace may hold in-flight commits a
            // resume should reclaim). Here we KNOW the worker pane is gone, so
            // best-effort force-release the dead lease. This matters MOST for
            // the pane-death signals: unlike a lost workspace (whose lease cube
            // already reclaimed), a dead pane's workspace dir still exists and
            // its lease is still `leased` — kept alive by the engine's own
            // DB-fallback heartbeat. Releasing it here is what actually frees
            // the slot; without it the reconciled row would stop blocking the
            // guard but the workspace would stay occupied until TTL. Failure is
            // benign: for the lost-workspace case the lease is very likely
            // already gone.
            if let Some(lease_id) = execution.cube_lease_id.as_deref()
                && let Err(err) = cube_client
                    .force_release_lease(lease_id, Some("execution-liveness reconcile: worker pane gone"))
                    .await
            {
                tracing::debug!(
                    execution_id = %execution.id,
                    lease_id,
                    error = %format!("{err:#}"),
                    "execution-liveness sweep: best-effort lease force-release failed (likely already released)",
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

/// Transition `execution` to `orphaned` and finalize its automation-run
/// bookkeeping and dispatch event — shared by every death signal in
/// [`reconcile_if_execution_dead`], funneled through
/// [`crate::execution_liveness::finalize_gone_execution`] (the single place
/// this orphan → triage-bookkeeping → dispatch-event flow lives, shared with
/// `dead_pane_sweep`) so it is written once.
///
/// `reason` is stamped on the row; `triage_death_reason` is a human-readable
/// clause (e.g. "its cube workspace `…` is gone") folded into the triage
/// `automation_runs` bookkeeping for `AutomationTriage` executions. `stage`
/// and `details` identify which signal fired on the emitted dispatch event.
///
/// Returns `true` when the row was (or already had been) reconciled to a
/// terminal status; `false` when the orphan failed and the row is still live
/// (a later pass retries).
async fn finalize_dead_execution(
    work_db: &WorkDb,
    dispatch_events: &dyn DispatchEventSink,
    execution: &WorkExecution,
    reason: &str,
    triage_death_reason: &str,
    stage: Stage,
    details: serde_json::Value,
) -> bool {
    crate::execution_liveness::finalize_gone_execution(
        work_db,
        dispatch_events,
        execution,
        reason,
        triage_death_reason,
        stage,
        details,
    )
    .await
}

/// Finalize `execution` iff it is a non-terminal LOCAL execution whose worker
/// pane is provably gone by a *restart-robust* signal. Returns `true` when the
/// row was (or already had been) reconciled to a terminal status; `false` when
/// there is no positive evidence of death and callers should keep treating it
/// as live.
///
/// Three independent death signals, each derived from durable state (the DB
/// row + the filesystem/`kill(pid,0)`) so the verdict survives an engine
/// restart that empties the in-memory `LiveWorkerStateRegistry`:
///
/// 1. **workspace dir gone** — the worker's cwd vanished (the 2026-06-14
///    workspace-root migration). Emits [`Stage::LostWorkspaceReconcile`].
/// 2. **pane pid dead** — the recorded `work_runs.worker_shell_pid` is gone
///    (`kill(pid, 0)` → `ESRCH`); the pane the app parented died with it (an
///    app relaunch). Emits [`Stage::ExecutionLivenessReconcile`].
/// 3. **pane never attached** — no pid was ever reported and the run has been
///    live past the pane-attach deadline (stalled before `pane_spawned`).
///    Emits [`Stage::ExecutionLivenessReconcile`].
///
/// Signals 2 and 3 close the exact gap that let the 2026-07-03 zombies survive
/// the T2168 fix: their workspace dirs were still on disk (so signal 1 never
/// fired), their cube leases were kept alive by the engine's own DB-fallback
/// heartbeat (so `cube_lease_auto_reap` never fired), and the in-memory
/// registry was empty after the restart (so `dead_pid_reconcile` never saw
/// them). See [`classify_pane_liveness`].
///
/// DB + filesystem + a `kill(pid,0)` probe, plus a trace event — no
/// cube/coordinator dependency — so it can be called both from the periodic
/// [`run_one_pass`] and inline from the coordinator's redundant-spawn guard.
pub async fn reconcile_if_execution_dead(
    work_db: &WorkDb,
    dispatch_events: &dyn DispatchEventSink,
    execution: &WorkExecution,
) -> bool {
    reconcile_if_execution_dead_at(work_db, dispatch_events, execution, crate::epoch_time::now_epoch_secs()).await
}

/// [`reconcile_if_execution_dead`] with the wall clock injected, so the
/// pane-attach-deadline age check is deterministic in tests (mirrors
/// [`crate::run_reconcile::probe_in_flight_runs`]'s `now_epoch_s` seam).
async fn reconcile_if_execution_dead_at(
    work_db: &WorkDb,
    dispatch_events: &dyn DispatchEventSink,
    execution: &WorkExecution,
    now_epoch: i64,
) -> bool {
    // Already settled — nothing to reconcile.
    if execution.status.is_terminal() {
        return false;
    }

    // Host safety + the pane pid in one read. A local filesystem/`kill`
    // probe only means anything for a local worker, so only reconcile when
    // the latest run ran on `host_id == "local"`. Anything else (remote host,
    // or no run recorded to judge from) is left alone so a live remote worker
    // is never falsely reaped.
    let (shell_pid, prior_status) = match work_db.latest_run_host_and_shell_pid_for_execution(&execution.id) {
        Ok(Some((host, pid))) if host == "local" => (pid, execution.status.as_str()),
        Ok(Some((host, _))) => {
            tracing::trace!(
                execution_id = %execution.id,
                %host,
                "execution-liveness reconcile: skipping non-local execution",
            );
            return false;
        }
        Ok(None) => return false,
        Err(err) => {
            tracing::debug!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "execution-liveness reconcile: could not resolve run host; skipping conservatively",
            );
            return false;
        }
    };

    // Signal 1 — the worker's cwd is gone (workspace-root migration). Kept as
    // a distinct `lost_workspace_reconcile` event: it is a different failure
    // mode (workspace relocation) than a dead pane, and existing tooling pins
    // that stage.
    if execution_workspace_dir_missing(execution) {
        let workspace_path = execution.workspace_path.clone().unwrap_or_default();
        let reason = format!(
            "lost-workspace reconcile: cube workspace directory `{workspace_path}` no longer exists on disk; \
             worker pane is gone (prior status `{prior_status}`)"
        );
        let reconciled = finalize_dead_execution(
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

        return reconciled;
    }

    // Signals 2 & 3 — the pane process is gone (restart-robust). The workspace
    // dir still exists (else signal 1 would have fired), so this is the
    // survived-the-T2168-fix shape.
    let started_epoch = execution.started_at.as_deref().and_then(|s| s.parse::<i64>().ok());
    let verdict = classify_pane_liveness(shell_pid, started_epoch, now_epoch);
    if !verdict.is_dead() {
        return false;
    }

    let age_in_status_secs = started_epoch.map(|s| now_epoch.saturating_sub(s));
    let pane_clause = match verdict {
        PaneLivenessVerdict::PidDead => format!(
            "its worker pane pid {} is gone (kill(pid,0) → ESRCH)",
            shell_pid.unwrap_or_default()
        ),
        PaneLivenessVerdict::NeverAttached => format!(
            "its worker pane never reported a shell pid within {}s of starting; it never attached",
            crate::execution_liveness::PANE_ATTACH_DEADLINE_SECS
        ),
        PaneLivenessVerdict::LiveOrIndeterminate => unreachable!("guarded by is_dead()"),
    };
    let reason = format!(
        "execution-liveness reconcile: {pane_clause} (prior status `{prior_status}`, age {age_in_status_secs:?}s)"
    );

    let reconciled = finalize_dead_execution(
        work_db,
        dispatch_events,
        execution,
        &reason,
        &pane_clause,
        Stage::ExecutionLivenessReconcile,
        serde_json::json!({
            "reason": verdict.reason(),
            "prior_status": prior_status,
            "age_in_status_secs": age_in_status_secs,
            "worker_shell_pid": shell_pid,
            "kind": execution.kind.as_str(),
        }),
    )
    .await;

    if reconciled {
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            prior_status,
            verdict = verdict.reason(),
            worker_shell_pid = ?shell_pid,
            age_in_status_secs = ?age_in_status_secs,
            "execution-liveness reconcile: finalized execution whose worker pane is gone",
        );
    }

    reconciled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use tempfile::TempDir;

    use crate::dispatch_events::{NoopDispatchEventSink, RecordingDispatchEventSink};
    use crate::execution_liveness::PANE_ATTACH_DEADLINE_SECS;
    use crate::work::{AutomationFireRecord, WorkDb};

    /// A PID guaranteed not to exist: spawn `true`, wait for it to exit,
    /// reuse its released pid. (Same trick the dead-PID sweep tests use.)
    fn dead_pid() -> i64 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i64;
        let _ = child.wait();
        pid
    }

    /// Parse an execution's `started_at` (epoch-seconds string) for building a
    /// deterministic injected clock in the pane-attach-deadline tests.
    fn started_epoch(exec: &WorkExecution) -> i64 {
        exec.started_at.as_deref().unwrap().parse::<i64>().unwrap()
    }
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
        let reconciled = reconcile_if_execution_dead(&db, &sink, &exec).await;
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
        assert!(reconcile_if_execution_dead(&db, &sink, &exec).await);

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
        let reconciled = reconcile_if_execution_dead(&db, &sink, &exec).await;
        assert!(
            !reconciled,
            "a live execution whose workspace exists must NOT be reconciled"
        );
        let after = db.get_execution(&exec.id).unwrap();
        assert_eq!(after.status, ExecutionStatus::WaitingHuman, "row must be left live");
    }

    /// The core 2026-07-03 fix: a zombie whose workspace dir STILL EXISTS (so
    /// the lost-workspace signal never fires) but whose recorded pane pid is
    /// dead — the pane died on an app restart, its dir + cube lease survived —
    /// is reconciled via the restart-robust pid probe and emits an
    /// `execution_liveness_reconcile` event naming the signal.
    #[tokio::test]
    async fn reconciles_zombie_whose_pane_pid_is_dead() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        // Workspace dir exists → the lost-workspace signal is silent.
        let real_dir = TempDir::new().unwrap();
        let exec = parked_triage_execution(&db, &automation, real_dir.path().to_str().unwrap(), "local");
        seed_dispatch_run(&db, &automation, &exec.id, 1_700_000_000);
        // The app reported a pane pid before the pane died; persist it as a pid
        // that no longer exists.
        assert!(db.set_run_worker_shell_pid_for_execution(&exec.id, dead_pid()).unwrap());

        let sink = RecordingDispatchEventSink::new();
        let reconciled = reconcile_if_execution_dead(&db, &sink, &exec).await;
        assert!(reconciled, "a zombie whose pane pid is dead must be reconciled");

        let after = db.get_execution(&exec.id).unwrap();
        assert_eq!(after.status, ExecutionStatus::Orphaned);

        let events = sink.events_for(&exec.id).await;
        let ev = events
            .iter()
            .find(|e| e.stage == "execution_liveness_reconcile")
            .unwrap_or_else(|| panic!("expected execution_liveness_reconcile event; got {events:#?}"));
        assert_eq!(ev.details.get("reason").and_then(|v| v.as_str()), Some("pane_pid_dead"));
        assert_eq!(
            ev.details.get("prior_status").and_then(|v| v.as_str()),
            Some("waiting_human")
        );

        // Triage bookkeeping is finalized off the pessimistic placeholder.
        let runs = db.list_automation_runs(&automation).unwrap();
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP);
        assert!(runs[0].finished_at.is_some());
    }

    /// The stalled-at-run_started shape: no pane pid was ever reported and the
    /// run has been live past the pane-attach deadline — the pane never came
    /// up. Reconciled via the age signal (the case every pid-driven reaper
    /// skips because `shell_pid <= 0`).
    #[tokio::test]
    async fn reconciles_zombie_whose_pane_never_attached() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let real_dir = TempDir::new().unwrap();
        let exec = parked_triage_execution(&db, &automation, real_dir.path().to_str().unwrap(), "local");
        seed_dispatch_run(&db, &automation, &exec.id, 1_700_000_000);
        // No pid ever persisted. Advance the injected clock past the deadline.
        let now = started_epoch(&exec) + PANE_ATTACH_DEADLINE_SECS + 1;

        let sink = RecordingDispatchEventSink::new();
        let reconciled = reconcile_if_execution_dead_at(&db, &sink, &exec, now).await;
        assert!(reconciled, "a zombie whose pane never attached must be reconciled");

        assert_eq!(db.get_execution(&exec.id).unwrap().status, ExecutionStatus::Orphaned);
        let events = sink.events_for(&exec.id).await;
        let ev = events
            .iter()
            .find(|e| e.stage == "execution_liveness_reconcile")
            .unwrap_or_else(|| panic!("expected execution_liveness_reconcile event; got {events:#?}"));
        assert_eq!(
            ev.details.get("reason").and_then(|v| v.as_str()),
            Some("pane_never_attached")
        );
    }

    /// A live pane (its recorded pid is our own, guaranteed-alive process) is
    /// NEVER reaped, even long past the attach deadline. The cardinal safety
    /// property: this reconciler must never steal a live worker's workspace.
    #[tokio::test]
    async fn leaves_execution_whose_pane_pid_is_alive() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let real_dir = TempDir::new().unwrap();
        let exec = parked_triage_execution(&db, &automation, real_dir.path().to_str().unwrap(), "local");
        assert!(
            db.set_run_worker_shell_pid_for_execution(&exec.id, std::process::id() as i64)
                .unwrap()
        );
        let now = started_epoch(&exec) + PANE_ATTACH_DEADLINE_SECS + 10_000;

        let sink = NoopDispatchEventSink;
        let reconciled = reconcile_if_execution_dead_at(&db, &sink, &exec, now).await;
        assert!(!reconciled, "a live-pane execution must never be reaped");
        assert_eq!(
            db.get_execution(&exec.id).unwrap().status,
            ExecutionStatus::WaitingHuman
        );
    }

    /// A just-spawned worker whose pid RPC is still in flight (no pid yet, but
    /// within the attach deadline) must be given time, not reaped.
    #[tokio::test]
    async fn leaves_no_pid_execution_within_attach_deadline() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let real_dir = TempDir::new().unwrap();
        let exec = parked_triage_execution(&db, &automation, real_dir.path().to_str().unwrap(), "local");
        let now = started_epoch(&exec) + PANE_ATTACH_DEADLINE_SECS - 1;

        let sink = NoopDispatchEventSink;
        let reconciled = reconcile_if_execution_dead_at(&db, &sink, &exec, now).await;
        assert!(
            !reconciled,
            "a fresh worker within the attach deadline must not be reaped"
        );
        assert_eq!(
            db.get_execution(&exec.id).unwrap().status,
            ExecutionStatus::WaitingHuman
        );
    }

    #[tokio::test]
    async fn never_reaps_remote_execution_on_local_probe() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        // Remote host: the missing local path must NOT trigger a reap.
        let exec = parked_triage_execution(&db, &automation, "/remote/only/path/mono-agent-036", "remote-1");

        let sink = NoopDispatchEventSink;
        let reconciled = reconcile_if_execution_dead(&db, &sink, &exec).await;
        assert!(
            !reconciled,
            "a remote worker must never be reaped by a local filesystem probe"
        );
        let after = db.get_execution(&exec.id).unwrap();
        assert_eq!(after.status, ExecutionStatus::WaitingHuman);
    }
}
