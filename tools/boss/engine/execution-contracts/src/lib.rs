//! Shared execution service contracts and worker-pool state.
//! Concrete host operations remain in the host executor and upper adapters.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use boss_engine_effort::SpawnConfig;
use boss_engine_recovery::execution_bookmark::{self, ExecutionBookmark};
use boss_engine_storage_values::Host;
use boss_engine_storage_values::pool_policy::{AUTOMATION_WORKER_ID_PREFIX, REVIEW_WORKER_ID_PREFIX};
use boss_engine_structured_output::StructuredOutputKind;
use boss_host_execution::recovery_pr::fetch_recovery_pr_base;
pub use boss_host_execution::types::{
    CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
};
use boss_protocol::{DispatchPauseScope, ExecutionStatus, FrontendEvent, WorkExecution, WorkItem};
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAttention {
    pub kind: String,
    pub title: String,
    pub body_markdown: String,
}

/// What a worker is waiting for after a run ends. Drives the lease
/// retain/release decision in the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunWaitState {
    /// Run finished cleanly with no further work expected (`completed` or
    /// equivalent terminal status). Workspace is released.
    Terminal,
    /// Worker is blocked on an upstream dependency. Workspace is released
    /// and re-leased when the work becomes ready again.
    WaitingDependency,
    /// Worker is awaiting human input/redirect. Workspace is retained so
    /// the next run can continue in-place.
    ///
    /// **Not** the post-spawn outcome of a pane-hosted run — that is
    /// [`Self::WorkerPaneAlive`]. A runner may only return this when it has
    /// positive evidence the worker is blocked on a person; a runner that
    /// merely handed off to a live agent has not.
    ///
    /// No runner currently constructs this — `PaneSpawnRunner` and the
    /// remote host adapter both return [`Self::WorkerPaneAlive`], and the
    /// only source of positive wait-on-a-person evidence today is the
    /// driver's own hook stream, mirrored onto the row by
    /// `boss_engine::awaiting_input_status` rather than returned from a runner.
    /// Retained for a runner that can observe a genuine block directly
    /// (out-of-band from the hook stream) rather than inferring it.
    WaitingHuman,
    /// Worker is awaiting human review of an open PR. Workspace retained.
    WaitingReview,
    /// Worker is awaiting merge of an approved PR. Workspace retained.
    WaitingMerge,
    /// The execution was cancelled (kanban drag to Backlog, force-stop)
    /// *during* its spawn window — after the worker pane came up but
    /// before the run could be recorded. The runner has already reaped
    /// the just-spawned pane; the coordinator must release the cube
    /// lease the cancel path deliberately left held and skip the normal
    /// completion recording (the row is already terminal). See
    /// `PaneSpawnRunner::run_execution` and the mid-spawn-cancel
    /// collision this closes.
    CancelledDuringSpawn,
    /// An agent pane was successfully spawned. The pane is alive and its
    /// agent is actively working; the engine has handed the turn loop off
    /// but nothing is blocked on a person. The execution stays in
    /// `running` until the agent's own lifecycle concludes it — a Stop
    /// hook driving `on_stop`'s completion machinery for a worker, or
    /// `finalize_pr_review_pass` → `record_worker_pr_completion` for a
    /// reviewer. Workspace is retained so the pane can continue.
    ///
    /// This is the outcome of every successful pane spawn, whatever the
    /// execution kind: `waiting_human` would be semantically wrong for any
    /// of them — nobody is waiting for a person while the agent is working.
    ///
    /// Using `running` is also what keeps the "AI reviewing" badge visible
    /// on kanban cards for the duration of a review — the badge queries
    /// `pr_review` executions in `running` status.
    WorkerPaneAlive,
}

impl RunWaitState {
    pub fn execution_status(self) -> ExecutionStatus {
        match self {
            RunWaitState::Terminal => ExecutionStatus::Completed,
            RunWaitState::WaitingDependency => ExecutionStatus::WaitingDependency,
            RunWaitState::WaitingHuman => ExecutionStatus::WaitingHuman,
            RunWaitState::WaitingReview => ExecutionStatus::WaitingReview,
            RunWaitState::WaitingMerge => ExecutionStatus::WaitingMerge,
            // The row is already `cancelled`; the coordinator never
            // drives a status transition for this variant. Report the
            // terminal status for completeness.
            RunWaitState::CancelledDuringSpawn => ExecutionStatus::Cancelled,
            // Agent pane is alive and working; execution stays `running`.
            RunWaitState::WorkerPaneAlive => ExecutionStatus::Running,
        }
    }

    pub fn release_workspace(self) -> bool {
        matches!(
            self,
            RunWaitState::Terminal | RunWaitState::WaitingDependency | RunWaitState::CancelledDuringSpawn
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub wait_state: RunWaitState,
    pub result_summary: Option<String>,
    pub attention: Option<RunAttention>,
    /// Pane slot the worker was actually allocated into, if this run
    /// hosts a tmux pane. The coordinator stamps this onto the
    /// run record's `agent_id` (as `worker-{slot_id}`) so `bossctl
    /// agents list` shows one entry per active pane instead of
    /// collapsing every run into the worker-pool placeholder. `None`
    /// means the runner doesn't have a pane (e.g., a test fake);
    /// the coordinator leaves agent_id alone.
    pub slot_id: Option<u8>,
    /// Resolved per-execution effort + model knobs the runner used
    /// to construct the worker's `claude` invocation. The coordinator
    /// surfaces this on the `pane_spawned` dispatch event so
    /// `bossctl dispatch diagnose <exec-id>` shows what model and
    /// effort value the worker actually launched with — design §Q2:
    /// "surfaces the chosen model, effort value, and level on the
    /// dispatch instrumentation stream." `None` for fake runners that
    /// don't go through the spawn-config resolver.
    pub spawn_config: Option<SpawnConfig>,
}

#[async_trait]
pub trait ExecutionRunner: Send + Sync {
    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome>;
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

    /// Like [`Self::publish_work_item_changed`], but for a change that touched
    /// only the runtime fields of this one card: its dispatch wait, PR poll
    /// state, merge-queue position or doc pointer. No card was added,
    /// removed, re-parented or re-statused and no dependency edge moved, so
    /// subscribers may re-read just this card instead of the whole product
    /// board. A site must call this only when its write meets that bar;
    /// anything uncertain stays on `publish_work_item_changed`, which is the
    /// full-board invalidation.
    ///
    /// The default forwards to `publish_work_item_changed`: a publisher that
    /// does not tell the classes apart treats every change as structural.
    async fn publish_work_item_runtime_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.publish_work_item_changed(product_id, work_item_id, reason).await;
    }

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

/// Hook invoked once per execution at the moment it transitions from
/// `claimed`/`ready` to `running` (`start_execution_run` succeeded). Production
/// wiring routes this into `boss_engine::completion::WorkerCompletionHandler::on_execution_started`,
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
/// `ExecutionCoordinator::set_execution_started_hook`.
#[derive(Debug, Default)]
pub struct NoopExecutionStartedHook;

#[async_trait]
impl ExecutionStartedHook for NoopExecutionStartedHook {
    async fn on_execution_started(&self, _execution_id: &str) {}
}

/// What a preemption teardown actually did to the victim. Mirrors the
/// subset of `boss_engine::completion::ForceReleaseOutcome` the dispatcher
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
/// (see `boss_engine::dispatch_spillover`).
///
/// Production wiring routes this into
/// `boss_engine::completion::WorkerCompletionHandler::force_release`, which
/// is the same incident-hardened teardown `bossctl agents stop` and the
/// stale-worker sweep use: it verifies the tmux token, tears down the
/// session, detaches the app viewer, and fires the
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
/// `ExecutionCoordinator::set_automation_preemptor`. Keeping the
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
/// by either mechanism. See `boss_engine::dispatch_spillover`.
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
/// clamped. Sized at 8 because automation demand regularly exceeds a smaller
/// pool.
pub const MAX_AUTOMATION_POOL_SIZE: usize = 8;

pub use boss_engine_storage_values::pool_policy::{DEFAULT_REVIEW_POOL_SIZE, MAX_REVIEW_POOL_SIZE};

/// Outcome of an engine-direct `cube workspace rebase` (rung 1 of the
/// merge-conflict escalation ladder — see
/// `docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`).
///
/// Mirrors the two terminal states `cube workspace rebase` reports on its
/// `--json` `payload`: `REBASED_CLEAN` (jj rebased with no conflicts and,
/// unless `--no-push` was passed, advanced+pushed the boss bookmark) or
/// `REBASED_WITH_CONFLICTS` (conflicts materialized in the working copy,
/// nothing pushed). The `conflicted_files` list is best-effort informational
/// and sourced from `jj resolve --list`; the cube adapter's parser strips
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
    /// How many *replay-only* conflicted commits `cube workspace rebase`
    /// collapsed forward to make the branch pushable (0 for an ordinary
    /// clean rebase, and always 0 on the conflict path).
    ///
    /// A rebase replays each commit individually, so an ancestor of the
    /// branch head can carry jj's conflict flag even when the head's rebased
    /// tree is already correct and `git merge-tree` reports the merge clean.
    /// `jj git push` refuses a range containing any conflicted commit, so
    /// before cube learned to collapse those the rebase was complete and
    /// unpushable — and rung 1 declined, sending a conflict that did not
    /// exist at the head to an agent. Non-zero here means cube collapsed
    /// them *and* proved the pushed tree byte-identical to the rebased head;
    /// it is carried purely so the ladder can say so in its trace rather
    /// than reporting an indistinguishable "clean rebase".
    pub linearized_commits: usize,
    /// Which guard refused the replay-only collapse, as cube's stable slug
    /// (`conflicted_head`, `bookmarked_ancestor`, `multiple_children`,
    /// `round_limit`, `range_still_conflicted`, `tree_drift`, `probe_error`,
    /// `nothing_to_collapse`, `no_head`, or `not_attempted`).
    ///
    /// `None` on a clean rebase (nothing declined) and on a cube older than
    /// the decline-reason plumbing. Without this every decline looked
    /// identical from the engine's side — "a real conflict at the head" was
    /// indistinguishable from "collapsed three commits then the tree drifted"
    /// — which is exactly the blind spot that made the mechanical path's
    /// silence undiagnosable. Stamped on the ladder's rung-1 fall-through
    /// trace so guard hits can be counted without a code change.
    pub linearize_decline: Option<String>,
}

#[async_trait]
pub trait CubeClient: Send + Sync {
    async fn recovery_pr_base(&self, origin: &str, pr: u64) -> Result<String> {
        fetch_recovery_pr_base(origin, pr).await
    }

    async fn create_execution_bookmark(
        &self,
        workspace: &Path,
        execution_id: &str,
        predecessor: Option<&boss_engine_recovery::execution_bookmark::ExecutionBookmark>,
        inherited_base: Option<&str>,
    ) -> Result<boss_engine_recovery::execution_bookmark::ExecutionBookmark> {
        let _ = (workspace, execution_id, predecessor, inherited_base);
        anyhow::bail!("execution bookmarks are not supported by this CubeClient")
    }
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
    /// Position the working copy in `workspace_path` as a fresh editable
    /// child commit atop an arbitrary commit `sha` (e.g. a merge commit),
    /// bypassing the PR-state resolution `goto_workspace` performs — the
    /// only entry point that works once the PR has merged or closed.
    /// Delegates to `cube workspace goto --workspace <path> --revision
    /// <sha>`. Idempotent.
    ///
    /// The default implementation errors so the many test doubles and the
    /// host-adapter layers need no change; only `CommandCubeClient`
    /// provides a real implementation.
    async fn goto_workspace_revision(&self, workspace_path: &Path, sha: &str) -> Result<()> {
        let _ = (workspace_path, sha);
        Err(anyhow!("goto_workspace_revision is not supported by this CubeClient"))
    }
    /// Repair the shared `pr/<pr>` alias in the store at `store` from GitHub's
    /// current head through Cube's guarded writer (`cube workspace
    /// repair-pr-bookmark`). Never moves a working copy.
    ///
    /// The default implementation errors so the many test doubles and the
    /// host-adapter layers need no change; only `CommandCubeClient`
    /// provides a real implementation.
    async fn repair_pr_bookmark(&self, store: &Path, pr: u64) -> Result<()> {
        let _ = (store, pr);
        Err(anyhow!("repair_pr_bookmark is not supported by this CubeClient"))
    }
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
    /// `CommandCubeClient` provides a real implementation.
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
    /// `rebase_workspace` — only `CommandCubeClient` implements it for
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
    /// host-adapter layers need no change; only `CommandCubeClient`
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
    /// / `push_resolution` above. Only `CommandCubeClient` overrides
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
    /// End-of-run release that resets the working copy so the workspace
    /// returns to the free pool. Defaults to [`Self::release_workspace`] so
    /// test doubles need no change; `CommandCubeClient` passes
    /// `--force-reset`.
    async fn release_workspace_with_reset(&self, lease_id: &str) -> Result<()> {
        self.release_workspace(lease_id).await
    }
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
pub struct WorkerPool {
    inner: Arc<Mutex<WorkerPoolInner>>,
    /// Slot count, fixed at construction (slots are only ever claimed or
    /// released, never added or removed). Cached outside the mutex so
    /// [`WorkerPool::capacity_sync`] can be read without an `await`.
    len: usize,
    /// Worker-id prefix for this pool (`"worker-"`, `AUTOMATION_WORKER_ID_PREFIX`,
    /// or `REVIEW_WORKER_ID_PREFIX`), stamped at construction. [`Self::claim_worker_force`]
    /// uses this — rather than a hardcoded main-pool prefix — so a forced claim that
    /// grows a review or automation pool still mints a worker id with that pool's
    /// prefix, keeping `pool_for_worker_id` / [`pool_dispatch_policy_for_worker_id`]
    /// routing correct regardless of which pool was force-claimed.
    prefix: &'static str,
    /// Hard cap this pool's force-claim growth may not exceed (`MAX_WORKER_POOL_SIZE`,
    /// [`MAX_AUTOMATION_POOL_SIZE`], or [`MAX_REVIEW_POOL_SIZE`] depending on which
    /// pool this is). See [`Self::prefix`]'s doc for why this must be pool-specific
    /// rather than always `MAX_WORKER_POOL_SIZE`.
    hard_cap: usize,
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
    /// `true` once this slot's execution has a cube workspace lease.
    /// A claimed slot with `working == false` is only acquiring a
    /// workspace and must not count toward the interactive concurrency cap.
    working: bool,
}

/// Snapshot of interactive-pool occupancy, splitting live workers from
/// slots that are only acquiring a workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerPoolOccupancy {
    /// Claimed slots whose workspace lease has already succeeded.
    pub working: usize,
    /// Claimed slots still acquiring a workspace (`cube_lease_id` not yet
    /// recorded). Excluded from [`WorkerPool::busy_count`].
    pub lease_pending: usize,
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

    fn new_with_prefix(size: usize, prefix: &'static str, hard_cap: usize) -> Self {
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
                working: false,
            })
            .collect();
        Self {
            len: clamped,
            prefix,
            hard_cap,
            inner: Arc::new(Mutex::new(WorkerPoolInner { workers })),
        }
    }

    /// This pool's hard cap on force-claim growth (see [`Self::prefix`]'s
    /// doc). Used by `ExecutionCoordinator::force_dispatch` to report an
    /// accurate cap in its error when a forced claim is rejected.
    pub fn hard_cap(&self) -> usize {
        self.hard_cap
    }

    /// Slot count, without locking. Equivalent to `capacity().await` but
    /// usable from sync callers (e.g. clamping a requested concurrency cap
    /// against the live pool size without threading `async` through every
    /// caller of `ExecutionCoordinator::set_max_concurrent_interactive_workers`).
    pub fn capacity_sync(&self) -> usize {
        self.len
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
        worker.working = false;
        let worker_id = worker.worker_id.clone();
        log_pool_claim(&worker_id, execution_id, selection);
        Some(worker_id)
    }

    /// Snapshot this pool's slots as the spillover policy's view type.
    /// Read-only; the returned data is a copy, so the caller must not
    /// assume it stays accurate once the lock is dropped (every decision
    /// derived from it is re-validated by the atomic claim below).
    pub async fn slot_views(&self) -> Vec<SlotView> {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .enumerate()
            .map(|(index, w)| SlotView {
                index,
                occupied: w.execution_id.is_some(),
                last_workspace_id: w.last_workspace_id.clone(),
            })
            .collect()
    }

    /// Claim a **Lower Decks** slot for an automation execution spilling
    /// out of a full automation pool. Returns `None` when page 1 has no
    /// free slot, or when the live-worker count is already at or above
    /// `max_concurrent_interactive_workers` — a spilled claim that has a
    /// workspace counts toward that cap exactly like a mainline claim
    /// does (see `MAX_CONCURRENT_INTERACTIVE_WORKERS`'s doc), so it must
    /// be gated the same way. Lease-pending slots are excluded. The
    /// working count and the claim happen under the same lock hold, so
    /// the cap check can never race against a concurrent `mark_working`.
    ///
    /// Unlike [`Self::claim_worker`], this never considers page 0: a
    /// spilling automation may not take a Bridge Crew slot even when
    /// Bridge Crew is entirely idle, so mainline always retains its 8
    /// slots (see `boss_engine::dispatch_spillover` for why). Selection and
    /// the occupancy test happen under one lock hold, so two concurrent
    /// spills can never land on the same slot.
    pub async fn claim_worker_spill(
        &self,
        execution_id: &str,
        preferred_workspace_id: Option<&str>,
        max_concurrent_interactive_workers: usize,
    ) -> Option<String> {
        let mut inner = self.inner.lock().await;
        let working = inner.workers.iter().filter(|w| w.working).count();
        if working >= max_concurrent_interactive_workers {
            return None;
        }
        let views: Vec<SlotView> = inner
            .workers
            .iter()
            .enumerate()
            .map(|(index, w)| SlotView {
                index,
                occupied: w.execution_id.is_some(),
                last_workspace_id: w.last_workspace_id.clone(),
            })
            .collect();
        let chosen_idx = select_spill_claim_index(&views, preferred_workspace_id, WORKER_PAGE_SIZE)?;
        let worker = &mut inner.workers[chosen_idx];
        worker.execution_id = Some(execution_id.to_owned());
        worker.working = false;
        let worker_id = worker.worker_id.clone();
        log_pool_claim(&worker_id, execution_id, "automation-spill");
        Some(worker_id)
    }

    /// Skip-the-queue claim used by `bossctl agents launch` and
    /// `ExecutionCoordinator::force_dispatch`'s recovery-probe path. Same
    /// deterministic, page-aware selection as `claim_worker`, but if every
    /// configured slot is busy and the pool is still below its own hard cap
    /// (see [`Self::hard_cap`]) we grow THIS pool by one fresh slot, named
    /// with THIS pool's [`Self::prefix`], and hand it back. Called on
    /// whichever pool the execution targets — main, automation, or review —
    /// so the returned worker id always carries the correct prefix and the
    /// grown slot is capped by that pool's own hard cap, never
    /// `MAX_WORKER_POOL_SIZE` regardless of which pool this is. Returns
    /// `None` only when the pool is already at its hard cap with no idle
    /// slot, so the launch/probe is rejected rather than silently
    /// overcommitting.
    pub async fn claim_worker_force(&self, execution_id: &str, preferred_workspace_id: Option<&str>) -> Option<String> {
        let mut inner = self.inner.lock().await;

        if let Some((chosen_idx, selection)) = select_claim_index(&inner.workers, preferred_workspace_id) {
            let worker = &mut inner.workers[chosen_idx];
            worker.execution_id = Some(execution_id.to_owned());
            worker.working = false;
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

        // Every existing slot is busy. Grow the pool — bounded by this
        // pool's own hard cap so the app's fixed pane workspace can always
        // render the forced worker.
        if inner.workers.len() >= self.hard_cap {
            return None;
        }
        let new_index = inner.workers.len();
        let worker = WorkerSlot {
            worker_id: format!("{}{}", self.prefix, new_index + 1),
            execution_id: Some(execution_id.to_owned()),
            last_workspace_id: None,
            working: false,
        };
        let id = worker.worker_id.clone();
        inner.workers.push(worker);
        log_pool_claim(&id, execution_id, "force-grow");
        Some(id)
    }

    /// Claim one SPECIFIC slot for `execution_id` — the re-adoption
    /// counterpart to [`Self::release_worker_if_execution`].
    ///
    /// Every other claim verb picks a slot for a *new* dispatch, so the pool
    /// chooses. Re-adoption is the opposite situation: the worker already
    /// exists and already occupies a pane in a known slot, and the pool's
    /// bookkeeping for it was wrongly released. The slot is an input, not an
    /// output.
    ///
    /// Restoring the claim is what makes a re-adoption stick. The pool's
    /// claim table is the `is_live` oracle for
    /// `boss_engine::orphan_sweep` and the redundant-spawn guard; while it says
    /// the slot is free, the work item stays eligible for re-dispatch no
    /// matter what the execution row says.
    ///
    /// Returns `true` when the slot is claimed by `execution_id` on return.
    /// Idempotent: a slot already claimed by the same execution succeeds
    /// without touching anything. Refuses (returns `false`) when the slot does
    /// not exist or is claimed by a DIFFERENT execution — yanking a live
    /// claim to re-adopt a survivor would create the very double-worker state
    /// re-adoption exists to prevent.
    pub async fn reclaim_slot(&self, worker_id: &str, execution_id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(worker) = inner.workers.iter_mut().find(|worker| worker.worker_id == worker_id) else {
            return false;
        };
        match worker.execution_id.as_deref() {
            Some(existing) if existing == execution_id => {
                worker.working = true;
                true
            }
            Some(_) => false,
            None => {
                worker.execution_id = Some(execution_id.to_owned());
                worker.working = true;
                log_pool_claim(worker_id, execution_id, "readopt");
                true
            }
        }
    }

    /// Release `worker_id` back to the idle pool. If `last_workspace_id`
    /// is provided we record it as the worker's affinity for future
    /// preferred-workspace claims.
    pub async fn release_worker(&self, worker_id: &str, last_workspace_id: Option<&str>) {
        let mut inner = self.inner.lock().await;
        if let Some(worker) = inner.workers.iter_mut().find(|worker| worker.worker_id == worker_id) {
            let released_execution = worker.execution_id.take();
            worker.working = false;
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
            worker.working = false;
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

    /// Number of slots whose execution already has a workspace lease.
    /// Used by the dispatcher's interactive-pool concurrency gate
    /// (`MAX_CONCURRENT_INTERACTIVE_WORKERS`). Slots that are only
    /// acquiring a workspace are excluded — see [`Self::occupancy`].
    pub async fn busy_count(&self) -> usize {
        self.occupancy().await.working
    }

    /// Split occupancy: live workers vs lease-pending claims.
    pub async fn occupancy(&self) -> WorkerPoolOccupancy {
        let inner = self.inner.lock().await;
        let mut occupancy = WorkerPoolOccupancy::default();
        for worker in &inner.workers {
            if worker.execution_id.is_some() {
                if worker.working {
                    occupancy.working += 1;
                } else {
                    occupancy.lease_pending += 1;
                }
            }
        }
        occupancy
    }

    /// Mark `worker_id` as past workspace-lease: it now counts toward the
    /// interactive concurrency cap. No-op if the slot is idle or missing.
    pub async fn mark_working(&self, worker_id: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(worker) = inner.workers.iter_mut().find(|worker| worker.worker_id == worker_id)
            && worker.execution_id.is_some()
        {
            worker.working = true;
        }
    }

    /// Claim a slot and mark it working. Test helper for filling the
    /// interactive cap with live workers rather than lease-pending claims.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn claim_working(&self, execution_id: &str, preferred_workspace_id: Option<&str>) -> Option<String> {
        let worker_id = self.claim_worker(execution_id, preferred_workspace_id).await?;
        self.mark_working(&worker_id).await;
        Some(worker_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn idle_count(&self) -> usize {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .filter(|worker| worker.execution_id.is_none())
            .count()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn worker_affinity(&self, worker_id: &str) -> Option<String> {
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
/// Crew 1..=8, Lower Decks 9..=16), `17..=24` automation, `25..=40`
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

pub use boss_engine_storage_values::pool_policy::{
    PoolDispatchPolicy, kind_always_dispatches_on_pool_driver, pool_dispatch_policy_for_worker_id,
    pool_driver_slug_for_execution_kind,
};

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

/// Who paused dispatch. Governs how the pause ends (the breaker auto-recovers;
/// an operator pause is manual-resume-only) and which overrides apply. What the
/// pause *holds* is the separate [`DispatchPauseScope`] — see
/// [`DispatchPause::reviews_held`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPauseOrigin {
    /// An operator toggled `bossctl dispatch pause` / the app's pause
    /// switch. By default ([`DispatchPauseScope::NewWork`]) a review is the
    /// lifecycle of a change already in flight, not new work, so reviews keep
    /// dispatching through an operator pause; the operator may instead choose
    /// [`DispatchPauseScope::AllSpawns`]. Manual-resume-only either way.
    Operator,
    /// The spawn-capability circuit breaker tripped (see
    /// `boss_engine::spawn_health`) because the app's worker-pane spawn path
    /// itself is broken. Exempting reviews here would just burn spawn
    /// attempts against the same dead path, so reviews are held like
    /// everything else.
    Breaker,
}

impl DispatchPauseOrigin {
    /// Stable string persisted to `state.db` under
    /// [`METADATA_KEY_DISPATCH_PAUSE_ORIGIN`].
    pub fn as_metadata_str(self) -> &'static str {
        match self {
            DispatchPauseOrigin::Operator => "operator",
            DispatchPauseOrigin::Breaker => "breaker",
        }
    }

    /// The scope an origin pauses with when nothing chose one: the long-standing
    /// behaviour — an operator pause exempts reviews, a breaker pause holds
    /// everything.
    pub fn default_scope(self) -> DispatchPauseScope {
        match self {
            DispatchPauseOrigin::Operator => DispatchPauseScope::NewWork,
            DispatchPauseOrigin::Breaker => DispatchPauseScope::AllSpawns,
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

/// A live dispatch pause: the whole of it, as one value.
///
/// Held behind a single lock inside `ExecutionCoordinator` and read as a
/// unit, so "is dispatch paused" and "what does this pause hold" can never be
/// answered from different instants. Resuming clears the episode whole, so no
/// field can survive a resume and describe a pause that no longer exists.
///
/// *Who* paused ([`Self::origin`]) and *what is held* ([`Self::scope`]) are
/// separate fields on purpose: an operator can hold every spawn without
/// pretending to be the circuit breaker, and the breaker's pause stays
/// distinguishable from an operator's in every audit surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchPause {
    /// Who paused. Governs auto-recovery and the per-request override, not
    /// the scope: see [`DispatchPauseOrigin`].
    pub origin: DispatchPauseOrigin,
    /// What is held, which determines [`Self::reviews_held`].
    pub scope: DispatchPauseScope,
    /// Epoch seconds at which this pause episode began.
    pub since_epoch_s: u64,
    /// Why. Non-empty by construction ([`boss_protocol::PauseReason`]):
    /// dispatch must never be found paused with no record of who paused it
    /// or why.
    pub reason: String,
}

impl DispatchPause {
    /// Whether this pause holds review-pool executions (`pr_review` and
    /// review guide) too.
    ///
    /// The default operator pause does not: a review is the lifecycle of a
    /// change already in flight, not new work, so reviews keep draining
    /// while the operator holds new work back. An [`DispatchPauseScope::AllSpawns`]
    /// pause does — whether the operator chose it or the breaker did, because
    /// what both mean is that the worker-spawn path is suspect and dispatching
    /// a review would just burn another attempt against it.
    pub fn reviews_held(&self) -> bool {
        self.scope.holds_reviews()
    }

    /// Whether the per-request pause override
    /// ([`DispatchAdmission::PauseBypassOverride`], `bossctl work start
    /// --force`) may admit a row through this pause: only an operator pause
    /// that keeps the default scope. A breaker pause says the spawn path is
    /// unhealthy; an operator [`DispatchPauseScope::AllSpawns`] pause says
    /// the same in the operator's own words — neither is overridable per
    /// request. (`bossctl agents launch`, [`DispatchAdmission::OperatorForced`],
    /// remains the explicit manual escape hatch for both.)
    pub fn accepts_pause_bypass(&self) -> bool {
        self.origin == DispatchPauseOrigin::Operator && !self.reviews_held()
    }

    /// One-line scope description, phrased for an operator. Kept next to
    /// [`Self::reviews_held`] so the words and the behaviour cannot drift:
    /// anything that renders a pause's scope for a human renders this.
    pub fn scope_description(&self) -> &'static str {
        match (self.origin, self.scope) {
            (_, DispatchPauseScope::NewWork) => {
                "new work is held; PR reviews and review guides are exempt and keep dispatching (a \
                 review is the lifecycle of a change already in flight)"
            }
            (DispatchPauseOrigin::Operator, DispatchPauseScope::AllSpawns) => {
                "every spawn is held except explicit `bossctl agents launch` overrides, PR reviews and review guides included — the operator chose \
                 the all-spawns scope because the spawn path itself is suspect; no recovery probe \
                 is admitted"
            }
            (DispatchPauseOrigin::Breaker, DispatchPauseScope::AllSpawns) => {
                "every pool is held, PR reviews included — the breaker tripped on the worker-spawn \
                 path itself, which reviews depend on too"
            }
        }
    }
}

/// Why a dispatch is being admitted. Every path that can reach a worker
/// spawn names one, and `ExecutionCoordinator::dispatch_hold_for` decides
/// from it — so a pause check cannot be forgotten by the next dispatch entry
/// point somebody adds. The gate lives at
/// `ExecutionCoordinator::schedule_execution`, the single chokepoint every
/// spawn passes through, rather than at each caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchAdmission {
    /// The ordinary path: this row came off the ready queue via
    /// `drain_ready_queue`. Fully subject to the pause.
    Queued,
    /// A human explicitly skipped the queue: `bossctl agents launch`, i.e.
    /// `RequestExecutionInput.force`. An operator naming one specific
    /// execution by hand overrides their own pause — including a breaker
    /// pause, which they may well be probing deliberately. Long-standing
    /// behaviour, preserved deliberately, and declared in
    /// `bossctl dispatch state` rather than left implicit.
    ///
    /// Not to be confused with the narrower per-request pause override
    /// designed in
    /// `tools/boss/docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`
    /// (`bossctl work start --force`), which deliberately does NOT apply to
    /// a breaker pause and does not grow the pool. That override has its own
    /// variant — see [`Self::PauseBypassOverride`] — rather than reusing this
    /// one, so the two cannot be conflated on rebase.
    OperatorForced,
    /// The spawn-capability breaker's half-open recovery canary (see
    /// `boss_engine::spawn_health::maybe_admit_recovery_probe`). Admitted only
    /// under a Breaker-origin pause, one at a time, on an exponential
    /// backoff, and never for a `pr_review` row — see
    /// `ExecutionCoordinator::dispatch_hold_for`.
    BreakerRecoveryProbe,
    /// The narrower per-request pause override
    /// (`RequestExecutionInput::bypass_dispatch_pause`, driven by
    /// `bossctl work start --force`; see
    /// `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`).
    /// `ExecutionCoordinator::dispatch_with_pause_bypass` has already
    /// confirmed the pause is operator-originated and consumed this
    /// execution's marker in `dispatch_pause_bypass_execution_ids` before
    /// `drain_ready_queue` ever reaches `schedule_execution` with this
    /// admission — so, unlike [`Self::OperatorForced`], it is admitted only
    /// when [`DispatchPauseOrigin::Operator`] still holds; a breaker pause
    /// refuses it exactly as it would an ordinary [`Self::Queued`] row.
    PauseBypassOverride,
    /// The row's work item is at the human-only `critical` priority
    /// (`work/critical_priority.rs`). `drain_ready_queue` keyed the row
    /// off `WorkDb::list_critical_ready_execution_ids` and skipped both
    /// the operator-pause gate and the interactive concurrency cap for
    /// it; at the chokepoint it is admitted under an operator pause and —
    /// like [`Self::PauseBypassOverride`] — still held by a breaker pause,
    /// since a broken spawn path cannot run urgent work either. Durable
    /// (it is a property of the row, not a one-shot marker), so a
    /// critical row that misses a slot this pass is admitted again next
    /// pass without any re-arming.
    Critical,
}

impl DispatchAdmission {
    /// Stable string for dispatch-event `details`.
    pub fn as_str(self) -> &'static str {
        match self {
            DispatchAdmission::Queued => "queued",
            DispatchAdmission::OperatorForced => "operator_forced",
            DispatchAdmission::BreakerRecoveryProbe => "breaker_recovery_probe",
            DispatchAdmission::PauseBypassOverride => "pause_bypass_override",
            DispatchAdmission::Critical => "critical",
        }
    }
}

/// A refusal from `ExecutionCoordinator::dispatch_hold_for`: this
/// execution may not spawn right now, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchHold {
    /// The pause doing the holding.
    pub pause: DispatchPause,
    /// The admission that was refused.
    pub admission: DispatchAdmission,
    /// Whether the held row targets the review pool — the discriminator for
    /// both the operator review exemption and the canary's review
    /// ineligibility, so a reader never has to re-derive it.
    pub targets_review_pool: bool,
    /// Operator-facing explanation, already naming the pause reason.
    pub detail: String,
}

impl std::fmt::Display for DispatchHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Result of a successful `ExecutionCoordinator::set_max_concurrent_interactive_workers`
/// call. `applied` is the value actually stored; `clamped_from` is `Some(requested)`
/// when the caller's request was above [`MAX_WORKER_POOL_SIZE`] and got rounded
/// down, so the RPC/CLI layer can surface a clear message rather than
/// pretending the request was honored verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetConcurrencyCapOutcome {
    pub applied: usize,
    pub clamped_from: Option<usize>,
}

/// One interactive-pool slot, as the spill policy sees it. Produced by
/// `WorkerPool::slot_views`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotView {
    /// Zero-based index into the pool's slot vector. `index / page_size`
    /// is the page: 0 = Bridge Crew, 1 = Lower Decks.
    pub index: usize,
    /// `true` when some execution currently holds this slot.
    pub occupied: bool,
    /// The workspace this slot last ran, for claim-time affinity.
    pub last_workspace_id: Option<String>,
}

/// Choose the Lower Decks slot an automation execution should spill into,
/// or `None` when page 1 is full (or does not exist, e.g. a test pool
/// smaller than one page).
///
/// Deliberately *not* the same function as the mainline
/// `select_claim_index` above: that one implements strict spillover for
/// mainline (lowest free slot in the lowest non-full page, so Bridge Crew
/// fills first). This one is the mirror image — it considers page 1 and
/// nothing else, so a spilling automation can never take a Bridge Crew
/// slot even when Bridge Crew is completely idle.
///
/// Within Lower Decks, workspace affinity wins if an idle slot last ran
/// the preferred workspace; otherwise the lowest free Lower Decks index
/// is chosen. Fully deterministic — no RNG — matching `select_claim_index`
/// so engine and app never disagree about which slot a dispatch lands on.
pub fn select_spill_claim_index(
    slots: &[SlotView],
    preferred_workspace_id: Option<&str>,
    page_size: usize,
) -> Option<usize> {
    // Page 1 == Lower Decks. A pool with fewer than `page_size` slots has
    // no page 1 at all, so `free` comes back empty and we return `None`.
    let free: Vec<usize> = slots
        .iter()
        .filter(|s| !s.occupied && s.index / page_size == 1)
        .map(|s| s.index)
        .collect();
    let lowest_free = *free.first()?;

    let affinity_idx = preferred_workspace_id.and_then(|target| {
        free.iter()
            .copied()
            .find(|&idx| slots[idx].last_workspace_id.as_deref() == Some(target))
    });

    Some(affinity_idx.unwrap_or(lowest_free))
}

/// Abstracts all host-specific operations: workspace lifecycle, worker
/// spawn, liveness probes, events-socket reattachment and transcript/log
/// readback.
///
/// `host_id = "local"` is the special case for the coordinator's own
/// machine. Every other id corresponds to a registered SSH-reachable
/// remote.
#[async_trait]
pub trait HostAdapter: Send + Sync {
    async fn recovery_pr_base(&self, origin: &str, pr: u64) -> Result<String> {
        fetch_recovery_pr_base(origin, pr).await
    }

    async fn create_execution_bookmark(
        &self,
        workspace: &Path,
        execution_id: &str,
        predecessor: Option<&ExecutionBookmark>,
        inherited_base: Option<&str>,
    ) -> Result<ExecutionBookmark> {
        let _ = (workspace, execution_id, predecessor, inherited_base);
        bail!("execution bookmarks are not supported by this host adapter")
    }

    async fn confirm_workspace_recovery(&self, record: &ExecutionBookmark, workspace: &Path) -> Result<()> {
        let _ = (record, workspace);
        bail!("workspace recovery confirmation is not supported by this host adapter")
    }

    async fn execution_bookmark_diff(&self, record: &ExecutionBookmark) -> Result<String> {
        let _ = record;
        bail!("execution bookmark inspection is not supported by this host adapter")
    }

    async fn restore_execution_bookmark(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        self_retry: bool,
    ) -> Result<bool> {
        let _ = (record, workspace, self_retry);
        bail!("execution bookmark recovery is not supported by this host adapter")
    }
    /// Drop the baseline left behind when both of an execution's heads are
    /// gone, so a self-retry can create fresh pointers.
    async fn discard_orphaned_execution_baseline(&self, record: &ExecutionBookmark) -> Result<()> {
        let _ = record;
        bail!("execution bookmark cleanup is not supported by this host adapter")
    }
    async fn restore_rebased_execution_bookmark(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        pr_bookmark: Option<&str>,
        base_branch: &str,
    ) -> Result<execution_bookmark::RestoreReport> {
        let _ = (record, workspace, pr_bookmark, base_branch);
        bail!("rebased execution recovery is not supported by this host adapter")
    }
    /// Validate and heal the baseline, returning its current durable identity.
    async fn checkpoint_execution_baseline(&self, record: &ExecutionBookmark) -> Result<String> {
        let _ = record;
        bail!("baseline checkpoint is not supported by this host adapter")
    }
    /// Resolve an open PR while preserving the pre-checkout recovery pointers.
    /// Returns false for a closed or merged PR, which cannot anchor recovery.
    async fn prepare_recovery_pr(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        repo_id: &str,
        pr: u64,
    ) -> Result<bool> {
        let _ = (record, workspace, repo_id, pr);
        bail!("recovery PR preparation is not supported by this host adapter")
    }
    /// Repair stranded, diverged or baseline-less pointers from an
    /// engine-recorded anchor. Returns None when the pointers are intact.
    async fn reconcile_execution_bookmark(
        &self,
        record: &ExecutionBookmark,
        pr_bookmark: Option<&str>,
        trunk: &str,
    ) -> Result<Option<execution_bookmark::Reconciliation>> {
        let _ = (record, pr_bookmark, trunk);
        bail!("execution bookmark reconciliation is not supported by this host adapter")
    }
    /// Drop an execution's head bookmarks after a deliberate fresh start so
    /// the same execution can create fresh pointers; returns what was
    /// released so the caller can record the commits.
    async fn release_execution_bookmark_heads(&self, record: &ExecutionBookmark) -> Result<Vec<(String, String)>> {
        let _ = record;
        bail!("execution bookmark release is not supported by this host adapter")
    }
    /// Forget a finished execution's pointers once a voucher proves its
    /// preserved work is published; unproven work keeps its recovery head.
    async fn prune_execution_bookmark(
        &self,
        record: &ExecutionBookmark,
        vouchers: &[execution_bookmark::Voucher],
        trunk: &str,
    ) -> Result<execution_bookmark::PruneOutcome> {
        let _ = (record, vouchers, trunk);
        bail!("execution bookmark pruning is not supported by this host adapter")
    }
    /// Stable host identifier (e.g. `"local"`, `"zakalwe"`).
    fn host_id(&self) -> &str;

    // ── Workspace lifecycle ─────────────────────────────────────────────────

    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle>;

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease>;

    async fn release_workspace(&self, lease_id: &str) -> Result<()>;

    /// End-of-run release that resets the working copy. Defaults to
    /// [`Self::release_workspace`].
    async fn release_workspace_with_reset(&self, lease_id: &str) -> Result<()> {
        self.release_workspace(lease_id).await
    }

    /// Missing attempt PID evidence is inconclusive, never proof of death.
    async fn probe_initial_input_alive(&self, _workspace: &str, _run_id: &str) -> Result<Option<bool>> {
        Ok(None)
    }

    /// Reclaim only input whose launch never started or whose reader is dead.
    async fn cleanup_initial_input(&self, _workspace: &str, _run_id: &str) -> Result<()> {
        Ok(())
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()>;

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()>;

    async fn create_change(&self, workspace_path: &Path, title: &str) -> Result<CubeChangeHandle>;

    /// Position the working copy in `workspace_path` as a fresh editable
    /// child commit atop PR `pr`'s current head. Delegates to
    /// `cube workspace goto --workspace <path> --pr <n>`. Idempotent: a
    /// no-op when `@` is already positioned on the PR branch head.
    async fn goto_workspace(&self, workspace_path: &Path, pr: u64) -> Result<()>;

    /// Position the working copy in `workspace_path` as a fresh editable
    /// child commit atop an arbitrary commit `sha` (e.g. a post-merge
    /// review's merge commit). Unlike [`Self::goto_workspace`], this never
    /// resolves or checks PR state, so it is the only entry point that
    /// works once a PR has merged or closed. Delegates to `cube workspace
    /// goto --workspace <path> --revision <sha>`. Idempotent.
    ///
    /// Default errors so most test doubles need no change; only the real
    /// adapters below provide it.
    async fn goto_workspace_revision(&self, workspace_path: &Path, sha: &str) -> Result<()> {
        let _ = (workspace_path, sha);
        Err(anyhow::anyhow!(
            "goto_workspace_revision is not supported by this adapter"
        ))
    }

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus>;

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>>;

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>>;

    /// Returns `(command_string, cwd)` for the subprocess that would be
    /// spawned with `args`. Used to populate dispatch-event `cube_command`
    /// fields for post-mortem diagnostics. Returns `None` for test doubles
    /// and remote adapters that don't use local subprocesses.
    fn command_repr(&self, args: &[&str]) -> Option<(String, String)>;

    // ── Worker spawn ────────────────────────────────────────────────────────

    /// Spawn a worker for the given execution in the leased workspace.
    ///
    /// For `LocalHostAdapter` this delegates to the `ExecutionRunner`
    /// (pane spawn via the macOS app). For `SshHostAdapter`
    /// this drives the SSH-exec path with remote-forwarded event socket.
    async fn spawn_worker(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome>;

    // ── Live-status + transcript readback ─────────────────────────

    /// Read up to `max_bytes` from the tail of the transcript at `path`
    /// on this host.
    ///
    /// `Ok(None)` means "the transcript is engine-local — read the path
    /// off the local filesystem". The default (and `LocalHostAdapter`)
    /// returns `None` so the existing local read path is unchanged.
    /// `SshHostAdapter` overrides this to pull the byte suffix over its
    /// `ControlMaster` and returns `Ok(Some(jsonl))` — giving the
    /// transcript-tail RPC the same bytes a local run would have read.
    async fn read_transcript_tail_bytes(&self, _path: &str, _max_bytes: u64) -> Result<Option<String>> {
        Ok(None)
    }

    /// Re-establish the reverse events-socket forward for a detached run
    /// (`run_id` is the worker's `BOSS_RUN_ID` / execution id) after an
    /// engine restart, so its hook stream reaches `engine_events_socket`
    /// again.
    ///
    /// `Ok(false)` means "nothing to reattach": a local worker was a
    /// child of the previous engine process and is already gone, so the
    /// default (and `LocalHostAdapter`) is a no-op. `SshHostAdapter`
    /// overrides this to clear the stale remote socket and re-open the
    /// `ssh -R` forward, returning `Ok(true)` on success.
    async fn reattach_events_forward(&self, _run_id: &str, _engine_events_socket: &str) -> Result<bool> {
        Ok(false)
    }

    /// Probe whether a detached remote worker process is still alive.
    ///
    /// Used by the periodic remote-lease reconciler
    /// (`boss_engine::remote_lease_reconcile`) to find `waiting_human` remote
    /// runs whose worker has died without a `Stop` — the remote analogue
    /// of the local `lost_workspace_sweep` (a `.exists()`/pid probe on the
    /// engine host is meaningless for a worker on another machine).
    ///
    /// - `Ok(Some(true))` — the process is alive.
    /// - `Ok(Some(false))` — the process is provably gone (`kill -0`
    ///   reports no such process). Positive evidence of death: safe to
    ///   reap + release.
    /// - `Ok(None)` — not applicable / can't probe (the default and
    ///   `LocalHostAdapter`; a local worker is covered by the local
    ///   sweeps). Never reaped on this signal.
    /// - `Err(..)` — the probe round-trip itself failed (host down, ssh
    ///   error). Inconclusive: callers must NOT treat this as death, or a
    ///   host outage would mass-reap every live worker on it.
    ///
    /// `SshHostAdapter` overrides this to run a `kill -0` over its
    /// `ControlMaster`.
    async fn probe_remote_worker_alive(&self, _remote_pid: i64) -> Result<Option<bool>> {
        Ok(None)
    }

    /// Whether any process on this host has its cwd inside `workspace`.
    /// Anything but a definite verdict is [`Occupancy::Unknown`], which keeps
    /// the lease: the default (no probe on this adapter) and
    /// `SshHostAdapter`'s transport failures both land there.
    async fn probe_workspace_occupancy(&self, _workspace: &Path) -> Result<Occupancy> {
        Ok(Occupancy::Unknown)
    }

    /// Read the tail of the worker's own stdout/stderr log for the run in
    /// `workspace_path` — the remote wrapper tees it to
    /// `<workspace>/.boss/worker.log`.
    ///
    /// This is the only place a remote worker's *startup* failure is ever
    /// written down. A worker that dies before its first hook produces no
    /// events, no transcript, and no `Stop`, so every engine-side surface is
    /// blank; meanwhile the actual cause — `Failed to authenticate. API
    /// Error: 401 OAuth access token has expired.` in the case that motivated
    /// this method — is sitting in that file, which the engine arranged to be
    /// written and then never read. Callers that have positive evidence a
    /// remote worker is gone pull this tail so the reason travels with the
    /// reap instead of being lost with the workspace.
    ///
    /// `Ok(None)` means "not applicable" — the default and
    /// `LocalHostAdapter`, whose workers log to a local pane, which is
    /// already directly readable. `Ok(Some(""))` means the log exists but is
    /// empty (or was never created), which is itself informative and distinct
    /// from `None`. An `Err` is a transport failure; it must never be
    /// mistaken for "the worker exited cleanly", so callers surface it as an
    /// unavailable log rather than an empty one.
    async fn read_worker_log_tail(&self, _workspace_path: &str, _max_bytes: u64) -> Result<Option<String>> {
        Ok(None)
    }

    /// Collect a structured-output artifact produced on this host into the
    /// engine-local destination.
    ///
    /// [`CollectOutcome::NotAvailable`] covers both "the worker has not
    /// written the artifact yet" and any transient condition (a DB error
    /// resolving the host, a missing adapter provider, an SSH transport
    /// failure probing or reading the descriptor) that is not itself proof
    /// the artifact is bad — callers should fall through to the
    /// transcript/nudge recovery path for these, not terminalize.
    /// [`CollectOutcome::Failed`] is reserved for positive evidence the
    /// artifact cannot be trusted: a descriptor that exists but names an
    /// invalid path, or an artifact that exists but whose copy into place
    /// failed. Only `Failed` should terminalize a completed remote review.
    async fn collect_structured_output(
        &self,
        _execution_id: &str,
        _kind: StructuredOutputKind,
        _destination: &Path,
    ) -> Result<CollectOutcome> {
        Ok(CollectOutcome::NotAvailable)
    }
}

/// Outcome of [`HostAdapter::collect_structured_output`]. See that method's
/// doc for the terminalize/fail-open contract each variant carries.
#[derive(Debug)]
pub enum CollectOutcome {
    /// The artifact was pulled and installed at the destination.
    Collected,
    /// Not available yet, or collection could not be attempted for a
    /// reason that is not proof the artifact itself is unusable. Not
    /// terminal — callers should fall back to transcript/nudge recovery.
    NotAvailable,
    /// Positive proof the artifact cannot be trusted. Terminal — callers
    /// should fail the execution, naming `reason`.
    Failed(String),
}

/// Resolves the [`HostAdapter`] the coordinator should use for a host the
/// scheduler just selected. This is the seam that replaces the single
/// hardcoded `LocalHostAdapter` in the dispatch loop: the local host
/// returns the existing local adapter unchanged, and every other
/// (SSH-reachable) host gets an `SshHostAdapter` over a persistent
/// `ControlMaster` connection.
#[async_trait]
pub trait HostAdapterProvider: Send + Sync {
    /// Return (or lazily build) the adapter for `host`. Errors surface as
    /// a pre-start failure in the dispatch loop, leaving the execution
    /// recoverable on a later kick.
    async fn adapter_for(&self, host: &Host) -> Result<Arc<dyn HostAdapter>>;
}

/// Whether any process has its cwd inside the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    Empty,
    Occupied,
    /// Probe failed; inferred death must not release.
    Unknown,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SendToAppError {
    #[error("no app session is registered")]
    NotRegistered,
    #[error("app session outbound queue is saturated; request could not be delivered")]
    SessionWedged,
    #[error("app disconnected before responding")]
    AppDisconnected,
    #[error("timed out waiting for app response")]
    Timeout,
    #[error("app responded with unexpected response kind for request kind {0}")]
    ResponseKindMismatch(&'static str),
}

/// Metadata key for the global dispatch-pause flag. `"1"` = paused, `"0"` or
/// absent = running. Persisted at every toggle so the pause survives an engine
/// restart. `pub` so the spawn-capability circuit breaker
/// (`boss_engine::spawn_health`) can persist the auto-pause it applies through the
/// same key the human toggle (`handle_set_dispatch_paused`) uses.
pub const METADATA_KEY_DISPATCH_PAUSED: &str = "dispatch_paused";
/// Metadata key storing the epoch-seconds timestamp at which dispatch was last
/// paused. Zero (or absent) means not paused.
pub const METADATA_KEY_DISPATCH_PAUSED_SINCE: &str = "dispatch_paused_since_epoch_s";
/// Metadata key storing the origin of the current pause — one of
/// [`DispatchPauseOrigin::as_metadata_str`]'s values.
/// Determines whether the pause exempts `pr_review` executions on restore;
/// absent (a pause persisted before this key existed) defaults to the
/// non-exempt `breaker` origin — see
/// [`DispatchPauseOrigin::from_metadata_str`].
pub const METADATA_KEY_DISPATCH_PAUSE_ORIGIN: &str = "dispatch_paused_origin";
/// Metadata key storing the scope of the current pause — one of
/// [`boss_protocol::DispatchPauseScope::as_str`]'s values — so a restart
/// restores what the pause *held*, not just who paused. Absent (a
/// pause persisted before this key existed) restores as the origin's default
/// scope; an unrecognised value restores as the stricter `all_spawns` — see
/// `load_dispatch_pause_scope`. `pub` for the same reason as the
/// origin key: the spawn-capability breaker persists its pause through it.
pub const METADATA_KEY_DISPATCH_PAUSE_SCOPE: &str = "dispatch_paused_scope";
/// Metadata key storing why dispatch is currently paused. Absent (or
/// empty) while running; always set alongside [`METADATA_KEY_DISPATCH_PAUSED`]
/// `"1"` and cleared alongside it going back to `"0"` — see
/// `boss_engine::coordinator::ExecutionCoordinator::pause_dispatch` /
/// `resume_dispatch`. `pub` for the same reason as
/// `METADATA_KEY_DISPATCH_PAUSED`: the spawn-capability breaker persists
/// its own reason through this key.
pub const METADATA_KEY_DISPATCH_PAUSE_REASON: &str = "dispatch_paused_reason";

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 8;

    fn slots(occupied: &[bool]) -> Vec<SlotView> {
        occupied
            .iter()
            .enumerate()
            .map(|(index, &occupied)| SlotView {
                index,
                occupied,
                last_workspace_id: None,
            })
            .collect()
    }

    /// A 16-slot pool: `bridge_busy` of Bridge Crew and `decks_busy` of
    /// Lower Decks occupied, filled from the low index of each page.
    fn paged(bridge_busy: usize, decks_busy: usize) -> Vec<SlotView> {
        let flags: Vec<bool> = (0..16)
            .map(|i| {
                if i < PAGE {
                    i < bridge_busy
                } else {
                    i - PAGE < decks_busy
                }
            })
            .collect();
        slots(&flags)
    }

    #[tokio::test]
    async fn worker_pool_clamps_size_to_hard_cap() {
        let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE + 4);
        assert_eq!(pool.capacity().await, MAX_WORKER_POOL_SIZE);
    }

    #[tokio::test]
    async fn worker_pool_prefers_workspace_affinity_over_lowest_index() {
        let pool = WorkerPool::new(2);

        // Deterministic selection fills the lowest free slot first, so the
        // two claims land on worker-1 then worker-2.
        let w_a = pool.claim_worker("exec-a", None).await.unwrap();
        let w_b = pool.claim_worker("exec-b", None).await.unwrap();
        assert_eq!(w_a, "worker-1");
        assert_eq!(w_b, "worker-2");
        pool.release_worker(&w_a, Some("ws-a")).await;
        pool.release_worker(&w_b, Some("ws-b")).await;

        // Preferring ws-b must pick the worker that recorded ws-b affinity
        // (worker-2), even though the lowest-index default would otherwise
        // pick worker-1.
        let claimed = pool.claim_worker("exec-c", Some("ws-b")).await.unwrap();
        assert_eq!(claimed, w_b);
        pool.release_worker(&claimed, Some("ws-b")).await;

        // Preferring an unknown workspace has no affinity match, so it falls
        // through to the deterministic lowest-index slot (worker-1).
        let fallback = pool.claim_worker("exec-d", Some("ws-unknown")).await.unwrap();
        assert_eq!(fallback, w_a);
    }

    /// `worker-{N}` and slot N must round-trip 1:1. The
    /// engine-owns-slots refactor depends on this — the runner
    /// derives the pane slot it sends to the app from the worker
    /// id the coordinator handed it. A regression in either format
    /// or parse would silently re-introduce two independent
    /// numbering systems.
    #[test]
    fn worker_id_and_slot_id_round_trip() {
        // Covers the full interactive pool — Bridge Crew (1..=8) and Lower
        // Decks (9..=16) — so the second page round-trips 1:1 too.
        for slot in 1u8..=MAX_WORKER_POOL_SIZE as u8 {
            let worker_id = WorkerPool::worker_id_for_slot(slot);
            assert_eq!(worker_id, format!("worker-{slot}"));
            assert_eq!(slot_id_from_worker_id(&worker_id), Some(slot));
        }
    }

    #[test]
    fn slot_id_from_worker_id_accepts_automation_pool_format() {
        // Automation-pool ordinals are offset by MAX_WORKER_POOL_SIZE (16) so the
        // two pools occupy disjoint slot ranges: interactive 1..=16, automation 17..=24.
        for ordinal in 1u8..=MAX_AUTOMATION_POOL_SIZE as u8 {
            let auto_worker_id = format!("auto-worker-{ordinal}");
            let expected_slot = ordinal + MAX_WORKER_POOL_SIZE as u8;
            assert_eq!(
                slot_id_from_worker_id(&auto_worker_id),
                Some(expected_slot),
                "expected Some({expected_slot}) for {auto_worker_id:?}"
            );
        }
        assert_eq!(slot_id_from_worker_id("auto-worker-0"), None);
        assert_eq!(slot_id_from_worker_id("auto-worker-"), None);
        assert_eq!(slot_id_from_worker_id("auto-worker-abc"), None);
    }

    #[test]
    fn worker_id_for_slot_round_trips_with_slot_id_from_worker_id() {
        // Interactive pool: slots 1..=16 → "worker-N" → back to the same slot.
        for slot in 1u8..=MAX_WORKER_POOL_SIZE as u8 {
            let wid = worker_id_for_slot(slot);
            assert_eq!(wid, format!("worker-{slot}"));
            assert_eq!(slot_id_from_worker_id(&wid), Some(slot));
        }
        // Automation pool: slots 17..=24 → "auto-worker-M" → back to the same slot.
        let automation_end = MAX_WORKER_POOL_SIZE as u8 + MAX_AUTOMATION_POOL_SIZE as u8;
        for slot in (MAX_WORKER_POOL_SIZE as u8 + 1)..=automation_end {
            let wid = worker_id_for_slot(slot);
            let expected_ordinal = slot as usize - MAX_WORKER_POOL_SIZE;
            assert_eq!(wid, format!("auto-worker-{expected_ordinal}"));
            assert_eq!(slot_id_from_worker_id(&wid), Some(slot));
        }
        // Review pool: slots 25..=40 → "review-M" → back to the same slot.
        for slot in (automation_end + 1)..=(automation_end + MAX_REVIEW_POOL_SIZE as u8) {
            let wid = worker_id_for_slot(slot);
            let expected_ordinal = slot as usize - MAX_WORKER_POOL_SIZE - MAX_AUTOMATION_POOL_SIZE;
            assert_eq!(wid, format!("review-{expected_ordinal}"));
            assert_eq!(slot_id_from_worker_id(&wid), Some(slot));
        }
    }

    #[test]
    fn slot_id_from_worker_id_accepts_review_pool_format() {
        // Review-pool ordinals are offset past both the interactive (16) and
        // automation (8) ranges, so they occupy slots 25..=40 — disjoint
        // from every other pool.
        for ordinal in 1u8..=MAX_REVIEW_POOL_SIZE as u8 {
            let review_worker_id = format!("review-{ordinal}");
            let expected_slot = ordinal + MAX_WORKER_POOL_SIZE as u8 + MAX_AUTOMATION_POOL_SIZE as u8;
            assert_eq!(
                slot_id_from_worker_id(&review_worker_id),
                Some(expected_slot),
                "expected Some({expected_slot}) for {review_worker_id:?}"
            );
        }
        assert_eq!(slot_id_from_worker_id("review-0"), None);
        assert_eq!(slot_id_from_worker_id("review-"), None);
        assert_eq!(slot_id_from_worker_id("review-abc"), None);
    }

    #[test]
    fn review_pool_slots_are_disjoint_from_other_pools() {
        // The slot IDs produced by review-N (25..=40) must not overlap
        // with any interactive-pool (1..=16) or automation-pool (17..=24) slot.
        let automation_ceiling = MAX_WORKER_POOL_SIZE + MAX_AUTOMATION_POOL_SIZE;
        for ordinal in 1u8..=MAX_REVIEW_POOL_SIZE as u8 {
            let review_wid = format!("review-{ordinal}");
            let slot = slot_id_from_worker_id(&review_wid).unwrap();
            assert!(
                slot as usize > automation_ceiling,
                "review-{ordinal} must map to slot > {automation_ceiling}, got {slot}"
            );
            // Verify the reverse also works: the slot maps back to a review- id.
            let back = worker_id_for_slot(slot);
            assert!(
                back.starts_with(REVIEW_WORKER_ID_PREFIX),
                "slot {slot} must produce a review-pool worker_id, got {back:?}"
            );
        }
    }

    #[test]
    fn worker_page_label_partitions_interactive_pool_only() {
        // Bridge Crew is page 0 (slots 1..=8), Lower Decks is page 1
        // (slots 9..=16). Non-interactive slots (automation/review/remote)
        // have no page label.
        for slot in 1u8..=WORKER_PAGE_SIZE as u8 {
            assert_eq!(worker_page_label(slot).as_deref(), Some("Bridge Crew"), "slot {slot}");
        }
        for slot in (WORKER_PAGE_SIZE as u8 + 1)..=MAX_WORKER_POOL_SIZE as u8 {
            assert_eq!(worker_page_label(slot).as_deref(), Some("Lower Decks"), "slot {slot}");
        }
        assert_eq!(worker_page_label(0), None);
        assert_eq!(
            worker_page_label(MAX_WORKER_POOL_SIZE as u8 + 1),
            None,
            "first automation slot has no page"
        );
        assert_eq!(
            worker_page_label(boss_protocol::REMOTE_SLOT_BASE),
            None,
            "remote virtual slot has no page"
        );
    }

    #[test]
    fn automation_pool_slots_are_disjoint_from_regular_pool() {
        // The slot IDs produced by auto-worker-N (17..=24) must not
        // overlap with any interactive-pool slot (1..=16).
        for ordinal in 1u8..=MAX_AUTOMATION_POOL_SIZE as u8 {
            let auto_wid = format!("auto-worker-{ordinal}");
            let slot = slot_id_from_worker_id(&auto_wid).unwrap();
            assert!(
                slot as usize > MAX_WORKER_POOL_SIZE,
                "auto-worker-{ordinal} must map to slot > {MAX_WORKER_POOL_SIZE}, got {slot}"
            );
            // Verify the reverse also works: the slot maps back to an auto-worker- id.
            let back = worker_id_for_slot(slot);
            assert!(
                back.starts_with(AUTOMATION_WORKER_ID_PREFIX),
                "slot {slot} must produce an automation-pool worker_id, got {back:?}"
            );
        }
    }

    #[test]
    fn slot_id_from_worker_id_rejects_garbage() {
        assert_eq!(slot_id_from_worker_id(""), None);
        assert_eq!(slot_id_from_worker_id("worker"), None);
        assert_eq!(slot_id_from_worker_id("worker-"), None);
        assert_eq!(slot_id_from_worker_id("worker-0"), None);
        assert_eq!(slot_id_from_worker_id("worker-abc"), None);
        assert_eq!(slot_id_from_worker_id("agent-1"), None);
    }

    #[test]
    fn pool_dispatch_policy_for_worker_id_pins_review_and_automation_to_claude_opus() {
        // Review and automation pools always dispatch on Claude at the strong
        // tier (Opus) per the automated-reviewer design §5 — independent of
        // whatever driver the reviewed/automated row itself carries. Main-pool
        // workers have no policy and fall through to the row's own driver /
        // the effort-driven default.
        let expected = PoolDispatchPolicy {
            driver: "claude",
            model_tier: boss_engine_effort::PoolModelTier::Strong,
        };
        for ordinal in 1u8..=MAX_REVIEW_POOL_SIZE as u8 {
            let wid = format!("review-{ordinal}");
            assert_eq!(
                pool_dispatch_policy_for_worker_id(&wid),
                Some(expected),
                "review pool worker {wid:?} must dispatch on Claude/Opus"
            );
        }
        for ordinal in 1u8..=MAX_AUTOMATION_POOL_SIZE as u8 {
            let wid = format!("auto-worker-{ordinal}");
            assert_eq!(
                pool_dispatch_policy_for_worker_id(&wid),
                Some(expected),
                "automation pool worker {wid:?} must dispatch on Claude/Opus"
            );
        }
        for ordinal in 1u8..=MAX_WORKER_POOL_SIZE as u8 {
            let wid = format!("worker-{ordinal}");
            assert_eq!(
                pool_dispatch_policy_for_worker_id(&wid),
                None,
                "main pool worker {wid:?} must have no pool dispatch policy"
            );
        }
    }

    /// The kind-side companion of the policy above must agree with it: a kind
    /// this reports as pool-pinned really does land on a pool whose worker ids
    /// carry a dispatch policy, and an ordinary implementation kind does not.
    /// Traffic allocation declines the former on the strength of this claim
    /// (`work::driver_allocation::decide_execution_driver`), so the two must not
    /// drift apart.
    #[test]
    fn kind_always_dispatches_on_pool_driver_matches_the_pinned_pools() {
        for (kind, worker_id) in [
            (boss_protocol::ExecutionKind::PrReview, "review-1"),
            (boss_protocol::ExecutionKind::AutomationTriage, "auto-worker-1"),
        ] {
            assert!(
                kind_always_dispatches_on_pool_driver(&kind),
                "{kind} runs on {worker_id}, whose driver is pinned"
            );
            assert!(
                pool_dispatch_policy_for_worker_id(worker_id).is_some(),
                "{worker_id} must carry a pinned dispatch policy"
            );
        }
        for kind in [
            boss_protocol::ExecutionKind::TaskImplementation,
            boss_protocol::ExecutionKind::ChoreImplementation,
            boss_protocol::ExecutionKind::RevisionImplementation,
            boss_protocol::ExecutionKind::InvestigationImplementation,
            boss_protocol::ExecutionKind::CiRemediation,
            boss_protocol::ExecutionKind::ConflictResolution,
        ] {
            assert!(
                !kind_always_dispatches_on_pool_driver(&kind),
                "{kind} dispatches on the main pool, which has no pinned driver"
            );
        }
        assert_eq!(pool_dispatch_policy_for_worker_id("worker-1"), None);
    }

    #[tokio::test]
    async fn worker_pool_claims_lowest_free_slot_deterministically() {
        // Claim-release-claim must always return to the lowest free slot —
        // the deterministic replacement for the old random spread. Every
        // claim after a release lands back on worker-1, never a higher slot.
        let pool = WorkerPool::new(4);
        for i in 0..50 {
            let claimed = pool.claim_worker(&format!("exec-{i}"), None).await.unwrap();
            assert_eq!(
                claimed, "worker-1",
                "deterministic claim must always pick the lowest free slot"
            );
            pool.release_worker(&claimed, None).await;
        }
        // Held claims fill strictly in ascending slot order.
        let mut held = Vec::new();
        for i in 0..4 {
            held.push(pool.claim_worker(&format!("hold-{i}"), None).await.unwrap());
        }
        assert_eq!(held, vec!["worker-1", "worker-2", "worker-3", "worker-4"]);
    }

    #[tokio::test]
    async fn worker_pool_strict_spillover_fills_bridge_crew_before_lower_decks() {
        // The interactive pool is two pages of WORKER_PAGE_SIZE. Bridge Crew
        // (page 0) must be fully occupied before any Lower Decks (page 1) slot
        // is claimed, and a freed Bridge Crew slot must be preferred over an
        // idle Lower Decks slot at the next claim (preference is claim-time
        // only — running Lower Decks workers are never migrated).
        let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE);

        // The first WORKER_PAGE_SIZE claims all land on Bridge Crew, in order.
        for n in 1..=WORKER_PAGE_SIZE {
            let claimed = pool.claim_worker(&format!("bc-{n}"), None).await.unwrap();
            assert_eq!(claimed, format!("worker-{n}"), "claim {n} must stay on Bridge Crew");
            assert_eq!(
                worker_page_label(slot_id_from_worker_id(&claimed).unwrap()).as_deref(),
                Some("Bridge Crew")
            );
        }

        // With all 8 Bridge Crew slots occupied, the 9th concurrent claim is
        // the first to spill into Lower Decks — worker-9, slot 9, page 1.
        let spill = pool.claim_worker("ld-1", None).await.unwrap();
        assert_eq!(spill, format!("worker-{}", WORKER_PAGE_SIZE + 1));
        let spill_slot = slot_id_from_worker_id(&spill).unwrap();
        assert_eq!(spill_slot, WORKER_PAGE_SIZE as u8 + 1);
        assert_eq!(worker_page_label(spill_slot).as_deref(), Some("Lower Decks"));

        // Free a Bridge Crew slot (worker-3). The next claim must reclaim it
        // rather than continuing to grow Lower Decks — strict spillover applies
        // at claim time, so a free page-0 slot always beats an idle page-1 one.
        pool.release_worker("worker-3", None).await;
        let reclaim = pool.claim_worker("bc-again", None).await.unwrap();
        assert_eq!(
            reclaim, "worker-3",
            "a freed Bridge Crew slot must be preferred over Lower Decks"
        );
    }

    #[test]
    fn spill_never_takes_a_bridge_crew_slot_even_when_bridge_crew_is_idle() {
        // The whole point of confining spill to page 1: Bridge Crew is
        // completely free here, and automation still lands on slot 8
        // (the first Lower Decks slot), leaving all 8 mainline slots.
        let s = paged(0, 0);
        assert_eq!(select_spill_claim_index(&s, None, PAGE), Some(8));
    }

    #[test]
    fn spill_picks_lowest_free_lower_decks_slot() {
        let s = paged(8, 3);
        assert_eq!(select_spill_claim_index(&s, None, PAGE), Some(11));
    }

    #[test]
    fn spill_returns_none_when_lower_decks_is_full() {
        // Bridge Crew idle, Lower Decks full: still no spill. Automation
        // waits rather than reaching into mainline's page.
        let s = paged(0, 8);
        assert_eq!(select_spill_claim_index(&s, None, PAGE), None);
    }

    #[test]
    fn spill_returns_none_for_a_pool_with_no_lower_decks_page() {
        // Test/config pools smaller than one page have no page 1.
        let s = slots(&[false, false, false]);
        assert_eq!(select_spill_claim_index(&s, None, PAGE), None);
    }

    #[test]
    fn spill_prefers_workspace_affinity_within_lower_decks() {
        let mut s = paged(8, 0);
        s[13].last_workspace_id = Some("ws-warm".to_owned());
        assert_eq!(select_spill_claim_index(&s, Some("ws-warm"), PAGE), Some(13));
    }

    #[test]
    fn spill_affinity_never_escapes_lower_decks_into_bridge_crew() {
        // The warm workspace was last run on a Bridge Crew slot. Affinity
        // must not drag the spill onto page 0 — correctness (mainline
        // headroom) outranks warmth.
        let mut s = paged(0, 0);
        s[2].last_workspace_id = Some("ws-warm".to_owned());
        assert_eq!(select_spill_claim_index(&s, Some("ws-warm"), PAGE), Some(8));
    }

    #[test]
    fn spill_falls_back_to_lowest_free_when_affinity_slot_is_busy() {
        let mut s = paged(8, 2);
        // Slot 9 has the warm workspace but is already occupied.
        s[9].last_workspace_id = Some("ws-warm".to_owned());
        assert_eq!(select_spill_claim_index(&s, Some("ws-warm"), PAGE), Some(10));
    }

    #[tokio::test]
    async fn cloned_pool_preserves_pending_working_spill_and_compare_release() {
        let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE);
        let clone = pool.clone();
        assert_eq!(pool.claim_worker("main", None).await.as_deref(), Some("worker-1"));
        assert_eq!(
            clone.occupancy().await,
            WorkerPoolOccupancy {
                working: 0,
                lease_pending: 1
            }
        );
        assert_eq!(
            clone.claim_worker_spill("spill", None, 1).await.as_deref(),
            Some("worker-9")
        );
        pool.mark_working("worker-1").await;
        assert!(clone.claim_worker_spill("blocked", None, 1).await.is_none());
        assert!(!clone.release_worker_if_execution("worker-1", "stale", None).await);
        assert!(
            clone
                .release_worker_if_execution("worker-1", "main", Some("warm"))
                .await
        );
        assert_eq!(
            pool.claim_worker("next", Some("warm")).await.as_deref(),
            Some("worker-1")
        );
        assert!(!pool.release_worker_if_execution("worker-1", "main", None).await);
        assert_eq!(pool.claims().await.len(), 2);
    }

    #[tokio::test]
    async fn simultaneous_claims_from_shared_pool_are_exclusive() {
        let pool = WorkerPool::new(1);
        let (first, second) = tokio::join!(pool.claim_worker("first", None), pool.claim_worker("second", None),);
        assert_ne!(first.is_some(), second.is_some());
        assert_eq!(pool.claims().await.len(), 1);
    }

    #[tokio::test]
    async fn runtime_publication_awaits_structural_fallback() {
        #[derive(Default)]
        struct Publisher(Mutex<Vec<String>>);
        #[async_trait]
        impl ExecutionPublisher for Publisher {
            async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
            async fn publish_work_item_changed(&self, product: &str, item: &str, reason: &str) {
                self.0.lock().await.push(format!("{product}/{item}/{reason}"));
            }
            async fn publish_frontend_event_on_product(&self, _: &str, _: FrontendEvent) {}
        }
        let publisher = Publisher::default();
        publisher
            .publish_work_item_runtime_changed("product", "item", "updated")
            .await;
        assert_eq!(*publisher.0.lock().await, ["product/item/updated"]);
    }

    #[test]
    fn run_outcomes_keep_live_panes_and_release_terminal_or_dependency_leases() {
        for (state, status, releases) in [
            (RunWaitState::Terminal, ExecutionStatus::Completed, true),
            (
                RunWaitState::WaitingDependency,
                ExecutionStatus::WaitingDependency,
                true,
            ),
            (RunWaitState::WaitingHuman, ExecutionStatus::WaitingHuman, false),
            (RunWaitState::WaitingReview, ExecutionStatus::WaitingReview, false),
            (RunWaitState::WaitingMerge, ExecutionStatus::WaitingMerge, false),
            (RunWaitState::CancelledDuringSpawn, ExecutionStatus::Cancelled, true),
            (RunWaitState::WorkerPaneAlive, ExecutionStatus::Running, false),
        ] {
            assert_eq!(state.execution_status(), status);
            assert_eq!(state.release_workspace(), releases);
        }
    }
}
