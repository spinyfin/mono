use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use boss_event_bus::{Event, EventBus, EventKind, TopicFilter};
use boss_protocol::{
    DispatchPauseScope, EngineToAppError, ExecutionKind, ExecutionStatus, LiveWorkerState, PauseReason, TaskKind,
    TaskStatus,
};
use tokio::sync::{Mutex, Semaphore};

use crate::config::RuntimeConfig;
use crate::conflict_diagnosis;
use crate::dispatch_events::{
    DispatchEvent, DispatchEventSink, NoopDispatchEventSink, Outcome as DispatchOutcome, Stage,
};
use crate::dispatch_inflight::InflightDispatches;
#[cfg(test)]
use crate::effort::PoolModelTier;
use crate::host_adapter::{LocalHostAdapter, LocalHostAdapterProvider};
use crate::host_registry::Host;
use crate::host_scheduling::{self, ChoreRequirements, HostSlot};
use crate::metrics::Registry;
use crate::work::{
    CreateAttentionItemInput, DispatchClaimOutcome, DispatchClass, FinishExecutionRunInput, PreStartFailureOutcome,
    WorkDb, WorkExecution, WorkItem, WorkRun,
};
use boss_execution_contracts::{ExecutionRunner, RunOutcome, RunWaitState};
use boss_execution_contracts::{HostAdapter, HostAdapterProvider};

// Phase-3 counter handles for the cube workspace lease boundary.
crate::register_counter!(
    CUBE_WORKSPACE_LEASE_ATTEMPTS,
    "cube_workspace_lease.attempts",
    "Number of cube workspace lease invocations attempted (each fallback counts separately).",
);
crate::register_counter!(
    CUBE_WORKSPACE_LEASE_SUCCESS,
    "cube_workspace_lease.success",
    "Number of cube workspace lease invocations that succeeded.",
);
crate::register_counter!(
    CUBE_WORKSPACE_LEASE_FAILURE,
    "cube_workspace_lease.failure",
    "Number of cube workspace lease sequences that exhausted all attempts and failed.",
);

/// Register all cube-workspace-lease counter handles with `registry`. Called
/// from [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&CUBE_WORKSPACE_LEASE_ATTEMPTS);
    registry.register_counter(&CUBE_WORKSPACE_LEASE_SUCCESS);
    registry.register_counter(&CUBE_WORKSPACE_LEASE_FAILURE);
}

#[cfg(test)]
pub use boss_execution_contracts::{
    AutomationPreemptor, ExecutionStartedHook, NoopAutomationPreemptor, NoopExecutionStartedHook, PreemptOutcome,
};
#[cfg(not(test))]
use boss_execution_contracts::{
    AutomationPreemptor, ExecutionStartedHook, NoopAutomationPreemptor, NoopExecutionStartedHook, PreemptOutcome,
};

/// Result of one preemption attempt by the dispatcher.
///
/// The two axes are deliberately separate: *did we tear a run down* and
/// *did we get a slot out of it*. They usually agree, but a freed slot can
/// be taken by a concurrent out-of-band claimer before the preempting
/// execution claims it. The once-per-pass cascade bound must key off the
/// first axis — a teardown that happened still counts against the pass's
/// single-preemption budget even if it bought this item nothing, or a
/// burst of mainline arrivals racing one unlucky slot could tear down
/// several automation runs in a single drain.
#[derive(Debug)]
enum PreemptionAttempt {
    /// Nothing was torn down: no eligible victim, the victim was
    /// mid-spawn, or teardown failed. No state changed.
    NotPreempted,
    /// A run was torn down and its work requeued. `claimed` is the slot
    /// taken for the preempting execution, or `None` if a concurrent
    /// claimer won the freed slot first.
    Preempted { claimed: Option<String> },
}

#[cfg(not(test))]
use boss_execution_contracts::{
    DEFAULT_REVIEW_POOL_SIZE, MAX_AUTOMATION_POOL_SIZE, MAX_REVIEW_POOL_SIZE, MAX_WORKER_POOL_SIZE,
};
#[cfg(test)]
pub use boss_execution_contracts::{
    DEFAULT_REVIEW_POOL_SIZE, MAX_AUTOMATION_POOL_SIZE, MAX_REVIEW_POOL_SIZE, MAX_WORKER_POOL_SIZE, WORKER_PAGE_COUNT,
    WORKER_PAGE_SIZE,
};

/// Default (and compile-time floor) for the interactive-pool concurrency
/// cap on concurrently-live INTERACTIVE ("normal") pool workers — a *row
/// dispatched from* the automation or review pool is never held by this
/// gate; those pools' own sizes govern their own home-pool dispatch. That
/// is NOT the same as saying automation never counts toward the number
/// this cap compares against: `busy_count()` counts claimed `worker_pool`
/// slots whose workspace lease has already succeeded (`working`), and a
/// spilled automation run (one that overflowed its full home pool onto a
/// Lower Decks interactive slot via `claim_worker_spill`) IS one of those
/// slots once it has a workspace, so it DOES count against this cap. A
/// slot that is only acquiring a workspace is tracked separately as
/// lease-pending and is excluded from this comparison — otherwise a
/// cube-lease timeout storm saturates the cap with zero live workers and
/// starves new mainline work. That is a deliberate policy choice, not an
/// oversight. The price is a bounded overshoot: the cap is compared against
/// `working` at admission, and a claim becomes `working` only when its lease
/// returns, so claims admitted while `working` sat just below the cap can all
/// complete their leases and push the live count above it. Pending claims
/// are bounded by [`MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO`] per repo (a
/// claim holds a permit until its lease returns), so the live count can
/// exceed the cap by at most that bound times the number of repos with a
/// lease in flight; further admissions are held until `working` drops back
/// below the cap. Explicit critical/forced launches bypass the cap by design.
/// Counting spilled automation that is actually running keeps
/// the cap's promise (the configured number of live interactive-pool
/// workers, spilled automation included, plus that bounded overshoot) intact,
/// while the once-per-pass automation-preemption fallback in
/// `drain_ready_queue` is what keeps mainline from starving behind spilled
/// automation at the cap — a preemption is a trade (one live worker for
/// another), so it can reclaim a slot from spilled automation without ever
/// pushing the live count past the cap. The interactive pool has 16 slots
/// across two pages (Bridge Crew + Lower Decks); the 2026-07-15 full-fleet
/// saturation experiment (22 live workers, load average ~152) pushed
/// individual task times past an hour and broke pane spawn acks, which is
/// why dispatch defaults to one page's worth — the pre-Lower-Decks size —
/// even though all 16 slots exist.
///
/// The live cap is settable at runtime via
/// [`ExecutionCoordinator::set_max_concurrent_interactive_workers`]
/// (`bossctl dispatch concurrency --set N`) and persisted in `state.db`, so
/// it survives an engine restart. This constant is the value the coordinator
/// seeds itself with at construction and the value tests get unless they
/// override it. Any live value is clamped to `[1, MAX_WORKER_POOL_SIZE]` —
/// enforced in `drain_ready_queue`, deliberately NOT in `claim_worker_force`,
/// where an explicit `bossctl agents launch` may exceed the cap.
pub const MAX_CONCURRENT_INTERACTIVE_WORKERS: usize = 8;

/// Ceiling on concurrent `cube workspace lease` calls against one repo
/// origin. Cube takes a per-repo `RepoLock` for the registry-mutation
/// portion of a lease and then drops it before the local jj probe and
/// setup so one other lease can overlap (`tools/cube/src/app/workspace.rs`).
/// Concurrent leases on one repo therefore queue on that lock rather than
/// run faster, and when a long operation such as `repo gc` holds it every
/// waiter runs into the lease timeout ([`CUBE_LEASE_TIMEOUT`]), so extra
/// attempts only add timeouts. The bound of 2 is derived from that lock
/// design (one lease holding the lock plus one overlapping its post-lock
/// setup); it is not a measured peak of healthy dispatch concurrency.
///
/// The permit covers the lease call only: it is taken before a slot is
/// claimed and released as soon as the lease returns, success or failure.
pub const MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO: usize = 2;

/// User-visible wait reason for a ready row held because this origin
/// already has [`MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO`] lease calls
/// in flight. Distinct from the interactive-cap wording: these rows are
/// not waiting for a worker slot.
pub(crate) const WAITING_FOR_WORKSPACE_REASON: &str = "Waiting for a workspace";

use boss_engine_storage_values::pool_policy::{AUTOMATION_WORKER_ID_PREFIX, REVIEW_WORKER_ID_PREFIX};

/// Execution kind string for reviewer agent runs. A `pr_review` execution
/// reviews a worker's PR read-only and always routes to the dedicated review
/// pool. Re-exported from `boss_protocol` so routing and the (future)
/// completion handler share one source of truth.
#[cfg(test)]
pub(crate) use boss_protocol::EXECUTION_KIND_PR_REVIEW;

/// Outer bound on how long the engine waits for a single
/// `cube workspace lease` invocation before declaring the attempt a
/// timeout failure. The value and its history live in
/// [`crate::cube_op_budget::LEASE`], which is also what the SSH transport
/// reads so a remote lease gets the same allowance a local one does.
///
/// This remains a genuine backstop, not a formality: it fires even if a
/// host adapter imposes no bound of its own (the local one does not), and
/// the transport-level bound underneath it is deliberately a little
/// shorter so the better-attributed error wins the race — see
/// [`crate::cube_op_budget::TRANSPORT_GRACE`].
const CUBE_LEASE_TIMEOUT: Duration = crate::cube_op_budget::LEASE;

/// Same, for `cube repo ensure`. See [`crate::cube_op_budget::REPO_ENSURE`].
const CUBE_REPO_ENSURE_TIMEOUT: Duration = crate::cube_op_budget::REPO_ENSURE;

/// Ceiling on how many handed-off dispatches may run their slow tail
/// (`cube repo ensure` → workspace lease → `goto` → `change create`)
/// concurrently.
///
/// `drain_ready_queue` hands each claimed row off to its own task so one slow
/// item can't head-of-line block the backlog behind it (a 2026-07-15
/// incident: a resumed 40-minute backlog drained at ~1/minute against ample
/// free slots, because every row waited out the row before it hitting a 30-60s
/// cube timeout).
///
/// The real bound on concurrent dispatches is the worker pools: a row is only
/// handed off *after* it claims a slot, so the three pools' combined hard caps
/// already cap in-flight dispatches. This constant is deliberately derived from
/// those caps rather than picked independently, so it is a backstop against
/// unbounded task spawning (a future hand-off path that forgets to claim first)
/// and NOT a pacing mechanism. Picking a smaller round number would make the
/// semaphore bind during exactly the workload this change exists to fix —
/// draining a large backlog into a fully free pool — quietly reintroducing the
/// head-of-line blocking one permit-width at a time.
const MAX_INFLIGHT_DISPATCHES: usize = MAX_WORKER_POOL_SIZE + MAX_AUTOMATION_POOL_SIZE + MAX_REVIEW_POOL_SIZE;

/// Backoff delays between successive pre-start retry attempts. Element N
/// is the sleep before attempt N+2 (the first retry, the second retry, …).
/// Three entries → up to 3 retries (4 total attempts), ~65s total budget,
/// before a pre-start failure surfaces to the operator.
///
/// This is a genuinely different seam from
/// [`crate::automation_scheduler::AUTOMATION_RETRY_HOLD_MAX_SECS`] (~3600s
/// budget), not a duplicate: this one is keyed on
/// `work_executions.pre_start_failure_count` / `dispatch_not_before` and
/// requires an execution row to already exist, whereas the scheduler's
/// budget covers the case where `dispatch_triage` itself couldn't create
/// one. The ~55x difference is deliberate — a blocker discovered before an
/// execution row exists (e.g. cube lease/git-remote failures ahead of the
/// automation firing at all) gets the much longer scheduler-side budget
/// because there is nothing else watching that occurrence in the meantime,
/// while a failure after the row exists gets this shorter budget since a
/// human already has a concrete execution to look at.
const PRE_START_RETRY_DELAYS: [Duration; 3] =
    [Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(45)];

/// User-visible interactive-cap hold text. Lease-pending slots are
/// never called "working".
pub(crate) fn interactive_cap_wait_reason(working: usize, lease_pending: usize) -> String {
    if lease_pending == 0 {
        format!("Waiting for an available worker ({working} working)")
    } else {
        format!("Waiting for an available worker ({working} working, {lease_pending} waiting for a workspace)")
    }
}

/// How often `run_execution`'s [`HeartbeatGuard`] re-stamps the cube
/// lease expiry. Cube's `DEFAULT_LEASE_TTL_SECS` (and the engine's
/// matching [`crate::cube_lease_heartbeat::LEASE_TTL_SECS`]) is 24 hours,
/// so a 5-minute cadence renews far more often than needed within one
/// TTL window — generous enough that many consecutive failed beats
/// (e.g., a transient cube subprocess failure) don't put the lease at
/// risk. The interval is kept short so pane-worker coverage still
/// matches the periodic sweep's cadence.
const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);

pub use boss_engine_storage_values::attention_kinds::CHAIN_SERIALIZED_STALL_ATTENTION_KIND;

/// How long a `ready` execution may sit deferred behind the same live chain
/// sibling before [`CHAIN_SERIALIZED_STALL_ATTENTION_KIND`] fires. Set
/// comfortably below the ~20-minute silent stall observed in the
/// chain-serialization re-defer stall incident so a human is alerted well
/// before that point going forward.
pub const CHAIN_SERIALIZED_STALL_THRESHOLD_SECS: i64 = 900;

/// Short `repo#N` reference for a canonical GitHub PR URL, for embedding in
/// operator-facing text (e.g. the `chain_serialized` wait reason). `None`
/// for anything that isn't a parseable GitHub PR URL.
fn pr_short_reference(pr_url: &str) -> Option<String> {
    let repo_path = boss_github::pr_url::repo_from_pr_url(pr_url)?;
    let number = boss_github::pr_url::pr_number_from_url(pr_url)?;
    let repo_name = repo_path.rsplit('/').next().unwrap_or(repo_path);
    Some(format!("{repo_name}#{number}"))
}

/// Outcome of [`ExecutionCoordinator::resolve_chain_hold`] for one execution.
enum ChainHold {
    /// No other chain member is live; dispatch may proceed normally.
    Clear,
    /// A live sibling genuinely blocks dispatch; the caller must defer
    /// (pre-claim / pre-lease) or refuse (post-lease). `review_held` is
    /// `true` when the blocking sibling is a `pr_review` execution — a
    /// review-vs-something-else hold, as opposed to a writer-vs-writer
    /// hold — purely for trace/UI labeling; the caller still treats both
    /// as equally blocking. `queue_len` is the total count of live chain
    /// siblings found (including `sibling` itself) — used to tell an
    /// operator "N queued ahead" rather than naming only the one
    /// currently live.
    Blocked {
        sibling: WorkExecution,
        review_held: bool,
        queue_len: usize,
    },
    /// A live read-only review sibling was found but bypassed: `execution` is
    /// a revision, and reviews are read-only so cannot
    /// race the shared jj backing store the guard protects (see
    /// `resolve_chain_hold`'s doc comment). The caller may proceed but
    /// should log the bypass so the trace distinguishes it from `Clear`.
    ReviewBypassed(WorkExecution),
}

/// Owns the per-run cube lease heartbeat task. Dropping the guard
/// aborts the heartbeat — used at the end of `run_execution` so the
/// heartbeat cannot outlive its lease.
///
/// Background: cube treats any lease whose `lease_expires_at_epoch_s`
/// has passed as eligible for reclamation. Without periodic
/// heartbeats, every worker that ran longer than the TTL was silently
/// susceptible to having its workspace's `@` reset by the next lease
/// call. The investigation chore for `mono-agent-001` (2026-05-12)
/// traced Worf's "`@` got re-pointed mid-flight" symptom to exactly
/// this.
///
/// ## Coverage split with `cube_lease_heartbeat`
///
/// This guard covers in-process / blocking runners: for any
/// `spawn_worker` implementation that blocks until the run completes
/// (test fakes, ACP-style runners), the guard is alive throughout the
/// run and fires at [`LEASE_HEARTBEAT_INTERVAL`].
///
/// For the production *pane-spawn* path, `spawn_worker` returns as soon
/// as the pane is handed off, and `run_execution` drops this guard
/// immediately afterwards — meaning the guard almost never fires a
/// single beat for a pane worker. That was the accurate root cause of
/// the recurrence: the guard existed but was dropped before it could
/// cover the pane worker's lifetime. The complementary fix is the
/// engine-wide periodic sweep in `crate::cube_lease_heartbeat`, which
/// covers pane workers via the live-worker registry and a DB-fallback
/// path for the post-restart gap.
struct HeartbeatGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl HeartbeatGuard {
    fn spawn(
        host_adapter: Arc<dyn HostAdapter>,
        lease_id: String,
        execution_id: String,
        run_id: String,
        worker_id: String,
    ) -> Self {
        Self::spawn_with_interval(
            host_adapter,
            lease_id,
            execution_id,
            run_id,
            worker_id,
            LEASE_HEARTBEAT_INTERVAL,
        )
    }

    /// Test seam: lets unit tests drive the heartbeat with a tiny
    /// interval (e.g., 50 ms) so they can exercise multiple beats
    /// without depending on tokio's paused-time API. Production
    /// callers go through [`Self::spawn`].
    fn spawn_with_interval(
        host_adapter: Arc<dyn HostAdapter>,
        lease_id: String,
        execution_id: String,
        run_id: String,
        worker_id: String,
        interval: Duration,
    ) -> Self {
        let handle = boss_task_scope::spawn(async move {
            // First tick fires immediately at start; the elapsed
            // interval is the *gap* between subsequent ticks. Skip
            // the first immediate tick so we don't issue a redundant
            // heartbeat the moment the lease was acquired.
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match host_adapter.heartbeat_lease(&lease_id, None).await {
                    Ok(()) => {
                        tracing::debug!(
                            %execution_id,
                            %run_id,
                            %worker_id,
                            %lease_id,
                            "extended cube lease via heartbeat"
                        );
                    }
                    Err(err) => {
                        // A single failed heartbeat is not fatal — the
                        // lease still has up to a TTL of remaining
                        // life before cube will reclaim it. Log
                        // structured at WARN so an operator
                        // investigating a future "`@` moved" report
                        // can grep for failed beats and see the gap.
                        tracing::warn!(
                            %execution_id,
                            %run_id,
                            %worker_id,
                            %lease_id,
                            ?err,
                            "cube lease heartbeat failed; will retry next interval"
                        );
                    }
                }
            }
        });
        Self { handle }
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(test)]
pub use boss_host_execution::types::{
    CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
};
#[cfg(not(test))]
use boss_host_execution::types::{
    CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
};

#[cfg(test)]
pub use boss_execution_contracts::{CubeClient, RebaseOutcome};
#[cfg(not(test))]
use boss_execution_contracts::{CubeClient, RebaseOutcome};

#[derive(Debug, Clone)]
pub struct CommandCubeClient {
    cfg: Arc<RuntimeConfig>,
}

/// Strip the trailing jj conflict-type descriptor (e.g. `"    2-sided
/// conflict"` or `"    2-sided conflict including 1 deletion"`) that `jj
/// resolve --list` glues onto each path, column-aligned with whitespace
/// padding. Paths can themselves contain spaces, so this deliberately
/// anchors on the `<N>-sided conflict` marker rather than splitting on
/// whitespace: it finds the marker, walks back over the digit run that
/// precedes it, and treats everything before that (whitespace-trimmed) as
/// the path. Lines that don't match the pattern (already-bare paths, or a
/// future jj format change) are returned unchanged.
fn strip_jj_conflict_descriptor(line: &str) -> &str {
    const MARKER: &str = "-sided conflict";
    let trimmed = line.trim();
    let Some(marker_idx) = trimmed.rfind(MARKER) else {
        return trimmed;
    };
    let digits_start = trimmed[..marker_idx]
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    if digits_start == marker_idx {
        // No digits immediately before the marker — not actually the
        // descriptor we're looking for.
        return trimmed;
    }
    let path = trimmed[..digits_start].trim_end();
    if path.is_empty() { trimmed } else { path }
}

/// Parse `cube workspace rebase --json`'s `payload` object
/// (`{status, pushed, conflicted_files, ...}`) into a [`RebaseOutcome`].
/// Shared by [`CommandCubeClient::rebase_workspace`] and
/// [`CommandCubeClient::rebase_workspace_no_push`] — the two commands differ
/// only by the `--no-push` flag, not by output shape. `conflicted_files`
/// entries come from `jj resolve --list` and may carry a trailing
/// conflict-type descriptor after the path (see
/// [`strip_jj_conflict_descriptor`]); that descriptor is stripped here so
/// every downstream consumer of [`RebaseOutcome::conflicted_files`] sees
/// bare paths.
fn parse_rebase_payload(payload: serde_json::Value) -> Result<RebaseOutcome> {
    let status = payload
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("cube workspace rebase returned no `status` field: {payload}"))?;
    let clean = match status {
        "clean" => true,
        "conflicts" => false,
        other => return Err(anyhow!("cube workspace rebase returned unexpected status `{other}`")),
    };
    let pushed = payload.get("pushed").and_then(|v| v.as_bool()).unwrap_or(false);
    // Absent on a cube older than the replay-only collapse; 0 is the correct
    // reading there (that cube never collapsed anything).
    let linearized_commits = payload.get("linearized_commits").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    // Absent on a clean rebase (nothing declined) and on a cube older than the
    // decline-reason plumbing.
    let linearize_decline = payload
        .get("linearize_decline")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let conflicted_files = payload
        .get("conflicted_files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| strip_jj_conflict_descriptor(s).to_owned())
                .collect()
        })
        .unwrap_or_default();
    Ok(RebaseOutcome {
        clean,
        pushed,
        conflicted_files,
        linearized_commits,
        linearize_decline,
    })
}

impl CommandCubeClient {
    pub fn new(cfg: Arc<RuntimeConfig>) -> Self {
        Self { cfg }
    }

    async fn run_json(&self, args: &[&str]) -> Result<serde_json::Value> {
        let cwd = self.cfg.work.cwd.clone();
        self.run_json_in_dir(&cwd, args).await
    }

    /// Like [`Self::run_json`] but runs the cube subprocess with an explicit
    /// working directory. Required for cwd-sensitive subcommands such as
    /// `cube workspace rebase`, which resolves the target workspace from the
    /// current directory rather than a `--workspace` flag.
    async fn run_json_in_dir(&self, cwd: &Path, args: &[&str]) -> Result<serde_json::Value> {
        let agent = self.cfg.agent()?;
        boss_host_execution::cube::LocalCube {
            command: agent.cube.command.clone().into(),
            args: agent.cube.args.clone(),
            cwd: cwd.to_path_buf(),
        }
        .run(args)
        .await
    }
}

#[async_trait]
impl crate::cube_commands::CubeJsonTransport for CommandCubeClient {
    async fn run_cube_json(&self, args: &[&str]) -> Result<serde_json::Value> {
        self.run_json(args).await
    }
}

#[async_trait]
impl CubeClient for CommandCubeClient {
    async fn create_execution_bookmark(
        &self,
        workspace: &Path,
        execution_id: &str,
        predecessor: Option<&boss_engine_recovery::execution_bookmark::ExecutionBookmark>,
        inherited_base: Option<&str>,
    ) -> Result<boss_engine_recovery::execution_bookmark::ExecutionBookmark> {
        boss_engine_recovery::execution_bookmark::create_from(
            &boss_engine_recovery::execution_bookmark::LocalJj,
            workspace,
            execution_id,
            "local",
            predecessor,
            inherited_base,
        )
        .await
    }
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        crate::cube_commands::ensure_repo(self, origin).await
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        crate::cube_commands::lease_workspace(
            self,
            repo_id,
            task,
            prefer_workspace_id,
            allow_dirty,
            exclude_workspace_ids,
        )
        .await
    }

    async fn create_change(&self, workspace_path: &Path, title: &str) -> Result<CubeChangeHandle> {
        crate::cube_commands::create_change(self, workspace_path, title).await
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        crate::cube_commands::release_workspace(self, lease_id).await
    }

    async fn release_workspace_with_reset(&self, lease_id: &str) -> Result<()> {
        crate::cube_commands::release_workspace_with_reset(self, lease_id).await
    }

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        crate::cube_commands::workspace_status(self, workspace_path).await
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        crate::cube_commands::heartbeat_lease(self, lease_id, ttl_seconds).await
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        crate::cube_commands::force_release_lease(self, lease_id, reason).await
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        crate::cube_commands::list_workspaces(self).await
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        crate::cube_commands::list_repos(self).await
    }

    async fn goto_workspace(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        crate::cube_commands::goto_workspace(self, workspace_path, "--pr", &pr.to_string()).await
    }

    async fn goto_workspace_revision(&self, workspace_path: &Path, sha: &str) -> Result<()> {
        crate::cube_commands::goto_workspace(self, workspace_path, "--revision", sha).await
    }

    async fn repair_pr_bookmark(&self, store: &Path, pr: u64) -> Result<()> {
        crate::cube_commands::repair_pr_bookmark(self, store, pr, crate::cube_op_budget::REPAIR_PR_BOOKMARK).await
    }

    async fn rebase_workspace(&self, workspace_path: &Path, pr: u64) -> Result<RebaseOutcome> {
        let pr_str = pr.to_string();
        // `cube workspace rebase` resolves the workspace from the current
        // directory (no `--workspace` flag), so it must run inside the leased
        // workspace. Without `--no-push`, a clean rebase advances and pushes
        // the boss bookmark itself. The `--json` output is the command's
        // `payload` object (`{status, pushed, conflicted_files, ...}`).
        let payload = self
            .run_json_in_dir(workspace_path, &["--json", "workspace", "rebase", "--pr", &pr_str])
            .await?;
        parse_rebase_payload(payload)
    }

    async fn rebase_workspace_no_push(&self, workspace_path: &Path, pr: u64) -> Result<RebaseOutcome> {
        let pr_str = pr.to_string();
        // Same command as `rebase_workspace` plus `--no-push`: a clean
        // rebase never advances/pushes the boss bookmark, so the real PR
        // branch is untouched. Used for the speculative/throwaway rebase
        // (the conflict-against-main result-gate layer) — the caller only
        // wants to observe whether the PR would conflict against current
        // `main`.
        let payload = self
            .run_json_in_dir(
                workspace_path,
                &["--json", "workspace", "rebase", "--pr", &pr_str, "--no-push"],
            )
            .await?;
        parse_rebase_payload(payload)
    }

    async fn push_resolution(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        let pr_str = pr.to_string();
        // `cube workspace push` resolves the workspace from the current
        // directory (no `--workspace` flag), so it must run inside the
        // leased workspace, same as `rebase_workspace` above.
        let payload = self
            .run_json_in_dir(workspace_path, &["--json", "workspace", "push", "--pr", &pr_str])
            .await?;
        let pushed = payload.get("pushed").and_then(|v| v.as_bool()).unwrap_or(false);
        if !pushed {
            return Err(anyhow!("cube workspace push did not report pushed=true: {payload}"));
        }
        Ok(())
    }

    async fn verify_deletion_tripwire(
        &self,
        repo_slug: &str,
        head_before: &str,
        base_sha: &str,
        pr_number: u64,
    ) -> Vec<String> {
        let Some(head_after) = crate::merge_parent_deletion::fetch_pr_head_sha(repo_slug, pr_number).await else {
            return Vec::new();
        };
        crate::merge_parent_deletion::compute_merged_parent_deletions(repo_slug, head_before, base_sha, &head_after)
            .await
    }

    fn command_repr(&self, args: &[&str]) -> Option<(String, String)> {
        let Ok(agent) = self.cfg.agent() else { return None };
        let cmd = std::iter::once(agent.cube.command.as_str())
            .chain(agent.cube.args.iter().map(String::as_str))
            .chain(args.iter().copied())
            .map(crate::ssh_transport::shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        let cwd = self.cfg.work.cwd.display().to_string();
        Some((cmd, cwd))
    }
}

#[cfg(test)]
pub use boss_execution_contracts::{
    PoolDispatchPolicy, WorkerClaim, WorkerPool, WorkerPoolOccupancy, kind_always_dispatches_on_pool_driver,
    pool_dispatch_policy_for_worker_id, pool_driver_slug_for_execution_kind, slot_id_from_worker_id,
    worker_id_for_slot, worker_page_label,
};
#[cfg(not(test))]
use boss_execution_contracts::{WorkerPool, slot_id_from_worker_id, worker_id_for_slot, worker_page_label};

/// If `err` is (transitively) a `SlotBusy` app-error from the spawn
/// flow, return the run id the app reported as occupying the slot
/// (`None` for apps predating the field, `Some` otherwise). Used by
/// [`ExecutionCoordinator::run_execution`]'s pane-spawn-failure branch
/// to enrich the `dispatch.jsonl` entry with which pane caused the
/// rejection.
///
/// `err` arrives `.with_context(...)`-wrapped by the spawn flow, so
/// the concrete `EngineToAppError` is not the outermost type — this
/// walks the anyhow source chain rather than downcasting `err`
/// directly, which would only ever match the context wrapper.
fn slot_busy_occupant(err: &anyhow::Error) -> Option<Option<String>> {
    match err.chain().find_map(|cause| cause.downcast_ref::<EngineToAppError>()) {
        Some(EngineToAppError::SlotBusy { occupying_run_id }) => Some(occupying_run_id.clone()),
        _ => None,
    }
}

#[cfg(test)]
pub use boss_execution_contracts::{ExecutionPublisher, NoopExecutionPublisher};
#[cfg(not(test))]
use boss_execution_contracts::{ExecutionPublisher, NoopExecutionPublisher};

#[cfg(test)]
pub use boss_execution_contracts::RevisionSource;

/// Lease-time occupancy guard (defect 3). Given the workspace cube just
/// handed us and a snapshot of the engine's live worker registry, return
/// the `run_id`/execution-id of a **tracked, live** worker that already
/// occupies that workspace — `Some` means we must NOT lease it.
///
/// A workspace is considered occupied iff some live-state entry, other
/// than the execution being dispatched, is (a) not in a terminal activity
/// (the slot is still held), (b) backed by a still-alive OS process
/// (`pid_alive`), and (c) recorded against the same `cube_workspace_id`
/// (resolved from the DB via `workspace_id_of_execution`). Cube's own
/// lease bookkeeping should already make this impossible; the guard is
/// the belt-and-suspenders that downgrades the duplicate-dispatch /
/// shared-workspace incident from data-interleaving to a refused lease.
///
/// Pure (all I/O is injected) so the decision is unit-testable without a
/// running engine. A dead-pid occupant (the worker genuinely gone, which
/// is the normal orphan-resume case) returns `None` — the workspace is
/// re-leasable; only a *live* occupant blocks.
pub(crate) fn occupying_live_worker(
    leased_workspace_id: &str,
    self_execution_id: &str,
    live: &[LiveWorkerState],
    workspace_id_of_execution: impl Fn(&str) -> Option<String>,
    pid_alive: impl Fn(i32) -> bool,
) -> Option<String> {
    live.iter()
        .filter(|s| s.run_id != self_execution_id)
        .filter(|s| !s.activity.is_terminal())
        .filter(|s| s.shell_pid > 0 && pid_alive(s.shell_pid))
        .find(|s| workspace_id_of_execution(&s.run_id).as_deref() == Some(leased_workspace_id))
        .map(|s| s.run_id.clone())
}

fn default_coordinator_metrics() -> Arc<Registry> {
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    crate::dispatch_metrics::register_metrics(&metrics);
    crate::answer_agent_observability::register_metrics(&metrics);
    metrics
}

#[cfg(test)]
pub use boss_execution_contracts::{
    DispatchAdmission, DispatchHold, DispatchPause, DispatchPauseOrigin, SetConcurrencyCapOutcome,
};
#[cfg(not(test))]
use boss_execution_contracts::{
    DispatchAdmission, DispatchHold, DispatchPause, DispatchPauseOrigin, SetConcurrencyCapOutcome,
};

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct ExecutionCoordinator {
    work_db: Arc<WorkDb>,
    worker_pool: WorkerPool,
    /// Dedicated pool for automation triage and automation-produced task
    /// executions. Sized independently from the main pool (default 3) so
    /// maintenance work never contends with interactive dispatch.
    #[builder(default = WorkerPool::new_automation(MAX_AUTOMATION_POOL_SIZE))]
    automation_pool: WorkerPool,
    /// Dedicated pool for `pr_review` reviewer executions. Sized
    /// independently (default small — see [`DEFAULT_REVIEW_POOL_SIZE`]) so
    /// review latency and always-Opus review spend stay isolated from both
    /// the main and automation pools.
    #[builder(default = WorkerPool::new_review(DEFAULT_REVIEW_POOL_SIZE))]
    review_pool: WorkerPool,
    /// The local-host adapter. Retained as the `local` special case and
    /// the backing adapter for the default provider; the dispatch loop
    /// resolves the per-execution adapter through `host_adapter_provider`.
    host_adapter: Arc<dyn HostAdapter>,
    /// Builds the right [`HostAdapter`] for the host the scheduler selects
    /// (local vs SSH-remote). Defaults to [`LocalHostAdapterProvider`]
    /// (every host → the local adapter), which preserves the historical
    /// local-only behaviour; production installs an
    /// [`crate::host_adapter::SshHostAdapterProvider`] via
    /// [`Self::set_host_adapter_provider`].
    host_adapter_provider: Arc<dyn HostAdapterProvider>,
    remote_pane_releaser: Option<Arc<dyn crate::completion::WorkerPaneReleaser>>,
    #[builder(default = Arc::new(NoopExecutionPublisher))]
    publisher: Arc<dyn ExecutionPublisher>,
    /// Structured stream of dispatch-pipeline events. Defaults to a
    /// no-op so legacy tests and short-lived callers don't need to
    /// stand one up; production wiring should install a
    /// [`crate::dispatch_events::JsonlFileSink`] via
    /// [`ExecutionCoordinator::set_dispatch_events`] before
    /// scheduling starts.
    #[builder(default = Arc::new(NoopDispatchEventSink))]
    dispatch_events: Arc<dyn DispatchEventSink>,
    /// Dispatches handed off by `drain_ready_queue` that have not yet reached
    /// `start_execution_run_on_host` (the point where the DB starts reporting
    /// them `running`).
    ///
    /// The chain single-writer guard and the double-spawn guard both read
    /// liveness from `work_executions.status`, which cannot see a dispatch
    /// that is still resolving its lease. When the drain loop awaited each
    /// dispatch inline, the DB was a complete picture by construction; with
    /// concurrent hand-off it no longer is, and the guards union it with this
    /// registry to stay correct. See [`crate::dispatch_inflight`].
    #[builder(default)]
    inflight_dispatches: InflightDispatches,
    /// Permits bounding concurrent handed-off dispatch tails to
    /// [`MAX_INFLIGHT_DISPATCHES`].
    #[builder(default = Arc::new(Semaphore::new(MAX_INFLIGHT_DISPATCHES)))]
    dispatch_slots: Arc<Semaphore>,
    /// `true` while a `run_scheduler` task is alive. `kick()` returns
    /// without spawning when this is already set; the alive scheduler
    /// is responsible for noticing the wakeup via `scheduling_pending`.
    #[builder(default)]
    scheduling_active: AtomicBool,
    /// Wakeup flag set by every `kick()` (whether or not it spawned a
    /// fresh scheduler). The running scheduler reads + resets this on
    /// each outer iteration so that a kick which arrived during the
    /// drain — i.e. between the last `list_ready_executions()` call
    /// and the scheduler relinquishing `scheduling_active` — re-enters
    /// the drain loop instead of being silently dropped. Closes the
    /// TOCTOU between "queue saw empty" and "active=false" that left
    /// fresh `ready` executions stranded with no scheduler running.
    #[builder(default)]
    scheduling_pending: AtomicBool,
    /// Engine event bus. `kick()` publishes `Event::DispatchReady` onto it
    /// when [`Self::enable_dispatch_ready_bus`] is on; otherwise it goes
    /// unused. Defaults to a fresh, subscriber-less bus so every existing
    /// constructor and test needs no changes — publishing to a bus with no
    /// subscribers is simply a no-op fan-out.
    #[builder(default = Arc::new(EventBus::new()))]
    event_bus: Arc<EventBus>,
    /// Kill-switch for routing `kick()` through [`Self::event_bus`] instead
    /// of latching `scheduling_pending`/`scheduling_active` directly. OFF by
    /// default — see [`crate::config::DEFAULT_ENABLE_DISPATCH_READY_BUS`].
    /// Seeded once at construction from `WorkConfig::enable_dispatch_ready_bus`
    /// via [`Self::set_enable_dispatch_ready_bus`] (a boot-time setting, not a
    /// live-toggle — unlike `dispatch_paused`, nothing flips this after
    /// startup); `app.rs` also gates [`Self::spawn_dispatch_ready_subscriber`]
    /// on this same flag so a bus-routed `kick()` always has a live
    /// subscriber whenever it's on.
    #[builder(default)]
    enable_dispatch_ready_bus: bool,
    /// Repo origin URLs the cold-pool probe has already inspected in
    /// this engine's lifetime. The probe runs once per URL on the
    /// first successful `ensure_repo` for that URL; subsequent
    /// dispatches against the same URL skip both the `cube repo list`
    /// round-trip and the attention-item write. Engine restart resets
    /// this; per `multi-repo-work-modeling.md` R4 the deduplication
    /// scope is engine-lifetime, not durable.
    #[builder(default)]
    repo_cold_probe_seen: Mutex<HashSet<String>>,
    /// Backoff delays between successive pre-start retry attempts.
    /// Defaults to [`PRE_START_RETRY_DELAYS`]. Tests may override via
    /// [`Self::with_pre_start_retry_delays`] to avoid real sleeps.
    #[builder(default = PRE_START_RETRY_DELAYS.to_vec())]
    pre_start_retry_delays: Vec<Duration>,
    /// Bounded dispatch-stagger window (seconds) for the "later" side of a
    /// high-overlap `merge_order` sibling pair. `0` (the default) disables the
    /// stagger — the non-blocking `merge_order` relation still sequences merges,
    /// but no dispatch offset is applied. Seeded from
    /// [`crate::config::WorkConfig::merge_order_stagger_secs`] (already clamped
    /// to [`crate::config::MAX_MERGE_ORDER_STAGGER_SECS`]) at construction.
    #[builder(default)]
    merge_order_stagger_secs: u64,
    /// Engine-wide counter registry. Defaults to a fresh local registry
    /// with the lease counters pre-registered so tests that do not call
    /// `set_metrics` still get valid increments. Production wires in the
    /// shared engine registry via `set_metrics` after construction.
    #[builder(default = default_coordinator_metrics())]
    metrics: Arc<Registry>,
    /// Hook called when an execution transitions to `running`.
    /// Defaults to [`NoopExecutionStartedHook`]; production installs
    /// the `WorkerCompletionHandler` via
    /// [`Self::set_execution_started_hook`] so the SHA-delta gate
    /// can snapshot the bound chore PR's head SHA at run start.
    #[builder(default = Arc::new(NoopExecutionStartedHook))]
    execution_started_hook: Arc<dyn ExecutionStartedHook>,
    /// Teardown path used to preempt a spilled automation run when a
    /// mainline item is starved of interactive slots. Defaults to
    /// [`NoopAutomationPreemptor`], which disables preemption; production
    /// installs the `WorkerCompletionHandler` via
    /// [`Self::set_automation_preemptor`].
    #[builder(default = Arc::new(NoopAutomationPreemptor))]
    automation_preemptor: Arc<dyn AutomationPreemptor>,
    /// The global dispatch pause, as one value: `None` when dispatch is
    /// running, `Some` while it is held. Every question about the pause —
    /// is it on, who set it, when, why, and does it hold reviews — is
    /// answered from this one snapshot, so no two of those answers can come
    /// from different instants and a resume cannot leave part of an episode
    /// behind. Seeded from the `dispatch_paused*` metadata keys at engine
    /// startup; persisted there on every toggle so the pause survives an
    /// engine restart.
    ///
    /// [`Self::pause_dispatch`] takes a [`PauseReason`] (a validated
    /// non-empty string) rather than an `Option<String>`, so there is no
    /// code path that can set this to `Some` without a recorded reason —
    /// dispatch must never be found paused anonymously.
    ///
    /// A plain `std::sync::Mutex` rather than the `tokio::sync::Mutex` used
    /// elsewhere in this struct: every access is a quick read-or-replace
    /// with no `.await` in between, so a blocking lock avoids forcing every
    /// caller (including the many sync `is_dispatch_paused()`-style call
    /// sites) to become `async`.
    #[builder(default)]
    dispatch_pause: std::sync::Mutex<Option<DispatchPause>>,
    /// Execution ids `drain_ready_queue`'s pause gate must let through this
    /// pass even though [`Self::is_dispatch_paused`] is `true` — the
    /// in-memory, never-persisted mechanism behind the pause-only
    /// forced-dispatch override (`RequestExecutionInput::
    /// bypass_dispatch_pause`; see
    /// `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`
    /// and `coordinator/dispatch_admission.rs`). An id is inserted only
    /// after `ExecutionCoordinator::evaluate_dispatch_admission` has
    /// confirmed the pause is operator-originated and every other
    /// constraint already clears, and is always removed again (dispatched
    /// or not) by the single drain pass
    /// `dispatch_with_pause_bypass` awaits — so membership never outlives
    /// one request, and the DB row itself never records that an override
    /// happened. This is deliberately NOT a `work_executions` column: the
    /// design's "Persistence" note is explicit that force must leave no
    /// durable badge on the row.
    #[builder(default)]
    dispatch_pause_bypass_execution_ids: std::sync::Mutex<std::collections::HashSet<String>>,
    /// One-dispatch `--host` routes. Kept out of `work_executions` so they
    /// cannot overwrite or clear the durable pin escape hatch.
    ///
    /// Process-local and best-effort, not a durable pin: it does NOT
    /// survive an engine restart. An execution still `ready` (dispatch
    /// paused, no free slot, a restart landing mid-queue, ...) when the
    /// engine comes back up loses its `--host` constraint and is placed
    /// by ordinary (unconstrained) host selection on the next drain, with
    /// no error or notice — this is a known limitation of keeping the
    /// constraint in memory rather than on the execution row. Entries are
    /// removed on the paths internal to dispatch that can fire while a
    /// request-scoped host is still pinned: successful dispatch
    /// (`schedule_execution`'s `start_execution_run_on_host` success arm),
    /// the ineligible-requested-host cancel, any pre-start-failure cancel
    /// (host adapter build, `cube repo ensure`, workspace lease, cube
    /// change create, `start_execution_run`), and the pause-override
    /// no-residue refusal — and only when the clearing cancel itself
    /// succeeds; a failed cancel deliberately leaves the entry in place so
    /// a retry cannot land unpinned. It is only ever read (never
    /// destructively taken) during selection itself
    /// (`select_host_for_execution`), so a mid-dispatch failure cannot
    /// silently lose the constraint before the retry decision is made.
    /// An execution cancelled through an external path instead (e.g.
    /// `bossctl executions cancel` / `work cancel`) is NOT cleared here:
    /// the entry is left behind for the process lifetime. That leak is
    /// harmless — execution ids are unique, so a stale entry can never
    /// bind to another execution — but it does mean this map is not
    /// guaranteed empty for every terminal execution.
    #[builder(default)]
    requested_host_ids: std::sync::Mutex<HashMap<String, String>>,
    /// Startup capability gate for the local tmux runtime. Unlike the
    /// operator/breaker pause state this is not user-resumable: a worker
    /// cannot be dispatched until the engine has recorded a passing probe.
    #[builder(default)]
    dispatch_preflight_block_reason: std::sync::Mutex<Option<String>>,
    /// `true` from engine startup until the local host's capability probe
    /// has written its result. Startup clears the local host's stale `auto`
    /// capability rows and then probes in the background so the frontend
    /// socket can bind first; while this is set, `drain_ready_queue` holds
    /// every execution instead of failing it with "driver discovery has
    /// not run". Distinct from `dispatch_preflight_block_reason` so lifting
    /// this hold can never lift a failed tmux preflight. See
    /// [`crate::host_registry::refresh_local_host_auto_capabilities`].
    #[builder(default)]
    local_capability_discovery_pending: AtomicBool,
    /// `true` from engine startup until tmux adoption and the boot-only
    /// reconcile have finished. Independent of
    /// `local_capability_discovery_pending` so a completed capability probe
    /// cannot `kick()` the scheduler into claiming a ready row that
    /// `release_stale_claimed_executions` would then revert. See
    /// [`ExecutionCoordinator::set_startup_recovery_pending`].
    #[builder(default)]
    startup_recovery_pending: AtomicBool,
    /// Global automation-pause flag — independent of `dispatch_paused`. When
    /// `true`: `drain_ready_queue` holds every execution bound for the
    /// automation pool (see [`Self::execution_targets_automation_pool`]),
    /// and [`crate::automation_triage::EngineTriageDispatcher::fire`] refuses
    /// to start a new triage pass (both the scheduler's and `boss automation
    /// run`'s fire path go through that one seam). Already-claimed automation
    /// workers are unaffected. Seeded from the `automation_paused` metadata
    /// key at engine startup; persisted there on every toggle so the pause
    /// survives an engine restart. See [`FrontendRequest::SetAutomationPaused`]
    /// (`boss_protocol`) for why this is deliberately a separate switch from
    /// `dispatch_paused` rather than folded into it.
    #[builder(default)]
    automation_paused: AtomicBool,
    /// Epoch seconds when automation was last paused. Zero means "not
    /// paused". Seeded at startup from `automation_paused_since_epoch_s` in
    /// `state.db`.
    #[builder(default)]
    automation_paused_since_epoch_s: AtomicU64,
    /// Why automation is currently paused. Same non-anonymity guarantee as
    /// [`Self::dispatch_paused_reason`]: `None` iff `automation_paused` is
    /// `false`, `Some` iff it is `true` — [`Self::pause_automation`] takes
    /// a required [`PauseReason`], and [`Self::resume_automation`] always
    /// clears it.
    #[builder(default)]
    automation_paused_reason: std::sync::Mutex<Option<String>>,
    /// Live per-slot worker registry, used by the lease-time occupancy
    /// guard to refuse leasing a workspace that is still the cwd of a
    /// tracked, live worker process (defect 3 — belt-and-suspenders
    /// against the duplicate-dispatch shared-workspace incident). `None`
    /// in tests that don't wire it; production installs it via
    /// [`Self::set_live_worker_states`]. When absent the guard fails open
    /// (the historical no-check behaviour).
    live_worker_states: Option<Arc<crate::live_worker_state::LiveWorkerStateRegistry>>,
    /// Workspace IDs refused by the occupancy guard, keyed by execution id.
    /// When cube hands us a workspace that the engine's live-worker registry
    /// knows is still occupied, we release the lease and record the workspace
    /// here so the *next* `cube workspace lease` call passes `--exclude` and
    /// skips it — breaking the livelock where the same occupied-but-"free"
    /// workspace is re-offered on every retry. Lives in memory only (no DB
    /// schema change needed); clears on engine restart, which is acceptable
    /// because at worst we refuse once more and re-populate the set.
    #[builder(default = Mutex::new(HashMap::new()))]
    refused_workspaces: Mutex<HashMap<String, Vec<String>>>,
    /// Ceiling used by the interactive-pool concurrency cap in
    /// `drain_ready_queue`. Defaults to [`MAX_CONCURRENT_INTERACTIVE_WORKERS`].
    /// Live-settable via [`Self::set_max_concurrent_interactive_workers`]
    /// (an `AtomicUsize` so it can change on a shared `&self`, mirroring
    /// `dispatch_paused`); `app.rs` seeds it from `state.db` at startup and
    /// `bossctl dispatch concurrency --set N` changes it at runtime. Tests
    /// that exercise automation spillover/preemption at small pool sizes
    /// raise it via [`Self::with_max_concurrent_interactive_workers`] so the
    /// cap doesn't collide with the path under test.
    #[builder(default = AtomicUsize::new(MAX_CONCURRENT_INTERACTIVE_WORKERS))]
    max_concurrent_interactive_workers: AtomicUsize,
    /// Change notifier for the four pause flags above (`dispatch_paused` /
    /// `automation_paused` and their since/origin/reason companions). Every
    /// state-changing [`Self::pause_dispatch`] / [`Self::resume_dispatch`] /
    /// [`Self::pause_automation`] / [`Self::resume_automation`] call bumps
    /// the generation counter; [`Self::subscribe_pause_state`] hands out
    /// receivers.
    ///
    /// This is keyed to the **state transition**, deliberately not to any
    /// caller: every pauser — the `Set*Paused` RPC handlers, the
    /// spawn-capability circuit breaker
    /// ([`crate::spawn_health::trip_spawn_capability_circuit`]), or any
    /// future programmatic one — is observed by construction and none of
    /// them needs to fire a notification itself. See
    /// `ServerState::spawn_pause_state_health_broadcaster`.
    ///
    /// A `watch` (not the engine [`EventBus`]) because this is
    /// latest-state-wins with no payload to lose: a subscriber that misses
    /// two rapid toggles still re-reads current state and pushes one
    /// correct snapshot, and the channel needs no opt-in flag or filter
    /// wiring to be live in every build.
    #[builder(default = tokio::sync::watch::channel(0).0)]
    pause_state_changed: tokio::sync::watch::Sender<u64>,
    /// Consecutive pre-start spawn failures per (driver, worker kind), fed
    /// by both arms of [`Self::run_execution`]'s spawn outcome and read by
    /// the engine health report. Visibility only — nothing in dispatch
    /// consults it. See [`crate::pre_start_streak`].
    #[builder(default)]
    pre_start_streaks: Arc<crate::pre_start_streak::PreStartStreakTracker>,
    /// Per-repo bound on concurrent cube workspace-lease attempts. Drain
    /// takes a permit before claiming a slot so a lease storm cannot fill
    /// the interactive pool. Defaults to
    /// [`MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO`]. Skipped on the builder
    /// because [`lease_limiter::RepoLeaseLimiter`] is crate-private; tests
    /// override via [`Self::with_max_concurrent_leases_per_repo`].
    #[builder(skip = lease_limiter::RepoLeaseLimiter::new(MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO))]
    repo_lease_limiter: lease_limiter::RepoLeaseLimiter,
    /// Outer bound on a single `cube workspace lease` invocation. Defaults
    /// to [`CUBE_LEASE_TIMEOUT`]. Tests override via
    /// [`Self::with_cube_lease_timeout`] so a hang can fire without waiting
    /// 90s.
    #[builder(default = CUBE_LEASE_TIMEOUT)]
    cube_lease_timeout: Duration,
}

mod blocked_workspace;
mod config;
mod dispatch_admission;
mod execution;
mod lease_limiter;
mod run;
mod scheduler;

pub use config::{LOCAL_CAPABILITY_DISCOVERY_PENDING_REASON, STARTUP_RECOVERY_PENDING_REASON};
pub use dispatch_admission::{PauseBypassOutcome, pause_bypass_decision};
pub use run::PANE_SPAWN_FAILED_ATTENTION_KIND;
pub use scheduler::ANSWER_AGENT_READY_AGE_ATTENTION_KIND;

/// Copy a [`crate::cube_commands::CubeCliError`]'s structured exit code +
/// stderr into a dispatch-event `details` object when the failure was a
/// non-zero cube CLI exit (the anaplian failure-mode A: cube granted the
/// lease then a setup step exited non-zero). No-op for any other error —
/// a local timeout, a plain `anyhow!` from a test fake — so the field set
/// stays additive. Walks the error chain so a future `.context(…)` wrap
/// doesn't hide the typed cause.
fn augment_details_with_cube_cli_error(details: &mut serde_json::Value, err: &anyhow::Error) {
    let Some(cube_err) = err
        .chain()
        .find_map(|source| source.downcast_ref::<crate::cube_commands::CubeCliError>())
    else {
        return;
    };
    let Some(obj) = details.as_object_mut() else {
        return;
    };
    obj.insert(
        "cube_exit_code".to_owned(),
        match cube_err.exit_code {
            Some(code) => serde_json::json!(code),
            None => serde_json::Value::Null,
        },
    );
    // Clip so a step that dumps a large trace can't bloat the JSONL line.
    obj.insert(
        "cube_stderr".to_owned(),
        serde_json::json!(cube_err.clipped_stderr(2000)),
    );
    obj.insert("cube_host".to_owned(), serde_json::json!(cube_err.host));
}

/// The owning project id for capability-requirement lookup, if any. A
/// project is its own subject; a product has none.
fn work_item_project_id(item: &WorkItem) -> Option<String> {
    match item {
        WorkItem::Project(p) => Some(p.id.clone()),
        WorkItem::Task(t) | WorkItem::Chore(t) => t.project_id.clone(),
        WorkItem::Product(_) => None,
    }
}

/// Render a one-line, human-readable summary of why no host was eligible,
/// for the no-eligible-host pre-start failure / attention item.
///
/// When every considered host is missing the same required driver, lead
/// with an explicit "no enabled host has driver X" so an operator reading
/// the queue never has to decode per-host capability lists to see the
/// hold reason.
fn summarize_ineligibility(report: &[host_scheduling::Eligibility], required_driver: Option<&str>) -> String {
    use host_scheduling::IneligibilityReason as R;
    if report.is_empty() {
        return "no hosts are registered".to_owned();
    }
    let driver_cap = required_driver.map(crate::host_capability_probe::driver_capability);
    let per_host: Vec<String> = report
        .iter()
        .map(|h| {
            let disabled = h.reasons.iter().any(|r| matches!(r, R::Disabled));
            let reasons: Vec<String> = h
                .reasons
                .iter()
                .filter_map(|r| match r {
                    R::Disabled => Some("disabled".to_owned()),
                    R::NoFreeSlots => Some("no free slots".to_owned()),
                    R::NotSelectedHost => Some("not the requested/pinned host".to_owned()),
                    R::DriverProbeNotRun => {
                        if disabled {
                            return None;
                        }
                        if h.host_id == "local" {
                            Some("driver discovery has not run; restart Boss".to_owned())
                        } else {
                            Some(format!(
                                "driver discovery has not run; run `bossctl hosts probe {}`",
                                h.host_id
                            ))
                        }
                    }
                    R::MissingCapabilities(missing) => {
                        // Prefer a driver-specific phrase when the only
                        // missing cap is the required driver so the hold
                        // names the binary an operator must install.
                        if let (Some(cap), Some(driver)) = (driver_cap.as_deref(), required_driver)
                            && missing.len() == 1
                            && missing.iter().any(|m| m == cap)
                        {
                            return Some(format!("missing driver {driver}"));
                        }
                        Some(format!("missing capabilities [{}]", missing.join(", ")))
                    }
                })
                .collect();
            format!("{}: {}", h.host_id, reasons.join(", "))
        })
        .collect();
    let detail = per_host.join("; ");
    // When the driver is the distinguishing constraint and no host is
    // eligible, put the operator-facing hold reason first.
    if let Some(driver) = required_driver {
        let all_miss_driver = report.iter().all(|h| {
            !h.eligible
                && !h.reasons.iter().any(|r| matches!(r, R::DriverProbeNotRun))
                && h.reasons.iter().any(|r| match r {
                    R::MissingCapabilities(missing) => missing
                        .iter()
                        .any(|m| m == &crate::host_capability_probe::driver_capability(driver)),
                    _ => false,
                })
        });
        let any_enabled = report
            .iter()
            .any(|h| !h.reasons.iter().any(|r| matches!(r, R::Disabled | R::NotSelectedHost)));
        if all_miss_driver && any_enabled {
            return format!("no enabled host has driver {driver}; {detail}");
        }
    }
    detail
}

/// One failing-check record after parsing `ci_remediations.failed_checks`
/// back from JSON. Mirrors `ci_watch::FailedCheckRecord` on the read side;
/// kept here as a separate owned type so the coordinator doesn't depend
/// on ci_watch's private serialization shape.
#[cfg(test)]
#[derive(Debug, serde::Deserialize)]
struct FailedCheckJson {
    #[allow(dead_code)]
    name: String,
    conclusion: String,
    provider: String,
    #[serde(default)]
    provider_job_id: Option<String>,
}

/// Pick the worst-failing entry from a JSON-encoded `failed_checks`
/// list. Worst-first ordering per design §"pre-spawn fetch": FAILURE >
/// TIMED_OUT > CANCELLED > everything else. Returns `None` when the
/// JSON is empty / malformed / has no entry with an identifiable
/// provider job id at all.
#[cfg(test)]
fn pick_worst_failing_check(failed_checks_json: &str) -> Option<FailedCheckJson> {
    let parsed: Vec<FailedCheckJson> = serde_json::from_str(failed_checks_json).ok()?;
    if parsed.is_empty() {
        return None;
    }
    parsed.into_iter().min_by_key(|c| match c.conclusion.as_str() {
        "FAILURE" => 0,
        "TIMED_OUT" => 1,
        "CANCELLED" => 2,
        "STARTUP_FAILURE" => 3,
        _ => 4,
    })
}

/// Why `drain_ready_queue` returned. Re-entering the outer scheduler
/// loop immediately is fine for `QueueEmpty` (the post-drain wakeup
/// check decides whether to actually re-loop); `PoolExhausted` is
/// also fine because the post-`release_worker` `kick()` will spawn a
/// fresh scheduler anyway, and we only re-loop here when
/// `scheduling_pending` was raised after we started this drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainOutcome {
    /// No more `ready` rows in the database.
    QueueEmpty,
    /// Found a `ready` row but the worker pool had no idle slot;
    /// deferred to whoever releases a worker next.
    PoolExhausted,
}

fn execution_task_summary(execution: &WorkExecution, work_item: &WorkItem) -> String {
    let summary = match work_item {
        WorkItem::Product(product) => format!("{} {}", execution.kind, product.name),
        WorkItem::Project(project) => format!("{} {}", execution.kind, project.name),
        WorkItem::Task(task) | WorkItem::Chore(task) => format!("{} {}", execution.kind, task.name),
    };
    // Cube persists this identity as last_task on release. Names alone cannot
    // establish that a later dirty lease still contains this execution's work.
    format!("{} {summary}", execution.id)
}

fn execution_change_title(execution: &WorkExecution, work_item: &WorkItem) -> String {
    match work_item {
        WorkItem::Product(product) => format!("{}: {}", execution.kind, product.name),
        WorkItem::Project(project) => format!("{}: {}", execution.kind, project.name),
        WorkItem::Task(task) | WorkItem::Chore(task) => {
            format!("{}: {}", execution.kind, task.name)
        }
    }
}

/// Does `repo`'s cube pool config look like the auto-provisioned
/// defaults that `cube repo ensure` writes when a brand-new origin
/// turns up — i.e. nothing the operator has customised?
///
/// The check is conservative: every field has to look default. If any
/// of `main_branch`, `workspace_root`, `workspace_prefix`, or `source`
/// has been touched, we trust the operator and stay silent. The
/// advisory exists to nudge users who never noticed cube auto-cloned
/// into `~/.local/share/cube/workspaces`; once they run
/// `cube repo add` the next probe sees customised fields and the item
/// no longer surfaces.
fn repo_has_default_pool_config(repo: &CubeRepoSummary) -> bool {
    if repo.main_branch != "main" {
        return false;
    }
    if repo.source.is_some() {
        return false;
    }
    let expected_prefix = format!("{}-agent-", repo.repo_id);
    if repo.workspace_prefix != expected_prefix {
        return false;
    }
    workspace_root_is_cube_default(&repo.workspace_root)
}

/// Heuristic for "cube auto-provisioned this `workspace_root`". The
/// engine can't directly ask cube what its data dir is, so we compare
/// against cube's documented defaults: `$CUBE_DATA_DIR/workspaces`,
/// `$XDG_DATA_HOME/cube/workspaces`, or `~/.local/share/cube/workspaces`.
/// Anything else — including the `~/Documents/dev/workspaces` layout
/// the workspace rules recommend — is treated as customised.
fn workspace_root_is_cube_default(workspace_root: &Path) -> bool {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("CUBE_DATA_DIR") {
        candidates.push(PathBuf::from(path).join("workspaces"));
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(path).join("cube/workspaces"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/share/cube/workspaces"));
    }
    candidates.iter().any(|candidate| candidate == workspace_root)
}

/// Body for the `repo_cold_pool` advisory. Mirrors the design doc Q6
/// recommendation block so the user gets the exact `cube repo ensure`
/// invocation, pre-filled with this repo's origin.
fn cold_repo_attention_body(repo: &CubeRepoSummary) -> String {
    format!(
        "First dispatch against `{repo_id}` ({origin}).\n\
         Cube auto-provisioned a pool at `{workspace_root}` with prefix `{prefix}`.\n\n\
         To re-register with a non-default origin, run:\n\n\
         ```\n\
         cube repo ensure --origin {origin}\n\
         ```\n\n\
         Each pool has a configurable workspace count (concurrent workers per repo). \
         For multi-repo products this matters — see \
         `tools/boss/docs/designs/multi-repo-work-modeling.md` Q6. This item is \
         advisory; dispatch is proceeding with cube defaults.",
        repo_id = repo.repo_id,
        origin = repo.origin,
        workspace_root = repo.workspace_root.display(),
        prefix = repo.workspace_prefix,
    )
}

#[cfg(test)]
#[path = "coordinator_tests/mod.rs"]
mod tests;
