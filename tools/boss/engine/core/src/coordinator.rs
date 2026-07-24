use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use boss_protocol::{
    EngineToAppError, ExecutionKind, ExecutionStatus, FrontendEvent, LiveWorkerState, TaskKind, TaskStatus,
};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::RuntimeConfig;
use crate::conflict_diagnosis;
use crate::dispatch_events::{
    DispatchEvent, DispatchEventSink, NoopDispatchEventSink, Outcome as DispatchOutcome, Stage,
};
use crate::host_adapter::{HostAdapter, HostAdapterProvider, LocalHostAdapter, LocalHostAdapterProvider};
use crate::host_registry::Host;
use crate::host_scheduling::{self, ChoreRequirements, HostSlot};
use crate::metrics::Registry;
use crate::runner::{ExecutionRunner, RunOutcome, RunWaitState};
use crate::spawn_flow::StartWorkerError;
use crate::work::{
    CreateAttentionItemInput, DispatchClass, FinishExecutionRunInput, PreStartFailureOutcome, WorkDb, WorkExecution,
    WorkItem, WorkRun,
};

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

/// Hook invoked once per execution at the moment it transitions from
/// `ready` to `running` (`start_execution_run` succeeded). Production
/// wiring routes this into [`crate::completion::WorkerCompletionHandler::on_execution_started`],
/// which snapshots the bound chore PR's head SHA into
/// `work_executions.pr_head_before` for the Stop-boundary SHA-delta
/// gate. Decoupled from `WorkerCompletionHandler` directly so the
/// coordinator module doesn't take a hard dependency on the
/// completion module's surface.
#[async_trait]
pub trait ExecutionStartedHook: Send + Sync {
    async fn on_execution_started(&self, execution_id: &str);
}

/// No-op hook used as the default. Production swaps it out via
/// [`ExecutionCoordinator::set_execution_started_hook`].
#[derive(Debug, Default)]
pub struct NoopExecutionStartedHook;

#[async_trait]
impl ExecutionStartedHook for NoopExecutionStartedHook {
    async fn on_execution_started(&self, _execution_id: &str) {}
}

/// What a preemption teardown actually did to the victim. Mirrors the
/// subset of [`crate::completion::ForceReleaseOutcome`] the dispatcher
/// needs to branch on, kept as its own type so the coordinator module
/// doesn't take a hard dependency on the completion module's surface
/// (same rationale as [`ExecutionStartedHook`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreemptOutcome {
    /// The victim's pane was reaped and its cube lease released. Its
    /// pool slot is free and its work is safe to requeue.
    Released,
    /// The victim had no live pane to reap — it is mid-spawn, and its
    /// lease is deliberately still held (the mid-spawn-cancel lease-hold
    /// case: releasing it now would hand cube a workspace the in-flight
    /// spawn is about to occupy).
    /// The caller MUST abandon the preemption: do not cancel, do not
    /// requeue. The in-flight run releases the lease itself once its
    /// spawn settles.
    MidSpawn,
    /// Teardown was attempted and failed. The victim may be in any
    /// state; the caller abandons the preemption and leaves recovery to
    /// the existing sweeps.
    Failed,
}

/// Teardown path the dispatcher uses to preempt an in-progress spilled
/// automation run when a mainline item is starved of interactive slots
/// (see [`crate::dispatch_spillover`]).
///
/// Production wiring routes this into
/// [`crate::completion::WorkerCompletionHandler::force_release`], which
/// is the same incident-hardened teardown `bossctl agents stop` and the
/// stale-worker sweep use: it tears down the libghostty pane, fires the
/// `reap_worker_process_tree` SIGTERM/SIGKILL ladder at the worker's
/// whole process group (so no orphaned `claude` survives holding build
/// locks — the #975/#1006 leak class), releases the pool slot, and
/// releases the cube lease. Reusing it — rather than open-coding a
/// teardown here — is what keeps preemption's process-tree reap and its
/// mid-spawn-cancel lease-hold refusal correct by construction.
#[async_trait]
pub trait AutomationPreemptor: Send + Sync {
    async fn preempt_worker(&self, execution_id: &str) -> PreemptOutcome;
}

/// No-op preemptor used as the default: reports every teardown as
/// [`PreemptOutcome::Failed`], which disables preemption entirely (the
/// dispatcher abandons the attempt and the mainline item waits for a
/// slot exactly as it does today). Production swaps it out via
/// [`ExecutionCoordinator::set_automation_preemptor`]. Keeping the
/// default inert means the many test doubles that never exercise
/// preemption need no changes.
#[derive(Debug, Default)]
pub struct NoopAutomationPreemptor;

#[async_trait]
impl AutomationPreemptor for NoopAutomationPreemptor {
    async fn preempt_worker(&self, _execution_id: &str) -> PreemptOutcome {
        PreemptOutcome::Failed
    }
}

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

/// Number of interactive-worker slots shown on a single page (one macOS
/// tab). The main pool is partitioned into fixed-size pages so that
/// capacity is expressed as `pages × page size` rather than a magic total —
/// keeping the model general as pages later grow (e.g. remote-worker-backed
/// pages) instead of hard-coding a flat pane count.
pub const WORKER_PAGE_SIZE: usize = 8;

/// Number of interactive-worker pages. Page 0 is "Bridge Crew" (the original
/// 8 slots); page 1 is "Lower Decks", a strict spillover pool the dispatcher
/// only claims into once every Bridge Crew slot is occupied. Bumping this is
/// the single knob that adds another page of capacity.
///
/// Lower Decks is also the only page automation may spill into when the
/// automation pool is full, and the only page a mainline item can reclaim
/// a slot from by preempting such a spill — Bridge Crew is never touched
/// by either mechanism. See [`crate::dispatch_spillover`].
pub const WORKER_PAGE_COUNT: usize = 2;

/// Hard cap on the interactive/main worker pool. The runtime config can
/// request a smaller pool, but values above this are clamped (with a
/// warning). Derived from the page geometry: `WORKER_PAGE_SIZE *
/// WORKER_PAGE_COUNT` (currently 16 = two pages of 8). All pool-namespace
/// derivations (`worker_id_for_slot`, `slot_id_from_worker_id`, and the app's
/// mirrored `workerSlotCount`) key off this constant so the interactive,
/// automation, and review ranges stay disjoint and engine/app agree on the
/// slot namespace across both pages.
pub const MAX_WORKER_POOL_SIZE: usize = WORKER_PAGE_SIZE * WORKER_PAGE_COUNT;

/// Hard cap on the automation worker pool. The runtime config can request a
/// smaller pool via `BOSS_AUTOMATION_POOL_SIZE`, but values above this are
/// clamped. Raised from 6 to 8 per operator request (2026-07-15): automation
/// demand regularly exceeds the pool.
pub const MAX_AUTOMATION_POOL_SIZE: usize = 8;

/// Hard cap on the review worker pool. The runtime config can request a
/// smaller pool via `BOSS_REVIEW_POOL_SIZE`, but values above this are
/// clamped. The third pool, modeled on the automation pool, that runs the
/// always-Opus `pr_review` reviewer agents. See design:
/// automated-reviewer-pass-on-every-agent-authored-pr.md
pub const MAX_REVIEW_POOL_SIZE: usize = 8;

/// Default review-pool slot count when `BOSS_REVIEW_POOL_SIZE` is unset.
/// Raised to 8 to match the main worker pool and reduce review-queue
/// contention when many PRs land simultaneously.
pub const DEFAULT_REVIEW_POOL_SIZE: usize = 8;

/// TEMPORARY hard cap on concurrently-live INTERACTIVE ("normal") pool
/// workers — a *row dispatched from* the automation or review pool is never
/// held by this gate; those pools' own sizes govern their own home-pool
/// dispatch. That is NOT the same as saying automation never counts toward
/// the number this cap compares against: `busy_count()` counts every claimed
/// `worker_pool` slot, and a spilled automation run (one that overflowed its
/// full home pool onto a Lower Decks interactive slot via `claim_worker_spill`)
/// IS one of those slots, so it DOES count against this cap. That is a
/// deliberate policy choice, not an oversight: counting spilled automation
/// keeps the cap's load-bearing promise (never more than
/// `MAX_CONCURRENT_INTERACTIVE_WORKERS` live interactive-pool workers, spilled
/// automation included) intact, while the once-per-pass automation-preemption
/// fallback in `drain_ready_queue` is what keeps mainline from starving behind
/// spilled automation at the cap — a preemption is a trade (one live worker
/// for another), so it can reclaim a slot from spilled automation without
/// ever pushing the live count past the cap. The interactive pool has 16
/// slots across two pages (Bridge Crew + Lower Decks), but the 2026-07-15
/// full-fleet saturation experiment (22 live workers, load average ~152)
/// pushed individual task times past an hour and broke pane spawn acks, so
/// dispatch is held to one page's worth — the pre-Lower-Decks size — even
/// though all 16 slots exist. Deliberately hardcoded per operator directive
/// until remote workers / the dynamic pane-budget model land (see
/// docs/designs/fleet-scaling-dynamic-panes-and-team-semantics.md); enforced
/// in `drain_ready_queue`, deliberately NOT in `claim_worker_force` (an
/// explicit operator `bossctl agents launch` may exceed the cap).
pub const MAX_CONCURRENT_INTERACTIVE_WORKERS: usize = 8;

/// Worker ID prefix for automation-pool slots. Distinct from the main-pool
/// `"worker-"` prefix so `pool_for_worker_id` can route releases to the
/// correct pool without an extra DB round-trip.
const AUTOMATION_WORKER_ID_PREFIX: &str = "auto-worker-";

/// Worker ID prefix for review-pool slots. Distinct from both the main-pool
/// `"worker-"` and automation-pool `"auto-worker-"` prefixes so
/// `pool_for_worker_id` can route releases to the review pool.
const REVIEW_WORKER_ID_PREFIX: &str = "review-";

/// Execution kind string for reviewer agent runs. A `pr_review` execution
/// reviews a worker's PR read-only and always routes to the dedicated review
/// pool. Re-exported from `boss_protocol` so routing and the (future)
/// completion handler share one source of truth.
#[cfg(test)]
pub(crate) use boss_protocol::EXECUTION_KIND_PR_REVIEW;

/// Upper bound on how long the engine waits for a single
/// `cube workspace lease` subprocess invocation before declaring the
/// attempt a timeout failure. The motivating incident
/// (`exec_18aec07893bd2e30_29`, 2026-05-12) sat in `worker_claimed/ok`
/// for ~46 seconds with no event because the cube subprocess never
/// returned and the engine was awaiting it unboundedly. With this
/// timeout the engine surfaces a `cube_workspace_lease_failed` event
/// and either falls back or fails cleanly within seconds.
const CUBE_LEASE_TIMEOUT: Duration = Duration::from_secs(30);

/// Same upper bound for `cube repo ensure`. `ensure_repo` is normally
/// fast (it's an idempotent record lookup), but the same hang class
/// applies if cube wedges, so we time-bound it too.
const CUBE_REPO_ENSURE_TIMEOUT: Duration = Duration::from_secs(60);

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

/// How often `run_execution`'s [`HeartbeatGuard`] re-stamps the cube
/// lease expiry. Cube's `DEFAULT_LEASE_TTL_SECS` is 30 minutes, so a
/// 5-minute cadence gives ~6 chances to renew within one TTL window
/// — generous enough that a single failed beat (e.g., a transient
/// cube subprocess failure) doesn't immediately put the lease at
/// risk.
const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// `work_attention_items.kind` filed when a `ready` execution has been
/// repeatedly deferred behind the SAME `chain_serialized` live sibling for
/// at least [`CHAIN_SERIALIZED_STALL_THRESHOLD_SECS`] — the 2026-07-09
/// chain-serialization re-defer stall incident (`task_18c07e0a815e6bd0_1de`):
/// the dispatcher re-evaluated and
/// re-deferred every ~10s for ~20 minutes with zero durable, user-visible
/// signal, so a human only noticed by grepping `engine-trace.jsonl`. Filed
/// once per stall (idempotent via [`WorkDb::upsert_work_item_attention`])
/// and resolved via [`WorkDb::resolve_external_tracker_attention`] the
/// moment the row is no longer chain-serialized (dispatched, or the sibling
/// was reconciled dead — see [`ExecutionCoordinator::live_chain_siblings`]).
pub const CHAIN_SERIALIZED_STALL_ATTENTION_KIND: &str = "chain_serialized_stall";

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
    /// A live `pr_review` sibling was found but bypassed: `execution` is a
    /// merge-conflict-fix revision, and reviews are read-only so cannot
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
        let handle = tokio::spawn(async move {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CubeRepoHandle {
    pub repo_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CubeWorkspaceLease {
    pub lease_id: String,
    pub workspace_id: String,
    pub workspace_path: PathBuf,
    /// Cube's positive recovery check, reported only on an `--allow-dirty`
    /// reclaim:
    ///
    /// - `Some(true)`  — the workspace still held work that exists on no
    ///   remote, i.e. cube recovered the dead worker's tree in place. This is
    ///   the good path: the work is live, with its jj operation log intact.
    /// - `Some(false)` — the lease succeeded but there was nothing left to
    ///   recover (the tree had already been reset). The caller must fall back
    ///   to the engine's saved recovery patch.
    /// - `None`        — not a recovery lease; the field does not apply.
    ///
    /// The distinction matters because lease success alone never proved
    /// recovery: a caller that inferred it would start from an empty tree
    /// while believing it was resuming.
    pub dirty_verified: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CubeChangeHandle {
    pub change_id: String,
}

/// Outcome of an engine-direct `cube workspace rebase` (rung 1 of the
/// merge-conflict escalation ladder — see
/// `docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`).
///
/// Mirrors the two terminal states `cube workspace rebase` reports on its
/// `--json` `payload`: `REBASED_CLEAN` (jj rebased with no conflicts and,
/// unless `--no-push` was passed, advanced+pushed the boss bookmark) or
/// `REBASED_WITH_CONFLICTS` (conflicts materialized in the working copy,
/// nothing pushed). The `conflicted_files` list is best-effort informational
/// and sourced from `jj resolve --list`; [`parse_rebase_payload`] strips
/// jj's trailing conflict-type descriptor from each entry, so callers always
/// see bare paths here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseOutcome {
    /// `true` for `REBASED_CLEAN`, `false` for `REBASED_WITH_CONFLICTS`.
    pub clean: bool,
    /// Whether the rebased branch was pushed to GitHub. Always `false` on
    /// the conflict path; `true` on a clean rebase unless `--no-push`.
    pub pushed: bool,
    /// Files still conflicted after the structural rebase (empty when clean).
    pub conflicted_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder, serde::Deserialize)]
#[builder(on(String, into))]
pub struct CubeWorkspaceStatus {
    pub workspace_id: String,
    pub workspace_path: PathBuf,
    pub state: String,
    pub lease_id: Option<String>,
    pub holder: Option<String>,
    pub task: Option<String>,
    pub leased_at_epoch_s: Option<i64>,
    pub lease_expires_at_epoch_s: Option<i64>,
}

/// Pool-config view of a repo as returned by `cube repo list --json`.
/// Used by the cold-pool probe in [`ExecutionCoordinator::schedule_execution`]
/// to decide whether the auto-provisioned defaults are worth flagging
/// to the operator. See `multi-repo-work-modeling.md` Q6.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder, serde::Deserialize)]
#[builder(on(String, into))]
pub struct CubeRepoSummary {
    #[serde(rename = "repo")]
    pub repo_id: String,
    pub origin: String,
    pub main_branch: String,
    pub workspace_root: PathBuf,
    pub workspace_prefix: String,
    #[serde(default)]
    pub source: Option<PathBuf>,
}

#[async_trait]
pub trait CubeClient: Send + Sync {
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle>;
    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease>;
    async fn create_change(&self, workspace_path: &Path, title: &str) -> Result<CubeChangeHandle>;
    /// Position the working copy in `workspace_path` as a fresh editable
    /// child commit atop PR `pr`'s current head. Delegates to
    /// `cube workspace goto --workspace <path> --pr <n>`. Idempotent.
    async fn goto_workspace(&self, workspace_path: &Path, pr: u64) -> Result<()>;
    /// Run an engine-direct `cube workspace rebase --pr <pr>` inside the
    /// leased workspace at `workspace_path`, rebasing the PR's boss branch
    /// onto the repo's integration branch. On a clean rebase the command
    /// advances and pushes the boss bookmark itself (rung 1 of the
    /// merge-conflict escalation ladder — no agent, zero tokens). See
    /// [`RebaseOutcome`].
    ///
    /// The default implementation errors so the many test doubles and the
    /// host-adapter layers need no change; the harness treats any error as
    /// "rung 1 unavailable" and falls through to the worker path. Only
    /// [`CommandCubeClient`] provides a real implementation.
    async fn rebase_workspace(&self, workspace_path: &Path, pr: u64) -> Result<RebaseOutcome> {
        let _ = (workspace_path, pr);
        Err(anyhow!("rebase_workspace is not supported by this CubeClient"))
    }
    /// Like [`Self::rebase_workspace`] but passes `--no-push`: run the
    /// engine-direct structural rebase without advancing/pushing the boss
    /// bookmark on a clean result, so the real PR branch is never mutated.
    /// Used for the speculative/throwaway rebase the merge poller's
    /// prediction sweep runs against in-review PRs to observe whether they
    /// would conflict against current `main` (the conflict-against-main
    /// result-gate layer of the merge-conflict-reduction design). Same
    /// default-Err pattern as
    /// `rebase_workspace` — only [`CommandCubeClient`] implements it for
    /// real.
    async fn rebase_workspace_no_push(&self, workspace_path: &Path, pr: u64) -> Result<RebaseOutcome> {
        let _ = (workspace_path, pr);
        Err(anyhow!("rebase_workspace_no_push is not supported by this CubeClient"))
    }
    /// Land whatever is now resolved in `workspace_path`'s working copy
    /// onto PR `pr`'s branch: advance the branch bookmark to `@` and push,
    /// via `cube workspace push --pr <pr>`. The engine-side counterpart to
    /// a worker hand-rolling `jj bookmark set` + `jj git push` — used by
    /// rung 0 of the merge-conflict escalation ladder (deterministic
    /// resolvers) to land a resolution it produced by editing the
    /// conflicted files directly, with no further rebase. Errors if `@`
    /// still has unresolved conflicts, or if the push itself fails.
    ///
    /// The default implementation errors so the many test doubles and the
    /// host-adapter layers need no change; only [`CommandCubeClient`]
    /// provides a real implementation.
    async fn push_resolution(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        let _ = (workspace_path, pr);
        Err(anyhow!("push_resolution is not supported by this CubeClient"))
    }
    /// The conflict-against-main result-gate layer: verify a resolution
    /// already pushed to PR `pr`'s branch against the both-parents deletion
    /// tripwire (`merge_parent_deletion::compute_merged_parent_deletions`).
    /// Returns the tripwire's rendered finding lines — empty means clean.
    /// Used by the merge-conflict escalation ladder's mechanical rungs (0
    /// and 1, `conflict_ladder.rs`) to vet an auto-retiring resolution the
    /// same way the worker-driven `pr_review` pass already vets rung 2/3's
    /// output (`completion.rs::compute_merge_parent_deletion_signoff`).
    ///
    /// Default: fails open (empty — no finding, matching
    /// `compute_merged_parent_deletions`'s own fail-open contract) and
    /// makes no network call, so the many test doubles need no change —
    /// same "unlisted keeps the default" convention as `rebase_workspace`
    /// / `push_resolution` above. Only [`CommandCubeClient`] overrides
    /// this with the real gh-backed check.
    async fn verify_deletion_tripwire(
        &self,
        _repo_slug: &str,
        _head_before: &str,
        _base_sha: &str,
        _pr_number: u64,
    ) -> Vec<String> {
        Vec::new()
    }
    async fn release_workspace(&self, lease_id: &str) -> Result<()>;
    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus>;
    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()>;
    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()>;
    /// Snapshot every workspace cube knows about. Returns one entry
    /// per workspace, the same shape `workspace_status` returns for a
    /// single workspace.
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>>;
    /// Snapshot every repo cube has registered. One round-trip;
    /// callers use it to inspect pool config for advisory checks like
    /// the cold-repo probe.
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>>;
    /// Returns `(command_string, cwd)` for the subprocess that would be
    /// spawned with `args`. Used to populate `cube_command`/`cube_cwd`
    /// in dispatch events so failures are reproducible from the terminal.
    /// Returns `None` for test doubles that don't use real subprocesses.
    fn command_repr(&self, _args: &[&str]) -> Option<(String, String)> {
        None
    }
}

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
        let mut command = Command::new(&agent.cube.command);
        command
            .args(&agent.cube.args)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = command.output().await.with_context(|| {
            format!(
                "failed to spawn Cube command: {} {}",
                agent.cube.command,
                agent.cube.args.join(" ")
            )
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let detail = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("exit status {}", output.status)
            };
            return Err(anyhow!("Cube command failed: {detail}"));
        }

        serde_json::from_slice(&output.stdout).context("failed to decode Cube JSON output")
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
        let workspace_arg = workspace_path.display().to_string();
        let pr_str = pr.to_string();
        let _ = self
            .run_json(&[
                "--json",
                "workspace",
                "goto",
                "--workspace",
                &workspace_arg,
                "--pr",
                &pr_str,
            ])
            .await?;
        Ok(())
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

#[derive(Debug, Clone)]
pub struct WorkerPool {
    inner: Arc<Mutex<WorkerPoolInner>>,
}

#[derive(Debug)]
struct WorkerPoolInner {
    workers: Vec<WorkerSlot>,
}

#[derive(Debug, Clone)]
struct WorkerSlot {
    worker_id: String,
    execution_id: Option<String>,
    last_workspace_id: Option<String>,
}

/// Deterministic, page-aware free-slot selection shared by
/// [`WorkerPool::claim_worker`] and [`WorkerPool::claim_worker_force`].
///
/// Returns the index of the slot to claim plus a short label describing why
/// it was chosen, or `None` when every slot is occupied.
///
/// Selection enforces **strict spillover across pages**: the candidate set is
/// restricted to the *lowest page* (a contiguous [`WORKER_PAGE_SIZE`] block of
/// slot indices) that still has a free slot, so a Lower Decks slot (page 1) is
/// only ever chosen once every Bridge Crew slot (page 0) is occupied. Within
/// that page, workspace affinity wins if an idle slot last ran the preferred
/// workspace; otherwise the lowest free index is chosen. Selection is fully
/// deterministic — no RNG — so the engine and app never disagree about which
/// slot a dispatch will land on.
fn select_claim_index(workers: &[WorkerSlot], preferred_workspace_id: Option<&str>) -> Option<(usize, &'static str)> {
    // `enumerate()` yields ascending indices and `filter` preserves order, so
    // `free` is sorted ascending and `free[0]` is the globally-lowest free slot.
    let free: Vec<usize> = workers
        .iter()
        .enumerate()
        .filter(|(_, w)| w.execution_id.is_none())
        .map(|(idx, _)| idx)
        .collect();
    let lowest_free = *free.first()?;
    let target_page = lowest_free / WORKER_PAGE_SIZE;

    // Affinity is honored only within the lowest non-full page, so it can
    // never jump the dispatch ahead into Lower Decks while Bridge Crew is free.
    let affinity_idx = preferred_workspace_id.and_then(|target| {
        free.iter().copied().find(|&idx| {
            idx / WORKER_PAGE_SIZE == target_page && workers[idx].last_workspace_id.as_deref() == Some(target)
        })
    });

    Some(match affinity_idx {
        Some(idx) => (idx, "affinity"),
        None => (lowest_free, "spillover"),
    })
}

/// A snapshot of one currently-claimed worker-pool slot: which logical
/// worker id is held and by which execution. Returned by
/// [`WorkerPool::claims`] so the pool-claim reconciler (and any
/// occupancy report) can pair a held slot with its execution and decide
/// whether the claim has outlived its execution. Unlike
/// [`WorkerPool::claimed_execution_ids`] (a bare set), this preserves the
/// `worker_id → execution_id` mapping the reconciler needs for a safe
/// compare-and-release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerClaim {
    pub worker_id: String,
    pub execution_id: String,
}

/// Emit a structured `worker_pool_claim` log pairing a slot with the
/// execution that just claimed it. Mirrors the release log in
/// [`WorkerPool::release_worker`]; together they make pool occupancy
/// observable from the engine log (and let an operator reconstruct
/// "which execution holds each claim" without instrumenting the pool).
fn log_pool_claim(worker_id: &str, execution_id: &str, selection: &str) {
    tracing::info!(worker_id, execution_id, selection, "worker_pool_claim worker claimed");
}

impl WorkerPool {
    pub fn new(size: usize) -> Self {
        Self::new_with_prefix(size, "worker-", MAX_WORKER_POOL_SIZE)
    }

    /// Construct an automation pool. Slots are named `auto-worker-N` so
    /// `pool_for_worker_id` can distinguish them from main-pool slots.
    /// Capped at [`MAX_AUTOMATION_POOL_SIZE`].
    pub fn new_automation(size: usize) -> Self {
        Self::new_with_prefix(size, AUTOMATION_WORKER_ID_PREFIX, MAX_AUTOMATION_POOL_SIZE)
    }

    /// Construct a review pool. Slots are named `review-N` so
    /// `pool_for_worker_id` can distinguish them from main- and
    /// automation-pool slots. Capped at [`MAX_REVIEW_POOL_SIZE`].
    pub fn new_review(size: usize) -> Self {
        Self::new_with_prefix(size, REVIEW_WORKER_ID_PREFIX, MAX_REVIEW_POOL_SIZE)
    }

    fn new_with_prefix(size: usize, prefix: &str, hard_cap: usize) -> Self {
        let clamped = if size > hard_cap {
            tracing::warn!(
                requested = size,
                cap = hard_cap,
                "worker pool size exceeds hard cap; clamping"
            );
            hard_cap
        } else {
            size
        };
        let workers = (0..clamped)
            .map(|index| WorkerSlot {
                worker_id: format!("{}{}", prefix, index + 1),
                execution_id: None,
                last_workspace_id: None,
            })
            .collect();
        Self {
            inner: Arc::new(Mutex::new(WorkerPoolInner { workers })),
        }
    }

    /// Claim an idle worker for `execution_id`. Selection is deterministic and
    /// page-aware (see [`select_claim_index`]): the lowest free slot in the
    /// lowest non-full page is chosen, with workspace affinity preferred within
    /// that page. For the interactive pool this yields **strict spillover** —
    /// a Lower Decks slot (page 1) is claimed only once every Bridge Crew slot
    /// (page 0) is occupied — and the choice is reproducible, so the engine and
    /// app never disagree about which slot a dispatch lands on.
    pub async fn claim_worker(&self, execution_id: &str, preferred_workspace_id: Option<&str>) -> Option<String> {
        let mut inner = self.inner.lock().await;
        let (chosen_idx, selection) = select_claim_index(&inner.workers, preferred_workspace_id)?;
        let worker = &mut inner.workers[chosen_idx];
        worker.execution_id = Some(execution_id.to_owned());
        let worker_id = worker.worker_id.clone();
        log_pool_claim(&worker_id, execution_id, selection);
        Some(worker_id)
    }

    /// Snapshot this pool's slots as the spillover policy's view type.
    /// Read-only; the returned data is a copy, so the caller must not
    /// assume it stays accurate once the lock is dropped (every decision
    /// derived from it is re-validated by the atomic claim below).
    pub(crate) async fn slot_views(&self) -> Vec<crate::dispatch_spillover::SlotView> {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .enumerate()
            .map(|(index, w)| crate::dispatch_spillover::SlotView {
                index,
                occupied: w.execution_id.is_some(),
                last_workspace_id: w.last_workspace_id.clone(),
            })
            .collect()
    }

    /// Claim a **Lower Decks** slot for an automation execution spilling
    /// out of a full automation pool. Returns `None` when page 1 has no
    /// free slot, or when the live-worker count is already at or above
    /// `max_concurrent_interactive_workers` — a spilled claim counts
    /// toward that cap exactly like a mainline claim does (see
    /// `MAX_CONCURRENT_INTERACTIVE_WORKERS`'s doc), so it must be gated
    /// the same way. The busy count and the claim happen under the same
    /// lock hold, so the cap check can never race against a concurrent
    /// claim on either path.
    ///
    /// Unlike [`Self::claim_worker`], this never considers page 0: a
    /// spilling automation may not take a Bridge Crew slot even when
    /// Bridge Crew is entirely idle, so mainline always retains its 8
    /// slots (see [`crate::dispatch_spillover`] for why). Selection and
    /// the occupancy test happen under one lock hold, so two concurrent
    /// spills can never land on the same slot.
    pub(crate) async fn claim_worker_spill(
        &self,
        execution_id: &str,
        preferred_workspace_id: Option<&str>,
        max_concurrent_interactive_workers: usize,
    ) -> Option<String> {
        let mut inner = self.inner.lock().await;
        let busy = inner.workers.iter().filter(|w| w.execution_id.is_some()).count();
        if busy >= max_concurrent_interactive_workers {
            return None;
        }
        let views: Vec<crate::dispatch_spillover::SlotView> = inner
            .workers
            .iter()
            .enumerate()
            .map(|(index, w)| crate::dispatch_spillover::SlotView {
                index,
                occupied: w.execution_id.is_some(),
                last_workspace_id: w.last_workspace_id.clone(),
            })
            .collect();
        let chosen_idx =
            crate::dispatch_spillover::select_spill_claim_index(&views, preferred_workspace_id, WORKER_PAGE_SIZE)?;
        let worker = &mut inner.workers[chosen_idx];
        worker.execution_id = Some(execution_id.to_owned());
        let worker_id = worker.worker_id.clone();
        log_pool_claim(&worker_id, execution_id, "automation-spill");
        Some(worker_id)
    }

    /// Skip-the-queue claim used by `bossctl agents launch`. Same
    /// deterministic, page-aware selection as `claim_worker`, but if every
    /// configured slot is busy and the pool is still below the hard
    /// cap (`MAX_WORKER_POOL_SIZE`) we grow the pool by one fresh slot
    /// and hand it back. Returns `None` only when the pool is already
    /// at the hard cap with no idle slot — at that point there's no
    /// pane the macOS app could render anyway, so the launch is
    /// rejected rather than silently overcommitting.
    pub async fn claim_worker_force(&self, execution_id: &str, preferred_workspace_id: Option<&str>) -> Option<String> {
        let mut inner = self.inner.lock().await;

        if let Some((chosen_idx, selection)) = select_claim_index(&inner.workers, preferred_workspace_id) {
            let worker = &mut inner.workers[chosen_idx];
            worker.execution_id = Some(execution_id.to_owned());
            let worker_id = worker.worker_id.clone();
            log_pool_claim(
                &worker_id,
                execution_id,
                match selection {
                    "affinity" => "force-affinity",
                    _ => "force-spillover",
                },
            );
            return Some(worker_id);
        }

        // Every existing slot is busy. Grow the pool — bounded by the
        // hard cap so the app's fixed pane workspace can always render the
        // forced worker.
        if inner.workers.len() >= MAX_WORKER_POOL_SIZE {
            return None;
        }
        let new_index = inner.workers.len();
        let worker = WorkerSlot {
            worker_id: format!("worker-{}", new_index + 1),
            execution_id: Some(execution_id.to_owned()),
            last_workspace_id: None,
        };
        let id = worker.worker_id.clone();
        inner.workers.push(worker);
        log_pool_claim(&id, execution_id, "force-grow");
        Some(id)
    }

    /// Release `worker_id` back to the idle pool. If `last_workspace_id`
    /// is provided we record it as the worker's affinity for future
    /// preferred-workspace claims.
    pub async fn release_worker(&self, worker_id: &str, last_workspace_id: Option<&str>) {
        let mut inner = self.inner.lock().await;
        if let Some(worker) = inner.workers.iter_mut().find(|worker| worker.worker_id == worker_id) {
            let released_execution = worker.execution_id.take();
            if let Some(workspace_id) = last_workspace_id {
                worker.last_workspace_id = Some(workspace_id.to_owned());
            }
            if let Some(execution_id) = released_execution {
                tracing::info!(
                    worker_id,
                    execution_id = %execution_id,
                    "worker_pool_release worker freed"
                );
            }
        }
    }

    /// Compare-and-release: free `worker_id` back to the idle pool only
    /// if it is currently claimed by exactly `execution_id`. Returns
    /// `true` when the slot was released, `false` when the slot was not
    /// found or is claimed by a different (or no) execution.
    ///
    /// This is the reconciler-safe variant of [`Self::release_worker`].
    /// The pool-claim reconciler snapshots a leaked claim, then releases
    /// it on a later await; in the gap between the two, normal teardown
    /// could have freed the slot and a fresh dispatch could have
    /// re-claimed the SAME slot for a different, live execution. An
    /// unconditional `release_worker` would yank that new claim; the
    /// execution-id guard makes the release a no-op in that race.
    pub async fn release_worker_if_execution(
        &self,
        worker_id: &str,
        execution_id: &str,
        last_workspace_id: Option<&str>,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        if let Some(worker) = inner.workers.iter_mut().find(|worker| worker.worker_id == worker_id)
            && worker.execution_id.as_deref() == Some(execution_id)
        {
            worker.execution_id = None;
            if let Some(workspace_id) = last_workspace_id {
                worker.last_workspace_id = Some(workspace_id.to_owned());
            }
            tracing::info!(
                worker_id,
                execution_id,
                "worker_pool_release worker freed (compare-and-release)"
            );
            return true;
        }
        false
    }

    /// Snapshot every currently-claimed slot as `(worker_id,
    /// execution_id)` pairs. Used by the pool-claim reconciler to walk
    /// the pool's OWN held slots (rather than the live-state registry)
    /// and by occupancy reporting to show which execution holds each
    /// slot. Preserves the slot→execution mapping that
    /// [`Self::claimed_execution_ids`] discards.
    pub async fn claims(&self) -> Vec<WorkerClaim> {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .filter_map(|w| {
                w.execution_id.as_ref().map(|execution_id| WorkerClaim {
                    worker_id: w.worker_id.clone(),
                    execution_id: execution_id.clone(),
                })
            })
            .collect()
    }

    pub async fn capacity(&self) -> usize {
        let inner = self.inner.lock().await;
        inner.workers.len()
    }

    /// Return true if at least one worker slot is idle (not currently
    /// claimed by an in-flight execution). Used by the orphan-active
    /// sweep to bail early rather than touching the DB when no worker
    /// could pick up a newly-queued execution.
    pub async fn has_idle_worker(&self) -> bool {
        let inner = self.inner.lock().await;
        inner.workers.iter().any(|w| w.execution_id.is_none())
    }

    /// Return the set of execution ids currently claimed by a worker
    /// slot. Used by the orphan-active sweep as the `is_live` oracle:
    /// an execution that is not claimed has no live worker driving it
    /// even if its DB status is still non-terminal.
    pub async fn claimed_execution_ids(&self) -> std::collections::HashSet<String> {
        let inner = self.inner.lock().await;
        inner.workers.iter().filter_map(|w| w.execution_id.clone()).collect()
    }

    /// Format a worker id for slot `slot_id`. Inverse of
    /// [`slot_id_from_worker_id`]; both sides of the
    /// engine-owns-allocation refactor lean on this string format
    /// being stable so `worker-{N}` and slot N stay 1:1.
    pub fn worker_id_for_slot(slot_id: u8) -> String {
        format!("worker-{}", slot_id)
    }

    /// Number of slots currently claimed by an execution. Used by the
    /// dispatcher's interactive-pool concurrency gate
    /// ([`MAX_CONCURRENT_INTERACTIVE_WORKERS`]).
    pub(crate) async fn busy_count(&self) -> usize {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .filter(|worker| worker.execution_id.is_some())
            .count()
    }

    #[cfg(test)]
    pub(crate) async fn idle_count(&self) -> usize {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .filter(|worker| worker.execution_id.is_none())
            .count()
    }

    #[cfg(test)]
    async fn worker_affinity(&self, worker_id: &str) -> Option<String> {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .find(|worker| worker.worker_id == worker_id)
            .and_then(|worker| worker.last_workspace_id.clone())
    }
}

/// Parse the trailing 1-indexed slot number out of a worker id.
/// Regular-pool `worker-{N}` ids map directly to slot N.
/// Automation-pool `auto-worker-{N}` ids map to slot
/// `N + MAX_WORKER_POOL_SIZE`; review-pool `review-{N}` ids map to
/// slot `N + MAX_WORKER_POOL_SIZE + MAX_AUTOMATION_POOL_SIZE` so the
/// three pools occupy disjoint slot ranges. With the current geometry
/// (`MAX_WORKER_POOL_SIZE = 16`) that is `1..=16` interactive (Bridge
/// Crew 1..=8, Lower Decks 9..=16), `17..=24` automation, `25..=32`
/// review — so "auto-worker-1" → slot 17, "review-1" → slot 25, never
/// colliding with another pool's range. Every boundary is derived from
/// the pool-size constants, so bumping [`WORKER_PAGE_COUNT`] shifts the
/// automation/review ranges up in lock-step on both the engine and app.
///
/// Returns `None` for ids that don't match any recognised shape
/// or whose suffix isn't a positive `u8`. Callers should treat
/// `None` as a programming error — the only producer is
/// [`WorkerPool::claim_worker`].
pub fn slot_id_from_worker_id(worker_id: &str) -> Option<u8> {
    if let Some(suffix) = worker_id.strip_prefix(REVIEW_WORKER_ID_PREFIX) {
        let ordinal = suffix.parse::<u8>().ok().filter(|n| *n >= 1)? as usize;
        return u8::try_from(ordinal + MAX_WORKER_POOL_SIZE + MAX_AUTOMATION_POOL_SIZE).ok();
    }
    if let Some(suffix) = worker_id.strip_prefix(AUTOMATION_WORKER_ID_PREFIX) {
        let ordinal = suffix.parse::<u8>().ok().filter(|n| *n >= 1)? as usize;
        return u8::try_from(ordinal + MAX_WORKER_POOL_SIZE).ok();
    }
    if let Some(suffix) = worker_id.strip_prefix("worker-") {
        return suffix.parse::<u8>().ok().filter(|n| *n >= 1);
    }
    None
}

/// Returns the pool-level model override for the given `worker_id`, or `None`
/// for the main pool (which has no override and falls through to the effort-
/// driven default).
///
/// Both the automation pool (`auto-worker-N`) and the review pool (`review-N`)
/// always pin to Opus, per the automated-reviewer design §5: "the review pool
/// sets its override to Opus unconditionally … reuses the automation pool's
/// override mechanism." Returning a `'static str` avoids an allocation —
/// callers pass this directly to [`crate::effort::resolve_spawn_config`].
pub fn pool_model_override_for_worker_id(worker_id: &str) -> Option<&'static str> {
    if worker_id.starts_with(REVIEW_WORKER_ID_PREFIX) || worker_id.starts_with(AUTOMATION_WORKER_ID_PREFIX) {
        Some("opus")
    } else {
        None
    }
}

/// Derive the canonical worker-id string for a pane slot id.
/// Inverse of [`slot_id_from_worker_id`]: regular-pool slots
/// (1..=MAX_WORKER_POOL_SIZE) produce `"worker-{N}"`; automation-pool
/// slots (MAX_WORKER_POOL_SIZE < slot ≤ MAX_WORKER_POOL_SIZE +
/// MAX_AUTOMATION_POOL_SIZE) produce `"auto-worker-{M}"`; review-pool
/// slots (beyond that) produce `"review-{M}"`, where M is the slot's
/// offset from the start of the owning pool's range. Callers that
/// release a pane slot must use this instead of
/// [`WorkerPool::worker_id_for_slot`] to ensure the release is
/// routed to the correct pool.
pub fn worker_id_for_slot(slot_id: u8) -> String {
    let slot = slot_id as usize;
    if slot <= MAX_WORKER_POOL_SIZE {
        format!("worker-{}", slot_id)
    } else if slot <= MAX_WORKER_POOL_SIZE + MAX_AUTOMATION_POOL_SIZE {
        format!("{}{}", AUTOMATION_WORKER_ID_PREFIX, slot - MAX_WORKER_POOL_SIZE)
    } else {
        format!(
            "{}{}",
            REVIEW_WORKER_ID_PREFIX,
            slot - MAX_WORKER_POOL_SIZE - MAX_AUTOMATION_POOL_SIZE
        )
    }
}

/// Human-readable page label for an interactive/main-pool `slot_id`, or
/// `None` for slots outside the interactive pool (automation, review, or
/// remote virtual slots). Page 0 is "Bridge Crew", page 1 is "Lower Decks";
/// further pages fall back to `Deck N` so the label never panics if
/// [`WORKER_PAGE_COUNT`] grows. Recorded into `dispatch.jsonl` details at
/// claim/spawn time so a failure on a Lower Decks slot is distinguishable
/// from a Bridge Crew one in `bossctl dispatch diagnose`.
pub fn worker_page_label(slot_id: u8) -> Option<String> {
    let slot = slot_id as usize;
    if !(1..=MAX_WORKER_POOL_SIZE).contains(&slot) {
        return None;
    }
    Some(match (slot - 1) / WORKER_PAGE_SIZE {
        0 => "Bridge Crew".to_owned(),
        1 => "Lower Decks".to_owned(),
        n => format!("Deck {}", n + 1),
    })
}

/// If `err` is (transitively) a `SlotBusy` app-error from the spawn
/// flow, return the run id the app reported as occupying the slot
/// (`None` for apps predating the field, `Some` otherwise). Used by
/// [`ExecutionCoordinator::run_execution`]'s pane-spawn-failure branch
/// to enrich the `dispatch.jsonl` entry with which pane caused the
/// rejection.
///
/// `err` arrives `.with_context(...)`-wrapped by the spawn flow, so
/// the concrete `StartWorkerError` is not the outermost type — this
/// walks the anyhow source chain rather than downcasting `err`
/// directly, which would only ever match the context wrapper.
fn slot_busy_occupant(err: &anyhow::Error) -> Option<Option<String>> {
    match err.chain().find_map(|cause| cause.downcast_ref::<StartWorkerError>()) {
        Some(StartWorkerError::AppError(EngineToAppError::SlotBusy { occupying_run_id })) => {
            Some(occupying_run_id.clone())
        }
        _ => None,
    }
}

/// Sink for `executions.<id>` topic invalidations. The engine wires this
/// to the topic broker; tests use a no-op or recording double.
#[async_trait]
pub trait ExecutionPublisher: Send + Sync {
    async fn publish(&self, execution_id: &str, work_item_id: &str, status: &str, reason: &str);

    /// Publish a work-tree invalidation on the work item's product
    /// topic so subscribers (the kanban view) re-fetch and pick up
    /// status changes the coordinator drove from a non-request path
    /// — e.g., the auto-advance of `tasks.status` to `'active'` that
    /// happens inside `start_execution_run`.
    async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str);

    /// Push a typed [`FrontendEvent`] verbatim on the work item's
    /// product topic. Used for activity-feed events such as
    /// `ConflictResolutionStarted` / `Succeeded` / `Failed` /
    /// `Abandoned` (design Q8) where subscribers need the full
    /// payload, not just a "refetch" hint.
    async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent);

    /// Nudge the execution scheduler to drain its ready queue. Called
    /// by the merge-poller's conflict-detection path after inserting a
    /// `conflict_resolution` execution so the worker is dispatched
    /// promptly rather than waiting for the next opportunistic kick.
    /// Default is a no-op — only the production `BrokerExecutionPublisher`
    /// overrides this.
    fn kick_scheduler(&self) {}
}

#[derive(Default)]
pub struct NoopExecutionPublisher;

#[async_trait]
impl ExecutionPublisher for NoopExecutionPublisher {
    async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
    async fn publish_work_item_changed(&self, _: &str, _: &str, _: &str) {}
    async fn publish_frontend_event_on_product(&self, _: &str, _: FrontendEvent) {}
}

/// Tiny abstraction so the coordinator can bump the shared work-revision
/// counter without depending on `ServerState`.
pub trait RevisionSource: Send + Sync {
    fn next(&self) -> u64;
}

impl RevisionSource for AtomicU64 {
    fn next(&self) -> u64 {
        self.fetch_add(1, Ordering::SeqCst) + 1
    }
}

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
    metrics
}

/// Why dispatch is currently paused. Determines whether `drain_ready_queue`
/// exempts `pr_review` executions from the pause — see
/// [`ExecutionCoordinator::dispatch_pause_exempts_reviews`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPauseOrigin {
    /// An operator toggled `bossctl dispatch pause` / the app's pause
    /// switch. A review is the lifecycle of a change already in flight, not
    /// new work, so reviews keep dispatching through an operator pause.
    Operator,
    /// The spawn-capability circuit breaker tripped (see
    /// [`crate::spawn_health`]) because the app's worker-pane spawn path
    /// itself is broken. Exempting reviews here would just burn spawn
    /// attempts against the same dead path, so reviews are held like
    /// everything else.
    Breaker,
}

impl DispatchPauseOrigin {
    /// Stable string persisted to `state.db` under
    /// [`crate::app::handler_helpers::METADATA_KEY_DISPATCH_PAUSE_ORIGIN`].
    pub fn as_metadata_str(self) -> &'static str {
        match self {
            DispatchPauseOrigin::Operator => "operator",
            DispatchPauseOrigin::Breaker => "breaker",
        }
    }

    /// Parse the persisted metadata value. Anything unrecognized (including
    /// absent, e.g. a pause persisted before this field existed) defaults to
    /// `Breaker` — the conservative choice that does NOT exempt reviews,
    /// since restoring an unknown pause as review-exempt could resume
    /// burning spawn attempts against a still-broken spawn path.
    pub fn from_metadata_str(value: Option<&str>) -> Self {
        match value {
            Some("operator") => DispatchPauseOrigin::Operator,
            _ => DispatchPauseOrigin::Breaker,
        }
    }
}

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
    /// Global dispatch-pause flag. When `true`, `drain_ready_queue` exits
    /// immediately without claiming any slots. Seeded from the `dispatch_paused`
    /// metadata key at engine startup; persisted there on every toggle so the
    /// pause survives an engine restart.
    #[builder(default)]
    dispatch_paused: AtomicBool,
    /// Epoch seconds when dispatch was last paused. Zero means "not paused".
    /// Seeded at startup from `dispatch_paused_since_epoch_s` in `state.db`.
    #[builder(default)]
    dispatch_paused_since_epoch_s: AtomicU64,
    /// Whether the current pause exempts `pr_review` executions from
    /// `drain_ready_queue`'s pause gate — `true` when the pause originated
    /// from [`DispatchPauseOrigin::Operator`], `false` for
    /// [`DispatchPauseOrigin::Breaker`]. Only meaningful while
    /// `dispatch_paused` is `true`; set on every `set_dispatch_paused(true, …)`
    /// call and otherwise left at its last value.
    #[builder(default)]
    dispatch_pause_exempts_reviews: AtomicBool,
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
    /// `drain_ready_queue`. Defaults to [`MAX_CONCURRENT_INTERACTIVE_WORKERS`];
    /// tests that exercise automation spillover/preemption in isolation from
    /// that unrelated, temporary cap raise it via
    /// [`Self::with_max_concurrent_interactive_workers`] so the two features
    /// don't collide at small pool sizes.
    #[builder(default = MAX_CONCURRENT_INTERACTIVE_WORKERS)]
    max_concurrent_interactive_workers: usize,
}

mod config;
mod execution;
mod run;
mod scheduler;

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
fn summarize_ineligibility(report: &[host_scheduling::Eligibility]) -> String {
    use host_scheduling::IneligibilityReason as R;
    if report.is_empty() {
        return "no hosts are registered".to_owned();
    }
    let per_host: Vec<String> = report
        .iter()
        .map(|h| {
            let reasons: Vec<String> = h
                .reasons
                .iter()
                .map(|r| match r {
                    R::Disabled => "disabled".to_owned(),
                    R::NoFreeSlots => "no free slots".to_owned(),
                    R::NotPinned => "not the pinned host".to_owned(),
                    R::MissingCapabilities(missing) => {
                        format!("missing capabilities [{}]", missing.join(", "))
                    }
                })
                .collect();
            format!("{}: {}", h.host_id, reasons.join(", "))
        })
        .collect();
    per_host.join("; ")
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
    match work_item {
        WorkItem::Product(product) => format!("{} {}", execution.kind, product.name),
        WorkItem::Project(project) => format!("{} {}", execution.kind, project.name),
        WorkItem::Task(task) | WorkItem::Chore(task) => format!("{} {}", execution.kind, task.name),
    }
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
