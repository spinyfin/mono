//! Injected lifecycle services, worker liveness, and teardown contracts.
//! Production implementations live in engine core.

use crate::{
    CubeClient, ExecutionPublisher, HostAdapter, HostAdapterProvider, LeaseReleaseOutcome, PaneStatus, TerminalCause,
};
use anyhow::Result;
use async_trait::async_trait;
use boss_dispatch_events::DispatchEventSink;
use boss_engine_metrics_registry::Registry;
use boss_protocol::{LiveWorkerState, WorkExecution};
use std::path::Path;
use std::sync::Arc;

/// Which termination path invoked `teardown_driver_workspace`, and why.
///
/// Both the entry and completion traces carry this, so "who tore this
/// execution down?" is answerable from the teardown line itself rather than
/// inferred from whichever sweep happened to log next to it.
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
    /// `spawn_ack_sweep` — driver-start verification found a pane with a
    /// shell but no driver-originated signal within the grace window.
    DriverStartTimeout,
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
            Self::DriverStartTimeout => "driver_start_timeout",
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

/// Result of probing whether a pid is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidStatus {
    Alive,
    Dead,
    PermissionDenied,
    Unknown,
}

/// Result of a [`WorkerPaneReleaser::release_pane`] call. Tells the
/// caller whether a live worker slot was actually found and reaped — the
/// signal that gates the cube-lease release in `WorkerCompletionHandler::force_release`.
///
/// The distinction exists to close a mid-spawn-cancel collision:
/// a worker whose pid has not yet materialized has no mapped slot, so the
/// pane release is a no-op (`NoLiveWorker`). Freeing its cube lease at
/// that point would hand a still-to-be-occupied workspace back to cube,
/// which then re-leases it to another execution — two live processes,
/// one working tree. The lease must stay held until the occupant is
/// genuinely gone; the in-flight run reaps + releases once its spawn
/// settles (see `PaneSpawnRunner::run_execution`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneReleaseOutcome {
    /// A mapped worker slot was found: its pane was torn down and its OS
    /// process tree signalled (SIGTERM, escalating to SIGKILL). The
    /// workspace is no longer occupied, so the caller may free the lease.
    Reaped,
    /// No slot was mapped for the run. Either the worker already released
    /// (idempotent second call) or — the case this distinction exists for
    /// — it is still mid-spawn with no pid yet, so nothing could be
    /// reaped. The caller MUST NOT free the cube lease on this outcome.
    NoLiveWorker,
}

/// Tears down the tmux session hosting `run_id` after verifying its token
/// and detaches its app viewer. Implementations must be idempotent: a duplicate
/// call after the slot has been released is a no-op, not an error.
#[async_trait]
pub trait WorkerPaneReleaser: Send + Sync {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome;

    /// Remove a retained live entry's roster name before its persona becomes reusable.
    async fn forget_persona_name(&self, _run_id: &str) {}

    /// The owning host has positively confirmed this remote worker is gone.
    async fn release_proven_dead_remote_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        self.release_pane(run_id).await
    }
}

/// `WorkerPaneReleaser` that does nothing — used when no app session
/// release is wired (tests, headless runs). Reports `Reaped` so the
/// lease-release path is unchanged for setups without a pane subsystem.
#[derive(Debug, Default)]
pub struct NoopWorkerPaneReleaser;

#[async_trait]
impl WorkerPaneReleaser for NoopWorkerPaneReleaser {
    async fn release_pane(&self, _run_id: &str) -> PaneReleaseOutcome {
        PaneReleaseOutcome::Reaped
    }
}

/// Tally from one remote-run reattach pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReattachSummary {
    /// Runs whose reverse events forward was re-established.
    pub reattached: usize,
    /// Runs whose host could not be resolved or whose adapter / forward
    /// re-establishment failed. Logged per-run; the run stays in the DB
    /// and a later pass (or manual `hosts probe`) can retry.
    pub failed: usize,
    /// Runs whose `host_id` no longer exists in the `hosts` table (host
    /// removed since the run started). Counted separately from `failed`
    /// because it is not an error the engine can recover from.
    pub host_missing: usize,
}

impl ReattachSummary {
    /// True when the pass touched at least one detached remote run.
    pub fn had_candidates(&self) -> bool {
        self.reattached + self.failed + self.host_missing > 0
    }
}

/// Counts from one remote-lease reconcile pass.
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

/// Narrow live-worker surface the coordinator (and the pane-death /
/// orphan-sweep corroboration paths that read the coordinator's registry)
/// need, without naming the engine-core registry type.
pub trait LiveWorkerLiveness: Send + Sync {
    fn is_run_live(&self, run_id: &str) -> bool;
    fn snapshot(&self) -> Vec<LiveWorkerState>;
    fn release_slot_for_run(&self, run_id: &str) -> Option<u8>;
    fn release_persona_for_run(&self, run_id: &str);
    fn last_event_at_for_run(&self, run_id: &str) -> Option<String>;
    fn current_tool_for_run(&self, run_id: &str) -> Option<String>;
}

/// Engine lifecycle operations consumed by the dispatch coordinator (lease
/// release, driver teardown, bookmark recovery, liveness reconcile, remote
/// reattach). The core lifecycle adapter implements it by delegating to the
/// owning lifecycle modules.
#[async_trait]
pub trait LifecycleServices: Send + Sync {
    /// Proven-death cube release used by the coordinator's spawn/completion
    /// paths: empty occupancy, best-effort teardown preservation on `adapter`,
    /// then `release_workspace_with_reset`.
    async fn apply_proven_workspace_release(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        cause: TerminalCause,
        adapter: &dyn HostAdapter,
    ) -> LeaseReleaseOutcome;

    async fn release_lease_after_proven_never_started(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        adapter: &dyn HostAdapter,
    ) -> LeaseReleaseOutcome;

    async fn release_refused_workspace(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        adapter: &dyn HostAdapter,
    ) -> LeaseReleaseOutcome;

    async fn release_lease_after_inferred_death(
        &self,
        execution: &WorkExecution,
        pane: PaneStatus,
        adapter: &dyn HostAdapter,
        reason: &str,
    ) -> LeaseReleaseOutcome;

    async fn teardown_driver_workspace(
        &self,
        execution_id: &str,
        workspace_path: Option<&Path>,
        reason: TeardownReason,
    );

    fn report_bookmark_failure(&self, execution: &WorkExecution, reason: &str);

    fn block_unrecoverable_bookmark(&self, execution: &WorkExecution, err: &anyhow::Error) -> anyhow::Result<bool>;

    async fn reconcile_if_execution_dead(
        &self,
        execution: &WorkExecution,
        dispatch_events: &dyn DispatchEventSink,
    ) -> bool;

    async fn reconcile_if_pane_dead(
        &self,
        execution: &WorkExecution,
        dispatch_events: &dyn DispatchEventSink,
        now_epoch_secs: i64,
        live_states: Option<&dyn LiveWorkerLiveness>,
    ) -> bool;

    fn probe_pid(&self, pid: i32) -> PidStatus;

    async fn reattach_remote_runs(
        &self,
        provider: &dyn HostAdapterProvider,
        engine_events_socket: &str,
    ) -> ReattachSummary;

    async fn reconcile_remote_leases(
        &self,
        provider: &dyn HostAdapterProvider,
        dispatch_events: &dyn DispatchEventSink,
        pane_releaser: Option<&dyn WorkerPaneReleaser>,
    ) -> RemoteLeaseReconcileOutcome;

    /// Ensure answer-agent counters exist; preserve counters already in a shared registry.
    fn register_answer_agent_metrics(&self, registry: &Registry);

    fn record_answer_agent_started(&self, registry: &Registry, queue_wait_ms: u64);

    fn record_conflict_class_counter(&self, registry: &Registry, product_id: &str, conflict_class: &str);

    async fn notify_review_guide_changed(
        &self,
        publisher: &Arc<dyn ExecutionPublisher>,
        root_task_id: &str,
        reason: &str,
    );

    fn is_unconfirmed_spawn_abort(&self, err: &anyhow::Error) -> bool;

    async fn verify_deletion_tripwire(
        &self,
        repo_slug: &str,
        head_before: &str,
        base_sha: &str,
        pr_number: u64,
    ) -> Vec<String>;
}

/// Cube that owns an execution's lease, plus the host that cube lives on.
///
/// Returned by [`ExecutionCubes::cube_for_execution`] so callers that care
/// about *which* host was resolved (e.g. emitting a remote-only ok event)
/// do not have to re-derive it by sniffing display labels from
/// [`CubeClient::command_repr`].
#[derive(Clone)]
pub struct ResolvedExecutionCube {
    pub cube: Arc<dyn CubeClient>,
    /// Host the execution's latest run was dispatched to. `"local"` when the
    /// engine's own cube issued (or will issue) the lease — including
    /// executions with no run row yet.
    pub host_id: String,
    /// Owning-host adapter for a remote cube, when routing could resolve one.
    /// Inferred-death release uses this to preserve unpublished work and probe
    /// occupancy; `None` keeps a remote lease.
    pub host_adapter: Option<Arc<dyn HostAdapter>>,
}

impl ResolvedExecutionCube {
    /// True when the lease lives on the engine's own cube.
    pub fn is_local(&self) -> bool {
        self.host_id == "local"
    }
}

/// Resolves the cube endpoint that actually owns an execution's lease.
///
/// A lease is tracked only by the cube on the host its workspace lives on,
/// so every heartbeat (and every auto-reap force-release) must go through
/// this seam. `local` resolves to the engine's own client; any other host
/// resolves through the same adapter provider the dispatch loop leased
/// with. Resolution failure is deliberately an `Err`, never a silent skip:
/// a heartbeat this sweep cannot route is a heartbeat that did not happen,
/// and the caller must count it as a failure and emit the error event so it
/// stays visible. See `heartbeat_one` in the cube lease heartbeat.
#[async_trait]
pub trait ExecutionCubes: Send + Sync {
    /// The cube that owns `execution_id`'s lease, and the host that cube
    /// lives on.
    async fn cube_for_execution(&self, execution_id: &str) -> Result<ResolvedExecutionCube>;
}

/// Result of atomically checking and fencing a never-started run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeverStartedReap {
    /// The run is now fenced against later driver signals.
    Committed,
    /// A driver signal won the race; the run must not be reaped.
    DriverSignalled,
    /// The slot no longer belongs to this run.
    SlotGone,
    /// The run was already fenced against later driver signals.
    AlreadyCommitted,
}

/// Driver observations and atomic reap fencing, supplied by the engine's live-state owner.
pub trait DriverSignals: Send + Sync {
    fn has_driver_signal_for_run(&self, run_id: &str) -> bool;
    fn first_hook_event_for_run(&self, run_id: &str) -> Option<String>;
    /// Atomically commit the reap fence unless a driver signal already won.
    /// A successful commit prevents later driver signals from reviving the run.
    fn commit_never_started_reap(&self, slot_id: u8, run_id: &str) -> NeverStartedReap;
}
