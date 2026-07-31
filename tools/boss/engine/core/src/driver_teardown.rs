//! Best-effort driver-workspace teardown, called from every execution
//! termination path (normal completion, stop, reap, orphaned/husk recovery,
//! app-crash reconciliation).
//!
//! Mirrors [`crate::driver::AgentDriver::provision_workspace`] with
//! [`crate::driver::AgentDriver::teardown_workspace`]: any driver that
//! creates per-run state outside the cube workspace (a per-worker config
//! dir, a cache dir, a socket, a temp credential file) gets a chance to
//! clean it up wherever the engine considers a run over.
//!
//! The cleanup handle is the opaque [`boss_protocol::DriverRuntimeState`]
//! the driver returned from provision, reloaded from the execution row —
//! never inferred from the engine environment or by scanning a shared
//! provider home. The driver itself is resolved via the registry from
//! the same `tasks.driver` → `products.default_driver` → engine-default
//! precedence used at spawn time ([`WorkDb::get_execution_driver_slug`]).

use std::path::Path;
use std::time::Instant;

use crate::driver::DriverRegistry;
use crate::work::WorkDb;

/// Which termination path invoked [`teardown_driver_workspace`], and why.
///
/// Both the entry and completion traces carry this, so "who tore this
/// execution down?" is answerable from the teardown line itself rather than
/// inferred from whichever sweep happened to log next to it. That matters
/// because ~12 distinct call sites reach this function and the teardown line
/// is frequently the last thing an aborting run emits: without the reason,
/// the coordinator's spawn-failure arm, a mid-spawn cancel, and any of the
/// sweeps are indistinguishable in the log.
///
/// Every variant maps to exactly one call site family. Adding a call site
/// means adding a variant — a call site that cannot name itself is a call
/// site nobody can attribute later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownReason {
    /// `coordinator::run_execution` — `ExecutionRunner::run_execution`
    /// returned `Err` before any pane existed.
    SpawnFailed,
    /// `coordinator::run_execution` — the run was cancelled while its spawn
    /// was still in flight; the runner already reaped the just-spawned pane.
    CancelledDuringSpawn,
    /// `coordinator::record_run_completion` — the run reached a wait state
    /// that releases the workspace.
    RunCompleted,
    /// `completion::finish_worker_teardown`; the payload is that function's
    /// own `path` label (`"no_op"`, `"pr_review"`, `"idle_park"`, …), so the
    /// completion family stays distinguishable from the sweeps without
    /// flattening which completion path it was.
    Completion(&'static str),
    /// `completion::force_release` — `bossctl agents stop`, cascade cancel,
    /// and every other explicit operator-driven teardown.
    ForceRelease,
    /// `ServerState::reap_run` — an operator reaped this run by hand.
    ManualReap,
    /// Engine-startup app-crash reconciliation.
    AppCrashReconcile,
    /// `host_reconcile` — the run's host went offline (disabled, drained, or
    /// removed from the registry).
    HostDrainReconcile,
    /// `terminal_work_sweep` — a live pane outlived its terminal work item.
    TerminalWorkReconcile,
    /// `dead_pid_sweep` — the worker's tracked pid is gone.
    DeadPidReconcile,
    /// `execution_liveness` — the pane died or never attached
    /// (dead-pane / lost-workspace reconcile).
    PaneLivenessReconcile,
    /// `transient_recovery` — the sweep orphaned the run before resuming it.
    TransientRecoveryReap,
    /// `cube_lease_heartbeat` — the lease failed to refresh often enough to
    /// auto-reap the execution.
    CubeLeaseAutoReap,
    /// `remote_lease_reconcile` — a remote worker process was provably gone.
    RemoteLeaseReconcile,
    /// `stale_worker_sweep` — the worker is alive but wedged past threshold.
    StaleWorkerReconcile,
    /// `spawn_ack_sweep` — no pid and no hook event ever arrived.
    SpawnAckTimeout,
}

impl TeardownReason {
    /// Stable snake_case family label for the `reason` trace field. Pair with
    /// [`Self::detail`], which carries the completion path for
    /// [`Self::Completion`] and is empty for every other variant — keeping
    /// the two apart is what stops a completion path label (a free-form
    /// string) from colliding with a reason name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpawnFailed => "spawn_failed",
            Self::CancelledDuringSpawn => "cancelled_during_spawn",
            Self::RunCompleted => "run_completed",
            Self::Completion(_) => "completion",
            Self::ForceRelease => "force_release",
            Self::ManualReap => "manual_reap",
            Self::AppCrashReconcile => "app_crash_reconcile",
            Self::HostDrainReconcile => "host_drain_reconcile",
            Self::TerminalWorkReconcile => "terminal_work_reconcile",
            Self::DeadPidReconcile => "dead_pid_reconcile",
            Self::PaneLivenessReconcile => "pane_liveness_reconcile",
            Self::TransientRecoveryReap => "transient_recovery_reap",
            Self::CubeLeaseAutoReap => "cube_lease_auto_reap",
            Self::RemoteLeaseReconcile => "remote_lease_reconcile",
            Self::StaleWorkerReconcile => "stale_worker_reconcile",
            Self::SpawnAckTimeout => "spawn_ack_timeout",
        }
    }

    /// Sub-label for the `reason_detail` trace field: the completion path for
    /// [`Self::Completion`], empty otherwise.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Completion(path) => path,
            _ => "",
        }
    }
}

/// How a teardown pass ended, for the completion trace's `outcome` field.
/// Every early return in [`resolve_and_teardown`] maps to one of these, so
/// the completion line always says why a teardown that logged `entered` did
/// or did not reach the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeardownOutcome {
    /// The resolved driver's `teardown_workspace` ran and returned `Ok`.
    Ok,
    /// The driver ran and returned an error (already logged in detail).
    DriverError,
    /// No `tasks` row / unknown execution — nothing to resolve.
    NoDriverSlug,
    /// The slug lookup itself failed.
    DriverSlugLookupFailed,
    /// The slug resolved but names no registered driver.
    UnknownDriver,
}

impl TeardownOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::DriverError => "driver_error",
            Self::NoDriverSlug => "no_driver_slug",
            Self::DriverSlugLookupFailed => "driver_slug_lookup_failed",
            Self::UnknownDriver => "unknown_driver",
        }
    }
}

/// Tear down driver-owned, out-of-workspace state for a terminated
/// execution. Never fails the caller: a teardown error is logged and
/// swallowed, since cleanup must not turn an otherwise-successful run into a
/// failure.
///
/// Loads the persisted [`boss_protocol::DriverRuntimeState`] (if any) and
/// the resolved driver slug from `work_db`, then hands both to the
/// registry-resolved driver. `workspace_path` is `None` when the
/// execution's workspace path was never recorded or was already cleared
/// by a racing teardown — callers must still call this unconditionally
/// in that case, since a driver may key its out-of-workspace state by
/// the recorded runtime state alone (e.g. a per-worker `CODEX_HOME`).
///
/// `reason` names the calling termination path; see [`TeardownReason`].
///
/// Emits a matched pair of traces — `entered` and `completed` — around every
/// path through the body, including the early returns. The pairing is the
/// point: with only an entry line, a teardown still running is
/// indistinguishable from one that finished, which is exactly what made the
/// 2026-07-31 abort read as "the engine stopped here" when it had in fact
/// finished this function in single-digit milliseconds and gone on to block
/// in the caller's `cube workspace release`.
pub async fn teardown_driver_workspace(
    work_db: &WorkDb,
    execution_id: &str,
    workspace_path: Option<&Path>,
    reason: TeardownReason,
) {
    #[cfg(test)]
    test_hooks::record_call();

    // Entry-level trace, unconditional and before any early return below, so
    // "did teardown run for this execution?" is answerable directly from the
    // trace instead of being inferred from a driver's own outcome log (e.g.
    // Codex's "teardown adopt finished" line, which never fires if this
    // function itself is never reached).
    tracing::info!(
        execution_id,
        reason = reason.as_str(),
        reason_detail = reason.detail(),
        workspace_path = ?workspace_path.map(Path::display),
        "driver workspace teardown: entered",
    );

    let started = Instant::now();
    let (outcome, driver_slug) = resolve_and_teardown(work_db, execution_id, workspace_path).await;
    tracing::info!(
        execution_id,
        reason = reason.as_str(),
        reason_detail = reason.detail(),
        driver = driver_slug.as_deref().unwrap_or(""),
        outcome = outcome.as_str(),
        elapsed_ms = started.elapsed().as_millis(),
        "driver workspace teardown: completed",
    );
}

/// Body of [`teardown_driver_workspace`], split out so the completion trace
/// covers every return path without a `defer`-style guard. Returns the
/// outcome and, once resolved, the driver slug the pass ran against.
async fn resolve_and_teardown(
    work_db: &WorkDb,
    execution_id: &str,
    workspace_path: Option<&Path>,
) -> (TeardownOutcome, Option<String>) {
    // Provider-session identity is engine-owned lifecycle state, independent
    // of whether the driver had filesystem runtime state to clean up. Clear it
    // before any early return below so every normal termination path prunes
    // the one persisted identity.
    if let Err(err) = work_db.clear_run_progress_session_identity(execution_id) {
        tracing::warn!(
            execution_id,
            error = %format!("{err:#}"),
            "driver workspace teardown: failed to clear progress session identity (non-fatal)",
        );
    }
    // Same lifecycle, same reason: the ingress resume point describes a
    // rollout the terminated run will never append to again, and readoption
    // only ever consults it for a run that is still live.
    if let Err(err) = work_db.clear_run_progress_ingress_checkpoint(execution_id) {
        tracing::warn!(
            execution_id,
            error = %format!("{err:#}"),
            "driver workspace teardown: failed to clear progress ingress checkpoint (non-fatal)",
        );
    }

    let runtime_state = match work_db.get_driver_runtime_state(execution_id) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "driver workspace teardown: failed to load driver_runtime_state (non-fatal)",
            );
            None
        }
    };

    let driver_slug = match work_db.get_execution_driver_slug(execution_id) {
        Ok(Some(slug)) => slug,
        Ok(None) => {
            // Unknown execution or no tasks row: nothing to resolve. A
            // missing slug with no runtime state is a pure no-op; a missing
            // slug *with* runtime state is a data-integrity problem we log.
            if runtime_state.is_some() {
                tracing::warn!(
                    execution_id,
                    "driver workspace teardown: runtime state present but no driver slug \
                     could be resolved; skipping teardown rather than inventing a home"
                );
            }
            return (TeardownOutcome::NoDriverSlug, None);
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "driver workspace teardown: failed to resolve driver slug (non-fatal)",
            );
            return (TeardownOutcome::DriverSlugLookupFailed, None);
        }
    };

    let registry = DriverRegistry::default();
    let driver = match registry.get(&driver_slug) {
        Some(d) => d.clone(),
        None => {
            tracing::warn!(
                execution_id,
                driver = %driver_slug,
                "driver workspace teardown: unknown driver slug; skipping"
            );
            return (TeardownOutcome::UnknownDriver, Some(driver_slug));
        }
    };

    if let Err(err) = driver
        .teardown_workspace(workspace_path, execution_id, runtime_state.as_ref())
        .await
    {
        tracing::warn!(
            execution_id,
            driver = %driver_slug,
            workspace_path = ?workspace_path.map(Path::display),
            has_runtime_state = runtime_state.is_some(),
            error = %format!("{err:#}"),
            "driver workspace teardown failed (non-fatal)",
        );
        return (TeardownOutcome::DriverError, Some(driver_slug));
    }
    (TeardownOutcome::Ok, Some(driver_slug))
}

/// Test-only call counter for [`teardown_driver_workspace`] — the entry
/// point every one of the ~15 termination-path call sites actually invokes
/// (they cannot inject a driver). A `thread_local`, not a shared atomic:
/// `#[tokio::test]` defaults to the `current_thread` runtime flavor, so each
/// test's async work — including everything a call site under test
/// transitively awaits — stays on that test's own OS thread, and the
/// counter never sees another, unrelated test's calls running in parallel.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::Cell;

    thread_local! {
        static CALL_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn record_call() {
        CALL_COUNT.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn reset() {
        CALL_COUNT.with(|c| c.set(0));
    }

    pub(crate) fn count() -> usize {
        CALL_COUNT.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
    use boss_protocol::DriverRuntimeState;

    #[tokio::test]
    async fn teardown_driver_workspace_succeeds_for_claude() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);
        let dir = tempfile::tempdir().unwrap();
        // No-op driver, no panic, nothing propagated — just confirm it runs.
        teardown_driver_workspace(&db, &execution.id, Some(dir.path()), TeardownReason::RunCompleted).await;
    }

    #[tokio::test]
    async fn teardown_driver_workspace_succeeds_with_no_path() {
        // Callers must invoke teardown even when the workspace path is
        // unknown (never recorded, or cleared by a racing teardown) — a
        // driver may key its out-of-workspace state by the recorded
        // runtime state alone.
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);
        teardown_driver_workspace(&db, &execution.id, None, TeardownReason::RunCompleted).await;
    }

    #[tokio::test]
    async fn teardown_prunes_progress_session_identity() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();
        assert!(
            !db.claim_run_progress_session_identity(&execution.id, "thread-a")
                .unwrap()
        );
        assert!(
            db.claim_run_progress_session_identity(&execution.id, "thread-a")
                .unwrap()
        );

        teardown_driver_workspace(&db, &execution.id, None, TeardownReason::RunCompleted).await;

        assert!(
            !db.claim_run_progress_session_identity(&execution.id, "thread-a")
                .unwrap(),
            "normal teardown must clear the durable identity"
        );
    }

    #[tokio::test]
    async fn teardown_loads_persisted_runtime_state_without_inferring() {
        // Contract: teardown reloads the opaque state from the execution
        // row and never invents a cleanup target. Claude ignores the
        // state (no-op); a future Codex driver would use the recorded
        // path rather than scanning a shared provider home.
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        let state = DriverRuntimeState::new(serde_json::json!({
            "codex_home": "/tmp/boss-codex-homes/exec-test"
        }));
        db.set_driver_runtime_state(&execution.id, Some(&state)).unwrap();

        // Round-trip through the dedicated getter (same path teardown uses).
        let loaded = db.get_driver_runtime_state(&execution.id).unwrap();
        assert_eq!(loaded.as_ref().map(|s| s.as_value()), Some(state.as_value()));

        // Teardown must not panic or fail the caller even with state present.
        teardown_driver_workspace(&db, &execution.id, None, TeardownReason::RunCompleted).await;

        // State survives teardown (idempotent; retention may still need it).
        let still = db.get_driver_runtime_state(&execution.id).unwrap();
        assert_eq!(still.as_ref().map(|s| s.as_value()), Some(state.as_value()));
    }

    #[tokio::test]
    async fn teardown_traces_name_their_caller_and_pair_entry_with_completion() {
        // Both halves of the 2026-07-31 instrumentation gap in one assertion:
        // `entered` must say WHO called it (a bare `entered` line was the last
        // thing a silently-aborted dispatch ever emitted, and nothing in it
        // distinguished the coordinator's spawn-failure arm from any of the
        // ~12 sweeps), and it must be followed by a `completed` line carrying
        // a duration — without which an in-flight teardown is
        // indistinguishable from one that finished milliseconds ago.
        let buffer = crate::test_support::log_capture::install();
        let starting_offset = buffer.lock().len();

        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        teardown_driver_workspace(&db, &execution.id, None, TeardownReason::SpawnFailed).await;

        let captured = String::from_utf8_lossy(&buffer.lock()[starting_offset..]).to_string();
        let our_lines: Vec<&str> = captured.lines().filter(|line| line.contains(&execution.id)).collect();

        let entered = our_lines
            .iter()
            .position(|line| line.contains("driver workspace teardown: entered"))
            .unwrap_or_else(|| panic!("no `entered` line for {}; got {our_lines:#?}", execution.id));
        assert!(
            our_lines[entered].contains("reason=\"spawn_failed\""),
            "the entry line must name its caller; got:\n{}",
            our_lines[entered],
        );

        let completed = our_lines
            .iter()
            .position(|line| line.contains("driver workspace teardown: completed"))
            .unwrap_or_else(|| panic!("no `completed` line for {}; got {our_lines:#?}", execution.id));
        assert!(
            completed > entered,
            "`completed` must follow `entered`; got {our_lines:#?}",
        );
        assert!(
            our_lines[completed].contains("reason=\"spawn_failed\"") && our_lines[completed].contains("elapsed_ms="),
            "the completion line must carry the reason and a duration; got:\n{}",
            our_lines[completed],
        );
        assert!(
            our_lines[completed].contains("outcome=\"ok\""),
            "a teardown that reached the driver cleanly reports outcome=ok; got:\n{}",
            our_lines[completed],
        );
    }

    #[test]
    fn teardown_reason_labels_are_stable_and_completion_keeps_its_path() {
        // The labels are what an operator greps for; pin the family/detail
        // split so a completion path label can never be mistaken for a reason.
        assert_eq!(TeardownReason::SpawnFailed.as_str(), "spawn_failed");
        assert_eq!(TeardownReason::SpawnFailed.detail(), "");
        assert_eq!(TeardownReason::CancelledDuringSpawn.as_str(), "cancelled_during_spawn");
        assert_eq!(TeardownReason::RunCompleted.as_str(), "run_completed");
        assert_eq!(TeardownReason::Completion("pr_review").as_str(), "completion");
        assert_eq!(TeardownReason::Completion("pr_review").detail(), "pr_review");
    }

    #[test]
    fn clear_execution_workspace_preserves_driver_runtime_state() {
        // Workspace release must not drop the cleanup handle — future
        // Codex retention operates only on a recorded root.
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        // Simulate a leased execution with a recorded runtime state.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions
                 SET cube_lease_id = 'lease-1',
                     cube_workspace_id = 'ws-1',
                     workspace_path = '/tmp/ws',
                     status = 'running'
                 WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();
        }
        let state = DriverRuntimeState::new(serde_json::json!({"codex_home": "/tmp/codex-home"}));
        db.set_driver_runtime_state(&execution.id, Some(&state)).unwrap();

        let cleared = db.clear_execution_workspace(&execution.id).unwrap();
        assert!(cleared.is_some());

        let reloaded = db.get_execution(&execution.id).unwrap();
        assert!(reloaded.workspace_path.is_none());
        assert_eq!(
            reloaded.driver_runtime_state.as_ref().map(|s| s.as_value()),
            Some(state.as_value()),
        );
    }
}
