//! Restart-robust reconciler for executions whose worker pane died with its
//! host app — the 2026-07-04 "app relaunch killed live panes" wedge.
//!
//! ## Why this exists
//!
//! A libghostty worker pane is a child of the macOS app process. When the app
//! relaunches (an update, a crash, an operator restart) every live worker's
//! shell dies with it — but the engine's `work_executions` rows survive, and
//! a pane worker parks in `waiting_human` the instant it spawns (the normal
//! post-spawn state; the row only leaves it when the worker's `Stop` hook
//! fires). A worker killed mid-run never fires `Stop`, so the row sits
//! `waiting_human` forever. Every existing safety net is blind to this exact
//! shape:
//!
//! - The app never tells the engine a pane died — there is no pane-died RPC.
//! - The cube lease stays green: the engine's own [`crate::cube_lease_heartbeat`]
//!   DB-fallback sweep renews the lease of every in-flight row, so the
//!   heartbeat-failure auto-reap (T2168) never fires for a dead-but-leased
//!   pane.
//! - The workspace directory survives, so [`crate::lost_workspace_sweep`]
//!   (which keys on the cwd being gone) never fires.
//! - [`crate::dead_pid_sweep`] *could* catch it — it probes the shell pid with
//!   `kill(pid, 0)` — but it is driven by the in-memory
//!   [`crate::live_worker_state::LiveWorkerStateRegistry`], which is EMPTY
//!   after an engine restart. An app relaunch that also restarts the engine
//!   (e.g. an app update) therefore wipes the only signal `dead_pid_sweep`
//!   has.
//! - The startup reconciler ([`crate::run_reconcile`]) only consults the cube
//!   lease, which is still green → verdict `Live` → never reconciled. This is
//!   why two clean engine restarts over the incident's zombies left them
//!   `waiting_human`.
//!
//! With the row stuck `waiting_human`, the redundant-spawn guard
//! (`schedule_execution`) refuses every subsequent spawn for that work item
//! with `redundant_spawn`, and the automation is permanently wedged.
//!
//! ## What it does
//!
//! It closes the gap by making pane liveness **durable and restart-robust**.
//! The app reports the real shell pid via `UpdateWorkerShellPid`, which the
//! engine now persists to `work_runs.shell_pid` (see
//! [`crate::work::WorkDb::set_run_shell_pid_for_execution`]). This sweep reads
//! that DB pid — NOT the in-memory registry — and probes it with the same
//! `kill(pid, 0)` primitive [`crate::dead_pid_sweep`] uses. A non-terminal
//! LOCAL execution whose durable shell pid reports `ESRCH` ("no such process")
//! is finalized through the proper terminal path
//! ([`crate::work::WorkDb::mark_execution_orphaned`], which stamps
//! `finished_at` and orphans its runs, deliberately **preserving** the cube
//! lease + workspace so the redispatch can resume the interrupted work in
//! place). Triage automation-run bookkeeping is finalized the same way
//! `lost_workspace_sweep` does, and a `pane_death_reconcile` trace event is
//! emitted.
//!
//! The same [`reconcile_if_pane_dead`] routine is called inline by the
//! redundant-spawn guard so a dead-pane zombie never blocks a spawn even
//! between sweep passes.
//!
//! ## Safety — only ever acts on positive evidence of death
//!
//! Every reap requires a `kill(pid, 0) == ESRCH` result on a pid the app
//! actually reported. It never reaps on absence of information:
//!
//! - **Host safety**: [`crate::work::WorkDb::latest_local_shell_pid_for_execution`]
//!   returns a pid ONLY for a `host_id = 'local'` run — a local pid probe is
//!   meaningless for a remote worker, so remote runs surface no pid and are
//!   never touched here.
//! - **No pid → skip**: an execution whose pid was never reported (surface
//!   never attached, or a pre-fix spawn) yields `None` and is left alone.
//! - **Alive / not-ours / probe-error → skip**: only `Dead` (ESRCH) reaps;
//!   `EPERM` (alive, not ours) and any other errno are treated conservatively
//!   as alive. Pid recycling can therefore only ever cause a *missed* reap
//!   (self-healing on a later pass), never a false one against a live worker.
//! - **Grace window** ([`PANE_DEATH_GRACE_SECS`]): an execution whose
//!   `started_at` is within the grace (or unset) is skipped, so a
//!   just-dispatched worker whose pid is still settling is never raced.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same pattern as
//! the other sweeps), so a pane killed by an app/engine relaunch is
//! reconciled — and its work resumed — within seconds of the next engine
//! start, without any hand-editing of the DB.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::WorkExecution;

use crate::coordinator::ExecutionCoordinator;
use crate::dead_pid_sweep::{PidStatus, probe_pid};
use crate::dispatch_events::{DispatchEventSink, Stage};
use crate::work::WorkDb;

/// Cadence for the periodic pass. Fires immediately on boot, then every
/// interval — fast enough that a pane killed mid-run is cleared and its work
/// resumed long before the next 15-minute automation fire.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Grace period after `started_at` (epoch seconds) during which a dead pid is
/// left alone. Comfortably above the app's shell-pid-report window (a single
/// 250ms retry after the surface attaches) so a worker whose pid is merely
/// still settling is never raced. Mirrors
/// [`crate::dead_pid_sweep::DEAD_PID_GRACE_SECS`]'s intent with extra headroom.
pub const PANE_DEATH_GRACE_SECS: i64 = 60;

/// Counts from one pass; logged at `info` when any reaping occurred.
#[derive(Debug, Default)]
pub struct PaneDeathSweepOutcome {
    pub reaped: usize,
}

impl crate::sweep_loop::SweepOutcome for PaneDeathSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }

    fn log(&self) {
        tracing::info!(reaped = self.reaped, "pane-death sweep: pass complete");
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`, firing
/// immediately on spawn so pre-restart dead-pane zombies clear on boot.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        async move { run_one_pass(work_db.as_ref(), Arc::clone(&coordinator), dispatch_events.as_ref()).await }
    })
}

/// Run a single pane-death reconciliation pass over every non-terminal
/// execution that recorded a workspace path (the live-with-a-pane set).
pub async fn run_one_pass(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
) -> PaneDeathSweepOutcome {
    let mut outcome = PaneDeathSweepOutcome::default();

    let candidates = match work_db.list_non_terminal_executions_with_workspace() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "pane-death sweep: failed to list candidate executions; skipping pass",
            );
            return outcome;
        }
    };

    let now_epoch_secs = crate::run_reconcile::current_epoch_s();
    for execution in candidates {
        if reconcile_if_pane_dead(work_db, dispatch_events, &execution, now_epoch_secs).await {
            outcome.reaped += 1;
        }
    }

    if outcome.reaped > 0 {
        // A cleared zombie unblocks the redundant-spawn guard for its work
        // item; kick the scheduler so the resume redispatch (which reclaims
        // the preserved workspace) happens immediately.
        coordinator.kick();
    }

    outcome
}

/// Finalize `execution` iff it is a non-terminal LOCAL execution whose durable
/// worker shell pid is provably dead (`kill(pid, 0) == ESRCH`). Returns `true`
/// when the row was (or already had been) reconciled to a terminal status;
/// `false` when it is NOT a dead-pane zombie and callers should keep treating
/// it as live.
///
/// `now_epoch_secs` is threaded in so the grace check uses a single clock read
/// per pass (and so tests can pin it). DB-only plus a trace event — no cube or
/// pool dependency — so it can be called both from the periodic
/// [`run_one_pass`] and inline from the coordinator's redundant-spawn guard.
pub async fn reconcile_if_pane_dead(
    work_db: &WorkDb,
    dispatch_events: &dyn DispatchEventSink,
    execution: &WorkExecution,
    now_epoch_secs: i64,
) -> bool {
    // Only reconcile states where the design expects a LIVE pane to still be
    // holding the workspace: `running` (a pr_review reviewer pane) and
    // `waiting_human` (the post-spawn park state) — exactly `is_live()`. A
    // dead pid in any OTHER non-terminal state is EXPECTED, not a zombie: a
    // `waiting_review`/`waiting_merge` execution's worker has already finished
    // its job, created its PR, and exited, so its shell pid is dead by design.
    // Reaping those would falsely orphan work that is correctly parked awaiting
    // a human. (Terminal states are covered by `is_live()` being false too.)
    if !execution.status.is_live() {
        return false;
    }

    // Grace guard: skip executions dispatched too recently (or with no
    // `started_at`) so a worker whose pid is still settling is never raced.
    let started_epoch = execution.started_at.as_deref().and_then(|s| s.parse::<i64>().ok());
    match started_epoch {
        Some(t) if now_epoch_secs - t >= PANE_DEATH_GRACE_SECS => {}
        _ => return false,
    }

    // The durable, restart-robust liveness signal: the shell pid the app
    // reported, persisted to `work_runs.shell_pid`. This lookup is ALSO the
    // host-safety gate — it returns a pid only for a `host_id = 'local'` run, so
    // a remote worker (whose pid lives on another machine, where a local
    // `kill(pid, 0)` is meaningless) surfaces `None` and is never touched here.
    // `None` (remote, never reported, or a pre-fix spawn) means we have no
    // evidence either way → leave it alone.
    let shell_pid = match work_db.latest_local_shell_pid_for_execution(&execution.id) {
        Ok(Some(pid)) if pid > 0 => pid,
        Ok(_) => return false,
        Err(err) => {
            tracing::debug!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "pane-death reconcile: could not read durable shell pid; skipping conservatively",
            );
            return false;
        }
    };

    // Only ESRCH ("no such process") is positive evidence of death. Alive,
    // alive-but-not-ours (EPERM), and any unexpected errno are treated as live
    // — pid recycling can then only ever cause a missed reap, never a false one.
    let probe_pid_i32 = match i32::try_from(shell_pid) {
        Ok(pid) => pid,
        Err(_) => return false,
    };
    if !matches!(probe_pid(probe_pid_i32), PidStatus::Dead) {
        return false;
    }

    let prior_status = execution.status.as_str();

    // Snapshot any uncommitted workspace work to a durable patch before the
    // workspace becomes eligible for resume/reset. Best-effort: a no-op-safe
    // call mirroring the other reap paths.
    let recovery_patch = crate::recovery_backup::backup_dead_execution(execution);

    let reason = format!(
        "pane-death reconcile: worker shell pid {shell_pid} no longer exists (kill(0)=ESRCH); \
         pane died with its host app (prior status `{prior_status}`)"
    );

    // Funnel the orphan → triage-bookkeeping → dispatch-event flow through the
    // shared reconciler finalize so it lives in one place (see
    // `execution_liveness::finalize_gone_execution`). `mark_execution_orphaned`
    // preserves the lease + workspace so the resume redispatch reclaims the
    // interrupted work in place.
    let reconciled = crate::execution_liveness::finalize_gone_execution(
        work_db,
        dispatch_events,
        execution,
        &reason,
        &format!("its worker shell pid {shell_pid} was gone (pane died with the host app)"),
        Stage::PaneDeathReconcile,
        serde_json::json!({
            "reason": "shell_pid_dead",
            "prior_status": prior_status,
            "shell_pid": shell_pid,
            "kind": execution.kind.as_str(),
            "recovery_patch": recovery_patch.as_deref().map(|p| p.display().to_string()),
        }),
    )
    .await;

    if reconciled {
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            prior_status,
            shell_pid,
            "pane-death reconcile: finalized execution whose worker pane is gone",
        );
    }

    reconciled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_events::NoopDispatchEventSink;
    use crate::test_support::*;
    use crate::work::{AutomationFireRecord, CreateChoreInput, WorkDb};
    use boss_protocol::{
        AUTOMATION_OUTCOME_FAILED_GAVE_UP, AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AUTOMATION_OUTCOME_PRODUCED_TASK,
        AutomationTrigger, CreateAutomationInput, ExecutionStatus, FinishExecutionRunInput,
    };

    /// Test-local alias for the shared epoch helper the production code uses.
    fn now_epoch_secs() -> i64 {
        crate::run_reconcile::current_epoch_s()
    }

    fn create_automation(db: &WorkDb, product_id: &str) -> String {
        db.create_automation(
            CreateAutomationInput::builder()
                .product_id(product_id.to_owned())
                .name("daily")
                .trigger(AutomationTrigger::Schedule {
                    cron: "0 14 * * *".to_owned(),
                    timezone: "UTC".to_owned(),
                })
                .standing_instruction("do the thing")
                .build(),
        )
        .unwrap()
        .id
    }

    /// A PID guaranteed not to exist: spawn `true`, wait for it to exit, reuse
    /// its released pid. (Same trick the dead-PID sweep tests use.)
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let _ = child.wait();
        pid
    }

    /// Create a triage execution, start its run on `host` (stamping `host_id` +
    /// `workspace_path`), record `shell_pid` on the run, then park it in
    /// `waiting_human` — reproducing a triage worker whose pane later dies.
    /// `started_at` is forced far enough in the past to clear the grace guard.
    fn parked_triage_execution(
        db: &WorkDb,
        automation_id: &str,
        workspace_path: &str,
        host: &str,
        shell_pid: Option<i32>,
    ) -> WorkExecution {
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
        if let Some(pid) = shell_pid {
            db.set_run_shell_pid_for_execution(&exec.id, pid as i64).unwrap();
        }
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&exec.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::WaitingHuman)
                .run_status("completed")
                .build(),
        )
        .unwrap();
        // Force started_at well before the grace window so the sweep considers it.
        let old = now_epoch_secs() - PANE_DEATH_GRACE_SECS - 300;
        db.force_started_at_for_test(&exec.id, old).unwrap();
        db.get_execution(&exec.id).unwrap()
    }

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

    /// The core invariant: a `waiting_human` execution whose durable shell pid
    /// is dead is reconciled to `orphaned` and its triage bookkeeping finalized.
    #[tokio::test]
    async fn reconciles_waiting_human_zombie_whose_pane_pid_is_dead() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-a", "local", Some(dead_pid()));
        seed_dispatch_run(&db, &automation, &exec.id, 1_700_000_000);
        assert_eq!(exec.status, ExecutionStatus::WaitingHuman);

        let sink = NoopDispatchEventSink;
        let reconciled = reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs()).await;
        assert!(
            reconciled,
            "a waiting_human zombie with a dead pane pid must be reconciled"
        );

        let after = db.get_execution(&exec.id).unwrap();
        assert_eq!(after.status, ExecutionStatus::Orphaned);
        assert!(after.finished_at.is_some(), "reconciled execution must be finalized");

        // The false "dispatched; awaiting …" detail is overwritten with the truth.
        let runs = db.list_automation_runs(&automation).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP);
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

    /// The lease and workspace columns are preserved (NOT cleared) so the
    /// resume redispatch can reclaim the interrupted work in place.
    #[tokio::test]
    async fn preserves_lease_and_workspace_for_resume() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-b", "local", Some(dead_pid()));

        let sink = NoopDispatchEventSink;
        assert!(reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs()).await);

        let after = db.get_execution(&exec.id).unwrap();
        assert_eq!(
            after.cube_lease_id.as_deref(),
            Some("lease-1"),
            "the lease must be preserved so resume reclaims the workspace"
        );
        assert_eq!(after.workspace_path.as_deref(), Some("/tmp/ws-b"));
    }

    /// A triage that created a task before its pane died is recorded as
    /// `produced_task` with the task linked — not silently dropped.
    #[tokio::test]
    async fn records_produced_task_when_triage_made_a_task_before_dying() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-c", "local", Some(dead_pid()));
        seed_dispatch_run(&db, &automation, &exec.id, 1_700_000_000);

        let task_id = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.as_str())
                    .name("produced by triage")
                    .autostart(false)
                    .build(),
            )
            .unwrap()
            .id;
        db.stamp_task_source_automation_for_test(&task_id, &automation, "todo")
            .unwrap();

        let sink = NoopDispatchEventSink;
        assert!(reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs()).await);

        let runs = db.list_automation_runs(&automation).unwrap();
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_PRODUCED_TASK);
        assert_eq!(runs[0].produced_task_id.as_deref(), Some(task_id.as_str()));
    }

    /// A live pid (this test process) is never reaped.
    #[tokio::test]
    async fn leaves_execution_whose_pane_pid_is_alive() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-d", "local", Some(std::process::id() as i32));

        let sink = NoopDispatchEventSink;
        assert!(
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs()).await,
            "an execution whose pane pid is alive must NOT be reconciled"
        );
        assert_eq!(
            db.get_execution(&exec.id).unwrap().status,
            ExecutionStatus::WaitingHuman
        );
    }

    /// No durable pid recorded (never reported / pre-fix spawn) → conservative
    /// skip; we never reap on absence of a pid.
    #[tokio::test]
    async fn leaves_execution_with_no_recorded_pid() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-e", "local", None);

        let sink = NoopDispatchEventSink;
        assert!(
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs()).await,
            "an execution with no recorded shell pid must NOT be reaped"
        );
        assert_eq!(
            db.get_execution(&exec.id).unwrap().status,
            ExecutionStatus::WaitingHuman
        );
    }

    /// A remote worker is never reaped by the local pid probe: the pid lookup
    /// filters `host_id = 'local'`, so a remote run surfaces no pid.
    #[tokio::test]
    async fn never_reaps_remote_execution() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        // Even with a dead-looking pid stored, the remote host must shield it.
        let exec = parked_triage_execution(&db, &automation, "/remote/ws", "remote-1", Some(dead_pid()));

        let sink = NoopDispatchEventSink;
        assert!(
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs()).await,
            "a remote worker must never be reaped by a local pid probe"
        );
        assert_eq!(
            db.get_execution(&exec.id).unwrap().status,
            ExecutionStatus::WaitingHuman
        );
    }

    /// A freshly-dispatched execution (started within the grace window) is
    /// skipped even if its recorded pid is dead — guards against racing a
    /// worker whose pid is still settling.
    #[tokio::test]
    async fn recent_started_at_is_skipped() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-f", "local", Some(dead_pid()));
        // Move started_at to now so the grace guard fires.
        db.force_started_at_for_test(&exec.id, now_epoch_secs()).unwrap();
        let exec = db.get_execution(&exec.id).unwrap();

        let sink = NoopDispatchEventSink;
        assert!(
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs()).await,
            "an execution within the grace window must not be reaped"
        );
    }

    /// A non-live parked state (`waiting_review`) whose worker has exited
    /// normally (dead pid by design) must NOT be reaped — only `running` /
    /// `waiting_human` (where a live pane is expected) are candidates.
    #[tokio::test]
    async fn non_live_status_with_dead_pid_is_skipped() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = db
            .create_automation_triage_execution(&automation, "https://github.com/test/repo")
            .unwrap();
        let (_e, run) = db
            .start_execution_run_on_host(
                &exec.id,
                "auto-worker-1",
                "repo-1",
                "lease-1",
                "mono-agent-028",
                "/tmp/ws-h",
                "local",
            )
            .unwrap();
        db.set_run_shell_pid_for_execution(&exec.id, dead_pid() as i64).unwrap();
        // Park in waiting_review — the worker finished and exited by design.
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&exec.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::WaitingReview)
                .run_status("completed")
                .build(),
        )
        .unwrap();
        db.force_started_at_for_test(&exec.id, now_epoch_secs() - PANE_DEATH_GRACE_SECS - 300)
            .unwrap();
        let exec = db.get_execution(&exec.id).unwrap();
        assert_eq!(exec.status, ExecutionStatus::WaitingReview);

        let sink = NoopDispatchEventSink;
        assert!(
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs()).await,
            "a waiting_review execution (worker exited by design) must not be reaped"
        );
        assert_eq!(
            db.get_execution(&exec.id).unwrap().status,
            ExecutionStatus::WaitingReview
        );
    }

    /// A terminal execution is a no-op.
    #[tokio::test]
    async fn terminal_execution_is_skipped() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-g", "local", Some(dead_pid()));
        db.mark_execution_orphaned(&exec.id, "pre-terminal").unwrap();
        let exec = db.get_execution(&exec.id).unwrap();

        let sink = NoopDispatchEventSink;
        assert!(!reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs()).await);
    }
}
