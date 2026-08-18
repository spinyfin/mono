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
//! It closes both sides of the gap by making pane liveness **durable and
//! restart-robust**.
//! The app reports the real shell pid via `UpdateWorkerShellPid`, which the
//! engine now persists to `work_runs.shell_pid` (see
//! [`crate::work::WorkDb::set_run_shell_pid_for_execution`]). This sweep reads
//! that DB pid — NOT the in-memory registry — and probes it with the same
//! `kill(pid, 0)` primitive [`crate::dead_pid_sweep`] uses.
//!
//! A non-terminal LOCAL execution whose durable shell pid reports `ESRCH`
//! ("no such process") is finalized through the proper terminal path
//! ([`crate::work::WorkDb::mark_execution_orphaned`], which stamps
//! `finished_at` and orphans its runs, deliberately **preserving** the cube
//! lease + workspace so the redispatch can resume the interrupted work in
//! place). Triage automation-run bookkeeping is finalized the same way
//! `lost_workspace_sweep` does, and a `pane_death_reconcile` trace event is
//! emitted.
//!
//! The inverse contradiction is a terminal execution whose recent durable pid
//! is still alive. Terminalization has already removed that run from the live
//! registry, so this same durable scan hands it to
//! [`crate::worker_readoption::LiveWorkerConvergence`]. Inferred terminal state
//! is re-adopted while the work item is still active; deliberate terminal state,
//! superseded runs, and inferred runs whose work item has closed are fully
//! released through the canonical pane/process/pool/workspace teardown.
//!
//! The same [`reconcile_if_pane_dead`] routine is called inline by the
//! redundant-spawn guard so a dead-pane zombie never blocks a spawn even
//! between sweep passes.
//!
//! ## Safety — only ever acts on positive process evidence
//!
//! Every action requires a definitive probe result on a pid the app actually
//! reported. It never acts on absence of information:
//!
//! - **Host safety**: [`crate::work::WorkDb::latest_local_shell_pid_for_execution`]
//!   returns a pid ONLY for a `host_id = 'local'` run — a local pid probe is
//!   meaningless for a remote worker, so remote runs surface no pid and are
//!   never touched here.
//! - **No pid → skip**: an execution whose pid was never reported (surface
//!   never attached, or a pre-fix spawn) yields `None` and is left alone.
//! - **Non-terminal rows require death**: only `Gone` (ESRCH) enters the orphan
//!   path; every ambiguous result is skipped.
//! - **Terminal rows require recent liveness**: only `Alive`, from a run inside
//!   the bounded pid-trust window, enters two-way convergence. The reap half
//!   repeats that bounded probe before signalling the process group.
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

use crate::coordinator::{CubeClient, ExecutionCoordinator};
use crate::dispatch_events::{DispatchEventSink, Stage};
use crate::durable_liveness::WorkerProcess;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;
use crate::worker_readoption::LiveWorkerConvergence;

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
    /// Terminal executions whose live durable process was handed to the
    /// two-way re-adopt/reap convergence policy.
    pub terminal_handoffs: usize,
}

impl crate::sweep_loop::SweepOutcome for PaneDeathSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0 || self.terminal_handoffs > 0
    }

    fn log(&self) {
        tracing::info!(
            reaped = self.reaped,
            terminal_handoffs = self.terminal_handoffs,
            "pane-death sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`, firing
/// immediately on spawn so pre-restart dead-pane zombies clear on boot.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    convergence: Arc<dyn LiveWorkerConvergence>,
    cube_client: Arc<dyn CubeClient>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        let convergence = Arc::clone(&convergence);
        let cube_client = Arc::clone(&cube_client);
        async move {
            run_one_pass(
                work_db.as_ref(),
                coordinator,
                dispatch_events.as_ref(),
                convergence.as_ref(),
                cube_client.as_ref(),
            )
            .await
        }
    })
}

/// Reconcile both durable contradictions: non-terminal executions whose pane
/// process is gone, and recent terminal executions whose process is alive.
pub async fn run_one_pass(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    convergence: &dyn LiveWorkerConvergence,
    cube_client: &dyn CubeClient,
) -> PaneDeathSweepOutcome {
    let mut outcome = PaneDeathSweepOutcome::default();
    let now_epoch_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    let live_states = coordinator.live_worker_states();

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

    for execution in candidates {
        if reconcile_if_pane_dead(work_db, dispatch_events, &execution, now_epoch_secs, live_states).await {
            outcome.reaped += 1;
            // `mark_execution_orphaned` deliberately leaves the cube lease
            // columns intact on the row (a resume redispatch may reclaim the
            // same workspace with its in-flight commits). But this sweep's
            // redispatch path is the plain orphan-active sweep, which
            // creates an entirely fresh execution with no memory of the old
            // lease, so a row reaped here that is never resumed in place
            // leaves its lease held forever — the "leases leak durably" half
            // of the false-reap incident, where a workspace stayed leased to
            // a terminal execution for 10+ hours. Best-effort force-release
            // it now, mirroring `lost_workspace_sweep::run_one_pass`'s
            // identical release for its own dead-worker reap. Failure is
            // benign: the lease may already be gone.
            if let Some(lease_id) = execution.cube_lease_id.as_deref()
                && let Err(err) = cube_client
                    .force_release_lease(lease_id, Some("pane-death reconcile: worker pane gone"))
                    .await
            {
                tracing::debug!(
                    execution_id = %execution.id,
                    lease_id,
                    error = %format!("{err:#}"),
                    "pane-death sweep: best-effort lease force-release failed (likely already released)",
                );
            }
        }
    }

    // A terminal row with a live process is the opposite contradiction: the
    // engine declared the run over, but the OS still sees its worker. Scan
    // durable state rather than the live registry (which terminalization
    // clears), and hand the fact to the existing two-way policy. A bounded
    // probe is mandatory because convergence may signal the recorded process
    // group; an old recycled pid is not evidence.
    let terminal_candidates = match work_db.list_recent_terminal_executions_with_local_shell_pid(
        crate::durable_liveness::REDISPATCH_PID_TRUST_SECS,
        now_epoch_secs,
    ) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "pane-death sweep: failed to list terminal executions with recent pids; skipping terminal convergence",
            );
            Vec::new()
        }
    };
    for execution in terminal_candidates {
        let process = crate::durable_liveness::probe_execution_worker_within(
            work_db,
            &execution.id,
            crate::durable_liveness::REDISPATCH_PID_TRUST_SECS,
            now_epoch_secs,
        );
        if !process.is_alive() {
            continue;
        }
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            execution_status = %execution.status,
            shell_pid = process.alive_pid(),
            "pane-death sweep: terminal execution still has a live worker; handing off to convergence",
        );
        convergence
            .converge_live_worker(&execution.id, "durable_state_scan")
            .await;
        outcome.terminal_handoffs += 1;
    }

    if outcome.reaped > 0 {
        // A cleared zombie unblocks the redundant-spawn guard for its work
        // item; kick the scheduler so the resume redispatch (which reclaims
        // the preserved workspace) happens immediately.
        coordinator.kick();
    }

    outcome
}

/// Durable, restart-robust evidence that a LOCAL execution's worker shell pid
/// is dead (`kill(pid, 0) == ESRCH`), gated by `is_live()` and the grace
/// window. Returns `Some(shell_pid)` only on positive evidence of death;
/// `None` on anything ambiguous (not live, still within the grace window, no
/// durable pid, or alive/EPERM/unexpected errno).
///
/// This is the **exclusive** implementation of the pid-death signal — see the
/// module doc on "exactly one reaper per death signal". [`reconcile_if_pane_dead`]
/// and `cube_lease_heartbeat::db_fallback_death_evidence` both call this
/// rather than re-probing, so a future change to the liveness rules (a wider
/// grace window, a new evidence source) lands here once instead of drifting
/// across call sites.
pub(crate) async fn shell_pid_death_evidence(
    work_db: &WorkDb,
    execution: &WorkExecution,
    now_epoch_secs: i64,
    live_states: Option<&LiveWorkerStateRegistry>,
) -> Option<i64> {
    // Only reconcile states where the design expects a LIVE pane to still be
    // holding the workspace: `running` (a pr_review reviewer pane) and
    // `waiting_human` (the post-spawn park state) — exactly `is_live()`. A
    // dead pid in any OTHER non-terminal state is EXPECTED, not a zombie: a
    // `waiting_review`/`waiting_merge` execution's worker has already finished
    // its job, created its PR, and exited, so its shell pid is dead by design.
    // Reaping those would falsely orphan work that is correctly parked awaiting
    // a human. (Terminal states are covered by `is_live()` being false too.)
    if !execution.status.is_live() {
        return None;
    }

    // Grace guard: skip executions dispatched too recently (or with no
    // `started_at`) so a worker whose pid is still settling is never raced.
    let started_epoch = execution.started_epoch();
    let started_epoch = match started_epoch {
        Some(t) if now_epoch_secs - t >= PANE_DEATH_GRACE_SECS => t,
        _ => return None,
    };

    // The durable, restart-robust liveness signal: the shell pid the app
    // reported, persisted to `work_runs.shell_pid`, probed through the shared
    // primitive so this sweep and the re-dispatch/re-adoption paths cannot
    // drift apart on what "the worker's process is gone" means.
    //
    // The lookup behind it is ALSO the host-safety gate — it returns a pid only
    // for a `host_id = 'local'` run, so a remote worker (whose pid lives on
    // another machine, where a local `kill(pid, 0)` is meaningless) surfaces
    // `Unknown` and is never touched here.
    //
    // Only `Gone` (ESRCH, "no such process") is positive evidence of death.
    // `Alive`, alive-but-not-ours (EPERM, folded into `Alive`), and `Unknown`
    // — remote, never reported, a pre-fix spawn, an unexpected errno, or a
    // failed read — all leave the execution alone, so pid recycling can only
    // ever cause a missed reap, never a false one. The unbounded probe is the
    // right one here: this path's verb is "reconcile a row", not "signal a
    // process", and its failure direction is to decline.
    let process = crate::durable_liveness::probe_execution_worker(work_db, &execution.id);

    // Corroborate a `Gone` verdict against the (in-memory) live-worker
    // registry before trusting it. Unlike `dead_pid_sweep`, this probe reads
    // `work_runs.shell_pid` — a *different*, restart-robust identity from the
    // registry's tracked pty foreground pid — but it is the same class of
    // fragile signal: a wrapper shell that exited/exec'd, or a reused pid,
    // makes `ESRCH` NOT proof the worker is dead. This is the root cause of
    // the "live workers false-reaped as orphaned" incident: this probe had no
    // corroboration at all, so it terminalized a demonstrably-live worker
    // 45ms after `dead_pid_sweep`'s corroborated probe on the very same
    // execution correctly declined to. `live_states` is `None` only when no
    // registry was wired up (a test, or a call site with no live-state
    // access); in that case the sweep falls back to trusting the bare probe,
    // exactly its pre-fix behavior.
    let process = match live_states {
        Some(live) => {
            let (process, reason) = crate::durable_liveness::corroborate_against_live_registry(
                process,
                live,
                &execution.id,
                started_epoch,
                now_epoch_secs,
            );
            if let Some(reason) = reason {
                tracing::warn!(
                    execution_id = %execution.id,
                    corroborating_activity = %reason,
                    "pane-death reconcile: durable shell pid probed dead but worker is demonstrably \
                     alive for this execution; the tracked pid was a transient/reused identity — \
                     NOT reaping (live-event false-reap guard)",
                );
            }
            process
        }
        None => process,
    };

    let shell_pid = match process {
        WorkerProcess::Gone { shell_pid } => i64::from(shell_pid),
        _ => return None,
    };

    Some(shell_pid)
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
    live_states: Option<&LiveWorkerStateRegistry>,
) -> bool {
    let Some(shell_pid) = shell_pid_death_evidence(work_db, execution, now_epoch_secs, live_states).await else {
        return false;
    };

    let prior_status = execution.status.as_str();

    // Snapshot any uncommitted workspace work to a durable patch before the
    // workspace becomes eligible for resume/reset. Best-effort: a no-op-safe
    // call mirroring the other reap paths.
    let recovery_patch = boss_engine_recovery::recovery_backup::backup_dead_execution(execution);

    // State only what was observed. This previously asserted "pane died with
    // its host app", which is a hardcoded causal claim this code never
    // verifies — it does not check whether the app is running. When five live
    // workers were killed by `husk_pane_sweep` while the app stayed up
    // throughout, this line appeared in the record for each of them and
    // pointed diagnosis squarely at an app crash that never happened. The pid
    // being gone is the observation; why it is gone is not known here.
    let reason = format!(
        "pane-death reconcile: worker shell pid {shell_pid} no longer exists (kill(0)=ESRCH); \
         cause not determined by this probe — the process may have exited on its own, or been \
         killed by another engine path such as a husk-pane retirement (prior status `{prior_status}`)"
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
    use crate::work::{AutomationFireRecord, WorkDb};
    use boss_protocol::{
        AUTOMATION_OUTCOME_FAILED_GAVE_UP, AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AUTOMATION_OUTCOME_PRODUCED_TASK,
        ExecutionStatus, FinishExecutionRunInput,
    };

    /// Test-local alias for the shared epoch helper the production code uses.
    fn now_epoch_secs() -> i64 {
        boss_engine_utils::epoch_time::now_epoch_secs()
    }

    fn create_automation(db: &WorkDb, product_id: &str) -> String {
        seed_daily_automation(db, product_id).id
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
        let reconciled = reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await;
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

    /// The double-finalize bug this closes: a triage execution whose Stop
    /// hook already finalized it (via `complete_pane_parked_execution`, the
    /// production finalizer's completion write) must be `completed` —
    /// terminal — and therefore invisible to this sweep, even though its
    /// durable shell pid is (by then, entirely expectedly) dead. Before the
    /// fix, the finalizer's loop over `active_run_ids_for_execution` found no
    /// still-open run (`PaneSpawnRunner` already closed the only run row at
    /// spawn-confirm time), so it silently left the row `waiting_human`
    /// forever — and this sweep would "reconcile" it a second time, stamping
    /// a misleading pane-died detail over the true finalize outcome.
    #[tokio::test]
    async fn finalized_triage_execution_is_invisible_to_the_sweep() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-finalized", "local", Some(dead_pid()));
        seed_dispatch_run(&db, &automation, &exec.id, 1_700_000_000);
        assert_eq!(exec.status, ExecutionStatus::WaitingHuman);

        // Simulate the Stop-driven finalizer's completion write — exactly
        // what `finalize_automation_triage` does today via
        // `complete_pane_parked_execution`.
        let completed = db
            .complete_pane_parked_execution(&exec.id, "completed", Some("automation triage: produced_task"))
            .unwrap();
        assert!(completed.is_some(), "a live execution must be completed");
        let after_finalize = db.get_execution(&exec.id).unwrap();
        assert_eq!(after_finalize.status, ExecutionStatus::Completed);
        assert!(after_finalize.finished_at.is_some());

        // The pane-death sweep must now be a no-op: `is_live()` gates the
        // already-terminal row out before the (still-dead) shell pid is even
        // probed, so the misleading pane-died reconcile never fires.
        let sink = NoopDispatchEventSink;
        let reconciled = reconcile_if_pane_dead(&db, &sink, &after_finalize, now_epoch_secs(), None).await;
        assert!(
            !reconciled,
            "a finalized triage execution must be invisible to the pane-death sweep"
        );
        let final_state = db.get_execution(&exec.id).unwrap();
        assert_eq!(
            final_state.status,
            ExecutionStatus::Completed,
            "the sweep must not re-finalize (and overwrite) an already-completed execution"
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
        assert!(reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await);

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

        let task_id = create_test_chore_manual(&db, product.as_str(), "produced by triage").id;
        db.stamp_task_source_automation_for_test(&task_id, &automation, "todo")
            .unwrap();

        let sink = NoopDispatchEventSink;
        assert!(reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await);

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
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await,
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
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await,
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
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await,
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
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await,
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
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await,
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
        assert!(!reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await);
    }

    /// Create a `waiting_human` chore execution on the `codex` driver whose
    /// durable shell pid is dead.
    fn parked_codex_execution(db: &WorkDb, boundary: Option<&str>) -> WorkExecution {
        let product = create_product(db);
        let work_item_id = create_active_chore(db, &product, "codex chore");
        db.update_work_item(
            &work_item_id,
            crate::work::WorkItemPatch {
                driver: Some("codex".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution_id = create_old_execution(db, &work_item_id);
        let (_exec, run) = db
            .start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws-codex")
            .unwrap();
        db.set_run_shell_pid_for_execution(&execution_id, dead_pid() as i64)
            .unwrap();
        if let Some(at) = boundary {
            db.record_run_turn_boundary_for_execution(&execution_id, at).unwrap();
        }
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&execution_id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::WaitingHuman)
                .run_status("completed")
                .build(),
        )
        .unwrap();
        db.force_started_at_for_test(&execution_id, now_epoch_secs() - PANE_DEATH_GRACE_SECS - 300)
            .unwrap();
        db.get_execution(&execution_id).unwrap()
    }

    /// Codex is now `Persistent` (the bare interactive TUI pivot — see
    /// `docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`), so a
    /// dead durable pid on a `waiting_human` Codex row is a genuine dead
    /// pane, delivered turn boundary or not: a dead durable pid is a dead
    /// pane, the turn-boundary record is not consulted, and reconciliation
    /// runs exactly as it already did for Claude.
    #[tokio::test]
    async fn a_finished_persistent_codex_worker_is_still_reconciled_as_a_dead_pane() {
        let (_d, db) = open_db();
        let exec = parked_codex_execution(&db, Some("2026-07-28T00:16:58Z"));

        let sink = NoopDispatchEventSink;
        assert!(
            reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await,
            "a persistent-driver worker's dead pid is a genuine dead pane, boundary or not",
        );
        assert_eq!(db.get_execution(&exec.id).unwrap().status, ExecutionStatus::Orphaned);
    }

    /// No delivered turn boundary is also still reconciled — unchanged by
    /// the `Persistent` flip, since this path never consulted the
    /// turn-boundary record in the first place.
    #[tokio::test]
    async fn a_worker_whose_run_delivered_no_turn_boundary_is_still_reconciled() {
        let (_d, db) = open_db();
        let exec = parked_codex_execution(&db, None);

        let sink = NoopDispatchEventSink;
        assert!(
            reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), None).await,
            "a worker with no delivered turn boundary is a genuine dead pane",
        );
        assert_eq!(db.get_execution(&exec.id).unwrap().status, ExecutionStatus::Orphaned);
    }

    // ─── corroboration (the "live workers false-reaped" incident) ─────────────
    //
    // Before this fix `reconcile_if_pane_dead` had NO corroboration at all —
    // unlike `dead_pid_sweep`, which already checked recent hook activity
    // before trusting a `Dead` verdict. That asymmetry is exactly the
    // incident: two sweeps consumed the same underlying "the tracked pid is
    // gone" signal 45ms apart in one tick, `dead_pid_sweep`'s corroborated
    // probe correctly declined to reap, and this sweep's uncorroborated one
    // terminalized the same, demonstrably-live execution anyway — which then
    // got a second, duplicate worker dispatched onto it. These tests are the
    // representative fixture for that class of same-tick occurrence.

    /// A worker whose durable shell pid probes dead, but which has emitted a
    /// hook well within the corroboration window, must NOT be terminalized —
    /// the tracked pid was a transient/reused identity, not proof of death.
    #[tokio::test]
    async fn recent_hook_activity_spares_a_falsely_probed_dead_pane() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-corrob", "local", Some(dead_pid()));
        seed_dispatch_run(&db, &automation, &exec.id, 1_700_000_000);

        let live_states = LiveWorkerStateRegistry::new();
        live_states.register_spawn(1, &exec.id, "claude-opus-4-7", 424242, None);
        use boss_protocol::WorkerEvent;
        live_states.apply_event(
            1,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        live_states.apply_event(
            1,
            &WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );

        let sink = NoopDispatchEventSink;
        let reconciled = reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), Some(&live_states)).await;
        assert!(
            !reconciled,
            "a worker with recent hook activity must not be reaped even when its tracked pid probes dead",
        );
        assert_eq!(
            db.get_execution(&exec.id).unwrap().status,
            ExecutionStatus::WaitingHuman,
            "the execution must stay untouched, not terminalized",
        );
    }

    /// A tool in flight (a long foreground build with no hook for minutes)
    /// also corroborates — mirrors the incident's "tool `Bash` in flight"
    /// same-tick occurrences.
    #[tokio::test]
    async fn tool_in_flight_spares_a_falsely_probed_dead_pane() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-corrob-tool", "local", Some(dead_pid()));

        let live_states = LiveWorkerStateRegistry::new();
        live_states.register_spawn(1, &exec.id, "claude-opus-4-7", 424242, None);
        use boss_protocol::WorkerEvent;
        live_states.apply_event(
            1,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );

        let sink = NoopDispatchEventSink;
        assert!(
            !reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), Some(&live_states)).await,
            "a tool in flight must spare the worker from the pane-death reap",
        );
    }

    /// A genuinely dead worker — no live-state entry at all for its
    /// execution id — is still reaped: corroboration narrows the false-reap
    /// window, it does not disable reaping.
    #[tokio::test]
    async fn no_live_state_entry_still_reaps_a_genuinely_dead_pane() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-corrob-dead", "local", Some(dead_pid()));

        let live_states = LiveWorkerStateRegistry::new();
        let sink = NoopDispatchEventSink;
        assert!(
            reconcile_if_pane_dead(&db, &sink, &exec, now_epoch_secs(), Some(&live_states)).await,
            "with no corroborating activity the worker must still be reaped",
        );
        assert_eq!(db.get_execution(&exec.id).unwrap().status, ExecutionStatus::Orphaned);
    }

    // ─── cube lease release on reap ────────────────────────────────────────────

    /// Records every `force_release_lease` call; every other [`CubeClient`]
    /// method is unreachable from this sweep.
    #[derive(Default)]
    struct RecordingCube {
        force_releases: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingCube {
        fn force_release_calls(&self) -> Vec<String> {
            self.force_releases.lock().unwrap().clone()
        }
    }

    crate::stub_cube_client! { RecordingCube {
        async fn force_release_lease(&self, lease_id: &str, _: Option<&str>) -> anyhow::Result<()> {
            self.force_releases.lock().unwrap().push(lease_id.to_owned());
            Ok(())
        }
    } }

    /// `run_one_pass` reaping a dead-pane zombie must force-release its cube
    /// lease. `mark_execution_orphaned` deliberately leaves the lease columns
    /// on the row (a resume redispatch may reclaim the workspace), but this
    /// sweep's actual redispatch path (the plain orphan-active sweep) never
    /// resumes in place — it dispatches an entirely fresh execution — so a
    /// row reaped without this release leaks its lease forever. This is the
    /// "leases leak durably" half of the false-reap incident.
    #[tokio::test]
    async fn run_one_pass_force_releases_the_reaped_executions_lease() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = parked_triage_execution(&db, &automation, "/tmp/ws-lease", "local", Some(dead_pid()));
        assert_eq!(
            exec.cube_lease_id.as_deref(),
            Some("lease-1"),
            "precondition: the parked execution carries a lease to release",
        );
        let db = Arc::new(db);

        let coordinator = make_coordinator(db.clone(), 1);
        let cube = RecordingCube::default();
        let sink = NoopDispatchEventSink;
        let convergence = crate::worker_readoption::NoopLiveWorkerConvergence;

        let outcome = run_one_pass(db.as_ref(), coordinator, &sink, &convergence, &cube).await;

        assert_eq!(outcome.reaped, 1);
        assert_eq!(
            cube.force_release_calls(),
            vec!["lease-1".to_owned()],
            "the reaped execution's cube lease must be force-released",
        );
    }
}
