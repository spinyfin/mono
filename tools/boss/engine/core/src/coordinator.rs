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
/// from [`crate::metrics::init_all`] at engine startup.
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
/// clamped. Raised from 3 to 6 (two rows of three in the Automations pane)
/// per operator request.
pub const MAX_AUTOMATION_POOL_SIZE: usize = 6;

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
/// Three entries → up to 3 retries (4 total attempts) before a pre-start
/// failure surfaces to the operator.
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
/// at least [`CHAIN_SERIALIZED_STALL_THRESHOLD_SECS`] — the 2026-07-09 T251
/// incident (`task_18c07e0a815e6bd0_1de`): the dispatcher re-evaluated and
/// re-deferred every ~10s for ~20 minutes with zero durable, user-visible
/// signal, so a human only noticed by grepping `engine-trace.jsonl`. Filed
/// once per stall (idempotent via [`WorkDb::upsert_work_item_attention`])
/// and resolved via [`WorkDb::resolve_external_tracker_attention`] the
/// moment the row is no longer chain-serialized (dispatched, or the sibling
/// was reconciled dead — see [`ExecutionCoordinator::live_chain_siblings`]).
pub const CHAIN_SERIALIZED_STALL_ATTENTION_KIND: &str = "chain_serialized_stall";

/// How long a `ready` execution may sit deferred behind the same live chain
/// sibling before [`CHAIN_SERIALIZED_STALL_ATTENTION_KIND`] fires. Set
/// comfortably below the ~20-minute silent stall observed in the T251
/// incident so a human is alerted well before that point going forward.
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
/// (it comes from `jj resolve --list`, so entries may carry a trailing
/// conflict-type descriptor after the path).
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
    /// would conflict against current `main` (Layer 4 / T10 of the
    /// merge-conflict-reduction design). Same default-Err pattern as
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

fn shell_quote(arg: &str) -> String {
    if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_owned()
    }
}

/// Parse `cube workspace rebase --json`'s `payload` object
/// (`{status, pushed, conflicted_files, ...}`) into a [`RebaseOutcome`].
/// Shared by [`CommandCubeClient::rebase_workspace`] and
/// [`CommandCubeClient::rebase_workspace_no_push`] — the two commands differ
/// only by the `--no-push` flag, not by output shape.
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
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
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
        // (Layer 4 / T10) — the caller only wants to observe whether the PR
        // would conflict against current `main`.
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

    fn command_repr(&self, args: &[&str]) -> Option<(String, String)> {
        let Ok(agent) = self.cfg.agent() else { return None };
        let cmd = std::iter::once(agent.cube.command.as_str())
            .chain(agent.cube.args.iter().map(String::as_str))
            .chain(args.iter().copied())
            .map(shell_quote)
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
/// Crew 1..=8, Lower Decks 9..=16), `17..=22` automation, `23..=30`
/// review — so "auto-worker-1" → slot 17, "review-1" → slot 23, never
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
}

/// Check out a leased cube workspace to the head commit of a PR, so a reviewer
/// worker can read full source at the PR head rather than working from a stale
/// or arbitrary baseline.
///
/// Steps:
/// 1. Fetch the current head OID from GitHub via `gh pr view`.
/// 2. `jj git fetch` — pull the remote refs into the local jj store.
/// 3. `jj new <sha>` — position the working copy on a fresh empty child of the
///    PR head. (`jj new`, not `jj edit`: a pushed PR head is immutable, so
///    `jj edit` fails deterministically; the empty child's tree equals the
///    head's, so the read-only reviewer still sees the PR-head files.)
///
/// Returns the head SHA on success. Any subprocess failure is returned as an
/// `Err` so the dispatcher can record a start failure and retry.
///
/// The caller is responsible for releasing the workspace on error.
impl ExecutionCoordinator {
    /// Convenience constructor for tests and simple callers. Wraps the
    /// provided `cube_client` and `execution_runner` in a
    /// `LocalHostAdapter` and calls [`Self::with_publisher`].
    pub fn new(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
    ) -> Self {
        let host_adapter = Arc::new(LocalHostAdapter::new(cube_client, execution_runner));
        Self::with_host_adapter_and_publisher(work_db, worker_pool, host_adapter, Arc::new(NoopExecutionPublisher))
    }

    /// Constructor that accepts a publisher alongside the cube/runner
    /// primitives. Wraps them in `LocalHostAdapter` and delegates to
    /// [`Self::with_host_adapter_and_publisher`].
    pub fn with_publisher(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self {
        let host_adapter = Arc::new(LocalHostAdapter::new(cube_client, execution_runner));
        Self::with_host_adapter_and_publisher(work_db, worker_pool, host_adapter, publisher)
    }

    /// Primary constructor for Phase 3+. Callers that need to dispatch
    /// to a non-local host (e.g. `SshHostAdapter`) build the adapter
    /// themselves and pass it here directly.
    pub fn with_host_adapter_and_publisher(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        host_adapter: Arc<dyn HostAdapter>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self {
        // Build a local registry for tests that never call `set_metrics`.
        // Pre-register the lease counter handles so `.inc()` never panics
        // on "counter not registered" in a test context.
        let local_metrics = Arc::new(Registry::new());
        register_metrics(&local_metrics);
        let host_adapter_provider: Arc<dyn HostAdapterProvider> =
            Arc::new(LocalHostAdapterProvider::new(Arc::clone(&host_adapter)));
        Self {
            work_db,
            worker_pool,
            automation_pool: WorkerPool::new_automation(MAX_AUTOMATION_POOL_SIZE),
            review_pool: WorkerPool::new_review(DEFAULT_REVIEW_POOL_SIZE),
            host_adapter,
            host_adapter_provider,
            publisher,
            dispatch_events: Arc::new(NoopDispatchEventSink),
            scheduling_active: AtomicBool::new(false),
            scheduling_pending: AtomicBool::new(false),
            repo_cold_probe_seen: Mutex::new(HashSet::new()),
            pre_start_retry_delays: PRE_START_RETRY_DELAYS.to_vec(),
            merge_order_stagger_secs: 0,
            metrics: local_metrics,
            execution_started_hook: Arc::new(NoopExecutionStartedHook),
            dispatch_paused: AtomicBool::new(false),
            dispatch_paused_since_epoch_s: AtomicU64::new(0),
            dispatch_pause_exempts_reviews: AtomicBool::new(false),
            live_worker_states: None,
            refused_workspaces: Mutex::new(HashMap::new()),
        }
    }

    /// Seed the bounded `merge_order` dispatch-stagger window (seconds).
    /// `app.rs` calls this with the (already-clamped)
    /// [`crate::config::WorkConfig::merge_order_stagger_secs`]; `0` disables
    /// the stagger. Tests set it directly to exercise the deferral.
    pub fn set_merge_order_stagger_secs(&mut self, secs: u64) {
        self.merge_order_stagger_secs = secs;
    }

    /// Override the automation pool. `app.rs` calls this with a pool sized
    /// from `BOSS_AUTOMATION_POOL_SIZE`; tests may supply a smaller pool.
    pub fn set_automation_pool(&mut self, pool: WorkerPool) {
        self.automation_pool = pool;
    }

    /// The local-host adapter. `app.rs` reads this to seed the production
    /// [`crate::host_adapter::SshHostAdapterProvider`] (which returns it
    /// verbatim for `host_id = "local"`).
    pub fn host_adapter(&self) -> Arc<dyn HostAdapter> {
        Arc::clone(&self.host_adapter)
    }

    /// Install the host-adapter provider used to build per-host adapters
    /// in the dispatch loop. `app.rs` wires the SSH-capable provider so
    /// the coordinator can route to registered remote hosts; tests inject
    /// recording/fake providers to assert routing.
    pub fn set_host_adapter_provider(&mut self, provider: Arc<dyn HostAdapterProvider>) {
        self.host_adapter_provider = provider;
    }

    /// Read the tail of a run's transcript that lives on host `host_id`.
    ///
    /// Returns `Ok(None)` for `host_id = "local"` — the transcript is on
    /// the engine's own filesystem, so the caller reads the recorded
    /// path directly. For a remote host, resolves the host + adapter and
    /// pulls the last `max_bytes` of `path` over SSH (the design's Q7
    /// readback, done on demand rather than via a streaming socket).
    /// `app.rs`'s `TailRunTranscript` handler routes remote runs through
    /// here so `bossctl agents transcript` / the transcript viewer work
    /// identically against a remote worker.
    pub async fn read_remote_transcript_tail(
        &self,
        host_id: &str,
        path: &str,
        max_bytes: u64,
    ) -> Result<Option<String>> {
        if host_id == "local" {
            return Ok(None);
        }
        let host = self
            .work_db
            .get_host(host_id)?
            .ok_or_else(|| anyhow!("unknown host '{host_id}' for remote transcript read"))?;
        let adapter = self.host_adapter_provider.adapter_for(&host).await?;
        adapter.read_transcript_tail_bytes(path, max_bytes).await
    }

    /// Re-establish reverse events forwards for every detached remote run
    /// after an engine restart. Thin binding of the coordinator's
    /// `work_db` + host-adapter provider to
    /// [`crate::remote_reattach::reattach_remote_runs`]; `app.rs` calls
    /// this once at startup so a remote worker that outlived the previous
    /// engine has its hook stream (and eventual completion) routed back.
    pub async fn reattach_remote_runs(&self, engine_events_socket: &str) -> crate::remote_reattach::ReattachSummary {
        crate::remote_reattach::reattach_remote_runs(
            &self.work_db,
            self.host_adapter_provider.as_ref(),
            engine_events_socket,
        )
        .await
    }

    /// Run one cross-host remote-lease reconcile pass and kick the
    /// scheduler if anything was reaped (a cleared remote zombie unblocks
    /// the redundant-spawn guard for its work item). Thin binding of the
    /// coordinator's `work_db` + host-adapter provider + dispatch-event
    /// sink to [`crate::remote_lease_reconcile::reconcile_remote_leases`];
    /// the periodic sweep in `app.rs` drives it.
    pub async fn reconcile_remote_leases_once(
        self: &Arc<Self>,
    ) -> crate::remote_lease_reconcile::RemoteLeaseReconcileOutcome {
        let outcome = crate::remote_lease_reconcile::reconcile_remote_leases(
            &self.work_db,
            self.host_adapter_provider.as_ref(),
            self.dispatch_events.as_ref(),
        )
        .await;
        if outcome.reaped > 0 {
            self.kick();
        }
        outcome
    }

    /// Return a clone of the automation worker pool handle. Used by
    /// `app.rs` to expose the pool's live state to the Agents-tab UI.
    pub fn automation_worker_pool(&self) -> WorkerPool {
        self.automation_pool.clone()
    }

    /// Override the review pool. `app.rs` calls this with a pool sized
    /// from `BOSS_REVIEW_POOL_SIZE`; tests may supply a smaller pool.
    pub fn set_review_pool(&mut self, pool: WorkerPool) {
        self.review_pool = pool;
    }

    /// Return a clone of the review worker pool handle. Used by `app.rs`
    /// to expose the pool's live state to the Agents-tab UI and by the
    /// pool-claim reconciler to sweep leaked review claims.
    pub fn review_worker_pool(&self) -> WorkerPool {
        self.review_pool.clone()
    }

    /// Return the union of execution ids currently claimed across ALL
    /// worker pools (main, automation, and review).
    ///
    /// The orphan-active sweep uses this as its liveness oracle so that
    /// executions claimed in the review or automation pools are correctly
    /// treated as live — not abandoned and re-dispatched.  Using only
    /// `worker_pool().claimed_execution_ids()` (the main pool) would miss
    /// review-pool claims and cause the sweep to abandon live reviewer
    /// executions ~90 s after they start.
    pub async fn all_claimed_execution_ids(&self) -> std::collections::HashSet<String> {
        let mut claimed = self.worker_pool.claimed_execution_ids().await;
        claimed.extend(self.automation_pool.claimed_execution_ids().await);
        claimed.extend(self.review_pool.claimed_execution_ids().await);
        claimed
    }

    /// Wire the execution-started hook. Production installs the
    /// `WorkerCompletionHandler` here so it can snapshot the bound
    /// chore PR's head SHA into `work_executions.pr_head_before`
    /// when an execution transitions to `running`.
    pub fn set_execution_started_hook(&mut self, hook: Arc<dyn ExecutionStartedHook>) {
        self.execution_started_hook = hook;
    }

    /// Wire the engine-global metrics registry into this coordinator.
    /// `app.rs` calls this once after `init_all` has registered the
    /// lease counter handles. Tests that omit this call use a pre-seeded
    /// local registry (created in `with_publisher`) so counter increments
    /// never panic.
    pub fn set_metrics(&mut self, metrics: Arc<Registry>) {
        self.metrics = metrics;
    }

    /// Wire the engine's live per-slot worker registry so the dispatch
    /// loop can run the lease-time occupancy guard (defect 3). `app.rs`
    /// calls this once with the shared registry; tests that want to
    /// exercise the guard install a registry, and those that don't leave
    /// it unset (the guard then fails open, preserving legacy behaviour).
    pub fn set_live_worker_states(&mut self, live: Arc<crate::live_worker_state::LiveWorkerStateRegistry>) {
        self.live_worker_states = Some(live);
    }

    /// Override the pre-start retry delay schedule. Pass an empty vec
    /// to disable retries entirely (immediate permanent failure); pass
    /// short durations in tests to avoid real sleeps.
    pub fn with_pre_start_retry_delays(mut self, delays: Vec<Duration>) -> Self {
        self.pre_start_retry_delays = delays;
        self
    }

    /// Install a dispatch-event sink. The production engine threads
    /// in a `JsonlFileSink` writing under the Boss state root; tests
    /// pass a `RecordingDispatchEventSink` to assert on the stage
    /// timeline.
    pub fn set_dispatch_events(&mut self, sink: Arc<dyn DispatchEventSink>) {
        self.dispatch_events = sink;
    }

    /// Builder-style equivalent for callers that construct the
    /// coordinator inside an `Arc::new(...)` chain.
    pub fn with_dispatch_events(mut self, sink: Arc<dyn DispatchEventSink>) -> Self {
        self.dispatch_events = sink;
        self
    }

    pub fn worker_pool(&self) -> WorkerPool {
        self.worker_pool.clone()
    }

    /// Pause or resume global dispatch. When `paused = true` the scheduler
    /// drain stops claiming worker slots for new executions from the main and
    /// automation pools; already-running executions are unaffected. `origin`
    /// determines whether `pr_review` executions are exempt from the pause —
    /// see [`DispatchPauseOrigin`] — and is ignored when resuming. Pass
    /// `paused_since_epoch_s = 0` when resuming (it is ignored).
    ///
    /// The caller is responsible for persisting the new state (including
    /// `origin`, via [`DispatchPauseOrigin::as_metadata_str`]) to `state.db`
    /// so it survives an engine restart — see the `handle_set_dispatch_paused`
    /// handler in `app/engine_meta.rs`.
    pub fn set_dispatch_paused(&self, paused: bool, paused_since_epoch_s: u64, origin: DispatchPauseOrigin) {
        self.dispatch_paused.store(paused, Ordering::Release);
        self.dispatch_paused_since_epoch_s
            .store(if paused { paused_since_epoch_s } else { 0 }, Ordering::Release);
        if paused {
            self.dispatch_pause_exempts_reviews
                .store(origin == DispatchPauseOrigin::Operator, Ordering::Release);
        }
    }

    /// `true` when dispatch is globally paused.
    pub fn is_dispatch_paused(&self) -> bool {
        self.dispatch_paused.load(Ordering::Acquire)
    }

    /// `true` when the current pause (if any) exempts `pr_review` executions
    /// from `drain_ready_queue`'s pause gate. Meaningless when
    /// [`Self::is_dispatch_paused`] is `false`.
    pub fn dispatch_pause_exempts_reviews(&self) -> bool {
        self.dispatch_pause_exempts_reviews.load(Ordering::Acquire)
    }

    /// The epoch-seconds timestamp at which dispatch was last paused, or
    /// `None` when not currently paused.
    pub fn dispatch_paused_since_epoch_s(&self) -> Option<u64> {
        let v = self.dispatch_paused_since_epoch_s.load(Ordering::Acquire);
        if v == 0 { None } else { Some(v) }
    }

    /// Return the pool that should handle `execution`.
    ///
    /// `pr_review` executions always route to the review pool — this is
    /// checked first so a reviewer of an automation-produced task still
    /// lands in the review pool, not the automation pool.
    /// `automation_triage` executions always route to the automation pool.
    /// Regular task executions route to the automation pool when the owning
    /// task has `source_automation_id IS NOT NULL` (it was produced by an
    /// automation). All other executions go to the main pool.
    fn pool_for_execution<'a>(&'a self, execution: &WorkExecution) -> &'a WorkerPool {
        if self.execution_targets_review_pool(execution) {
            &self.review_pool
        } else if self.execution_targets_automation_pool(execution) {
            &self.automation_pool
        } else {
            &self.worker_pool
        }
    }

    /// `true` when `execution` must run on the dedicated review pool —
    /// i.e. it is a `pr_review` reviewer execution.
    fn execution_targets_review_pool(&self, execution: &WorkExecution) -> bool {
        execution.kind == ExecutionKind::PrReview
    }

    fn execution_targets_automation_pool(&self, execution: &WorkExecution) -> bool {
        if execution.kind == ExecutionKind::AutomationTriage {
            return true;
        }
        matches!(
            self.work_db.source_automation_id_for_work_item(&execution.work_item_id),
            Ok(Some(_))
        )
    }

    /// Return the pool that owns `worker_id`. Automation-pool slots carry the
    /// `"auto-worker-"` prefix and review-pool slots the `"review-"` prefix,
    /// both stamped at construction time; everything else is the main pool.
    fn pool_for_worker_id<'a>(&'a self, worker_id: &str) -> &'a WorkerPool {
        if worker_id.starts_with(REVIEW_WORKER_ID_PREFIX) {
            &self.review_pool
        } else if worker_id.starts_with(AUTOMATION_WORKER_ID_PREFIX) {
            &self.automation_pool
        } else {
            &self.worker_pool
        }
    }

    pub fn kick(self: &Arc<Self>) {
        // Order matters: `scheduling_pending` must be written BEFORE we
        // contend on `scheduling_active`. If we lose the swap race
        // (another scheduler is already running) the alive scheduler
        // will read `scheduling_pending` after it drains and notice
        // the wakeup; if we win, the fresh scheduler will reset
        // pending on its way into the drain loop.
        self.scheduling_pending.store(true, Ordering::Release);
        if self.scheduling_active.swap(true, Ordering::AcqRel) {
            tracing::debug!(
                "scheduler_kick outcome=noop reason=already_running — wakeup latched via scheduling_pending"
            );
            return;
        }
        tracing::debug!("scheduler_kick outcome=spawn — starting new run_scheduler task");
        let coordinator = self.clone();
        tokio::spawn(async move {
            coordinator.run_scheduler().await;
        });
    }

    /// Spawn a background task that periodically wakes the scheduler and
    /// surfaces a warning when a `ready` execution has been sitting in
    /// the queue for longer than one heartbeat interval.
    ///
    /// Rationale. The dispatch happy path is: kanban drag → insert
    /// `ready` execution → [`kick`] → `run_scheduler` picks the row up
    /// and emits `request_recorded` within milliseconds. PR #345 closed
    /// the canonical kick/drain TOCTOU by latching every kick into
    /// [`scheduling_pending`], but a `ready` row that stalls at
    /// `status_transition` (no follow-up `request_recorded`) was seen
    /// in the wild — see `exec_18af3ba5259d32a8_12` (2026-05-13), which
    /// sat for 131s before the 90s-age orphan-active reconciler
    /// (PR #429) abandoned it and inserted a fresh redispatch.
    ///
    /// The heartbeat is a second line of defence, not a replacement for
    /// either mechanism:
    ///
    /// * It calls [`kick`] regardless of the in-memory active flag, so
    ///   any kick that was lost to a race the existing latching can't
    ///   cover is re-issued within one interval. The scheduler still
    ///   serializes drains through `scheduling_active`, so two
    ///   schedulers can never run concurrently.
    /// * When the heartbeat actually observes a stranded `ready` row
    ///   (anything older than the interval), it logs a `warn!` line
    ///   carrying the execution id so an operator sees the failure on
    ///   the first occurrence instead of waiting for the orphan
    ///   reconciler. "Fail loudly" was an explicit constraint of the
    ///   reporting work item.
    /// * PR #429's orphan-active reconciler stays intact: that path
    ///   handles the harder case where the execution row itself is
    ///   stale (worker dead, row claimed but not `ready`), which this
    ///   heartbeat does NOT address.
    pub fn spawn_scheduler_heartbeat(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let coordinator = self.clone();
        tokio::spawn(async move {
            // Stagger startup so the first beat doesn't race the
            // engine's own boot-time `kick()` (see `app.rs`).
            tokio::time::sleep(interval).await;
            let interval_ms = interval.as_millis() as u64;
            loop {
                let stranded = coordinator.stranded_ready_executions(interval_ms);
                if !stranded.is_empty() {
                    tracing::warn!(
                        count = stranded.len(),
                        oldest_age_ms = stranded
                            .iter()
                            .map(|(_, age_ms)| *age_ms)
                            .max()
                            .unwrap_or(0),
                        execution_ids = ?stranded
                            .iter()
                            .map(|(id, _)| id.as_str())
                            .collect::<Vec<_>>(),
                        "scheduler heartbeat: ready execution(s) older than \
                         the heartbeat interval found — kick/drain handoff \
                         may have dropped a wakeup; re-kicking now",
                    );
                }
                coordinator.kick();
                tokio::time::sleep(interval).await;
            }
        })
    }

    /// Return every `ready` execution whose `created_at` is older than
    /// `min_age_ms` milliseconds ago, paired with its age in
    /// milliseconds. Used by [`spawn_scheduler_heartbeat`] to surface
    /// stranded rows; kept as a separate method so the heartbeat path
    /// is testable without involving any timers.
    fn stranded_ready_executions(&self, min_age_ms: u64) -> Vec<(String, u64)> {
        let ready = match self.work_db.list_ready_executions() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "scheduler heartbeat: failed to list ready executions; skipping pass",
                );
                return Vec::new();
            }
        };
        let now_secs = crate::epoch_time::now_epoch_secs() as u64;
        let cutoff_ms = min_age_ms;
        ready
            .into_iter()
            .filter_map(|exec| {
                let created_at_secs: u64 = exec.created_at.parse().ok()?;
                let age_ms = now_secs.saturating_sub(created_at_secs).saturating_mul(1000);
                if age_ms < cutoff_ms {
                    return None;
                }
                // A `ready` row that the per-PR single-writer guard is
                // deliberately holding behind a live chain sibling is NOT
                // stranded — it is correctly queued and will dispatch when
                // the sibling reaps. Excluding it keeps the heartbeat's
                // "kick/drain handoff may have dropped a wakeup" warning
                // honest (it would otherwise fire every interval for the
                // entire lifetime of the live sibling). Fail open: if the
                // chain query errors, treat the row as stranded as before.
                if matches!(
                    self.work_db.live_execution_elsewhere_in_chain(&exec.work_item_id),
                    Ok(Some(_))
                ) {
                    return None;
                }
                Some((exec.id, age_ms))
            })
            .collect()
    }

    /// Skip-the-queue dispatch for `bossctl agents launch`. Looks the
    /// execution up directly, claims a worker via
    /// `WorkerPool::claim_worker_force` (which grows the pool by one
    /// slot up to the hard cap when every configured slot is busy),
    /// and runs the same `schedule_execution` path the auto-dispatcher
    /// uses. Returns the worker id we landed on so callers can echo it
    /// back to the human.
    ///
    /// Errors when the execution is not in `ready` (already claimed by
    /// the auto-dispatcher in a race, terminal, or unknown), or when
    /// the worker pool is already at the hard cap with no idle slot.
    pub async fn force_dispatch(self: &Arc<Self>, execution_id: &str) -> Result<String> {
        let execution = self
            .work_db
            .get_execution(execution_id)
            .with_context(|| format!("failed to look up execution {execution_id}"))?;
        if execution.status != ExecutionStatus::Ready {
            return Err(anyhow!(
                "execution {execution_id} is in status {status:?}, not ready — cannot force-dispatch",
                status = execution.status,
            ));
        }
        let preferred_workspace_id = execution.preferred_workspace_id.clone();
        let worker_id = self
            .worker_pool
            .claim_worker_force(&execution.id, preferred_workspace_id.as_deref())
            .await
            .ok_or_else(|| {
                anyhow!(
                    "worker pool already at hard cap ({MAX_WORKER_POOL_SIZE}); cannot \
                     force-dispatch {execution_id}"
                )
            })?;
        if let Err(err) = self.schedule_execution(&execution, &worker_id).await {
            self.worker_pool
                .release_worker(&worker_id, preferred_workspace_id.as_deref())
                .await;
            return Err(err);
        }
        Ok(worker_id)
    }

    async fn run_scheduler(self: Arc<Self>) {
        // Lossless-wakeup loop. The `scheduling_pending` flag is reset
        // at the top of each iteration so we have a clean "have we
        // seen any new kicks since this drain started?" reading at
        // the bottom. The pattern handles three race classes:
        //
        //   1. Kick during drain: caught by the post-drain
        //      `scheduling_pending.load()` and re-enters the inner
        //      loop without releasing `scheduling_active`.
        //   2. Kick after we declared no-pending but before we set
        //      `scheduling_active=false`: the kicker observed active=true
        //      and noop'd, but our second `scheduling_pending.load()`
        //      (after active=false) picks it up and we re-acquire
        //      active to resume draining.
        //   3. Kick after we set `scheduling_active=false`: the kicker
        //      spawns a fresh scheduler; we observe that via the
        //      swap returning `true` and exit cleanly.
        //
        // Without this, the original `_guard`/`break` pattern lost
        // wakeups in the narrow window between "queue empty" and
        // "guard drops" — kicks landing in that window noop'd against
        // `scheduling_active=true` and the new `ready` row sat
        // forever with no scheduler running to pick it up. That is
        // the symptom motivating this fix (see `task_18ae9d21044843b8_44`).
        loop {
            self.scheduling_pending.store(false, Ordering::Release);
            let drain_outcome = self.drain_ready_queue().await;

            // Pool-exhaustion exits don't re-loop here: another
            // scheduler will spawn from the post-`release_worker`
            // `kick()`, and re-looping immediately would just hit the
            // same exhaustion. Fall through to the same active-release
            // logic — `scheduling_pending` may still have been set,
            // and respecting it lets a "fresh row arrived while we
            // were blocked on the pool" case re-attempt once a worker
            // is free without waiting for the next external event.
            let _ = drain_outcome;

            if self.scheduling_pending.load(Ordering::Acquire) {
                // A kick raced us during drain. Reset and re-drain
                // without giving up `scheduling_active`.
                continue;
            }

            // Relinquish the active flag. Any kick that lands from
            // here on will see `scheduling_active=false` on its swap
            // and spawn its own scheduler — but a kick that races
            // between this store and the post-store load below still
            // needs to be caught, hence the second check.
            self.scheduling_active.store(false, Ordering::Release);
            if !self.scheduling_pending.load(Ordering::Acquire) {
                return;
            }
            // A kick landed in the gap. Try to re-claim active; if
            // someone else (a freshly spawned scheduler) already has
            // it, they'll handle the drain.
            if self.scheduling_active.swap(true, Ordering::AcqRel) {
                return;
            }
            // We re-acquired; loop back to drain.
        }
    }

    /// Resolve `WorkDb::live_execution_elsewhere_in_chain` the way every
    /// caller actually needs it: as "is another execution on this PR/chain
    /// genuinely still alive", not "does a row exist whose `status` column
    /// says `running`/`waiting_human`".
    ///
    /// `status IN ('running', 'waiting_human')` is a *paper* liveness
    /// signal — a row can sit `waiting_human` forever after its worker died
    /// without a `Stop` hook (the 2026-06-14 incident this exact gap
    /// re-created for chain-serialization: T251 / `exec_18af40745c552070_26`,
    /// a 56-day-old `waiting_human` zombie with no live pane, wedged every
    /// subsequent execution on its PR/chain behind `chain_serialized` in a
    /// ~10s dispatcher loop until a human noticed and ran `bossctl agents
    /// reap` by hand). `schedule_execution`'s double-spawn guard already
    /// runs the sibling through the same-work-item zombie reconcilers before
    /// treating it as blocking (see the "Liveness gate" comment there); this
    /// applies the identical reconciliation to the *cross-work-item* chain
    /// sibling this guard inspects, using the same two positive-evidence
    /// checks: [`crate::lost_workspace_sweep::reconcile_if_workspace_lost`]
    /// (cube workspace directory gone) and
    /// [`crate::dead_pane_sweep::reconcile_if_pane_dead`] (durable shell pid
    /// is `ESRCH`). Both only ever act on positive evidence of death, so
    /// this can only ever *unblock* a wrongly-serialized dispatch — it can
    /// never falsely treat a genuinely live sibling as dead.
    ///
    /// Reconciling one dead sibling can reveal a second, older live sibling
    /// earlier in the chain (a further `waiting_human` execution masked
    /// behind the one just reaped), so the check re-queries in a small
    /// bounded loop rather than returning after a single reconciliation.
    /// Returns EVERY live chain sibling of `work_item_id`, reconciling
    /// zombies along the way. `resolve_chain_hold`'s
    /// review-bypass decision must be made against the full set: the
    /// underlying `member_ids` walk is chain-root-first
    /// ([`crate::work::dispatch::WorkDb::live_executions_elsewhere_in_chain`]),
    /// so trusting only the first live sibling lets a root `pr_review` mask
    /// a live descendant *writer* — reintroducing the exact two-writer
    /// T1577/T1815 hazard this guard exists to prevent.
    async fn live_chain_siblings(&self, work_item_id: &str) -> Result<Vec<WorkExecution>> {
        const MAX_RECONCILE_ATTEMPTS: u8 = 4;
        for _ in 0..MAX_RECONCILE_ATTEMPTS {
            let siblings = self.work_db.live_executions_elsewhere_in_chain(work_item_id)?;
            if siblings.is_empty() {
                return Ok(siblings);
            }
            let mut any_reconciled = false;
            for sibling in &siblings {
                let reconciled_lost_workspace = crate::lost_workspace_sweep::reconcile_if_workspace_lost(
                    self.work_db.as_ref(),
                    self.dispatch_events.as_ref(),
                    sibling,
                )
                .await;
                let reconciled_dead_pane = !reconciled_lost_workspace
                    && crate::dead_pane_sweep::reconcile_if_pane_dead(
                        self.work_db.as_ref(),
                        self.dispatch_events.as_ref(),
                        sibling,
                        crate::run_reconcile::current_epoch_s(),
                    )
                    .await;
                if reconciled_lost_workspace || reconciled_dead_pane {
                    any_reconciled = true;
                    tracing::warn!(
                        work_item_id,
                        reconciled_execution_id = %sibling.id,
                        reason = if reconciled_dead_pane { "pane_dead" } else { "workspace_lost" },
                        "chain-serialization guard: 'live' chain sibling's worker pane is gone; \
                         reconciled it and re-checking for still-live siblings",
                    );
                }
            }
            if !any_reconciled {
                return Ok(siblings);
            }
        }
        // Exhausted retries without converging on a stable answer (e.g. a
        // pathological chain with many zombies reconciling one per pass).
        // Fail closed: treat whatever is there now as live rather than risk
        // co-dispatching two workers onto the same shared jj backing store.
        self.work_db.live_executions_elsewhere_in_chain(work_item_id)
    }

    /// Resolve the per-PR single-writer chain check for `execution`,
    /// applying the review-yields-to-conflict-fix carve-out: a live
    /// `pr_review` sibling never blocks a merge-conflict-fix revision
    /// (`DispatchClass::MergeConflictRevision`).
    ///
    /// Rationale (the 2026-07-10 T270/T258 priority-inversion incident): a
    /// `pr_review` execution is strictly read-only — never writes, commits,
    /// or pushes (enforced by the reviewer CLAUDE.md, its tool denylist, and
    /// its prompt mandate; see `crate::pr_review` module docs) — so it
    /// cannot participate in the writer-vs-writer T1577/T1815 hazard this
    /// guard exists to prevent (two *writers* rebasing/rewriting each
    /// other's commits on the shared jj backing store). Meanwhile a pending
    /// merge-conflict fix is urgent and, once it lands, immediately
    /// invalidates whatever the in-flight review was looking at anyway —
    /// the completion path's revision-triggered-review re-fire
    /// (`enable_revision_triggered_reviews`) already spawns a fresh review
    /// pass against the new head, so nothing is lost by not waiting for the
    /// stale one to finish. Every other pairing — writer vs writer, writer
    /// vs anything else, or a *non*-conflict revision (CI-fix,
    /// review-findings, operator-filed) waiting behind a review — keeps
    /// serializing exactly as before; only this one combination bypasses.
    ///
    /// The bypass decision is made against **every** live chain sibling, not
    /// just the first one a naive single-sibling lookup would return. The chain
    /// walk is root-first, so trusting a single sibling meant a live
    /// `pr_review` on the chain root could mask a live *writer* further down
    /// the chain (a descendant conflict-fix revision) — bypassing would then
    /// co-dispatch a second writer alongside that still-live one, the exact
    /// two-writer T1577/T1815 hazard this guard exists to prevent. So the
    /// bypass only fires when EVERY live sibling is a review; if even one is
    /// a non-review (writer), this fails closed to `Blocked`.
    ///
    /// Shared by all three chain-guard call sites (`drain_ready_queue`'s
    /// pre-claim check, `schedule_execution`'s pre-lease backstop, and its
    /// post-lease TOCTOU assertion) so the bypass decision — and therefore
    /// whether a merge-conflict revision ever gets refused — is identical
    /// at every checkpoint. Without that consistency a checkpoint later in
    /// the pipeline could re-defer what an earlier one just bypassed,
    /// wedging the row in a defer loop instead of actually dispatching it.
    async fn resolve_chain_hold(&self, execution: &WorkExecution) -> Result<ChainHold> {
        let siblings = self.live_chain_siblings(&execution.work_item_id).await?;
        let Some(first_sibling) = siblings.first().cloned() else {
            return Ok(ChainHold::Clear);
        };
        let queue_len = siblings.len();
        let all_review_siblings = siblings.iter().all(|s| s.kind == ExecutionKind::PrReview);
        let is_conflict_revision = matches!(
            self.work_db.classify_work_item_for_dispatch(&execution.work_item_id),
            Ok(DispatchClass::MergeConflictRevision)
        );
        if all_review_siblings && is_conflict_revision {
            Ok(ChainHold::ReviewBypassed(first_sibling))
        } else {
            // Prefer surfacing a non-review (writer) sibling in the
            // `Blocked` outcome when one is present, since that is the
            // actually-blocking reason — a mix of a review and a writer
            // sibling should report the writer, not the review, in trace
            // output and wait-reason labeling.
            let sibling = siblings
                .iter()
                .find(|s| s.kind != ExecutionKind::PrReview)
                .cloned()
                .unwrap_or(first_sibling);
            Ok(ChainHold::Blocked {
                sibling,
                review_held: all_review_siblings,
                queue_len,
            })
        }
    }

    /// Build the operator-facing string persisted into
    /// `dispatch_wait_reason` (and rendered verbatim on the kanban card)
    /// for a `ChainHold::Blocked` outcome. Names the concrete blocking
    /// task and PR instead of the opaque engine-internal "sibling"
    /// vocabulary — see the T2469 incident (mono#1901) where the card read
    /// "Waiting — blocked behind a live PR sibling" with no way to tell
    /// what a "sibling" was, which task was blocking, or which PR was
    /// involved. When more than one sibling is queued, names the count and
    /// the currently-live one.
    fn chain_serialized_wait_reason(&self, sibling: &WorkExecution, review_held: bool, queue_len: usize) -> String {
        let sibling_task = self
            .resolve_execution_work_item(sibling)
            .ok()
            .and_then(|item| match item {
                WorkItem::Task(task) | WorkItem::Chore(task) => Some(task),
                _ => None,
            });
        let sibling_label = sibling_task
            .as_ref()
            .map(|task| format!("{} '{}'", task.short_label(), task.name))
            .unwrap_or_else(|| sibling.work_item_id.clone());
        // The chain-root task's `pr_url` (set once the PR exists) is the
        // reliable source — a root `chore_implementation`/`task_implementation`
        // execution never carries its own `pr_url` (the PR doesn't exist yet
        // when it was dispatched), only revision executions do. Fall back to
        // the execution's own `pr_url` for the revision-sibling case.
        let pr_ref = sibling_task
            .as_ref()
            .and_then(|task| task.pr_url.as_deref())
            .or(sibling.pr_url.as_deref())
            .and_then(pr_short_reference)
            .map(|r| format!(" on {r}"))
            .unwrap_or_default();
        let queue_prefix = if queue_len > 1 {
            format!("{queue_len} revisions queued; currently running: ")
        } else {
            String::new()
        };
        let cause = if review_held {
            "an automated PR review runs at a time"
        } else {
            "revisions on the same PR run one at a time"
        };
        format!("blocked by {queue_prefix}{sibling_label}{pr_ref} ({cause})")
    }

    /// After `execution` has sat `ready` and chain-serialized for at least
    /// [`CHAIN_SERIALIZED_STALL_THRESHOLD_SECS`], file a durable
    /// [`CHAIN_SERIALIZED_STALL_ATTENTION_KIND`] attention on its work item
    /// so a human notices without grepping `engine-trace.jsonl` — the T251
    /// incident sat in this exact state, re-deferred every ~10s, for ~20
    /// silent minutes before a human found it by hand.
    ///
    /// Uses `execution.created_at` as the "stuck since" clock: a `ready`
    /// row is re-evaluated every drain pass, so its age is a reasonable
    /// proxy for how long it has been waiting (a row that spent time in
    /// `waiting_dependency` before promotion only makes this an
    /// under-estimate, never a false alarm). Idempotent — repeated calls
    /// while the stall persists are a no-op after the first.
    fn surface_chain_serialized_stall_if_overdue(&self, execution: &WorkExecution, sibling: &WorkExecution) {
        let Some(created_at) = execution.created_epoch() else {
            return;
        };
        let elapsed = crate::run_reconcile::current_epoch_s() - created_at;
        if elapsed < CHAIN_SERIALIZED_STALL_THRESHOLD_SECS {
            return;
        }
        let title = "Execution stuck behind a chain-serialized sibling".to_owned();
        let body = format!(
            "Execution `{}` (work item `{}`) has been deferred for ~{} minutes with \
             `reason=chain_serialized`, waiting behind live sibling execution `{}` \
             (work item `{}`).\n\n\
             If that sibling is actually still working, this will clear on its own once \
             it finishes. If it is actually a dead worker (its pane exited without a `Stop` \
             hook), the engine's periodic zombie sweeps (`lost_workspace_sweep` / \
             `dead_pane_sweep`) reconcile it automatically on their next pass; if it \
             persists, `bossctl agents reap {}` clears it by hand.",
            execution.id,
            execution.work_item_id,
            elapsed / 60,
            sibling.id,
            sibling.work_item_id,
            sibling.id,
        );
        if let Err(err) = self.work_db.upsert_work_item_attention(
            &execution.work_item_id,
            CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
            &title,
            &body,
        ) {
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                ?err,
                "drain: failed to raise chain_serialized_stall attention",
            );
        }
    }

    /// Drain every currently-`ready` execution. Returns the reason the
    /// drain stopped so the caller can decide whether to re-enter
    /// immediately (queue empty + pending wakeup) or yield (pool
    /// exhausted).
    /// Drain the `ready` execution queue, routing each execution to the
    /// correct pool (main, automation, or review). Per-pool exhaustion is
    /// handled independently: a full pool does not block dispatch on the
    /// other pools.
    ///
    /// All `ready` rows are fetched once at the top of each drain pass.
    /// Executions whose pool is already known to be exhausted are skipped
    /// for this pass; they remain `ready` and will be picked up on the
    /// next `kick()` triggered by `release_worker_and_kick`.
    async fn drain_ready_queue(self: &Arc<Self>) -> DrainOutcome {
        // Global pause gate. `pr_review` executions are the lifecycle of a
        // change already in flight, not new work, so an operator-originated
        // pause exempts them — they keep draining into the review pool while
        // main/automation rows are held. A breaker-originated pause (the
        // app's spawn path itself is broken — see `spawn_health.rs`) exempts
        // nothing, since dispatching a review would just burn another spawn
        // attempt against the same dead path.
        let paused = self.dispatch_paused.load(Ordering::Acquire);
        let reviews_exempt_from_pause = paused && self.dispatch_pause_exempts_reviews.load(Ordering::Acquire);

        let executions = match self.work_db.list_ready_executions() {
            Ok(e) => e,
            Err(err) => {
                tracing::error!(?err, "failed to list ready executions");
                return DrainOutcome::QueueEmpty;
            }
        };

        if executions.is_empty() {
            return DrainOutcome::QueueEmpty;
        }

        if paused {
            let review_count = executions
                .iter()
                .filter(|e| self.execution_targets_review_pool(e))
                .count();
            let held_count = executions.len() - review_count;
            if reviews_exempt_from_pause {
                tracing::debug!(
                    held_count,
                    review_exempt_count = review_count,
                    "drain_ready_queue: dispatch is globally paused — holding non-review rows, \
                     draining review-pool exemptions",
                );
            } else {
                tracing::debug!(
                    held_count,
                    review_exempt_count = 0,
                    "drain_ready_queue: dispatch is globally paused — skipping (breaker pause, no exemptions)",
                );
                return DrainOutcome::QueueEmpty;
            }
        }

        let mut main_pool_exhausted = false;
        let mut auto_pool_exhausted = false;
        let mut review_pool_exhausted = false;

        // Per-pool candidate counts for this drain pass, computed up front
        // so the `request_recorded` event below can report "how many other
        // eligible rows this execution's dispatch class beat" without a
        // second query per row. `executions` is already sorted by
        // `(DispatchClass, priority, created_at, id)` (see
        // `WorkDb::list_ready_executions`), so within a pool the row order
        // below IS the priority order — a class=1 row that appears first is
        // winning against every other row counted for its pool here.
        let pool_ready_counts: HashMap<&'static str, usize> = {
            let mut counts: HashMap<&'static str, usize> = HashMap::new();
            for execution in &executions {
                let is_review = self.execution_targets_review_pool(execution);
                let is_automation = !is_review && self.execution_targets_automation_pool(execution);
                let label = if is_review {
                    "review"
                } else if is_automation {
                    "automation"
                } else {
                    "main"
                };
                *counts.entry(label).or_insert(0) += 1;
            }
            counts
        };

        for execution in executions {
            let preferred_workspace_id = execution.preferred_workspace_id.clone();
            // Classify the target pool. Review is checked first (and excludes
            // the others) so a reviewer of an automation-produced task is
            // counted against the review pool, not the automation pool.
            let is_review = self.execution_targets_review_pool(&execution);
            let is_automation = !is_review && self.execution_targets_automation_pool(&execution);
            let is_main = !is_review && !is_automation;
            let pool_label = if is_review {
                "review"
            } else if is_automation {
                "automation"
            } else {
                "main"
            };

            // Dispatch is paused and this row isn't exempt: leave it `ready`
            // for the next drain after resume. Reached only when
            // `reviews_exempt_from_pause` is true (a non-exempt pause already
            // returned above), so this holds every non-review row while
            // review rows fall through to normal dispatch below.
            if paused && !is_review {
                continue;
            }

            // Skip executions for pools we already know are full.
            // They remain `ready` and will be retried on the next kick.
            if is_review && review_pool_exhausted {
                continue;
            }
            if is_automation && auto_pool_exhausted {
                continue;
            }
            if is_main && main_pool_exhausted {
                continue;
            }

            // Dispatch-class + "why it won" bookkeeping (operator directive:
            // revisions before tasks/chores, ordered by revision kind).
            // Recomputed here (rather than threaded through from
            // `list_ready_executions`) because it's only needed for the
            // trace, not the hot dispatch path. See `DispatchClass`.
            let dispatch_class = self
                .work_db
                .classify_work_item_for_dispatch(&execution.work_item_id)
                .unwrap_or(DispatchClass::OtherWork);
            let pool_ready_count = pool_ready_counts.get(pool_label).copied().unwrap_or(1);

            // Stage 1: request_recorded
            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::RequestRecorded, DispatchOutcome::Ok, &execution.id)
                        .with_work_item(&execution.work_item_id)
                        .with_details(serde_json::json!({
                            "preferred_workspace_id": preferred_workspace_id,
                            "pool": pool_label,
                            "dispatch_class": dispatch_class.as_ordinal(),
                            "dispatch_class_label": dispatch_class.label(),
                            "pool_ready_count": pool_ready_count,
                            "beaten_candidates": pool_ready_count.saturating_sub(1),
                        })),
                )
                .await;
            tracing::info!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                preferred_workspace_id = ?preferred_workspace_id,
                pool = pool_label,
                dispatch_class = dispatch_class.as_ordinal(),
                dispatch_class_label = dispatch_class.label(),
                beaten_candidates = pool_ready_count.saturating_sub(1),
                "spawn_attempt status=ready -> picked_up"
            );

            // Per-PR single-writer guard (T1577 / T1815 incident): defer this
            // execution if ANOTHER work item on the same PR/revision chain is
            // already live. Checked BEFORE claiming a worker so a serialized
            // row never burns a slot or pollutes its dispatch timeline. The
            // row stays `ready` and re-attempts on the next kick (which fires
            // when the live sibling reaps), so it runs strictly after it.
            // `schedule_execution` re-checks this as the chokepoint backstop
            // for the `force_dispatch` path. Goes through `resolve_chain_hold`
            // (not the raw `WorkDb` query) so a `waiting_human` sibling whose
            // worker pane is actually dead doesn't wedge this row forever —
            // see `live_chain_siblings`'s docs for the T251 incident this
            // closes — and so a merge-conflict-fix revision never waits
            // behind a read-only review (see `resolve_chain_hold`'s docs for
            // the T270/T258 priority-inversion incident this closes).
            match self.resolve_chain_hold(&execution).await {
                Ok(ChainHold::Blocked {
                    sibling,
                    review_held,
                    queue_len,
                }) => {
                    let event_reason = if review_held {
                        "chain_serialized_review_held"
                    } else {
                        "chain_serialized"
                    };
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        live_sibling_execution_id = %sibling.id,
                        live_sibling_work_item_id = %sibling.work_item_id,
                        pool = pool_label,
                        review_held,
                        "spawn_attempt status=ready -> deferred reason={event_reason}"
                    );
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Skipped, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_details(serde_json::json!({
                                    "reason": event_reason,
                                    "review_held": review_held,
                                    "live_sibling_execution_id": sibling.id,
                                    "live_sibling_work_item_id": sibling.work_item_id,
                                })),
                        )
                        .await;
                    // Operator-facing wait reason: names the concrete blocking
                    // task/PR instead of the opaque "PR sibling" wording (T2469
                    // incident — the card read "blocked behind a live PR
                    // sibling" with no way to tell what a sibling was or which
                    // task/PR was involved). `dispatch_events`/tracing above
                    // keep the terse `event_reason` code for stats grouping;
                    // this is the string persisted into `dispatch_wait_reason`
                    // and rendered verbatim on the kanban card.
                    let wait_reason = self.chain_serialized_wait_reason(&sibling, review_held, queue_len);
                    if let Err(err) = self.work_db.set_dispatch_wait_reason(&execution.id, &wait_reason) {
                        tracing::warn!(execution_id = %execution.id, ?err, "failed to record dispatch_wait_reason");
                    }
                    self.surface_chain_serialized_stall_if_overdue(&execution, &sibling);
                    // Leave the row `ready`; do NOT mark any pool exhausted —
                    // other executions in this pass may still dispatch.
                    continue;
                }
                Ok(ChainHold::ReviewBypassed(sibling)) => {
                    // Reviews are read-only — a pending merge-conflict fix
                    // must not wait the length of a review run behind one.
                    // Fall through and dispatch this pass; the review keeps
                    // running (it will self-terminate normally, and its
                    // findings will already be stale against the fix's
                    // upcoming push — `enable_revision_triggered_reviews`
                    // fires a fresh pass once it lands).
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        live_sibling_execution_id = %sibling.id,
                        live_sibling_work_item_id = %sibling.work_item_id,
                        pool = pool_label,
                        "spawn_attempt status=ready -> chain_hold_bypassed reason=review_yields_to_conflict_fix"
                    );
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Ok, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_details(serde_json::json!({
                                    "chain_hold_bypassed": "review_yields_to_conflict_fix",
                                    "review_held": true,
                                    "live_sibling_execution_id": sibling.id,
                                    "live_sibling_work_item_id": sibling.work_item_id,
                                })),
                        )
                        .await;
                    if let Err(err) = self.work_db.resolve_external_tracker_attention(
                        &execution.work_item_id,
                        CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
                    ) {
                        tracing::warn!(
                            execution_id = %execution.id,
                            work_item_id = %execution.work_item_id,
                            ?err,
                            "drain: failed to resolve chain_serialized_stall attention on bypass",
                        );
                    }
                    // Fall through to normal dispatch below.
                }
                Ok(ChainHold::Clear) => {
                    // No longer (or never) chain-serialized — clear any stall
                    // attention a prior pass raised for this work item so it
                    // doesn't linger `open` once dispatch actually proceeds.
                    if let Err(err) = self.work_db.resolve_external_tracker_attention(
                        &execution.work_item_id,
                        CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
                    ) {
                        tracing::warn!(
                            execution_id = %execution.id,
                            work_item_id = %execution.work_item_id,
                            ?err,
                            "drain: failed to resolve chain_serialized_stall attention on unblock",
                        );
                    }
                }
                Err(err) => {
                    // Fail open: a DB error must not wedge the queue. The
                    // `schedule_execution` backstop still guards the spawn.
                    tracing::warn!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        ?err,
                        "drain: chain single-writer check failed — proceeding without pre-claim defer",
                    );
                }
            }

            // merge_order dispatch stagger (direction 2, optional, default off):
            // when configured, the "later" side of a high-overlap merge_order
            // pair whose "first" side is still in flight gets a one-shot bounded
            // dispatch offset so the two workers' diffs interleave less. This is
            // NOT a block and never waits for a merge — the row simply becomes
            // dispatchable again after the window via the `dispatch_not_before`
            // gate + the scheduler heartbeat. It runs after the chain-hold gate
            // (so a serialized row is never double-handled) and before claiming a
            // slot (so a staggered row never burns a worker). Fail open on any DB
            // error — a stagger check must never wedge the queue.
            if self.merge_order_stagger_secs > 0 {
                match self.work_db.maybe_stagger_merge_order_dispatch(
                    &execution.id,
                    &execution.work_item_id,
                    self.merge_order_stagger_secs,
                ) {
                    Ok(Some(not_before)) => {
                        tracing::info!(
                            execution_id = %execution.id,
                            work_item_id = %execution.work_item_id,
                            not_before,
                            stagger_secs = self.merge_order_stagger_secs,
                            pool = pool_label,
                            "spawn_attempt status=ready -> deferred reason=merge_order_stagger"
                        );
                        self.dispatch_events
                            .emit(
                                DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Skipped, &execution.id)
                                    .with_work_item(&execution.work_item_id)
                                    .with_details(serde_json::json!({
                                        "reason": "merge_order_stagger",
                                        "not_before": not_before,
                                        "stagger_secs": self.merge_order_stagger_secs,
                                    })),
                            )
                            .await;
                        // Leave the row `ready` (now with a future
                        // `dispatch_not_before`); do NOT mark any pool exhausted.
                        continue;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            work_item_id = %execution.work_item_id,
                            ?err,
                            "drain: merge_order stagger check failed — dispatching without offset",
                        );
                    }
                }
            }

            let pool = self.pool_for_execution(&execution);
            let Some(worker_id) = pool
                .claim_worker(&execution.id, preferred_workspace_id.as_deref())
                .await
            else {
                // This pool is fully claimed. Record exhaustion and continue
                // so executions for the other pools can still be dispatched.
                let pool_capacity = pool.capacity().await;
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    pool_capacity,
                    pool = pool_label,
                    "spawn_attempt status=ready -> deferred reason=pool_exhausted"
                );

                // Ghost-active invariant check (main pool only; automation and
                // review executions are excluded from the normal kanban).
                if is_main {
                    let orphans = self.work_db.list_active_chores_without_live_run().unwrap_or_default();
                    if !orphans.is_empty() {
                        tracing::warn!(
                            ghost_active = ?orphans,
                            pool_capacity,
                            "active chores without a running execution after pool exhaustion \
                             — `boss chore list --status active` and `bossctl agents list` will \
                             diverge until a slot frees up"
                        );
                    }
                }

                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Skipped, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_details(serde_json::json!({
                                "reason": "pool_exhausted",
                                "pool": pool_label,
                                "pool_capacity": pool_capacity,
                            })),
                    )
                    .await;
                if let Err(err) = self.work_db.set_dispatch_wait_reason(&execution.id, "pool_exhausted") {
                    tracing::warn!(execution_id = %execution.id, ?err, "failed to record dispatch_wait_reason");
                }

                if is_review {
                    review_pool_exhausted = true;
                } else if is_automation {
                    auto_pool_exhausted = true;
                    // For automation triage executions, mark the automation_runs
                    // row as `pool_throttled` (not `failed_will_retry`) so the UI
                    // shows "Queued" rather than a failure badge.
                    if execution.kind == ExecutionKind::AutomationTriage {
                        let detail = format!(
                            "automation pool exhausted ({pool_capacity}/{pool_capacity} busy); \
                             triage queued, will dispatch when a slot frees"
                        );
                        if let Err(err) = self
                            .work_db
                            .update_automation_run_for_pool_throttle(&execution.id, &detail)
                        {
                            tracing::warn!(
                                execution_id = %execution.id,
                                ?err,
                                "failed to record pool_throttled outcome on automation_runs row",
                            );
                        }
                    }
                } else {
                    main_pool_exhausted = true;
                }
                continue;
            };

            // Record the physical slot + page the claim landed on so a later
            // spawn failure on this slot is attributable to Bridge Crew vs
            // Lower Decks in `bossctl dispatch diagnose` (the page is `null`
            // for automation/review pools, which are single-page).
            let claimed_slot = slot_id_from_worker_id(&worker_id);
            let claimed_page = claimed_slot.and_then(worker_page_label);
            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Ok, &execution.id)
                        .with_work_item(&execution.work_item_id)
                        .with_worker(&worker_id)
                        .with_details(serde_json::json!({
                            "pool": pool_label,
                            "slot_id": claimed_slot,
                            "page": claimed_page,
                        })),
                )
                .await;
            if let Err(err) = self.work_db.clear_dispatch_wait_reason(&execution.id) {
                tracing::warn!(execution_id = %execution.id, ?err, "failed to clear dispatch_wait_reason");
            }

            match self.schedule_execution(&execution, &worker_id).await {
                Ok(()) => {
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        worker_id = %worker_id,
                        "spawn_attempt status=ready -> spawned"
                    );
                }
                Err(err) => {
                    tracing::error!(
                        ?err,
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        worker_id = %worker_id,
                        "spawn_attempt status=ready -> failed reason=schedule_execution_error"
                    );
                    self.pool_for_worker_id(&worker_id)
                        .release_worker(&worker_id, preferred_workspace_id.as_deref())
                        .await;
                }
            }
        }

        if main_pool_exhausted || auto_pool_exhausted || review_pool_exhausted {
            DrainOutcome::PoolExhausted
        } else {
            DrainOutcome::QueueEmpty
        }
    }

    /// Resolve the [`WorkItem`] an execution operates on.
    ///
    /// For a normal execution this is the persisted task/chore/product/
    /// project. An `automation_triage` execution, though, binds to an
    /// `automations.id` — there is no task row for `get_work_item` to find —
    /// so we synthesize an in-memory `Chore` carrying the automation's
    /// product/name/repo. The synthetic item only feeds the task-centric
    /// spawn plumbing (cube task label, change title, product resolution);
    /// the runner branches on `kind` to render the triage preamble and the
    /// completion handler branches on `kind` to run the outcome detector, so
    /// the synthetic fields never drive real task work.
    ///
    /// An `answer_agent` execution (P3b) binds to a `work_comments.id` for
    /// the same reason — see [`crate::work::WorkDb::create_answer_agent_execution`]
    /// — so it gets the same synthetic-item treatment via
    /// [`Self::synthetic_answer_agent_work_item`].
    fn resolve_execution_work_item(&self, execution: &WorkExecution) -> Result<WorkItem> {
        if execution.kind == ExecutionKind::AutomationTriage
            && let Some(item) = self.synthetic_triage_work_item(execution)
        {
            return Ok(item);
        }
        if execution.kind == ExecutionKind::AnswerAgent
            && let Some(item) = self.synthetic_answer_agent_work_item(execution)
        {
            return Ok(item);
        }
        self.work_db.get_work_item(&execution.work_item_id)
    }

    /// Build the synthetic `Chore` work item for an `automation_triage`
    /// execution from the bound automation. `None` when the automation row is
    /// gone (deleted mid-flight) — the caller then falls back to the normal
    /// `get_work_item`, which fails cleanly.
    fn synthetic_triage_work_item(&self, execution: &WorkExecution) -> Option<WorkItem> {
        let automation = self.work_db.get_automation(&execution.work_item_id).ok().flatten()?;
        let task = boss_protocol::Task::builder()
            .id(automation.id.clone())
            .product_id(automation.product_id.clone())
            .kind(TaskKind::Chore)
            .name(format!("Automation triage: {}", automation.name))
            .description(automation.standing_instruction.clone())
            .status(TaskStatus::Active)
            .repo_remote_url(execution.repo_remote_url.clone())
            .created_at(automation.created_at.clone())
            .updated_at(automation.updated_at.clone())
            .build();
        Some(WorkItem::Chore(task))
    }

    /// Build the synthetic `Chore` work item for an `answer_agent`
    /// execution (P3b) from the bound comment and its resolved doc owner.
    /// `None` when the comment is gone, or its doc owner no longer resolves
    /// (both are the same "engine state raced under us mid-flight"
    /// tolerance `synthetic_triage_work_item` already applies — the caller
    /// falls back to the normal `get_work_item`, which fails cleanly).
    ///
    /// `product_id` is the doc owner task's product — needed for host
    /// capability resolution ([`Self::select_host_for_execution`]); `name`/
    /// `description` surface the question in cube's task label / change
    /// title. Like the triage synthetic item, these fields only feed spawn
    /// plumbing: the runner (P3b) composes the real answer-agent prompt
    /// separately, and the completion handler branches on `kind` to
    /// finalise the run instead of doing PR detection.
    fn synthetic_answer_agent_work_item(&self, execution: &WorkExecution) -> Option<WorkItem> {
        let comment = self.work_db.get_comment(&execution.work_item_id).ok().flatten()?;
        let doc_owner = self
            .work_db
            .resolve_doc_owner(&comment.artifact_kind, &comment.artifact_id)
            .ok()
            .flatten()?;
        let owner_item = self.work_db.get_work_item(&doc_owner.task_id).ok()?;
        let product_id = work_item_product_id(&owner_item);
        let short_quote = if comment.body.chars().count() > 60 {
            format!("{}…", comment.body.chars().take(60).collect::<String>())
        } else {
            comment.body.clone()
        };
        let task = boss_protocol::Task::builder()
            .id(comment.id.clone())
            .product_id(product_id)
            .kind(TaskKind::Chore)
            .name(format!("Answer comment: {short_quote}"))
            .description(comment.body.clone())
            .status(TaskStatus::Active)
            .repo_remote_url(execution.repo_remote_url.clone())
            .created_at(comment.created_at.clone())
            .updated_at(comment.updated_at.clone())
            .build();
        Some(WorkItem::Chore(task))
    }

    /// Pick the host this execution should run on. Honours the pin escape
    /// hatch (`work_executions.pinned_host_id`) and the capability filter,
    /// then ranks the survivors by branch affinity / free slots — see
    /// [`crate::host_scheduling::select_host`].
    ///
    /// The local host is never slot-gated here: the worker pool already
    /// bounded local concurrency before dispatch reached this point, and
    /// `hosts.local.pool_size` defaults to 1, so double-gating on it would
    /// throttle local dispatch to a single concurrent worker. We therefore
    /// report the local slot as always-free (`active_runs = 0`) and let
    /// only remote hosts be gated by their `work_runs` active count.
    ///
    /// Returns the selected [`Host`] or an error describing why nothing was
    /// eligible (consumed by the caller as a recoverable pre-start
    /// failure).
    fn select_host_for_execution(&self, execution: &WorkExecution, work_item: &WorkItem) -> Result<Host> {
        let pinned = self.work_db.execution_pinned_host(&execution.id).unwrap_or_else(|err| {
            tracing::warn!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "host-selection: failed to read pinned host; treating as unpinned",
            );
            None
        });

        // Capability requirements union over the chore + its product +
        // its project. Empty today (no writer yet), which leaves every
        // enabled host capability-eligible — preserving local behaviour.
        let product_id = work_item_product_id(work_item);
        let project_id = work_item_project_id(work_item);
        let mut subject_ids: Vec<&str> = vec![execution.work_item_id.as_str(), product_id.as_str()];
        if let Some(pid) = project_id.as_deref() {
            subject_ids.push(pid);
        }
        let required_capabilities = self
            .work_db
            .required_capabilities_for_subject_ids(&subject_ids)
            .unwrap_or_else(|err| {
                tracing::warn!(
                    execution_id = %execution.id,
                    error = %format!("{err:#}"),
                    "host-selection: failed to read capability requirements; treating as none",
                );
                BTreeSet::new()
            });

        let hosts = self.work_db.list_hosts().context("host-selection: list hosts")?;
        let active = self.work_db.active_runs_per_host().unwrap_or_default();

        let slots: Vec<HostSlot> = hosts
            .iter()
            .map(|host| {
                let capabilities = self
                    .work_db
                    .list_host_capabilities(&host.id)
                    .map(|caps| caps.into_iter().map(|c| c.capability).collect::<BTreeSet<_>>())
                    .unwrap_or_default();
                let active_runs = if host.id == "local" {
                    0
                } else {
                    *active.get(&host.id).unwrap_or(&0)
                };
                HostSlot {
                    host: host.clone(),
                    capabilities,
                    active_runs,
                    // Branch-affinity tiebreaker is deferred (PR4): the
                    // affinity key is the PR branch, which is unset until
                    // the first run pushes. Free-slots-first is the
                    // design's documented v1 fallback for the first run.
                    had_prior_run_on_branch: false,
                }
            })
            .collect();

        let requirements = ChoreRequirements {
            required_capabilities,
            pinned_host_id: pinned,
        };
        let (picked, report) = host_scheduling::select_host(&requirements, &slots);
        match picked {
            Some(host_id) => hosts
                .into_iter()
                .find(|h| h.id == host_id)
                .ok_or_else(|| anyhow!("selected host '{host_id}' is missing from the registry")),
            None => Err(anyhow!(
                "no eligible host for execution {}: {}",
                execution.id,
                summarize_ineligibility(&report),
            )),
        }
    }

    async fn schedule_execution(self: &Arc<Self>, execution: &WorkExecution, worker_id: &str) -> Result<()> {
        // Double-spawn guard (Bug A): if another execution for this
        // work_item is already live (running or waiting_human), this
        // execution is a redundant duplicate created by the orphan sweep
        // racing with a still-active pane. Abandon it without spawning
        // so "execution run completed" doesn't fire prematurely.
        match self
            .work_db
            .get_live_execution_for_work_item(&execution.work_item_id, &execution.id)
        {
            Ok(Some(live)) => {
                // Liveness gate (waiting_human-zombie fix, 2026-06-14 incident):
                // `get_live_execution_for_work_item` returns any row in
                // `status IN ('running','waiting_human')`, but that is a *paper*
                // liveness signal. A row can sit `waiting_human` forever after
                // its worker died without a `Stop` hook — e.g. the cube
                // workspace-root migration relocated the pool out from under
                // three running triage panes, so their rows stayed `waiting_human`
                // and every subsequent fire died right here with `redundant_spawn`.
                // Before treating this execution as a redundant duplicate, verify
                // the blocker is *actually* live. Two positive-death signals make
                // a `waiting_human`/`running` blocker a zombie that must not block
                // this spawn: (1) its recorded workspace directory has vanished
                // (`lost_workspace`); or (2) its worker pane's durable shell pid is
                // gone (`pane_death` — the 2026-07-04 app-relaunch wedge, where the
                // pane died with the host app but the cube lease stayed green and
                // the workspace dir survived, so only a pid probe reveals it).
                // Either one reconciles the blocker to terminal and lets this spawn
                // proceed instead of wedging behind `redundant_spawn` forever.
                let reconciled_lost_workspace = crate::lost_workspace_sweep::reconcile_if_workspace_lost(
                    self.work_db.as_ref(),
                    self.dispatch_events.as_ref(),
                    &live,
                )
                .await;
                let reconciled_dead_pane = !reconciled_lost_workspace
                    && crate::dead_pane_sweep::reconcile_if_pane_dead(
                        self.work_db.as_ref(),
                        self.dispatch_events.as_ref(),
                        &live,
                        crate::run_reconcile::current_epoch_s(),
                    )
                    .await;
                if reconciled_lost_workspace || reconciled_dead_pane {
                    tracing::warn!(
                        execution_id = %execution.id,
                        reconciled_execution_id = %live.id,
                        work_item_id = %execution.work_item_id,
                        reason = if reconciled_dead_pane { "pane_dead" } else { "workspace_lost" },
                        "spawn_attempt: prior 'live' execution's worker pane is gone; \
                         reconciled it and proceeding with this spawn",
                    );
                    // Not redundant after all — fall through to the rest of dispatch.
                } else {
                    tracing::warn!(
                        execution_id = %execution.id,
                        live_execution_id = %live.id,
                        work_item_id = %execution.work_item_id,
                        "spawn_attempt: redundant — another execution is already live; deferring to that one",
                    );
                    if let Err(err) = self.work_db.mark_execution_redundant(&execution.id) {
                        tracing::error!(
                            execution_id = %execution.id,
                            ?err,
                            "spawn_attempt: failed to mark redundant execution abandoned",
                        );
                    }
                    // Honest automation bookkeeping: an `automation_triage` fire
                    // that dies here pre-spawn must record the real reason on its
                    // `automation_runs` row, overwriting the pessimistic
                    // "dispatched; awaiting triage worker decision" placeholder the
                    // scheduler stamps at fire time (which is a lie once dispatch
                    // failed before the worker ever ran).
                    if execution.kind == ExecutionKind::AutomationTriage
                        && let Err(err) = self.work_db.finalize_automation_triage_run(
                            &execution.id,
                            boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY,
                            None,
                            Some(
                                "dispatch aborted pre-spawn at host_selected: redundant_spawn \
                                 (superseded by another live execution)",
                            ),
                        )
                    {
                        tracing::warn!(
                            execution_id = %execution.id,
                            ?err,
                            "spawn_attempt: failed to record redundant_spawn outcome on automation_runs row",
                        );
                    }
                    // Emit a terminal event so the dispatch timeline doesn't
                    // silently stall at `worker_claimed/ok` for 30s until the
                    // watchdog fires. The execution is already marked redundant
                    // (terminal DB state), so `host_selected:error` is the
                    // correct closer — no `record_start_failure` needed.
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_worker(worker_id)
                                .with_details(serde_json::json!({
                                    "reason": "redundant_spawn",
                                    "live_execution_id": live.id,
                                })),
                        )
                        .await;
                    return Err(anyhow::anyhow!(
                        "redundant spawn: execution {} for work_item {} superseded by live execution {}",
                        execution.id,
                        execution.work_item_id,
                        live.id,
                    ));
                }
            }
            Ok(None) => {}
            Err(err) => {
                // Non-fatal: if the DB check fails, proceed with the
                // spawn rather than blocking all dispatches. The worst
                // case is the double-spawn race we're trying to prevent,
                // which is the pre-existing behaviour.
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "spawn_attempt: live-execution check failed — proceeding without dedup guard",
                );
            }
        }

        // Per-PR single-writer guard (T1577 / T1815 incident). The
        // double-spawn guard above only sees executions on this exact
        // work_item; it cannot see a *sibling* execution that targets the
        // SAME PR. A revision (conflict-resolution, ci-fix, review-findings,
        // operator) is a distinct work item whose chain root owns the PR,
        // and cube co-locates every same-PR worker on ONE shared jj backing
        // store — so a second live execution anywhere in the chain rebases
        // and rewrites the first's commits. Every dispatch entry point
        // funnels through here, so this one check serializes ALL of them:
        // the conflict-resolution and ci-fix auto-spawn paths included.
        //
        // The auto-dispatcher (`drain_ready_queue`) applies this same guard
        // BEFORE claiming a worker (so it never wastes a slot or emits a
        // misleading `worker_claimed` timeline for a deferred row); this
        // copy is the backstop for `force_dispatch` (`bossctl agents
        // launch`) and any future direct caller, closing the chokepoint.
        //
        // Unlike the redundant-duplicate guard above, a chain sibling is NOT
        // redundant — it has its own real work — so we DEFER rather than
        // abandon: the execution stays `ready` and is re-attempted on the
        // next scheduler kick (which fires when the live sibling reaps), so
        // it runs strictly after the live one finishes.
        //
        // Goes through `resolve_chain_hold` (not the raw `WorkDb` query) so
        // this backstop shares the pre-claim guard's zombie reconciliation —
        // see that method's docs — and its review-yields-to-conflict-fix
        // carve-out, so a merge-conflict revision the pre-claim check in
        // `drain_ready_queue` just bypassed isn't immediately re-deferred
        // here (which would otherwise wedge it in a defer loop instead of
        // ever actually dispatching).
        match self.resolve_chain_hold(execution).await {
            Ok(ChainHold::Blocked {
                sibling, review_held, ..
            }) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    live_sibling_execution_id = %sibling.id,
                    live_sibling_work_item_id = %sibling.work_item_id,
                    review_held,
                    "spawn_attempt: deferred — another execution on the same PR/chain is live; \
                     serializing behind it rather than co-dispatching onto the shared jj store",
                );
                // Leave the execution `ready` (do NOT abandon). The caller
                // releases the claimed worker on this `Err`, and the next
                // kick re-evaluates the still-`ready` row.
                //
                // Emit a terminal event so the dispatch timeline advances
                // past `worker_claimed/ok` immediately — otherwise the
                // stall watchdog fires ~30s later, masking the real reason
                // (chain serialization) in the timeline. The execution is
                // not actually failed; on the next kick it will re-attempt
                // and may succeed. The `error` outcome is necessary here
                // because `is_terminal_event` only recognises `outcome ==
                // "error"` (besides `pane_spawned/ok`) as closing the stage.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_details(serde_json::json!({
                                "reason": "chain_serialized_backstop",
                                "review_held": review_held,
                                "live_sibling_execution_id": sibling.id,
                                "live_sibling_work_item_id": sibling.work_item_id,
                            })),
                    )
                    .await;
                return Err(anyhow::anyhow!(
                    "serialized: execution {} for work_item {} deferred behind live chain sibling {} (work_item {})",
                    execution.id,
                    execution.work_item_id,
                    sibling.id,
                    sibling.work_item_id,
                ));
            }
            Ok(ChainHold::ReviewBypassed(sibling)) => {
                tracing::info!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    live_sibling_execution_id = %sibling.id,
                    live_sibling_work_item_id = %sibling.work_item_id,
                    "spawn_attempt: proceeding — live sibling is a read-only pr_review, \
                     bypassed for this merge-conflict revision",
                );
            }
            Ok(ChainHold::Clear) => {}
            Err(err) => {
                // Non-fatal: proceed rather than blocking all dispatches.
                // The post-lease assertion below is the defense-in-depth
                // backstop for the single-writer invariant.
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "spawn_attempt: chain single-writer check failed — proceeding without serialization guard",
                );
            }
        }

        // At-dispatch gate check: if the work item is still gated by an
        // unmet prerequisite, the execution must not be dispatched. This
        // closes the timing window where a `ready` row was created before
        // a `blocks` dep edge committed (or before the gate check in
        // `reconcile_work_item_execution` ran). Downgrade to
        // `waiting_dependency` so the execution leaves the ready queue
        // and gets re-promoted when the gate clears.
        match self.work_db.gating_prereqs_for(&execution.work_item_id) {
            Ok(prereqs) if !prereqs.is_empty() => {
                let names = prereqs.join(", ");
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    gating_prereqs = %names,
                    "spawn_attempt: execution for gated work item — downgrading to waiting_dependency and skipping dispatch",
                );
                if let Err(err) = self.work_db.downgrade_ready_to_waiting_dependency(&execution.id) {
                    tracing::error!(
                        execution_id = %execution.id,
                        ?err,
                        "spawn_attempt: failed to downgrade gated execution",
                    );
                }
                // Emit a terminal event so the dispatch timeline advances
                // past `worker_claimed/ok` immediately and the stall
                // watchdog doesn't misattribute the hold to worker claim.
                // The execution is downgraded to `waiting_dependency` (not
                // failed); on gate clearance `dep_unblock_sweep` re-promotes
                // it to `ready` and the next kick re-dispatches.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_details(serde_json::json!({
                                "reason": "gating_prereqs_blocked",
                                "gating_prereqs": prereqs,
                            })),
                    )
                    .await;
                return Err(anyhow::anyhow!(
                    "gated: execution {} for {} blocked by [{}]",
                    execution.id,
                    execution.work_item_id,
                    names,
                ));
            }
            Ok(_) => {}
            Err(err) => {
                // Non-fatal: proceed rather than blocking all dispatches.
                // The work item's gating state is re-evaluated on the next
                // kick, so a transient DB error here at most allows one
                // erroneous dispatch.
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "spawn_attempt: gate check failed — proceeding without gating guard",
                );
            }
        }

        let work_item = match self
            .resolve_execution_work_item(execution)
            .with_context(|| format!("failed to resolve work item {}", execution.work_item_id))
        {
            Ok(work_item) => work_item,
            Err(err) => {
                // Previously a bare `?`: the execution returned to the
                // drain loop with no dispatch event and no start-failure
                // record, so it sat at `worker_claimed` until the stall
                // watchdog reaped it ~30s later. Emit a terminal
                // `host_selected:error` so the timeline names the blocker
                // and the watchdog stops re-flagging it, then record the
                // start failure so the row flips out of `worker_claimed`
                // immediately.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_error(&err)
                            .with_details(serde_json::json!({ "reason": "work_item_unresolved" })),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    ("work_item_unresolved", "Could not resolve work item for execution"),
                    &err,
                )?;
                return Err(err);
            }
        };
        let task = execution_task_summary(execution, &work_item);

        // Merge-conflict revision already-resolved guard: a revision
        // spawned to fix a merge conflict can sit `ready` for a while
        // (worker-pool contention) before a slot frees up. In that window
        // the periodic merge-poller sweep may independently notice the
        // bound PR is mergeable again and retire the linked
        // `conflict_resolutions` ledger row to `succeeded`
        // (`conflict_watch::on_resolved`) without ever touching this
        // now-unnecessary revision task/execution. Dispatching a worker
        // here would just have it discover "nothing to do" and become the
        // produce-a-PR nudge loop described in the `nudge_breaker` module
        // doc. Check the ledger (already kept fresh by that sweep) before
        // ever leasing a workspace.
        if execution.kind == ExecutionKind::RevisionImplementation
            && let WorkItem::Task(ref revision_task) = work_item
            && revision_task.kind == TaskKind::Revision
            && let Some(crz_id) = revision_task
                .created_via
                .strip_prefix(boss_protocol::CREATED_VIA_MERGE_CONFLICT_PREFIX)
        {
            match self.work_db.get_conflict_resolution(crz_id) {
                Ok(Some(ref attempt)) if attempt.status == "succeeded" => {
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        attempt_id = %attempt.id,
                        "spawn_attempt: merge-conflict revision's bound PR was already resolved \
                         before dispatch — retiring without spawning a worker",
                    );
                    match self
                        .work_db
                        .retire_stale_revision_before_dispatch(&execution.id, &revision_task.id)
                    {
                        Ok(task_transitioned) => {
                            if task_transitioned {
                                self.publisher
                                    .publish_work_item_changed(
                                        &revision_task.product_id,
                                        &revision_task.id,
                                        "merge_conflict_already_resolved",
                                    )
                                    .await;
                            }
                            self.dispatch_events
                                .emit(
                                    DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                                        .with_work_item(&execution.work_item_id)
                                        .with_worker(worker_id)
                                        .with_details(serde_json::json!({
                                            "reason": "merge_conflict_already_resolved",
                                            "attempt_id": attempt.id,
                                        })),
                                )
                                .await;
                            return Err(anyhow::anyhow!(
                                "skipped: execution {} for work_item {} not spawned — merge conflict \
                                 already resolved before dispatch (attempt {})",
                                execution.id,
                                execution.work_item_id,
                                attempt.id,
                            ));
                        }
                        Err(err) => {
                            tracing::warn!(
                                execution_id = %execution.id,
                                ?err,
                                "spawn_attempt: failed to retire stale already-resolved revision; \
                                 proceeding with dispatch",
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        execution_id = %execution.id,
                        crz_id,
                        ?err,
                        "spawn_attempt: failed to look up linked conflict_resolution; proceeding with dispatch",
                    );
                }
            }
        }

        // Host selection (distributed-execution PR3): pick the host this
        // execution should run on, then build the matching adapter (local
        // vs SSH-remote) and route the whole dispatch through it. A
        // no-eligible-host result is a recoverable pre-start failure — it
        // backs off and raises an attention item rather than hot-looping,
        // and a later kick retries once a host comes online / tags change.
        let selected_host = match self.select_host_for_execution(execution, &work_item) {
            Ok(host) => host,
            Err(err) => {
                // No event was emitted here before, so a `no_eligible_host`
                // failure was invisible in the per-execution timeline — the
                // watchdog reaped it as a `worker_claimed` stall. Emit a
                // terminal `host_selected:error` so the blocker is named in
                // dispatch.jsonl before recording the start failure.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_error(&err)
                            .with_details(serde_json::json!({ "reason": "no_eligible_host" })),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    ("no_eligible_host", "No eligible host for execution"),
                    &err,
                )?;
                return Err(err);
            }
        };
        let adapter = match self.host_adapter_provider.adapter_for(&selected_host).await {
            Ok(adapter) => adapter,
            Err(err) => {
                // Same silent-gap fix as the host-selection branch above:
                // a host was chosen but its adapter could not be built
                // (e.g. SSH unreachable). Make it observable + terminal.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_error(&err)
                            .with_details(serde_json::json!({
                                "reason": "host_adapter_unavailable",
                                "host_id": selected_host.id.clone(),
                            })),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    ("host_adapter_unavailable", "Could not build host adapter"),
                    &err,
                )?;
                return Err(err);
            }
        };
        // Host chosen and adapter ready: emit the success milestone so the
        // claimed -> repo-ensure handoff is no longer a blind spot in the
        // timeline (closes the gap that hid the automation-pool stall).
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_details(serde_json::json!({ "host_id": selected_host.id.clone() })),
            )
            .await;
        tracing::info!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            host_id = %selected_host.id,
            "host-selection: routing execution to host",
        );

        // Mirror the argv `ensure_repo` actually drives so the dispatch-event
        // `cube_command` is reproducible from a terminal: a bare resolver
        // slug goes positionally (`repo ensure <name>`), a URL via `--origin`.
        let ensure_args = crate::repo_slug::repo_ensure_args(&execution.repo_remote_url);
        // Record the attempt *before* the subprocess, mirroring
        // `cube_workspace_lease_attempted`. `cube repo ensure` on a cold
        // repo can outrun the `worker_claimed` stall threshold; with this
        // marker the watchdog attributes such a stall to the ensure
        // subprocess instead of the (already-completed) worker claim.
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeRepoEnsureAttempted, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_invocation(adapter.command_repr(&ensure_args))
                    .with_details(serde_json::json!({
                        "repo_remote_url": execution.repo_remote_url,
                        "timeout_ms": CUBE_REPO_ENSURE_TIMEOUT.as_millis() as u64,
                    })),
            )
            .await;
        let repo = match tokio::time::timeout(
            CUBE_REPO_ENSURE_TIMEOUT,
            adapter.ensure_repo(&execution.repo_remote_url),
        )
        .await
        {
            Ok(Ok(repo)) => repo,
            Ok(Err(err)) => {
                let ensure_repr = adapter.command_repr(&ensure_args);
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeRepoEnsureFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_error(&err)
                            .with_cube_invocation(ensure_repr)
                            .with_details(serde_json::json!({ "host_id": selected_host.id.clone() })),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    ("cube_repo_ensure_failed", "Cube `repo ensure` failed"),
                    &err,
                )?;
                return Err(err);
            }
            Err(_elapsed) => {
                let err = anyhow!(
                    "cube `repo ensure` timed out after {}s",
                    CUBE_REPO_ENSURE_TIMEOUT.as_secs()
                );
                let ensure_repr = adapter.command_repr(&ensure_args);
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeRepoEnsureFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_error(&err)
                            .with_cube_invocation(ensure_repr)
                            .with_details(serde_json::json!({
                                "reason": "timeout",
                                "timeout_ms": CUBE_REPO_ENSURE_TIMEOUT.as_millis() as u64,
                                "host_id": selected_host.id.clone(),
                            })),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    ("cube_repo_ensure_failed", "Cube `repo ensure` timed out"),
                    &err,
                )?;
                return Err(err);
            }
        };
        self.maybe_probe_cold_repo(execution, &adapter).await;
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeRepoEnsured, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_repo(&repo.repo_id)
                    .with_cube_invocation(adapter.command_repr(&ensure_args)),
            )
            .await;

        // PR number to pass to `cube workspace goto` after the lease.
        // Set for pr_review and revision_implementation executions that have a PR URL.
        let pr_for_goto: Option<u64> = match execution.kind {
            ExecutionKind::RevisionImplementation => execution
                .pr_url
                .as_deref()
                .and_then(boss_github::pr_url::pr_number_from_url)
                // `execution.pr_url` is not reliably stamped on every revision dispatch
                // path (e.g. orphan-sweep re-dispatch, user-initiated `bossctl work start`).
                // Fall back to the chain root's PR URL — the same authoritative lookup
                // used by completion.rs — so positioning is never skipped for revisions.
                .or_else(|| {
                    self.work_db
                        .get_revision_chain_root_pr_url(&execution.work_item_id)
                        .as_deref()
                        .and_then(boss_github::pr_url::pr_number_from_url)
                }),
            ExecutionKind::PrReview => match &work_item {
                WorkItem::Task(task) | WorkItem::Chore(task) => task
                    .pr_url
                    .as_deref()
                    .filter(|u| !u.is_empty())
                    .and_then(boss_github::pr_url::pr_number_from_url),
                _ => None,
            },
            _ => None,
        };

        let lease = match self
            .lease_workspace_with_fallback(execution, worker_id, &repo, &task, &adapter)
            .await
        {
            Ok(lease) => lease,
            Err(err) => {
                // The lease helper has already emitted attempt /
                // failure events for every try; convert the final
                // failure into the start-failure record so the
                // execution row flips to `failed` cleanly instead of
                // wedging in `worker_claimed`.
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    Some(repo.repo_id.as_str()),
                    ("cube_workspace_lease_failed", "Cube `workspace lease` failed"),
                    &err,
                )?;
                return Err(err);
            }
        };

        // Lease-time occupancy guard (defect 3). Cube should never hand us
        // a workspace that is still the cwd of a live worker — but the
        // duplicate-dispatch incident proved it can when an upstream bug
        // frees a lease while the worker's process is still alive. Before
        // we commit this run to the workspace, refuse (and loudly log) if
        // the engine's own live-worker registry still tracks a live
        // process there. A refused lease retries via the normal pre-start
        // backoff; an interleaved working copy silently corrupts two
        // workers' edits. Only runs when the registry is wired
        // (production); fails open otherwise. The probe is keyed by
        // run_id/execution_id, so it never trips on our own (not-yet-
        // spawned) execution.
        if let Some(live_states) = self.live_worker_states.as_ref() {
            let snapshot = live_states.snapshot();
            let occupant = occupying_live_worker(
                &lease.workspace_id,
                &execution.id,
                &snapshot,
                |eid| self.work_db.get_execution(eid).ok().and_then(|e| e.cube_workspace_id),
                |pid| {
                    !matches!(
                        crate::dead_pid_sweep::probe_pid(pid),
                        crate::dead_pid_sweep::PidStatus::Dead
                    )
                },
            );
            if let Some(occupant_run_id) = occupant {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_workspace_id = %lease.workspace_id,
                    workspace_path = %lease.workspace_path.display(),
                    occupied_by = %occupant_run_id,
                    "REFUSING lease: cube returned a workspace still occupied by a live tracked worker \
                     — refusing rather than interleaving two workers in one working copy (defect 3)",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(serde_json::json!({
                                "reason": "workspace_occupied_by_live_worker",
                                "occupied_by_execution_id": occupant_run_id,
                                "workspace_path": lease.workspace_path.display().to_string(),
                            })),
                    )
                    .await;
                // Record this workspace as refused so the next lease call
                // passes --exclude and skips it, breaking the livelock where
                // cube's deterministic ordering re-offers the same occupied
                // workspace on every retry attempt.
                self.refused_workspaces
                    .lock()
                    .await
                    .entry(execution.id.clone())
                    .or_default()
                    .push(lease.workspace_id.clone());

                // Hand the workspace straight back so it isn't stranded.
                if let Err(release_err) = adapter.release_workspace(&lease.lease_id).await {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after refusing an occupied lease",
                    );
                }
                let err = anyhow!(
                    "leased workspace {} is occupied by live worker {}",
                    lease.workspace_id,
                    occupant_run_id
                );
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    Some(repo.repo_id.as_str()),
                    (
                        "cube_workspace_occupied",
                        "Cube leased a workspace occupied by a live worker",
                    ),
                    &err,
                )?;
                return Err(err);
            }
        }

        // Per-PR single-writer assertion (defense in depth). The pre-claim
        // chain guard at the top of `schedule_execution` already deferred
        // any execution whose PR/chain has a live sibling, but there is a
        // TOCTOU window between that check and committing this run to the
        // leased workspace: a sibling could have gone live in between. We
        // re-assert the invariant HERE, immediately before spawning onto the
        // shared jj backing store — the irreversible step. The occupancy
        // guard above only catches a sibling in the SAME workspace; two
        // same-PR workers in DIFFERENT cube workspaces still share one
        // backing store and corrupt each other, which this catches. On a
        // violation we release the lease and refuse rather than interleave.
        //
        // Goes through `resolve_chain_hold` for the same reason as the other
        // two call sites: a `waiting_human` "sibling" that is actually a
        // dead worker must not refuse this spawn forever, and a live
        // `pr_review` sibling must not refuse a merge-conflict revision the
        // earlier checkpoints already bypassed for it (see
        // `resolve_chain_hold`'s docs) — otherwise this defense-in-depth
        // assertion would defeat the bypass right before the irreversible
        // spawn step.
        match self.resolve_chain_hold(execution).await {
            Ok(ChainHold::Blocked {
                sibling, review_held, ..
            }) => {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_workspace_id = %lease.workspace_id,
                    live_sibling_execution_id = %sibling.id,
                    live_sibling_work_item_id = %sibling.work_item_id,
                    review_held,
                    "REFUSING spawn: another execution on the same PR/chain went live after the \
                     pre-claim guard — refusing rather than handing two same-PR workers the shared \
                     jj backing store (single-writer invariant)",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(serde_json::json!({
                                "reason": "chain_sibling_went_live",
                                "review_held": review_held,
                                "live_sibling_execution_id": sibling.id,
                                "live_sibling_work_item_id": sibling.work_item_id,
                            })),
                    )
                    .await;
                // Hand the workspace back so it isn't stranded, then leave
                // the execution `ready` (the deferral path) so it re-attempts
                // once the sibling reaps.
                if let Err(release_err) = adapter.release_workspace(&lease.lease_id).await {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after refusing a chain-sibling-racing spawn",
                    );
                }
                return Err(anyhow!(
                    "serialized: execution {} for work_item {} refused after lease — chain sibling {} (work_item {}) went live",
                    execution.id,
                    execution.work_item_id,
                    sibling.id,
                    sibling.work_item_id,
                ));
            }
            Ok(ChainHold::ReviewBypassed(_)) | Ok(ChainHold::Clear) => {}
            Err(err) => {
                // Fail open: a DB error here must not wedge dispatch. The
                // pre-claim guard already covered the common case.
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "spawn_attempt: post-lease chain single-writer assertion failed to query — proceeding",
                );
            }
        }
        {
            let mut lease_args = vec![
                "--json",
                "workspace",
                "lease",
                repo.repo_id.as_str(),
                "--task",
                task.as_str(),
                // Mirror the flag the actual lease call passes (see
                // `cube_commands::lease_workspace`) so this diagnostic repr
                // reproduces exactly what ran.
                "--release-on-setup-failure",
            ];
            if let Some(p) = execution.preferred_workspace_id.as_deref() {
                lease_args.extend_from_slice(&["--prefer", p]);
            }
            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::CubeWorkspaceLeased, DispatchOutcome::Ok, &execution.id)
                        .with_work_item(&execution.work_item_id)
                        .with_worker(worker_id)
                        .with_cube_repo(&repo.repo_id)
                        .with_cube_lease(&lease.lease_id)
                        .with_cube_workspace(&lease.workspace_id)
                        .with_cube_invocation(adapter.command_repr(&lease_args)),
                )
                .await;
        }
        let change_title = execution_change_title(execution, &work_item);

        // For PR-targeting executions, run `cube workspace goto --workspace <path>
        // --pr <n>` AFTER the lease to position the working copy on the PR branch
        // head. This must happen before handing the workspace to the worker.
        // If positioning fails, abort dispatch with a diagnosable stage.
        if let Some(pr) = pr_for_goto {
            let goto_repr = adapter.command_repr(&[
                "--json",
                "workspace",
                "goto",
                "--workspace",
                &lease.workspace_path.display().to_string(),
                "--pr",
                &pr.to_string(),
            ]);
            match adapter.goto_workspace(&lease.workspace_path, pr).await {
                Ok(()) => {
                    tracing::info!(
                        execution_id = %execution.id,
                        kind = execution.kind.as_str(),
                        workspace_path = %lease.workspace_path.display(),
                        pr,
                        "workspace positioned via cube workspace goto",
                    );
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::CubeWorkspacePositioned, DispatchOutcome::Ok, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_worker(worker_id)
                                .with_cube_repo(&repo.repo_id)
                                .with_cube_lease(&lease.lease_id)
                                .with_cube_workspace(&lease.workspace_id)
                                .with_cube_invocation(goto_repr)
                                .with_details(serde_json::json!({
                                    "pr": pr,
                                    "kind": execution.kind.as_str(),
                                })),
                        )
                        .await;
                }
                Err(err) => {
                    if let Err(release_err) = adapter.release_workspace(&lease.lease_id).await {
                        tracing::error!(
                            ?release_err,
                            lease_id = %lease.lease_id,
                            "failed to release workspace after goto positioning failure"
                        );
                    }
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(
                                Stage::CubeWorkspacePositioningFailed,
                                DispatchOutcome::Error,
                                &execution.id,
                            )
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_error(&err)
                            .with_cube_invocation(goto_repr),
                        )
                        .await;
                    self.record_start_failure(
                        Arc::clone(self),
                        execution,
                        worker_id,
                        Some(repo.repo_id.as_str()),
                        (
                            "cube_workspace_positioning_failed",
                            "Cube `workspace goto` positioning failed",
                        ),
                        &err,
                    )?;
                    return Err(err);
                }
            }
        }

        // For PR-targeting executions the workspace is now positioned on the PR
        // head — skip create_change (there is nothing to create; the worker edits
        // or reviews the branch directly). For all other executions create a fresh
        // jj change via `cube change create`.
        let change: Option<CubeChangeHandle> = if pr_for_goto.is_some() {
            None
        } else {
            // Normal path (pr_review without a PR URL, and all non-review/
            // non-revision executions): create a fresh jj change via `cube
            // change create`.
            let workspace_path_str = lease.workspace_path.display().to_string();
            let change_repr: Option<(String, String)> = adapter.command_repr(&[
                "--json",
                "change",
                "create",
                "--workspace",
                &workspace_path_str,
                "--title",
                &change_title,
            ]);
            match adapter.create_change(&lease.workspace_path, &change_title).await {
                Ok(change) => {
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::CubeChangeCreated, DispatchOutcome::Ok, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_worker(worker_id)
                                .with_cube_repo(&repo.repo_id)
                                .with_cube_lease(&lease.lease_id)
                                .with_cube_workspace(&lease.workspace_id)
                                .with_cube_invocation(change_repr)
                                .with_details(serde_json::json!({
                                    "change_id": change.change_id,
                                    "change_title": change_title,
                                })),
                        )
                        .await;
                    Some(change)
                }
                Err(err) => {
                    if let Err(release_err) = adapter.release_workspace(&lease.lease_id).await {
                        tracing::error!(
                            ?release_err,
                            lease_id = %lease.lease_id,
                            "failed to release workspace after change creation failure"
                        );
                    }
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::CubeChangeCreated, DispatchOutcome::Error, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_worker(worker_id)
                                .with_cube_repo(&repo.repo_id)
                                .with_cube_lease(&lease.lease_id)
                                .with_cube_workspace(&lease.workspace_id)
                                .with_error(&err)
                                .with_cube_invocation(change_repr.clone()),
                        )
                        .await;
                    self.record_start_failure(
                        Arc::clone(self),
                        execution,
                        worker_id,
                        Some(repo.repo_id.as_str()),
                        ("cube_change_create_failed", "Cube `change create` failed"),
                        &err,
                    )?;
                    return Err(err);
                }
            }
        };

        match self.work_db.start_execution_run_on_host(
            &execution.id,
            worker_id,
            &repo.repo_id,
            &lease.lease_id,
            &lease.workspace_id,
            &lease.workspace_path.display().to_string(),
            &selected_host.id,
        ) {
            Ok((execution, run)) => {
                let worker_id_owned = worker_id.to_owned();
                tracing::info!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    cube_lease_id = %lease.lease_id,
                    cube_workspace_id = %lease.workspace_id,
                    cube_change_id = ?change.as_ref().map(|c| &c.change_id),
                    workspace_path = %lease.workspace_path.display(),
                    "started execution run"
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::RunStarted, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(serde_json::json!({
                                "run_id": run.id,
                            })),
                    )
                    .await;
                self.publisher
                    .publish(
                        &execution.id,
                        &execution.work_item_id,
                        execution.status.as_str(),
                        "execution_started",
                    )
                    .await;
                // Auto-advance bumped `tasks.status` to `'active'`
                // inside the same transaction. Broadcast a work-tree
                // invalidation so kanban subscribers re-fetch and
                // move the card to the Doing column.
                if let Ok(work_item) = self.resolve_execution_work_item(&execution) {
                    self.publisher
                        .publish_work_item_changed(
                            &work_item_product_id(&work_item),
                            &execution.work_item_id,
                            "execution_started_auto_advance",
                        )
                        .await;
                }
                // For automation triage executions, advance the
                // automation_runs row from its queued/pessimistic state
                // (`pool_throttled` or `failed_will_retry`) to
                // `triage_running` now that a pool slot is held and the
                // agent is about to start. The completion handler will
                // overwrite this with the terminal outcome.
                if execution.kind == ExecutionKind::AutomationTriage
                    && let Err(err) = self.work_db.mark_automation_run_triage_started(&execution.id)
                {
                    tracing::warn!(
                        execution_id = %execution.id,
                        ?err,
                        "failed to mark automation run triage_running on start",
                    );
                }
                // Resume-bounce SHA-delta gate: capture the bound
                // chore PR's head SHA into the execution row BEFORE
                // the worker spawns and starts pushing. The Stop
                // boundary uses this snapshot to decide whether the
                // run contributed to the bound PR. Best-effort: the
                // hook logs and swallows every failure mode (no
                // bound PR, slug/number parse failure, GitHub fetch
                // failure), and the gate treats a missing snapshot
                // as "inapplicable" — never noisier than the
                // pre-change behaviour.
                self.execution_started_hook.on_execution_started(&execution.id).await;
                let coordinator = self.clone();
                tokio::spawn(async move {
                    coordinator
                        .run_execution(execution, run, work_item, worker_id_owned, lease, change, adapter)
                        .await;
                });
                Ok(())
            }
            Err(err) => {
                let release_result = adapter.release_workspace(&lease.lease_id).await;
                if let Err(release_err) = release_result {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after run start failure"
                    );
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::RunStarted, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_error(&err),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    Some(repo.repo_id.as_str()),
                    ("execution_run_start_failed", "`start_execution_run` failed"),
                    &err,
                )?;
                Err(err)
            }
        }
    }

    /// Cold-repo probe (design doc Q6, Follow-up chore #8). The first
    /// time a given repo URL flows through `ensure_repo` in this
    /// engine's lifetime, ask cube `repo list --json` once and check
    /// whether the entry for this URL is sitting on cube's
    /// auto-provisioned defaults — i.e. nothing was customised with
    /// `cube repo add` / `cube repo configure`. If so, raise an
    /// advisory `repo_cold_pool` `WorkAttentionItem` against the
    /// execution naming the exact override command.
    ///
    /// Best-effort by design: never blocks dispatch, never returns an
    /// error to the caller. A failed `list_repos` round-trip is logged
    /// at WARN and the URL is still marked seen so we don't retry the
    /// probe every dispatch — engine restart re-probes per R4.
    async fn maybe_probe_cold_repo(self: &Arc<Self>, execution: &WorkExecution, adapter: &Arc<dyn HostAdapter>) {
        let origin = execution.repo_remote_url.clone();
        {
            let mut seen = self.repo_cold_probe_seen.lock().await;
            if !seen.insert(origin.clone()) {
                return;
            }
        }

        let repos = match adapter.list_repos().await {
            Ok(repos) => repos,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    execution_id = %execution.id,
                    repo_remote_url = %origin,
                    "cold-repo probe: `cube repo list` failed — skipping advisory check"
                );
                return;
            }
        };

        let Some(repo) = repos.iter().find(|r| r.origin == origin) else {
            tracing::debug!(
                execution_id = %execution.id,
                repo_remote_url = %origin,
                "cold-repo probe: ensured repo not present in `cube repo list` snapshot"
            );
            return;
        };

        if !repo_has_default_pool_config(repo) {
            return;
        }

        let title = format!(
            "Cold cube pool for `{repo_id}` — using auto-provisioned defaults",
            repo_id = repo.repo_id,
        );
        let body = cold_repo_attention_body(repo);
        let input = CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: "repo_cold_pool".to_owned(),
            status: None,
            title,
            body_markdown: body,
            resolved_at: None,
        };
        match self.work_db.create_attention_item(input) {
            Ok(item) => {
                tracing::info!(
                    attention_id = %item.id,
                    execution_id = %execution.id,
                    repo_id = %repo.repo_id,
                    repo_remote_url = %origin,
                    "cold-repo probe: raised advisory attention item"
                );
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    execution_id = %execution.id,
                    repo_id = %repo.repo_id,
                    repo_remote_url = %origin,
                    "cold-repo probe: failed to persist attention item — dispatch continues"
                );
            }
        }
    }

    /// Reclaim a stale cube lease still held against `workspace_id` by a
    /// dead (now-terminal) execution, so a hard-prefer resume can
    /// re-lease that exact workspace and recover the in-flight jj
    /// checkout. See [`crate::work::WorkDb::stale_lease_to_reclaim_for_workspace`]
    /// and issue #962 for the full rationale.
    ///
    /// Best-effort: probes cube's live view (`list_workspaces`) for the
    /// lease currently bound to `workspace_id`, cross-checks it against
    /// the engine's own record (only a lease whose owning execution is
    /// terminal and unclaimed is eligible), and force-releases it. Every
    /// failure mode is logged and swallowed — the caller proceeds to the
    /// normal lease attempt regardless, so a flaky cube probe never
    /// blocks a resume.
    async fn reclaim_stale_lease_for_resume(
        &self,
        execution: &WorkExecution,
        worker_id: &str,
        workspace_id: &str,
        adapter: &Arc<dyn HostAdapter>,
    ) {
        let snapshot = match adapter.list_workspaces().await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    workspace_id,
                    error = format!("{err:#}"),
                    "stale-lease reclaim: cube workspace list failed; proceeding to lease without reclaim",
                );
                return;
            }
        };
        let Some(workspace) = snapshot.iter().find(|w| w.workspace_id == workspace_id) else {
            // Cube doesn't list the workspace, or it's already free —
            // nothing to reclaim, the lease attempt can proceed.
            return;
        };
        if workspace.state != "leased" {
            return;
        }
        let Some(current_lease_id) = workspace.lease_id.as_deref() else {
            return;
        };

        // Only reclaim a lease the engine can prove belongs to a dead
        // (terminal, unclaimed) execution for this workspace.
        let stale_lease_id = match self
            .work_db
            .stale_lease_to_reclaim_for_workspace(workspace_id, current_lease_id)
        {
            Ok(Some(id)) => id,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    workspace_id,
                    current_lease_id,
                    ?err,
                    "stale-lease reclaim: DB lookup failed; proceeding to lease without reclaim",
                );
                return;
            }
        };

        let reason = format!(
            "boss engine: reclaiming stale lease for UI-crash resume of execution {} (workspace {workspace_id})",
            execution.id,
        );
        match adapter
            .force_release_lease(&stale_lease_id, Some(reason.as_str()))
            .await
        {
            Ok(()) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    worker_id,
                    workspace_id,
                    reclaimed_lease_id = %stale_lease_id,
                    "stale-lease reclaim: force-released dead worker's lease so resume can re-lease its workspace",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_details(serde_json::json!({
                                "step": "stale_lease_reclaim",
                                "workspace_id": workspace_id,
                                "reclaimed_lease_id": stale_lease_id.as_str(),
                            })),
                    )
                    .await;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    worker_id,
                    workspace_id,
                    stale_lease_id = %stale_lease_id,
                    error = format!("{err:#}"),
                    "stale-lease reclaim: force-release failed; proceeding to lease attempt anyway",
                );
            }
        }
    }

    /// Lease a cube workspace for `execution`, emitting a structured
    /// attempt/failure event for every try and falling back to "any
    /// free workspace" when an unprefixed lease fails.
    ///
    /// Behaviour matrix:
    ///
    /// | preferred set? | first attempt      | on first failure                          |
    /// |----------------|--------------------|-------------------------------------------|
    /// | no             | without `--prefer` | retry once without `--prefer` (`any_free`) |
    /// | yes            | with `--prefer`    | terminal failure (preserves continuity)   |
    ///
    /// When `preferred_workspace_id` is set the caller needs a specific
    /// workspace (e.g. resuming a prior run). Silently landing elsewhere
    /// would lose state continuity, so we fail fast and let the scheduler
    /// retry the dispatch later. When no preference is set any free
    /// workspace is acceptable, so a single bad workspace cannot block
    /// the entire dispatch.
    ///
    /// Each subprocess invocation is bounded by [`CUBE_LEASE_TIMEOUT`]
    /// so the engine cannot wedge indefinitely waiting on cube — the
    /// motivating incident sat in `worker_claimed/ok` for ~46s with
    /// no event because the cube call never returned.
    async fn lease_workspace_with_fallback(
        &self,
        execution: &WorkExecution,
        worker_id: &str,
        repo: &CubeRepoHandle,
        task: &str,
        adapter: &Arc<dyn HostAdapter>,
    ) -> Result<CubeWorkspaceLease> {
        let prefer = execution.preferred_workspace_id.as_deref();
        let allow_dirty = execution.allow_dirty;
        // Soft-prefer (OQ5): revision_implementation executions set
        // prefer_is_soft = true so a missing or leased preferred workspace
        // degrades silently to any free workspace rather than failing hard.
        // Orphan-resume executions use the hard "none" policy (prefer_is_soft
        // = false) because their state lives only in that specific workspace.
        // allow_dirty additionally suppresses the cube-side reset so the
        // recovering worker lands on the dirty tree; it implies a hard-fail
        // (no fallback) because the uncommitted work is only in that workspace.
        let fallback_policy = if prefer.is_none() || execution.prefer_is_soft {
            "any_free"
        } else {
            "none"
        };

        // Look up any workspaces that were refused for this execution by the
        // occupancy guard on a previous dispatch attempt. Passing them as
        // `--exclude` to cube breaks the livelock where cube's deterministic
        // candidate ordering keeps re-offering the same occupied workspace.
        let refused: Vec<String> = self
            .refused_workspaces
            .lock()
            .await
            .get(&execution.id)
            .cloned()
            .unwrap_or_default();
        let refused_refs: Vec<&str> = refused.iter().map(|s| s.as_str()).collect();

        // Stale-lease reclaim (issue #962 — UI-crash resume).
        //
        // A hard-prefer resume targets the exact workspace the dead
        // worker was leased into, because the in-flight jj checkout the
        // human wants recovered lives only there. But after a UI crash
        // the dead execution's cube lease is intentionally left intact
        // (the startup reaper preserves it), so cube still reports that
        // workspace as `leased` and will refuse a fresh
        // `--prefer <workspace>` lease — failing the resume outright and
        // stranding the local work. Before attempting the prefer lease,
        // reclaim the dead lease if (and only if) the engine can prove
        // it belongs to a now-terminal execution and no live execution
        // claims the workspace. Best-effort: any probe/reclaim error is
        // logged and we fall through to the normal lease attempt rather
        // than blocking the resume.
        if let Some(workspace_id) = prefer.filter(|_| !execution.prefer_is_soft) {
            self.reclaim_stale_lease_for_resume(execution, worker_id, workspace_id, adapter)
                .await;
        }

        // Build the lease args for attempt 1 so we can attach the
        // exact command to both the attempted and failed events.
        let mut attempt1_args = vec![
            "--json",
            "workspace",
            "lease",
            repo.repo_id.as_str(),
            "--task",
            task,
            "--release-on-setup-failure",
        ];
        if let Some(p) = prefer {
            attempt1_args.extend_from_slice(&["--prefer", p]);
        }
        if allow_dirty {
            attempt1_args.push("--allow-dirty");
        }
        for excluded in &refused_refs {
            attempt1_args.extend_from_slice(&["--exclude", excluded]);
        }
        let attempt1_repr = adapter.command_repr(&attempt1_args);

        // First attempt: use the preferred workspace if the caller
        // pinned one. Emit `cube_workspace_lease_attempted` *before*
        // the subprocess so the timeline shows what we tried even
        // when cube hangs and never returns.
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_repo(&repo.repo_id)
                    .with_cube_invocation(attempt1_repr.clone())
                    .with_details(serde_json::json!({
                        "attempt": 1,
                        "prefer_workspace_id": prefer,
                        "fallback_policy": fallback_policy,
                        "allow_dirty": allow_dirty,
                        "timeout_ms": CUBE_LEASE_TIMEOUT.as_millis() as u64,
                        "excluded_workspace_ids": refused,
                    })),
            )
            .await;

        CUBE_WORKSPACE_LEASE_ATTEMPTS.inc(&self.metrics);
        let first_err = match self
            .invoke_lease(
                repo,
                task,
                (prefer, allow_dirty),
                CUBE_LEASE_TIMEOUT,
                adapter,
                &refused_refs,
            )
            .await
        {
            Ok(lease) => {
                CUBE_WORKSPACE_LEASE_SUCCESS.inc(&self.metrics);
                return Ok(lease);
            }
            Err((reason, err)) => {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    prefer = ?prefer,
                    allow_dirty,
                    reason,
                    error = format!("{err:#}"),
                    "cube workspace lease attempt failed"
                );
                let mut details = serde_json::json!({
                    "attempt": 1,
                    "prefer_workspace_id": prefer,
                    "reason": reason,
                    "fallback_policy": fallback_policy,
                    "allow_dirty": allow_dirty,
                    "excluded_workspace_ids": refused,
                });
                augment_details_with_cube_cli_error(&mut details, &err);
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_error(&err)
                            .with_cube_invocation(attempt1_repr)
                            .with_details(details),
                    )
                    .await;
                err
            }
        };

        // Fallback only kicks in when the first attempt had no workspace
        // preference, OR when prefer_is_soft is true (revision_implementation
        // uses a soft prefer for cache warmth only — losing the preferred
        // workspace is a non-event, not a continuity failure).
        // With a hard prefer (prefer set + prefer_is_soft = false), the
        // caller needs that specific workspace (orphan-resume); silently
        // landing elsewhere would lose local commit state.
        // allow_dirty additionally implies hard-fail: the uncommitted patch
        // lives only in the named workspace, so landing elsewhere is
        // meaningless and must surface an error rather than silently
        // dispatching to a clean workspace.
        if prefer.is_some() && (!execution.prefer_is_soft || allow_dirty) {
            CUBE_WORKSPACE_LEASE_FAILURE.inc(&self.metrics);
            return Err(first_err);
        }

        let mut attempt2_args = vec![
            "--json",
            "workspace",
            "lease",
            repo.repo_id.as_str(),
            "--task",
            task,
            "--release-on-setup-failure",
        ];
        for excluded in &refused_refs {
            attempt2_args.extend_from_slice(&["--exclude", excluded]);
        }
        let attempt2_repr = adapter.command_repr(&attempt2_args);

        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_repo(&repo.repo_id)
                    .with_cube_invocation(attempt2_repr.clone())
                    .with_details(serde_json::json!({
                        "attempt": 2,
                        "prefer_workspace_id": serde_json::Value::Null,
                        "fallback_policy": "none",
                        "timeout_ms": CUBE_LEASE_TIMEOUT.as_millis() as u64,
                        "fallback_from_prefer": prefer,
                        "excluded_workspace_ids": refused,
                    })),
            )
            .await;

        CUBE_WORKSPACE_LEASE_ATTEMPTS.inc(&self.metrics);
        match self
            .invoke_lease(repo, task, (None, false), CUBE_LEASE_TIMEOUT, adapter, &refused_refs)
            .await
        {
            Ok(lease) => {
                CUBE_WORKSPACE_LEASE_SUCCESS.inc(&self.metrics);
                Ok(lease)
            }
            Err((reason, err)) => {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    reason,
                    error = format!("{err:#}"),
                    "cube workspace lease fallback also failed"
                );
                let mut details = serde_json::json!({
                    "attempt": 2,
                    "prefer_workspace_id": serde_json::Value::Null,
                    "reason": reason,
                    "fallback_policy": "none",
                    "fallback_from_prefer": prefer,
                    "excluded_workspace_ids": refused,
                });
                augment_details_with_cube_cli_error(&mut details, &err);
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_error(&err)
                            .with_cube_invocation(attempt2_repr)
                            .with_details(details),
                    )
                    .await;
                CUBE_WORKSPACE_LEASE_FAILURE.inc(&self.metrics);
                Err(err)
            }
        }
    }

    /// Run one `cube workspace lease` invocation under
    /// [`CUBE_LEASE_TIMEOUT`]. Returns `(reason, error)` so the caller
    /// can label the dispatch event with `"timeout"` vs `"cube_error"`
    /// without re-parsing the message.
    async fn invoke_lease(
        &self,
        repo: &CubeRepoHandle,
        task: &str,
        // (prefer_workspace_id, allow_dirty) — bundled to keep the
        // parameter count under clippy::too_many_arguments.
        lease_opts: (Option<&str>, bool),
        timeout: Duration,
        adapter: &Arc<dyn HostAdapter>,
        exclude_workspace_ids: &[&str],
    ) -> std::result::Result<CubeWorkspaceLease, (&'static str, anyhow::Error)> {
        let (prefer_workspace_id, allow_dirty) = lease_opts;
        match tokio::time::timeout(
            timeout,
            adapter.lease_workspace(
                &repo.repo_id,
                task,
                prefer_workspace_id,
                allow_dirty,
                exclude_workspace_ids,
            ),
        )
        .await
        {
            Ok(Ok(lease)) => Ok(lease),
            Ok(Err(err)) => Err(("cube_error", err)),
            Err(_elapsed) => Err((
                "timeout",
                anyhow!("cube workspace lease timed out after {}s", timeout.as_secs()),
            )),
        }
    }

    /// Record a pre-start failure and either schedule an automatic retry
    /// or surface a permanent failure to the operator.
    ///
    /// Safe-to-retry stages (no worker side effects yet):
    /// `cube_repo_ensure`, `workspace_lease`, `change_create`,
    /// `run_start` (DB-only failure, transaction rolled back).
    ///
    /// Do NOT call this for post-`run_started` failures — those require
    /// `finish_execution_run`.
    fn record_start_failure(
        &self,
        coordinator: Arc<ExecutionCoordinator>,
        execution: &WorkExecution,
        worker_id: &str,
        cube_repo_id: Option<&str>,
        // (attention_kind, attention_title) — bundled to keep the
        // parameter count under clippy::too_many_arguments.
        attention: (&str, &str),
        error: &anyhow::Error,
    ) -> Result<()> {
        let (attention_kind, attention_title) = attention;
        let (execution, run, outcome) = self.work_db.record_pre_start_failure(
            &execution.id,
            worker_id,
            cube_repo_id,
            &error.to_string(),
            &self.pre_start_retry_delays,
        )?;

        match outcome {
            PreStartFailureOutcome::Retry { delay } => {
                tracing::info!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    pre_start_failure_count = execution.pre_start_failure_count,
                    max_retries = self.pre_start_retry_delays.len(),
                    delay_secs = delay.as_secs(),
                    "pre-start failure will retry after backoff"
                );
                // After the backoff window expires, promote the execution
                // back into the ready queue and wake the scheduler. Until
                // then `dispatch_not_before` keeps it invisible to
                // `list_ready_executions`.
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    coordinator.kick();
                });
            }
            PreStartFailureOutcome::PermanentFail => {
                tracing::warn!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    pre_start_failure_count = execution.pre_start_failure_count,
                    error = %error,
                    "recorded execution start failure"
                );

                // Maint task 6 — transient-retry wiring on `dispatch_not_before`:
                // an `automation_triage` execution that exhausts its pre-start
                // retries is the design's `failed_gave_up` terminal state.
                // Finalise the matching `automation_runs` row so the Automations
                // tab shows the occurrence was abandoned (the schedule already
                // advanced past it when the scheduler fired the triage). Until
                // this point the run sat at the pessimistic `failed_will_retry`.
                if execution.kind == ExecutionKind::AutomationTriage
                    && let Err(err) = self.work_db.finalize_automation_triage_run(
                        &execution.id,
                        boss_protocol::AUTOMATION_OUTCOME_FAILED_GAVE_UP,
                        None,
                        Some(&format!(
                            "triage pre-start failed permanently after {} attempt(s): {error}",
                            execution.pre_start_failure_count
                        )),
                    )
                {
                    tracing::warn!(
                        execution_id = %execution.id,
                        ?err,
                        "failed to mark automation run failed_gave_up after permanent triage pre-start failure",
                    );
                }

                // Surface every permanent pre-start failure as a
                // `WorkAttentionItem` so the failure is diagnosable in one
                // bossctl call instead of needing a tracing-log tail.
                let err = format!("{error:#}");
                let attention_body = format!(
                    "Execution `{execution_id}` could not start on worker `{worker_id}` \
                     after {attempts} attempt(s).\n\n\
                     **Error:** {err}\n\n\
                     Inspect `dispatch-events/executions/{execution_id}/dispatch.jsonl` \
                     for the full stage timeline.",
                    execution_id = execution.id,
                    attempts = execution.pre_start_failure_count,
                );
                if let Err(attention_err) = self.work_db.create_attention_item(CreateAttentionItemInput {
                    execution_id: Some(execution.id.clone()),
                    work_item_id: None,
                    kind: attention_kind.to_owned(),
                    status: None,
                    title: attention_title.to_owned(),
                    body_markdown: attention_body,
                    resolved_at: None,
                }) {
                    tracing::error!(
                        ?attention_err,
                        execution_id = %execution.id,
                        "failed to record attention item for execution start failure",
                    );
                }

                // Stop the silent claim → fail → release → re-queue loop
                // (the "waiting for a slot" vs. "failing to start"
                // ambiguity): bounce the work item to Backlog with
                // `autostart` cleared and the failure reason/error stamped
                // directly on the row so the kanban card renders it
                // inline. Guarded on `status IN ('todo', 'active')`, so
                // this is a no-op for review-phase dispatch kinds
                // (`pr_review`, `ci_remediation`, `conflict_resolution`)
                // whose work item sits in `in_review`/`blocked` — bouncing
                // those would erase review context.
                match self
                    .work_db
                    .bounce_dispatch_failed_to_backlog(&execution.work_item_id, attention_kind, &err)
                {
                    Ok(true) => tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        reason = attention_kind,
                        "bounced work item to backlog after permanent pre-start dispatch failure",
                    ),
                    Ok(false) => {}
                    Err(bounce_err) => tracing::error!(
                        ?bounce_err,
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        "failed to bounce work item to backlog after permanent pre-start dispatch failure",
                    ),
                }

                let publisher = self.publisher.clone();
                let execution_id = execution.id.clone();
                let work_item_id = execution.work_item_id.clone();
                let status_str = execution.status.as_str();
                let product_id = match self.work_db.get_work_item(&work_item_id) {
                    Ok(item) => Some(work_item_product_id(&item)),
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            %work_item_id,
                            "failed to resolve product for runtime broadcast"
                        );
                        None
                    }
                };
                tokio::spawn(async move {
                    publisher
                        .publish(&execution_id, &work_item_id, status_str, "execution_start_failed")
                        .await;
                    if let Some(product_id) = product_id {
                        publisher
                            .publish_work_item_changed(&product_id, &work_item_id, "execution_start_failed")
                            .await;
                    }
                });
            }
        }
        Ok(())
    }

    // `change` is `None` for `pr_review` executions that checked out the PR
    // head directly; `Some` for all other executions that created a jj change.
    #[allow(clippy::too_many_arguments)]
    async fn run_execution(
        self: Arc<Self>,
        execution: WorkExecution,
        run: WorkRun,
        work_item: WorkItem,
        worker_id: String,
        lease: CubeWorkspaceLease,
        change: Option<CubeChangeHandle>,
        adapter: Arc<dyn HostAdapter>,
    ) {
        // Keep the cube lease alive for blocking runners (in-process test
        // fakes, ACP-style runners). For pane-spawn (the production path),
        // spawn_worker returns immediately and this guard is dropped below
        // without ever firing — pane workers are covered instead by the
        // engine-wide periodic sweep in `crate::cube_lease_heartbeat`.
        // See the HeartbeatGuard doc comment for the full coverage split.
        let heartbeat = HeartbeatGuard::spawn(
            Arc::clone(&adapter),
            lease.lease_id.clone(),
            execution.id.clone(),
            run.id.clone(),
            worker_id.clone(),
        );

        // Pre-spawn: collect the merge-tree diagnosis for revision_implementation
        // executions with merge-conflict provenance so compose_revision_directive
        // injects it into the worker prompt. No-op for other provenance.
        if execution.kind == ExecutionKind::RevisionImplementation {
            self.collect_revision_conflict_diagnosis_pre_spawn(&execution, &work_item, &lease)
                .await;
        }

        let run_outcome = adapter
            .spawn_worker(
                &worker_id,
                &execution,
                &work_item,
                lease.workspace_path.as_path(),
                change.as_ref().map(|c| c.change_id.as_str()),
            )
            .await;
        drop(heartbeat);

        // Pane-spawn runs hand the slot to a live libghostty pane; the
        // WorkerPool slot must remain claimed until that pane is torn
        // down by `ServerState::release_worker_pane` (completion, force
        // release, or engine shutdown). Releasing it here would let a
        // concurrent dispatch re-claim the same slot while the pane
        // still owns it, and the app would reject `SpawnWorkerPane`
        // with `SlotBusy`. Non-pane runs (test fakes, future
        // ACP-style runners) leave `slot_id = None` and still need
        // the inline release.
        let defer_pool_slot_release = matches!(
            run_outcome.as_ref(),
            Ok(outcome) if outcome.slot_id.is_some()
        );

        match run_outcome {
            // Mid-spawn cancel (T981): the worker was cancelled while it
            // was still spawning. The runner has already reaped the
            // just-spawned pane; our job is to release the cube lease the
            // cancel path deliberately left held (so a still-occupied
            // workspace was never handed back to cube) and to skip the
            // normal completion recording — the row is already
            // `cancelled`, so `finish_execution_run` would reject it.
            Ok(outcome) if outcome.wait_state == RunWaitState::CancelledDuringSpawn => {
                // Claim ownership of the lease atomically before calling
                // cube, mirroring `force_release`: whichever path clears
                // the workspace columns first owns the release, so a
                // concurrent `force_release` and this branch can't issue
                // a duplicate cube release against the same lease.
                let released = match self.work_db.clear_execution_workspace(&execution.id) {
                    Ok(Some(lease_id)) => match adapter.release_workspace(&lease_id).await {
                        Ok(()) => true,
                        Err(err) => {
                            tracing::error!(
                                ?err,
                                execution_id = %execution.id,
                                run_id = %run.id,
                                lease_id = %lease_id,
                                "failed to release deferred lease after mid-spawn cancel",
                            );
                            false
                        }
                    },
                    // Already cleared by a racing force_release that saw
                    // the slot mapped and reaped + released itself.
                    Ok(None) => false,
                    Err(err) => {
                        tracing::error!(
                            ?err,
                            execution_id = %execution.id,
                            "failed to clear workspace columns after mid-spawn cancel",
                        );
                        false
                    }
                };
                tracing::warn!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id = %worker_id,
                    released_workspace = released,
                    "reconciled mid-spawn cancel: worker pane reaped, deferred lease released",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PaneSpawned, DispatchOutcome::Skipped, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(&worker_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(serde_json::json!({
                                "run_id": run.id,
                                "cancelled_during_spawn": true,
                                "released_workspace": released,
                            })),
                    )
                    .await;
                // The pane was already torn down by the runner (which
                // also released the pool slot), and `defer_pool_slot_release`
                // is false for this outcome (slot_id = None), so the tail
                // below frees the pool slot idempotently.
            }
            Ok(outcome) => {
                // Capture the resolved spawn knobs (effort level,
                // claude effort value, model) before `outcome` moves
                // into `record_run_completion` — they ride along on
                // the `pane_spawned` dispatch event below so a
                // diagnose verb can answer "what did the worker
                // actually launch with" without scraping process
                // argv. `None` from test fake runners that don't go
                // through `effort::resolve_spawn_config`.
                let spawn_config_for_event = outcome.spawn_config.clone();
                // If the runner allocated a real pane slot for this
                // run, stamp it onto the run record's agent_id so
                // `bossctl agents list` and related views show one
                // entry per active pane. Test runners that don't
                // allocate a pane leave slot_id as None and the
                // worker-pool placeholder (worker_id) stays as the
                // agent_id.
                let run = if let Some(slot_id) = outcome.slot_id {
                    let agent_id = worker_id_for_slot(slot_id);
                    match self.work_db.set_run_agent_id(&run.id, &agent_id) {
                        Ok(updated) => updated,
                        Err(err) => {
                            tracing::error!(
                                ?err,
                                execution_id = %execution.id,
                                run_id = %run.id,
                                slot_id,
                                "failed to stamp pane slot onto run record"
                            );
                            run
                        }
                    }
                } else {
                    run
                };
                if let Err(err) = self
                    .record_run_completion(&execution, &run, &lease, &worker_id, outcome, &adapter)
                    .await
                {
                    tracing::error!(
                        ?err,
                        execution_id = %execution.id,
                        run_id = %run.id,
                        worker_id = %worker_id,
                        "failed to record execution completion"
                    );
                }
                // Successful spawn → emit a structured `pane_spawned`
                // event so consumers can pair it with the
                // `cube_workspace_leased` event that preceded it and
                // see the full timeline. The `spawn_config` details
                // carry the effort + model tuple the dispatcher just
                // resolved — design §Q2 calls this out explicitly so
                // `bossctl dispatch diagnose <exec-id>` can answer
                // "which model / effort did this worker actually
                // launch with."
                let mut details = serde_json::json!({
                    "run_id": run.id,
                    "slot_id": slot_id_from_worker_id(&worker_id),
                    "page": slot_id_from_worker_id(&worker_id).and_then(worker_page_label),
                });
                if let Some(spawn) = spawn_config_for_event {
                    details["spawn_config"] = serde_json::json!({
                        "effort_level": spawn.effort_level.map(|level| level.as_str()),
                        "claude_effort": spawn.claude_effort,
                        "model": spawn.model,
                        "prompt_addendum_applied": spawn.prompt_addendum.is_some(),
                        "model_floor": spawn.model_floor,
                    });
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PaneSpawned, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(&worker_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(details),
                    )
                    .await;
            }
            Err(err) => {
                let released = match adapter.release_workspace(&lease.lease_id).await {
                    Ok(()) => true,
                    Err(release_err) => {
                        tracing::error!(
                            ?release_err,
                            execution_id = %execution.id,
                            run_id = %run.id,
                            lease_id = %lease.lease_id,
                            "failed to release workspace after run failure"
                        );
                        false
                    }
                };
                let error_text = err.to_string();

                // Historical silent-release path: a pane-spawn
                // failure (libghostty IPC drop, slot busy, prompt
                // composition error) inside `run_execution` marked
                // the run `failed` and released the lease without
                // raising anything the operator could see. Attach a
                // `WorkAttentionItem` to this run so the failure
                // turns up in the kanban "Attention" lane and via
                // `ListAttentionItems`. The structured event below
                // gives tooling a parallel signal.
                let err_detail = format!("{err:#}");
                let attention = Some(CreateAttentionItemInput {
                    execution_id: Some(execution.id.clone()),
                    work_item_id: None,
                    kind: "pane_spawn_failed".to_owned(),
                    status: None,
                    title: "Worker pane failed to spawn".to_owned(),
                    body_markdown: format!(
                        "Execution `{exec_id}` leased workspace `{ws}` but the worker pane never came up.\n\n\
                         **Error:** {err_detail}\n\n\
                         The lease was {release_state}. Inspect \
                         `dispatch-events/executions/{exec_id}/dispatch.jsonl` for the full stage timeline.",
                        exec_id = execution.id,
                        ws = lease.workspace_id,
                        release_state = if released {
                            "released back to cube"
                        } else {
                            "still held by the engine (release failed — see the engine log)"
                        },
                    ),
                    resolved_at: None,
                });

                match self.work_db.finish_execution_run(
                    FinishExecutionRunInput::builder()
                        .execution_id(&execution.id)
                        .run_id(&run.id)
                        .execution_status(ExecutionStatus::Failed)
                        .run_status("failed")
                        .error_text(error_text.as_str())
                        .clear_workspace_lease(released)
                        .maybe_attention(attention)
                        .build(),
                ) {
                    Ok((execution, _run, _)) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            run_id = %run.id,
                            worker_id = %worker_id,
                            error = %err,
                            released_workspace = released,
                            "execution run failed"
                        );
                        let mut error_details = serde_json::json!({
                            "run_id": run.id,
                            "released_workspace": released,
                            "slot_id": slot_id_from_worker_id(&worker_id),
                            "page": slot_id_from_worker_id(&worker_id).and_then(worker_page_label),
                        });
                        // A `SlotBusy` spawn rejection means the engine and
                        // the app disagree about slot occupancy — the
                        // engine already knew which slot it requested
                        // (`worker_id` above), but not which pane the app
                        // reports as squatting it. Surface both explicitly
                        // so `dispatch.jsonl` is self-diagnosing instead of
                        // requiring a coordinator to cross-reference the
                        // husk pane by hand.
                        if let Some(occupying_run_id) = slot_busy_occupant(&err) {
                            error_details["slot_busy"] = serde_json::json!({
                                "slot_id": slot_id_from_worker_id(&worker_id),
                                "occupying_run_id": occupying_run_id,
                            });
                        }
                        self.dispatch_events
                            .emit(
                                DispatchEvent::new(Stage::PaneSpawned, DispatchOutcome::Error, &execution.id)
                                    .with_work_item(&execution.work_item_id)
                                    .with_worker(&worker_id)
                                    .with_cube_lease(&lease.lease_id)
                                    .with_cube_workspace(&lease.workspace_id)
                                    .with_error(&err)
                                    .with_details(error_details),
                            )
                            .await;
                        // Clear the card out of `active`. The run is
                        // already recorded `failed` and the workspace
                        // released, but the work item itself stays
                        // `active` — so the kanban keeps the green
                        // "Doing" card and the orphan-active sweep
                        // re-dispatches the same doomed spawn every
                        // cycle. Demote it back to To-Do so the failure
                        // (already surfaced as a `pane_spawn_failed`
                        // attention item) is recoverable rather than a
                        // silent green-flicker strand.
                        //
                        // Exception: PrReview spawn failures are engine
                        // infrastructure bugs (e.g. slot-range mismatch),
                        // not task regressions. Demoting the work item
                        // here would silently move a reviewed PR back to
                        // To-Do, erasing the review context. Leave the
                        // task in place — the attention item already
                        // surfaces the failure for the operator.
                        if execution.kind != ExecutionKind::PrReview {
                            match self.work_db.demote_active_work_item_to_todo(&execution.work_item_id) {
                                Ok(true) => tracing::info!(
                                    execution_id = %execution.id,
                                    work_item_id = %execution.work_item_id,
                                    "demoted work item to todo after pane-spawn failure",
                                ),
                                Ok(false) => {}
                                Err(demote_err) => tracing::error!(
                                    ?demote_err,
                                    work_item_id = %execution.work_item_id,
                                    "failed to demote work item out of active after pane-spawn failure",
                                ),
                            }
                        } else {
                            tracing::info!(
                                execution_id = %execution.id,
                                work_item_id = %execution.work_item_id,
                                "skipping demote for pr_review spawn failure — engine infrastructure issue, not a task regression",
                            );
                        }
                        self.publisher
                            .publish(
                                &execution.id,
                                &execution.work_item_id,
                                execution.status.as_str(),
                                "execution_run_failed",
                            )
                            .await;
                        if let Ok(item) = self.work_db.get_work_item(&execution.work_item_id) {
                            self.publisher
                                .publish_work_item_changed(
                                    &work_item_product_id(&item),
                                    &execution.work_item_id,
                                    "execution_run_failed",
                                )
                                .await;
                        }
                        // A pane-spawn failure is terminal — the execution is
                        // now `failed` and the workspace has been released. If
                        // this was an automation triage run, the matching
                        // `automation_runs` row is still sitting at the
                        // pessimistic `failed_will_retry` that the scheduler
                        // stamped when it dispatched the triage execution.
                        // Flip it to `failed_gave_up` so the Automations tab
                        // shows an accurate terminal state instead of implying
                        // a self-healing retry is pending (it is not: a
                        // pane-spawn failure like an invalid worker_id format
                        // will not recover on its own).
                        if execution.kind == ExecutionKind::AutomationTriage
                            && let Err(finalize_err) = self.work_db.finalize_automation_triage_run(
                                &execution.id,
                                boss_protocol::AUTOMATION_OUTCOME_FAILED_GAVE_UP,
                                None,
                                Some(&format!("pane spawn failed: {error_text}")),
                            )
                        {
                            tracing::warn!(
                                execution_id = %execution.id,
                                ?finalize_err,
                                "failed to mark automation run failed_gave_up after pane-spawn failure",
                            );
                        }
                    }
                    Err(record_err) => {
                        tracing::error!(
                            ?record_err,
                            execution_id = %execution.id,
                            run_id = %run.id,
                            worker_id = %worker_id,
                            "failed to record execution run failure"
                        );
                    }
                }
            }
        }

        if !defer_pool_slot_release {
            self.release_worker_and_kick(&worker_id, Some(lease.workspace_id.as_str()))
                .await;
        }
    }

    /// Phase 3 cutover: for revision_implementation executions with merge-conflict
    /// provenance, resolve the linked `conflict_resolutions` row (via
    /// `created_via = "merge-conflict:<crz_id>"`) and collect its diagnosis:
    /// resolve the `conflict_resolutions` row a merge-conflict revision was
    /// spawned from (via `created_via = "merge-conflict:<crz_id>"`) and
    /// collect its diagnosis. No-op when the revision's provenance is not a
    /// merge conflict (e.g. operator/CI-fix revisions), or when a diagnosis
    /// is already stored (a respawn).
    async fn collect_revision_conflict_diagnosis_pre_spawn(
        &self,
        execution: &WorkExecution,
        work_item: &WorkItem,
        lease: &CubeWorkspaceLease,
    ) {
        let created_via = match work_item {
            WorkItem::Task(task) | WorkItem::Chore(task) => task.created_via.as_str(),
            _ => return,
        };
        let Some(crz_id) = created_via.strip_prefix(boss_protocol::CREATED_VIA_MERGE_CONFLICT_PREFIX) else {
            return;
        };
        let attempt = match self.work_db.get_conflict_resolution(crz_id) {
            Ok(Some(a)) => a,
            Ok(None) => {
                tracing::debug!(
                    execution_id = %execution.id,
                    crz_id,
                    "collect_conflict_diagnosis: revision's linked attempt row missing; skipping",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    crz_id,
                    ?err,
                    "collect_conflict_diagnosis: failed to look up revision's linked attempt; skipping",
                );
                return;
            }
        };
        if attempt.conflict_diagnosis.is_some() {
            tracing::debug!(
                attempt_id = %attempt.id,
                "collect_conflict_diagnosis: diagnosis already present on linked attempt; skipping",
            );
            return;
        }
        self.collect_conflict_diagnosis_for_attempt(&attempt, lease).await;
    }

    /// Run `conflict_diagnosis::collect` in the leased workspace and persist
    /// the result on `attempt`. Shared by the bespoke `conflict_resolution`
    /// path and the Phase 3 merge-conflict revision path. Best-effort —
    /// failures are logged but never propagate.
    async fn collect_conflict_diagnosis_for_attempt(
        &self,
        attempt: &crate::work::ConflictResolution,
        lease: &CubeWorkspaceLease,
    ) {
        let base_sha = attempt.base_sha_at_trigger.as_deref().unwrap_or("");
        let head_sha = attempt.head_sha_before.as_deref().unwrap_or("");
        if base_sha.is_empty() || head_sha.is_empty() {
            tracing::debug!(
                attempt_id = %attempt.id,
                "collect_conflict_diagnosis: missing base/head sha; skipping",
            );
            return;
        }

        let diagnosis = match conflict_diagnosis::collect(&lease.workspace_path, base_sha, head_sha).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    workspace_path = %lease.workspace_path.display(),
                    ?err,
                    "collect_conflict_diagnosis: git spawn failed; using errored diagnosis",
                );
                conflict_diagnosis::ConflictDiagnosis::errored(base_sha, head_sha, format!("git spawn failed: {err}"))
            }
        };

        let json = match serde_json::to_string(&diagnosis) {
            Ok(j) => j,
            Err(err) => {
                tracing::warn!(attempt_id = %attempt.id, ?err, "collect_conflict_diagnosis: failed to serialize diagnosis");
                return;
            }
        };

        match self.work_db.set_conflict_resolution_diagnosis(&attempt.id, &json) {
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "collect_conflict_diagnosis: failed to persist diagnosis; continuing without it",
                );
            }
            Ok(updated) => {
                tracing::debug!(
                    attempt_id = %attempt.id,
                    conflicted_files = diagnosis.files.len(),
                    "collect_conflict_diagnosis: diagnosis persisted",
                );
                let counted = updated.and_then(|row| row.conflict_class.clone().map(|class| (row.product_id, class)));
                if let Some((product_id, class)) = counted {
                    crate::merge_poller::record_conflict_class_counter(&self.metrics, &product_id, &class);
                }
            }
        }
    }

    /// Release `worker_id` back to the pool, then rescan + kick to
    /// pick up newly-eligible work. Used at the tail of non-pane
    /// `run_execution` calls and from [`ServerState::release_worker_pane`]
    /// for the deferred pane-spawn case — the engine and the app must
    /// agree on which slots are busy, so the WorkerPool free signal is
    /// paired with the libghostty pane teardown rather than firing as
    /// soon as the spawn RPC returns.
    pub async fn release_worker_and_kick(self: &Arc<Self>, worker_id: &str, last_workspace_id: Option<&str>) {
        self.pool_for_worker_id(worker_id)
            .release_worker(worker_id, last_workspace_id)
            .await;
        self.rescan_active_dispatch_after_release();
        self.kick();
    }

    /// Compare-and-release variant of [`Self::release_worker_and_kick`]
    /// for the pool-claim reconciler: free `worker_id` only if it is
    /// still claimed by exactly `execution_id`, then rescan + kick if it
    /// was actually freed. Returns whether the slot was released.
    ///
    /// The execution-id guard makes this safe against the re-claim race
    /// the reconciler is exposed to (snapshot a leaked claim, release it
    /// later) — see [`WorkerPool::release_worker_if_execution`]. The
    /// rescan + kick only fire on a real release so a no-op (already
    /// freed, or re-claimed by a live execution) doesn't churn the
    /// scheduler.
    pub async fn release_pool_claim_if_execution(self: &Arc<Self>, worker_id: &str, execution_id: &str) -> bool {
        let released = self
            .pool_for_worker_id(worker_id)
            .release_worker_if_execution(worker_id, execution_id, None)
            .await;
        if released {
            self.rescan_active_dispatch_after_release();
            self.kick();
        }
        released
    }

    /// Steady-state rescan of `tasks.status = 'active'` work that
    /// never made it onto a worker. The create-time path already
    /// queues a `ready` execution and `kick()`s the scheduler, but a
    /// chore whose dispatch failed (cube lease error, kanban drag
    /// while the pool was full, worker died after starting) leaves
    /// the kanban card in `active` with a *terminal* (or absent)
    /// execution row — `list_ready_executions` skips it and `kick()`
    /// alone is not enough to reanimate it. Running
    /// [`WorkDb::rescan_active_dispatch`] before each kick fixes
    /// that: items whose latest execution is terminal (or missing)
    /// get a fresh `ready` row, and the scheduler picks them up on
    /// the just-released worker. Errors are logged and swallowed —
    /// the rescan is a best-effort opportunistic sweep, not a hard
    /// invariant.
    fn rescan_active_dispatch_after_release(&self) {
        match self.work_db.rescan_active_dispatch() {
            Ok(redispatched) if !redispatched.is_empty() => {
                tracing::info!(
                    count = redispatched.len(),
                    ids = ?redispatched,
                    "rescanned waiting active work after worker release",
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::error!(?err, "active-dispatch rescan failed after worker release; continuing",);
            }
        }
    }

    async fn record_run_completion(
        &self,
        execution: &WorkExecution,
        run: &WorkRun,
        lease: &CubeWorkspaceLease,
        worker_id: &str,
        outcome: RunOutcome,
        adapter: &Arc<dyn HostAdapter>,
    ) -> Result<()> {
        let release_workspace = outcome.wait_state.release_workspace();
        let released = if release_workspace {
            match adapter.release_workspace(&lease.lease_id).await {
                Ok(()) => true,
                Err(err) => {
                    tracing::error!(
                        ?err,
                        execution_id = %execution.id,
                        run_id = %run.id,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after successful run"
                    );
                    false
                }
            }
        } else {
            false
        };

        let attention = outcome.attention.map(|attention| CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: attention.kind,
            status: None,
            title: attention.title,
            body_markdown: attention.body_markdown,
            resolved_at: None,
        });

        let (execution, run, attention) = self.work_db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&execution.id)
                .run_id(&run.id)
                .execution_status(outcome.wait_state.execution_status())
                .run_status("completed")
                .maybe_result_summary(outcome.result_summary.as_deref())
                .clear_workspace_lease(released)
                .maybe_attention(attention)
                .build(),
        )?;

        tracing::info!(
            execution_id = %execution.id,
            run_id = %run.id,
            worker_id,
            execution_status = %execution.status,
            run_status = %run.status,
            attention_created = attention.is_some(),
            released_workspace = released,
            "execution run completed"
        );
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                execution.status.as_str(),
                "execution_run_completed",
            )
            .await;
        if let Ok(item) = self.work_db.get_work_item(&execution.work_item_id) {
            self.publisher
                .publish_work_item_changed(
                    &work_item_product_id(&item),
                    &execution.work_item_id,
                    "execution_run_completed",
                )
                .await;
        }
        Ok(())
    }
}

fn work_item_product_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(p) => p.id.clone(),
        WorkItem::Project(p) => p.product_id.clone(),
        WorkItem::Task(t) | WorkItem::Chore(t) => t.product_id.clone(),
    }
}

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
mod tests {
    use crate::test_support::*;
    use std::future::pending;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;
    use tokio::time::sleep;

    use super::{
        AUTOMATION_WORKER_ID_PREFIX, CHAIN_SERIALIZED_STALL_ATTENTION_KIND, CHAIN_SERIALIZED_STALL_THRESHOLD_SECS,
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
        DispatchPauseOrigin, EXECUTION_KIND_PR_REVIEW, ExecutionCoordinator, ExecutionKind, Host, HostAdapter,
        HostAdapterProvider, MAX_AUTOMATION_POOL_SIZE, MAX_REVIEW_POOL_SIZE, MAX_WORKER_POOL_SIZE,
        REVIEW_WORKER_ID_PREFIX, WORKER_PAGE_SIZE, WorkerPool, occupying_live_worker, pick_worst_failing_check,
        pool_model_override_for_worker_id, slot_busy_occupant, slot_id_from_worker_id, worker_id_for_slot,
        worker_page_label,
    };
    use crate::spawn_flow::StartWorkerError;
    use boss_protocol::{EngineToAppError, ExecutionStatus};

    /// Lease-time occupancy guard (defect 3, regression test c). The
    /// pure decision: a workspace is "occupied" only by a tracked worker
    /// with a *live* process and non-terminal activity on that workspace;
    /// a dead-pid occupant (the orphan-resume case) is re-leasable, and
    /// the dispatching execution never blocks itself.
    #[test]
    fn occupying_live_worker_blocks_only_a_live_tracked_occupant() {
        use boss_protocol::{LiveWorkerState, WorkerActivity};

        // exec-live: alive process; exec-dead: process gone. Both are
        // recorded against the SAME workspace cube just handed us.
        let mut live = LiveWorkerState::new_spawning(1, "exec-live", "opus", 4242, None);
        live.activity = WorkerActivity::Working;
        let mut dead = LiveWorkerState::new_spawning(2, "exec-dead", "opus", 5151, None);
        dead.activity = WorkerActivity::Working;

        let workspace_of = |eid: &str| match eid {
            "exec-live" | "exec-dead" | "exec-new" => Some("mono-agent-021".to_owned()),
            _ => None,
        };
        let pid_alive = |pid: i32| pid == 4242; // only exec-live is alive

        // A redispatch CANNOT lease a workspace occupied by a live process.
        assert_eq!(
            occupying_live_worker("mono-agent-021", "exec-new", &[live.clone()], workspace_of, pid_alive),
            Some("exec-live".to_owned()),
            "a live occupant must block the lease",
        );

        // A dead-pid occupant does NOT block — the workspace is genuinely
        // free (normal orphan-resume).
        assert_eq!(
            occupying_live_worker("mono-agent-021", "exec-new", &[dead], workspace_of, pid_alive),
            None,
            "a dead occupant must not block the lease",
        );

        // The dispatching execution never blocks itself.
        let mut myself = LiveWorkerState::new_spawning(3, "exec-new", "opus", 4242, None);
        myself.activity = WorkerActivity::Working;
        assert_eq!(
            occupying_live_worker("mono-agent-021", "exec-new", &[myself], workspace_of, pid_alive),
            None,
            "the dispatching execution must never block itself",
        );

        // A terminal-activity occupant has released its slot — not occupying.
        let mut terminated = LiveWorkerState::new_spawning(4, "exec-live", "opus", 4242, None);
        terminated.activity = WorkerActivity::Terminated;
        assert_eq!(
            occupying_live_worker("mono-agent-021", "exec-new", &[terminated], workspace_of, pid_alive),
            None,
            "a terminated worker no longer occupies its workspace",
        );

        // Occupancy is workspace-scoped: a live worker on a DIFFERENT
        // workspace doesn't block this lease.
        assert_eq!(
            occupying_live_worker("mono-agent-099", "exec-new", &[live], workspace_of, pid_alive),
            None,
            "occupancy must be scoped to the leased workspace",
        );
    }

    #[test]
    fn pick_worst_failing_check_prefers_failure() {
        let json = serde_json::json!([
            {"name": "infra", "conclusion": "CANCELLED", "target_url": "https://buildkite.com/o/p/builds/2#j", "provider": "buildkite", "provider_job_id": "j"},
            {"name": "tests", "conclusion": "FAILURE", "target_url": "https://buildkite.com/o/p/builds/3#k", "provider": "buildkite", "provider_job_id": "k"},
            {"name": "x", "conclusion": "TIMED_OUT", "target_url": "https://buildkite.com/o/p/builds/4#l", "provider": "buildkite", "provider_job_id": "l"},
        ])
        .to_string();
        let picked = pick_worst_failing_check(&json).expect("expected one entry");
        assert_eq!(picked.conclusion, "FAILURE");
        assert_eq!(picked.provider, "buildkite");
        assert_eq!(picked.provider_job_id.as_deref(), Some("k"));
    }

    #[test]
    fn pick_worst_failing_check_handles_malformed_json() {
        assert!(pick_worst_failing_check("{not json}").is_none());
        assert!(pick_worst_failing_check("[]").is_none());
    }

    #[test]
    fn pick_worst_failing_check_falls_back_to_only_entry() {
        let json = serde_json::json!([
            {"name": "n", "conclusion": "STARTUP_FAILURE", "target_url": "u", "provider": "github_actions", "provider_job_id": "1"},
        ])
        .to_string();
        let picked = pick_worst_failing_check(&json).expect("entry");
        assert_eq!(picked.conclusion, "STARTUP_FAILURE");
    }
    use crate::runner::{ExecutionRunner, RunAttention, RunOutcome, RunWaitState};
    use crate::work::{
        AddDependencyInput, CreateChoreInput, CreateExecutionInput, CreateProductInput, CreateProjectInput,
        CreateTaskInput, FinishExecutionRunInput, RequestExecutionInput, TaskStatus, WorkDb, WorkExecution, WorkItem,
    };

    /// Recorded args for each `lease_workspace` call:
    /// `(repo_id, task, prefer_workspace_id, allow_dirty, exclude_workspace_ids)`.
    type LeaseCall = (String, String, Option<String>, bool, Vec<String>);

    #[derive(Default)]
    struct FakeCubeClient {
        ensure_calls: Mutex<Vec<String>>,
        lease_calls: Mutex<Vec<LeaseCall>>,
        goto_calls: Mutex<Vec<(String, u64)>>,
        create_calls: Mutex<Vec<(String, String)>>,
        release_calls: Mutex<Vec<String>>,
        status_calls: Mutex<Vec<PathBuf>>,
        heartbeat_calls: Mutex<Vec<(String, Option<u64>)>>,
        force_release_calls: Mutex<Vec<(String, Option<String>)>>,
        /// Counts how many times `list_repos` has been invoked. Tests
        /// for the cold-pool probe assert this equals 1 across two
        /// dispatches against the same URL (probe is engine-lifetime
        /// deduped).
        list_repos_calls: Mutex<u32>,
        /// Snapshot returned by `list_repos`. Default is the empty
        /// slice — most tests don't exercise the cold-pool probe and
        /// the empty list short-circuits before any attention item is
        /// written.
        repos: Mutex<Vec<CubeRepoSummary>>,
        fail_ensure: bool,
        fail_lease: bool,
        /// Model the anaplian failure-mode A: cube exits non-zero on a
        /// post-lease setup step, surfaced as a typed [`CubeCliError`]
        /// carrying the exit code + stderr (as the remote SSH adapter
        /// does). Lets a test assert those signals reach the dispatch
        /// event `details`.
        fail_lease_with_cube_cli_error: bool,
        /// Simulate cube refusing a `--prefer` request because the
        /// preferred workspace is held: `lease_workspace` errors when
        /// `prefer_workspace_id` is `Some(_)`. Models the "prefer set,
        /// no fallback" path — the engine should fail fast rather than
        /// silently landing on a different workspace.
        fail_lease_when_prefer_set: bool,
        /// Fail the first N lease calls (0-indexed), then succeed. Used
        /// to model a single bad workspace being skipped via `any_free`
        /// retry when `preferred_workspace_id=null`.
        fail_first_n_leases: usize,
        fail_create: bool,
        fail_goto: bool,
        next_workspace_id: Mutex<Option<String>>,
        /// Ordered queue of workspace IDs to return from successive
        /// `lease_workspace` calls. When non-empty, dequeues from the
        /// front; when empty, falls through to `next_workspace_id` or the
        /// default "mono-agent-001". Used by livelock tests that need the
        /// first lease call to return an occupied workspace and subsequent
        /// calls to return a free one.
        workspace_id_queue: Mutex<std::collections::VecDeque<String>>,
        /// Canned response for `list_workspaces` — lets a test model cube
        /// reporting a workspace still leased to a dead worker so the
        /// stale-lease reclaim path (issue #962) can be exercised.
        list_workspaces_response: Mutex<Vec<CubeWorkspaceStatus>>,
    }

    impl FakeCubeClient {
        fn with_next_workspace_id(self, id: impl Into<String>) -> Self {
            *self.next_workspace_id.try_lock().expect("uncontended") = Some(id.into());
            self
        }

        fn with_workspace_id_queue(self, ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
            *self.workspace_id_queue.try_lock().expect("uncontended") = ids.into_iter().map(|s| s.into()).collect();
            self
        }

        fn with_repos(self, repos: Vec<CubeRepoSummary>) -> Self {
            *self.repos.try_lock().expect("uncontended") = repos;
            self
        }

        fn with_list_workspaces(self, rows: Vec<CubeWorkspaceStatus>) -> Self {
            *self.list_workspaces_response.try_lock().expect("uncontended") = rows;
            self
        }
    }

    crate::stub_cube_client! { FakeCubeClient {
        async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
            self.ensure_calls.lock().await.push(origin.to_owned());
            if self.fail_ensure {
                return Err(anyhow!("cube repo ensure failed"));
            }
            Ok(CubeRepoHandle {
                repo_id: "mono".to_owned(),
            })
        }

        async fn lease_workspace(
            &self,
            repo_id: &str,
            task: &str,
            prefer_workspace_id: Option<&str>,
            allow_dirty: bool,
            exclude_workspace_ids: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            let mut calls = self.lease_calls.lock().await;
            let call_index = calls.len();
            calls.push((
                repo_id.to_owned(),
                task.to_owned(),
                prefer_workspace_id.map(str::to_owned),
                allow_dirty,
                exclude_workspace_ids.iter().map(|s| s.to_string()).collect(),
            ));
            drop(calls);
            if self.fail_lease_with_cube_cli_error {
                return Err(crate::cube_commands::CubeCliError {
                    host: "anaplian".to_owned(),
                    exit_code: Some(1),
                    stderr: "setup step `copy-config-secrets` failed: cp: backend/config-secrets.toml: \
                             No such file or directory"
                        .to_owned(),
                    stdout: String::new(),
                }
                .into());
            }
            if self.fail_lease {
                return Err(anyhow!("cube workspace lease failed"));
            }
            if self.fail_lease_when_prefer_set && prefer_workspace_id.is_some() {
                return Err(anyhow!(
                    "cube workspace lease failed: preferred workspace held by another worker"
                ));
            }
            if call_index < self.fail_first_n_leases {
                return Err(anyhow!("cube workspace lease failed: workspace has uncommitted work"));
            }
            // Queue takes priority; falls through to next_workspace_id, then prefer,
            // then the default. The queue lets tests model a sequence of
            // workspace responses (e.g. occupied-then-free for livelock tests).
            let workspace_id = self
                .workspace_id_queue
                .lock()
                .await
                .pop_front()
                .or_else(|| self.next_workspace_id.try_lock().ok().and_then(|g| g.clone()))
                .or_else(|| prefer_workspace_id.map(str::to_owned))
                .unwrap_or_else(|| "mono-agent-001".to_owned());
            Ok(CubeWorkspaceLease {
                lease_id: "lease-1".to_owned(),
                workspace_id: workspace_id.clone(),
                workspace_path: PathBuf::from(format!("/tmp/{workspace_id}")),
            })
        }

        async fn create_change(&self, workspace_path: &std::path::Path, title: &str) -> Result<CubeChangeHandle> {
            self.create_calls
                .lock()
                .await
                .push((workspace_path.display().to_string(), title.to_owned()));
            if self.fail_create {
                return Err(anyhow!("cube change create failed"));
            }
            Ok(CubeChangeHandle {
                change_id: "chg-1".to_owned(),
            })
        }

        async fn goto_workspace(&self, workspace_path: &std::path::Path, pr: u64) -> Result<()> {
            self.goto_calls
                .lock()
                .await
                .push((workspace_path.display().to_string(), pr));
            if self.fail_goto {
                return Err(anyhow!("cube workspace goto failed"));
            }
            Ok(())
        }

        async fn release_workspace(&self, lease_id: &str) -> Result<()> {
            self.release_calls.lock().await.push(lease_id.to_owned());
            Ok(())
        }

        async fn workspace_status(&self, workspace_path: &std::path::Path) -> Result<CubeWorkspaceStatus> {
            self.status_calls.lock().await.push(workspace_path.to_path_buf());
            Ok(CubeWorkspaceStatus::builder()
                .workspace_id("mono-agent-001")
                .workspace_path(workspace_path.to_path_buf())
                .state("leased")
                .lease_id("lease-1")
                .holder("boss/0")
                .task("test task")
                .leased_at_epoch_s(1_700_000_000)
                .lease_expires_at_epoch_s(1_700_001_800)
                .build())
        }

        async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
            self.heartbeat_calls
                .lock()
                .await
                .push((lease_id.to_owned(), ttl_seconds));
            Ok(())
        }

        async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
            self.force_release_calls
                .lock()
                .await
                .push((lease_id.to_owned(), reason.map(str::to_owned)));
            Ok(())
        }

        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(self.list_workspaces_response.lock().await.clone())
        }

        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            *self.list_repos_calls.lock().await += 1;
            Ok(self.repos.lock().await.clone())
        }
    } }

    /// Recorded args for each `run` call:
    /// `(worker_id, execution_id, workspace_path, cube_change_id)`.
    type RunnerCall = (String, String, String, Option<String>);

    struct FakeExecutionRunner {
        calls: Mutex<Vec<RunnerCall>>,
        fail: bool,
        pending: bool,
        /// If `Some`, the runner reports this slot id back to the
        /// coordinator in the `RunOutcome`, simulating a successful
        /// `SpawnWorkerPane` round-trip. Used to verify that the
        /// coordinator stamps the slot-based agent_id onto the run
        /// record.
        slot_id: Option<u8>,
        /// Resolved spawn knobs the fake runner reports back. `None`
        /// matches the default fake-runner contract (no effort/model
        /// resolution happened). Production `PaneSpawnRunner` always
        /// fills this in — tests that want to assert on the
        /// dispatcher's effort/model surfacing set it explicitly.
        spawn_config: Option<crate::effort::SpawnConfig>,
        /// When `true`, simulate the T981 mid-spawn cancel: cancel the
        /// execution row (via `work_db`, mirroring the real cancel path
        /// that ran while the spawn was in flight) and report
        /// `RunWaitState::CancelledDuringSpawn`. The coordinator must
        /// then release the deferred lease and skip completion recording.
        cancelled_during_spawn: bool,
        /// When `true`, simulate the T267 provisional spawn: the real
        /// `PaneSpawnRunner` converts a `SpawnWorkerPane` ack timeout into
        /// a provisional (unverified) spawn — the pane may be live, so the
        /// run is tracked in `waiting_human` with its slot retained and the
        /// lease is NOT released. The fake returns that same outcome so the
        /// coordinator-side contract (no release, no duplicate dispatch,
        /// still tracked) can be asserted. The Timeout→provisional
        /// conversion itself is unit-tested in `spawn_flow`.
        ack_timed_out: bool,
        /// Handle used by the `cancelled_during_spawn` path to cancel the
        /// row before returning. `None` for the default fake.
        work_db: Option<Arc<WorkDb>>,
    }

    impl Default for FakeExecutionRunner {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: false,
                pending: false,
                slot_id: None,
                spawn_config: None,
                cancelled_during_spawn: false,
                ack_timed_out: false,
                work_db: None,
            }
        }
    }

    #[async_trait]
    impl ExecutionRunner for FakeExecutionRunner {
        async fn run_execution(
            &self,
            worker_id: &str,
            execution: &WorkExecution,
            work_item: &WorkItem,
            workspace_path: &std::path::Path,
            cube_change_id: Option<&str>,
        ) -> Result<RunOutcome> {
            self.calls.lock().await.push((
                worker_id.to_owned(),
                execution.id.clone(),
                workspace_path.display().to_string(),
                cube_change_id.map(str::to_owned),
            ));
            if self.pending {
                pending::<()>().await;
            }
            if self.fail {
                return Err(anyhow!("worker prompt failed"));
            }

            if self.cancelled_during_spawn {
                // Mirror the real race: the cancel landed while the
                // spawn round-trip was in flight, marking the row
                // cancelled. The runner (having reaped the pane) reports
                // CancelledDuringSpawn so the coordinator releases the
                // lease the cancel path left held.
                if let Some(db) = &self.work_db {
                    db.cancel_running_execution(&execution.id)
                        .expect("cancel row in fake mid-spawn cancel");
                }
                return Ok(RunOutcome {
                    wait_state: RunWaitState::CancelledDuringSpawn,
                    result_summary: Some("cancelled during spawn".to_owned()),
                    attention: None,
                    slot_id: None,
                    spawn_config: None,
                });
            }

            if self.ack_timed_out {
                // Mirror the real PaneSpawnRunner after a SpawnWorkerPane
                // ack timeout: a PROVISIONAL spawn. The pane may be live,
                // so the run is tracked in `waiting_human` with its slot
                // retained (slot_id = Some ⇒ the coordinator defers the
                // pool-slot release and does NOT release the workspace
                // lease). No attention item — this is not a failure.
                return Ok(RunOutcome {
                    wait_state: RunWaitState::WaitingHuman,
                    result_summary: Some("provisional spawn: SpawnWorkerPane ack timed out".to_owned()),
                    attention: None,
                    slot_id: Some(1),
                    spawn_config: None,
                });
            }

            Ok(RunOutcome {
                wait_state: RunWaitState::WaitingHuman,
                result_summary: Some(format!("finished {}", execution.kind)),
                attention: Some(RunAttention {
                    kind: "review_required".to_owned(),
                    title: format!("Review {}", execution.kind),
                    body_markdown: format!("Review {}", test_work_item_name(work_item)),
                }),
                slot_id: self.slot_id,
                spawn_config: self.spawn_config.clone(),
            })
        }
    }

    async fn wait_for_execution_status(db: &WorkDb, execution_id: &str, expected: ExecutionStatus) {
        for _ in 0..100 {
            let execution = db.get_execution(execution_id).unwrap();
            if execution.status == expected {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("execution {execution_id} never reached status `{expected}`");
    }

    fn test_work_item_name(work_item: &WorkItem) -> &str {
        match work_item {
            WorkItem::Product(product) => &product.name,
            WorkItem::Project(project) => &project.name,
            WorkItem::Task(task) | WorkItem::Chore(task) => &task.name,
        }
    }

    #[tokio::test]
    async fn read_remote_transcript_tail_local_returns_none_and_unknown_host_errors() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let coordinator = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner::default()),
        );

        // "local" short-circuits to None so the RPC reads the local fs.
        assert_eq!(
            coordinator
                .read_remote_transcript_tail("local", "/whatever.jsonl", 1024)
                .await
                .unwrap(),
            None,
        );

        // An unknown host is a hard error (the run referenced a host that
        // is no longer registered) rather than a silent empty read.
        let err = coordinator
            .read_remote_transcript_tail("ghost", "/whatever.jsonl", 1024)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost"), "got: {err}");
    }

    #[tokio::test]
    async fn schedules_ready_execution_into_running_run() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();
        wait_for_execution_status(
            db.as_ref(),
            &db.list_executions(Some(&chore.id)).unwrap()[0].id,
            ExecutionStatus::Running,
        )
        .await;

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        assert_eq!(execution.status, ExecutionStatus::Running);
        assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.agent_id, "worker-1");
        assert_eq!(run.status, "active");
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
        assert_eq!(cube.ensure_calls.lock().await.len(), 1);
        assert_eq!(cube.lease_calls.lock().await.len(), 1);
        assert_eq!(cube.create_calls.lock().await.len(), 1);
        assert_eq!(runner.calls.lock().await.len(), 1);
        assert_eq!(runner.calls.lock().await[0].3.as_deref(), Some("chg-1"));
    }

    /// Host-adapter provider that records every host the dispatch loop
    /// asks it to build an adapter for, then returns a single fixed inner
    /// adapter. Lets a routing test assert *which* host was selected
    /// without standing up a full SSH-remote adapter double — the inner
    /// adapter still drives the FakeCubeClient-backed lease/change/spawn.
    struct RecordingHostAdapterProvider {
        inner: Arc<dyn HostAdapter>,
        requested: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl HostAdapterProvider for RecordingHostAdapterProvider {
        async fn adapter_for(&self, host: &Host) -> Result<Arc<dyn HostAdapter>> {
            self.requested.lock().await.push(host.id.clone());
            Ok(Arc::clone(&self.inner))
        }
    }

    /// PR3 routing: an execution pinned to a registered remote host is
    /// dispatched through that host's adapter (the dispatch loop asks the
    /// provider for `zakalwe`, not `local`) and the run is attributed to
    /// the pinned host via `work_runs.host_id`.
    #[tokio::test]
    async fn pinned_execution_routes_to_remote_host_and_persists_host_id() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        // Register a remote host with spare slots so it survives the
        // free-slots gate.
        db.add_host("zakalwe", "user@zakalwe", 2, &[]).unwrap();

        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Pinned cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        // Pin the ready execution to the remote host.
        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        db.set_execution_pinned_host(&execution.id, Some("zakalwe")).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let mut coordinator_inner =
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        let provider = Arc::new(RecordingHostAdapterProvider {
            inner: coordinator_inner.host_adapter(),
            requested: Mutex::new(Vec::new()),
        });
        coordinator_inner.set_host_adapter_provider(provider.clone());
        let coordinator = Arc::new(coordinator_inner);

        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

        // The dispatch loop resolved the adapter for the pinned host —
        // and never for `local`.
        let requested = provider.requested.lock().await.clone();
        assert!(
            requested.iter().any(|h| h == "zakalwe"),
            "expected the provider to be asked for the pinned host, got {requested:?}",
        );
        assert!(
            !requested.iter().any(|h| h == "local"),
            "pinned execution must not route through local, got {requested:?}",
        );

        // The run is attributed to the pinned host.
        let run_ids = db.active_run_ids_for_execution(&execution.id).unwrap();
        assert_eq!(run_ids.len(), 1, "exactly one active run expected");
        assert_eq!(
            db.run_host(&run_ids[0]).unwrap().as_deref(),
            Some("zakalwe"),
            "work_runs.host_id must record the selected host",
        );
    }

    /// PR3 routing: an execution pinned to a host that is registered but
    /// disabled finds no eligible host. The dispatch records a
    /// `no_eligible_host` pre-start failure (leaving the row recoverable)
    /// and never starts a run.
    #[tokio::test]
    async fn pin_to_disabled_host_yields_no_eligible_host() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        db.add_host("zakalwe", "user@zakalwe", 2, &[]).unwrap();
        db.set_host_enabled("zakalwe", false).unwrap();

        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Pinned to disabled");
        db.reconcile_product_executions(&product.id).unwrap();
        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        db.set_execution_pinned_host(&execution.id, Some("zakalwe")).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        // No retry delays → a single pre-start failure is terminal, so the
        // assertion doesn't race a backoff timer.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&execution.id, None)
            .await
            .expect("worker available");
        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(result.is_err(), "no eligible host must fail the dispatch");

        // No worker run was ever started, and no cube work happened.
        assert!(db.active_run_ids_for_execution(&execution.id).unwrap().is_empty());
        assert_eq!(cube.ensure_calls.lock().await.len(), 0);
        // The failure surfaced as a `no_eligible_host` attention item.
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(
            items.iter().any(|i| i.kind == "no_eligible_host"),
            "expected a no_eligible_host attention item, got {:?}",
            items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
        );
    }

    /// The `no_eligible_host` pre-start failure used to emit NO dispatch
    /// event, so the per-execution timeline went silent after
    /// `worker_claimed` and the stall watchdog mislabelled it a
    /// `worker_claimed` stall ~30s later (the exact shape that hid the
    /// automation-pool stall). It must now emit a terminal
    /// `host_selected:error` carrying the reason, so the blocker is named
    /// in dispatch.jsonl and — because the watchdog treats any `error`
    /// outcome as terminal — it is never re-flagged as a stall.
    #[tokio::test]
    async fn no_eligible_host_emits_terminal_host_selected_error_event() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        db.add_host("zakalwe", "user@zakalwe", 2, &[]).unwrap();
        db.set_host_enabled("zakalwe", false).unwrap();

        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Pinned to disabled");
        db.reconcile_product_executions(&product.id).unwrap();
        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        db.set_execution_pinned_host(&execution.id, Some("zakalwe")).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&execution.id, None)
            .await
            .expect("worker available");
        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(result.is_err(), "no eligible host must fail the dispatch");

        let events = recording.events_for(&execution.id).await;
        let host_selected = events
            .iter()
            .find(|e| e.stage == "host_selected")
            .unwrap_or_else(|| panic!("expected a host_selected event; got {events:#?}"));
        assert_eq!(
            host_selected.outcome, "error",
            "no_eligible_host must surface as host_selected:error",
        );
        assert_eq!(
            host_selected.details.get("reason").and_then(|v| v.as_str()),
            Some("no_eligible_host"),
            "host_selected:error must name the blocker reason",
        );
        assert!(
            host_selected.error_message.is_some(),
            "host_selected:error must carry the ineligibility detail",
        );
        // Terminal for the stall watchdog (any error outcome is terminal),
        // so the silent `worker_claimed` stall can never re-present.
        assert!(
            crate::dispatch_reader::is_terminal_event(host_selected),
            "host_selected:error must be a terminal dispatch event",
        );
        // The failure short-circuited before any cube repo work.
        assert_eq!(cube.ensure_calls.lock().await.len(), 0);
    }

    /// `cube_default_workspace_root_for_test` mirrors the production
    /// helper so tests can construct a `workspace_root` value that
    /// `workspace_root_is_cube_default` would accept, without
    /// mutating process-wide env vars (which would race other tests
    /// in the same crate).
    fn cube_default_workspace_root_for_test() -> PathBuf {
        if let Some(d) = std::env::var_os("CUBE_DATA_DIR") {
            return PathBuf::from(d).join("workspaces");
        }
        if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(d).join("cube/workspaces");
        }
        let home = std::env::var_os("HOME").expect(
            "test requires HOME, CUBE_DATA_DIR, or XDG_DATA_HOME to be set so we can \
             construct a cube-default workspace_root that the helper recognises",
        );
        PathBuf::from(home).join(".local/share/cube/workspaces")
    }

    /// Q6 / Follow-up chore #8: the cold-repo probe raises an
    /// advisory `repo_cold_pool` attention item on the first dispatch
    /// against a previously-unseen URL whose cube pool config matches
    /// auto-provision defaults. Across two dispatches against the
    /// same URL only one item is written, and `cube repo list` is
    /// only called once — both dispatches still drive the execution
    /// to `running`.
    #[tokio::test]
    async fn cold_repo_probe_raises_advisory_once_across_repeated_dispatches() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let origin = "git@github.com:spinyfin/mono.git";
        let product = create_test_product_with_repo(&db, "Boss", Some(origin));
        // Two chores → two executions against the same product/URL.
        let chore_a = create_test_chore(&db, product.id.clone(), "Cleanup A");
        let chore_b = create_test_chore(&db, product.id.clone(), "Cleanup B");
        db.reconcile_product_executions(&product.id).unwrap();

        // Cube reports a single repo whose pool config exactly
        // matches the auto-provisioned defaults — `cube repo add`
        // / `cube repo configure` were never run.
        let default_repo = CubeRepoSummary {
            repo_id: "mono".to_owned(),
            origin: origin.to_owned(),
            main_branch: "main".to_owned(),
            workspace_root: cube_default_workspace_root_for_test(),
            workspace_prefix: "mono-agent-".to_owned(),
            source: None,
        };
        let cube = Arc::new(FakeCubeClient::default().with_repos(vec![default_repo]));
        // Pool size 2 so both executions can dispatch concurrently.
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(2),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();

        let exec_a = db.list_executions(Some(&chore_a.id)).unwrap().pop().unwrap();
        let exec_b = db.list_executions(Some(&chore_b.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &exec_a.id, ExecutionStatus::Running).await;
        wait_for_execution_status(db.as_ref(), &exec_b.id, ExecutionStatus::Running).await;

        // Two ensure_repo calls (one per execution), but list_repos
        // was deduplicated to exactly one round-trip.
        assert_eq!(cube.ensure_calls.lock().await.len(), 2);
        assert_eq!(*cube.list_repos_calls.lock().await, 1);

        // Exactly one advisory item across both executions. It
        // attaches to the execution that hit the probe first.
        let attn_a = db.list_attention_items(&exec_a.id).unwrap();
        let attn_b = db.list_attention_items(&exec_b.id).unwrap();
        let cold_items: Vec<_> = attn_a
            .iter()
            .chain(attn_b.iter())
            .filter(|item| item.kind == "repo_cold_pool")
            .collect();
        assert_eq!(
            cold_items.len(),
            1,
            "expected exactly one repo_cold_pool item across both executions, \
             got {} (exec_a: {} items, exec_b: {} items)",
            cold_items.len(),
            attn_a.len(),
            attn_b.len(),
        );
        let item = cold_items[0];
        assert_eq!(item.status, "open");
        assert!(
            item.body_markdown
                .contains("cube repo ensure --origin git@github.com:spinyfin/mono.git"),
            "body should name the override command verbatim; got: {}",
            item.body_markdown,
        );
        assert!(
            item.body_markdown.contains(origin),
            "body should echo the repo origin; got: {}",
            item.body_markdown,
        );
    }

    /// A repo whose cube pool config has been customised (custom
    /// `workspace_root` or `workspace_prefix`) is the steady-state we
    /// don't want to nag about. Even though it's the first dispatch
    /// in this engine's lifetime, no `repo_cold_pool` item should
    /// land.
    #[tokio::test]
    async fn cold_repo_probe_stays_silent_when_pool_is_customised() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let origin = "git@github.com:spinyfin/mono.git";
        let product = create_test_product_with_repo(&db, "Boss", Some(origin));
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let custom_repo = CubeRepoSummary {
            repo_id: "mono".to_owned(),
            origin: origin.to_owned(),
            main_branch: "main".to_owned(),
            workspace_root: PathBuf::from("/Users/operator/Documents/dev/workspaces"),
            workspace_prefix: "mono-agent-".to_owned(),
            source: None,
        };
        let cube = Arc::new(FakeCubeClient::default().with_repos(vec![custom_repo]));
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();
        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

        assert_eq!(*cube.list_repos_calls.lock().await, 1);
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(
            items.iter().all(|i| i.kind != "repo_cold_pool"),
            "no repo_cold_pool item should be raised for a customised pool; got: {:?}",
            items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn repo_has_default_pool_config_recognises_defaults_only() {
        use super::{CubeRepoSummary, repo_has_default_pool_config};
        // A repo whose every field matches the auto-provisioned
        // defaults — the case the probe should flag.
        let default_root = cube_default_workspace_root_for_test();
        let base = CubeRepoSummary {
            repo_id: "nimbus".to_owned(),
            origin: "git@github.com:myorg/nimbus.git".to_owned(),
            main_branch: "main".to_owned(),
            workspace_root: default_root.clone(),
            workspace_prefix: "nimbus-agent-".to_owned(),
            source: None,
        };
        assert!(repo_has_default_pool_config(&base));

        // A custom main_branch means the operator has touched the
        // config — stay silent.
        let mut customised = base.clone();
        customised.main_branch = "trunk".to_owned();
        assert!(!repo_has_default_pool_config(&customised));

        // `source` overlay means the user is sharing a local clone;
        // pool is explicitly configured.
        let mut with_source = base.clone();
        with_source.source = Some(PathBuf::from("/Users/dev/Documents/dev/nimbus"));
        assert!(!repo_has_default_pool_config(&with_source));

        // Custom workspace_prefix that doesn't match the auto-derived
        // `{repo_id}-agent-` shape.
        let mut custom_prefix = base.clone();
        custom_prefix.workspace_prefix = "nimbus-pool-".to_owned();
        assert!(!repo_has_default_pool_config(&custom_prefix));

        // Custom workspace_root anywhere outside cube's data dir.
        let mut custom_root = base;
        custom_root.workspace_root = PathBuf::from("/Users/dev/Documents/dev/workspaces");
        assert!(!repo_has_default_pool_config(&custom_root));
    }

    #[tokio::test]
    async fn slot_id_from_outcome_is_stamped_onto_run_agent_id() {
        // When the runner reports a real pane slot back via
        // RunOutcome.slot_id, the coordinator must overwrite the run
        // record's `agent_id` with `worker-{slot}` before recording
        // completion. This is what makes `bossctl agents list` show
        // one entry per active pane instead of collapsing every
        // dispatched run into the worker-pool placeholder.
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        // Pool has only one slot, so the worker-pool placeholder
        // would otherwise be `worker-1`. The runner reports slot 5
        // — the assertion below proves the slot value won, not the
        // pool placeholder.
        let runner = Arc::new(FakeExecutionRunner {
            slot_id: Some(5),
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.agent_id, "worker-5");
    }

    #[tokio::test]
    async fn pane_spawn_run_does_not_release_worker_pool_slot() {
        // The libghostty pane outlives the `run_execution` call —
        // PaneSpawnRunner returns Ok(WaitingHuman) the instant the
        // SpawnWorkerPane RPC completes, but the user-visible worker
        // is just getting started. If the coordinator freed the
        // WorkerPool slot at that moment, the next dispatch could
        // re-claim the slot and the app would reject the spawn with
        // SlotBusy. Outcomes that carry slot_id = Some(N) must keep
        // the slot claimed until `release_worker_pane` fires.
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            slot_id: Some(1),
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

        // Slot 1 still belongs to the (notionally) live pane. Only
        // `release_worker_pane` (driven by completion / force release
        // / shutdown) is allowed to free it.
        assert_eq!(
            coordinator.worker_pool().idle_count().await,
            0,
            "WorkerPool slot must stay claimed while the libghostty pane is alive"
        );
    }

    #[tokio::test]
    async fn release_worker_and_kick_frees_pool_slot() {
        // The deferred-release helper called from
        // `ServerState::release_worker_pane` after the pane RPC
        // returns. After it runs, the matching pool slot is idle
        // again and the next claim succeeds.
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(2),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        let claimed = coordinator
            .worker_pool()
            .claim_worker("exec-pre", None)
            .await
            .expect("pool has free slots");
        assert_eq!(coordinator.worker_pool().idle_count().await, 1);

        coordinator.release_worker_and_kick(&claimed, Some("ws-1")).await;

        assert_eq!(
            coordinator.worker_pool().idle_count().await,
            2,
            "release_worker_and_kick must return the slot to the idle pool",
        );
        // Idempotent: a second release on the same already-idle slot
        // is a no-op (the pane-spawn lifecycle can racily re-enter
        // this path from completion + chore-done).
        coordinator.release_worker_and_kick(&claimed, Some("ws-1")).await;
        assert_eq!(coordinator.worker_pool().idle_count().await, 2);
    }

    #[tokio::test]
    async fn missing_slot_id_leaves_worker_pool_placeholder_in_agent_id() {
        // Runners without a pane leave slot_id = None. The coordinator
        // must not touch agent_id in that case — the worker-pool
        // placeholder set at run-create time stays.
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.agent_id, "worker-1");
    }

    #[tokio::test]
    async fn successful_run_moves_execution_to_waiting_human_and_releases_worker() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

        let execution = db.get_execution(&execution.id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(coordinator.worker_pool().idle_count().await, 1);
        assert_eq!(db.list_attention_items(&execution.id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn start_failure_marks_execution_failed_and_releases_worker() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![]),
        );
        coordinator.kick();
        wait_for_execution_status(
            db.as_ref(),
            &db.list_executions(Some(&chore.id)).unwrap()[0].id,
            ExecutionStatus::Failed,
        )
        .await;

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        assert_eq!(execution.status, ExecutionStatus::Failed);
        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.status, "failed");
        assert_eq!(run.error_text.as_deref(), Some("cube workspace lease failed"));
        assert_eq!(coordinator.worker_pool().idle_count().await, 1);
    }

    /// Operators previously saw lease failures show up as a vague
    /// "no slot available" because the engine swallowed the cube
    /// stderr. The dispatcher now logs the full anyhow chain at
    /// `tracing::error!` *before* `record_start_failure` writes its
    /// own warn line, so the verbatim cube stderr lands in the
    /// engine log. Stale-working-copy recovery is owned by cube
    /// (cube PR #254); this test only pins the loud-logging
    /// contract.
    #[tokio::test]
    async fn lease_failure_logs_cube_stderr_at_error_before_recording_failure() {
        let buffer = log_capture::install();
        let starting_offset = buffer.lock().len();

        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        // No retries: go straight to permanent failure so the test does
        // not have to wait through exponential backoff delays.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![]),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

        // Slice out only the bytes written after the test started so
        // we don't trip over events emitted by other parallel tests
        // sharing the same global subscriber.
        let captured = String::from_utf8_lossy(&buffer.lock()[starting_offset..]).to_string();
        let our_lines: Vec<&str> = captured.lines().filter(|line| line.contains(&execution_id)).collect();
        assert!(
            !our_lines.is_empty(),
            "expected captured log lines for execution {execution_id}, got nothing.\n\
             Full slice was:\n{captured}"
        );

        let error_idx = our_lines
            .iter()
            .position(|line| line.contains("ERROR") && line.contains("cube workspace lease attempt failed"))
            .unwrap_or_else(|| {
                panic!(
                    "expected a tracing::error! log for the cube lease failure;\n\
                     captured lines for this execution were:\n{:#?}",
                    our_lines
                )
            });
        let error_line = our_lines[error_idx];
        // The fake's lease error message *is* the simulated cube
        // stderr; the engine must surface it verbatim rather than
        // truncating or pattern-matching.
        assert!(
            error_line.contains("cube workspace lease failed"),
            "error log line must include the cube stderr verbatim, got:\n{error_line}"
        );

        let warn_idx = our_lines
            .iter()
            .position(|line| line.contains("WARN") && line.contains("recorded execution start failure"))
            .unwrap_or_else(|| {
                panic!(
                    "expected a tracing::warn! log from record_start_failure;\n\
                     captured lines for this execution were:\n{:#?}",
                    our_lines
                )
            });

        assert!(
            error_idx < warn_idx,
            "error log must precede record_start_failure's warn log; \
             got error at {error_idx}, warn at {warn_idx}.\n\
             Captured lines:\n{:#?}",
            our_lines
        );
    }

    /// Shared per-process tracing capture used by tests that need
    /// to assert on log output. We can't install a per-test
    /// subscriber because cargo runs library tests in parallel
    /// threads of the same process and `set_global_default`
    /// rejects a second installer. Tests that opt in slice the
    /// shared buffer by execution_id (which is unique per test) to
    /// isolate their own events.
    mod log_capture {
        use std::io;
        use std::sync::{Arc, Mutex, OnceLock};

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        pub(super) struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

        impl SharedBuffer {
            pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
                self.0.lock().expect("shared log buffer poisoned")
            }
        }

        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl io::Write for SharedWriter {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .expect("shared log buffer poisoned")
                    .extend_from_slice(data);
                Ok(data.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        struct SharedMakeWriter(Arc<Mutex<Vec<u8>>>);

        impl<'a> MakeWriter<'a> for SharedMakeWriter {
            type Writer = SharedWriter;

            fn make_writer(&'a self) -> Self::Writer {
                SharedWriter(self.0.clone())
            }
        }

        pub(super) fn install() -> SharedBuffer {
            static BUFFER: OnceLock<SharedBuffer> = OnceLock::new();
            BUFFER
                .get_or_init(|| {
                    let buffer = SharedBuffer(Arc::new(Mutex::new(Vec::new())));
                    let subscriber = tracing_subscriber::fmt()
                        .with_writer(SharedMakeWriter(buffer.0.clone()))
                        .with_ansi(false)
                        .with_target(false)
                        .with_max_level(tracing::Level::TRACE)
                        .finish();
                    // Tolerate the "already set" race: another test
                    // binary or a stray init in the same process
                    // shouldn't sink the suite. The capture only
                    // works if our subscriber wins, but if it
                    // doesn't, the assertions below will fail
                    // loudly with a clear "no captured lines"
                    // message.
                    let _ = tracing::subscriber::set_global_default(subscriber);
                    buffer
                })
                .clone()
        }
    }

    /// Regression for the silent-release dispatch failure: when the
    /// pane-spawn step inside `run_execution` fails — libghostty IPC
    /// drop, prompt composition error, runner panic, all surface
    /// here as `Err(_)` from `ExecutionRunner::run_execution` — the
    /// coordinator MUST raise a `WorkAttentionItem` AND emit a
    /// structured `pane_spawned` error event. Before this fix
    /// landed, the run flipped to `failed` and the lease was
    /// released, but nothing surfaced to `bossctl agents list` or
    /// the kanban view; operators had nothing to chase. The
    /// `RecordingDispatchEventSink` below asserts the stage timeline
    /// reaches `pane_spawned: error`; the `list_attention_items`
    /// assertion proves the WorkAttentionItem made it to disk.
    #[tokio::test]
    async fn pane_spawn_failure_raises_attention_item_and_dispatch_event() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            fail: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

        // The execution went all the way through the lease + change
        // creation. `rescan_active_dispatch_after_release` will
        // re-queue the chore (pre-existing retry behavior, since
        // `start_execution_run` flipped tasks.status to `active`
        // before the spawn failed), so cube fakes may be invoked
        // multiple times — pin only "at least once each".
        assert!(!cube.lease_calls.lock().await.is_empty());
        assert!(!cube.create_calls.lock().await.is_empty());
        // The lease is released after the pane-spawn failure — before
        // the fix, this release was the *only* observable signal that
        // anything went wrong.
        assert!(cube.release_calls.lock().await.iter().any(|id| id == "lease-1"));

        // Loud signal #1: the WorkAttentionItem is what surfaces in
        // the kanban "Attention" lane and through `ListAttentionItems`.
        // The exact count varies — once the run finishes_execution_run
        // with `failed`, `rescan_active_dispatch_after_release` will
        // see the chore is still in `active` status (auto-advanced
        // when `start_execution_run` committed) and re-queue another
        // ready execution, which fails again. That retry behavior is
        // pre-existing; this test only pins the loud-failure contract:
        // every failed pane spawn raises exactly one attention item.
        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert!(
            !attention_items.is_empty(),
            "pane-spawn failure must raise at least one attention item; got nothing",
        );
        let first = &attention_items[0];
        assert_eq!(first.kind, "pane_spawn_failed");
        assert!(
            first.body_markdown.contains("worker pane never came up"),
            "attention body should describe the failure mode; got {:?}",
            first.body_markdown,
        );
        assert!(
            first.body_markdown.contains("worker prompt failed"),
            "attention body should include the original error; got {:?}",
            first.body_markdown,
        );

        // Loud signal #2: a structured `pane_spawned: error` event in
        // the dispatch stream, so external tooling can flag it
        // without scanning tracing logs.
        let events = recording.events_for(&execution_id).await;
        let pane_event = events
            .iter()
            .find(|event| event.stage == "pane_spawned" && event.outcome == "error")
            .unwrap_or_else(|| panic!("expected a pane_spawned:error event for {execution_id}; got {events:#?}"));
        assert!(
            pane_event
                .error_message
                .as_deref()
                .is_some_and(|msg| msg.contains("worker prompt failed")),
            "pane_spawned event must include the underlying error; got {:?}",
            pane_event.error_message,
        );
        // The stage timeline before the failure should also be
        // visible — request_recorded, worker_claimed, cube stages,
        // run_started — so an operator can confirm dispatch did get
        // through every earlier handoff. `cube_workspace_lease_attempted`
        // sits between `cube_repo_ensured` and `cube_workspace_leased`
        // and pins what the engine asked cube to do (preferred
        // workspace, fallback policy) for diagnose visibility.
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
        for expected in [
            "request_recorded",
            "worker_claimed",
            "cube_repo_ensured",
            "cube_workspace_lease_attempted",
            "cube_workspace_leased",
            "cube_change_created",
            "run_started",
            "pane_spawned",
        ] {
            assert!(
                stages.contains(&expected),
                "stage `{expected}` missing from dispatch timeline; got {stages:?}",
            );
        }
    }

    /// T267 regression (outcome 3): a slow `SpawnWorkerPane` ack that
    /// nonetheless spawned the pane must NOT be treated as a spawn
    /// failure. The real `PaneSpawnRunner` now converts the ack timeout
    /// into a PROVISIONAL spawn (waiting_human + slot retained); the fake
    /// returns that same outcome. The coordinator must then:
    ///   - keep the execution TRACKED in `waiting_human` (non-terminal),
    ///   - NOT release the cube workspace lease (a live pane may occupy it),
    ///   - NOT mark the run failed or emit a `pane_spawned: error` event,
    ///   - NOT leave a duplicate execution behind (the incident's second
    ///     worker came from the failed+demoted work item being re-dispatched).
    ///
    /// The Timeout→provisional conversion itself is unit-tested in
    /// `spawn_flow`; this pins the coordinator-side contract.
    #[tokio::test]
    async fn ack_timeout_provisional_spawn_is_tracked_not_failed_or_duplicated() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            ack_timed_out: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;

        // Tracked, not failed — the pane may be live and doing work.
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
        // The lease is retained on the tracked row (a provisional pane may
        // be occupying the workspace) — clearing it would let the workspace
        // be re-leased out from under a live worker.
        assert_eq!(
            execution.cube_lease_id.as_deref(),
            Some("lease-1"),
            "the tracked provisional execution must keep its cube lease",
        );

        // No release-while-occupied: the coordinator must not hand the
        // workspace back to cube for a provisional spawn.
        let releases = cube.release_calls.lock().await.clone();
        assert!(
            !releases.iter().any(|id| id == "lease-1"),
            "cube lease must NOT be released for a provisional (ack-timeout) spawn; releases: {releases:?}",
        );

        // No duplicate dispatch: exactly one execution exists for the
        // chore. In the incident, the failed+demoted work item spawned a
        // second worker; keeping the run tracked (is_live) makes the
        // orphan-active sweep skip it, so no duplicate is created.
        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(
            executions.len(),
            1,
            "a provisional spawn must not leave a duplicate execution behind; got {executions:#?}",
        );

        // The dispatch stream records a normal `pane_spawned: ok`, never a
        // `pane_spawned: error` — this was not a failure.
        let events = recording.events_for(&execution_id).await;
        assert!(
            events.iter().any(|e| e.stage == "pane_spawned" && e.outcome == "ok"),
            "expected a pane_spawned:ok event for the provisional spawn; got {events:#?}",
        );
        assert!(
            !events.iter().any(|e| e.stage == "pane_spawned" && e.outcome == "error"),
            "a provisional spawn must NOT emit a pane_spawned:error event; got {events:#?}",
        );
    }

    /// When a pane-spawn fails for an `automation_triage` execution, the
    /// matching `automation_runs` row must be flipped from the scheduler's
    /// pessimistic `failed_will_retry` to `failed_gave_up`. Without this,
    /// a non-self-healing failure (e.g. invalid worker_id format) leaves
    /// the Automations tab showing a pending retry that will never happen.
    #[tokio::test]
    async fn pane_spawn_failure_finalises_automation_run_to_failed_gave_up() {
        use crate::work::{AutomationFireRecord, CreateAutomationInput};
        use boss_protocol::{AUTOMATION_OUTCOME_FAILED_GAVE_UP, AutomationTrigger};

        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let automation = db
            .create_automation(CreateAutomationInput {
                product_id: product.id.clone(),
                name: "Nightly check".to_owned(),
                repo_remote_url: None,
                trigger: AutomationTrigger::Schedule {
                    cron: "0 2 * * *".to_owned(),
                    timezone: "UTC".to_owned(),
                },
                standing_instruction: "audit the repo".to_owned(),
                open_task_limit: 1,
                catch_up_window_secs: None,
                enabled: true,
                created_via: None,
            })
            .unwrap();

        // Create the triage execution that the scheduler would normally create.
        let triage_exec = db
            .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
            .unwrap();

        // Record the automation run at the pessimistic `failed_will_retry`
        // that the scheduler stamps when it dispatches (schedule advanced).
        let scheduled_for: i64 = 1_000_000;
        db.record_automation_run_and_advance(
            AutomationFireRecord::builder()
                .automation_id(automation.id.clone())
                .scheduled_for(scheduled_for)
                .started_at(scheduled_for)
                .outcome(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                .triage_execution_id(triage_exec.id.clone())
                .build(),
        )
        .unwrap();

        // Confirm the run is `failed_will_retry` before we touch the coordinator.
        let run_before = db
            .automation_run_for_triage_execution(&triage_exec.id)
            .unwrap()
            .expect("automation run must exist");
        assert_eq!(
            run_before.outcome, "failed_will_retry",
            "precondition: scheduler stamps failed_will_retry on dispatch"
        );

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            fail: true,
            ..FakeExecutionRunner::default()
        });
        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        // Wire in a 1-slot automation pool so the triage execution gets
        // dispatched (it targets the automation pool, not the main pool).
        coord.set_automation_pool(WorkerPool::new_automation(1));
        let coordinator = Arc::new(coord);
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &triage_exec.id, ExecutionStatus::Failed).await;

        // The automation run must now show `failed_gave_up`, not `failed_will_retry`.
        let run_after = db
            .automation_run_for_triage_execution(&triage_exec.id)
            .unwrap()
            .expect("automation run must still exist");
        assert_eq!(
            run_after.outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP,
            "pane-spawn failure must finalize automation run to failed_gave_up; \
             got {:?} — the Automations tab would show a phantom pending retry",
            run_after.outcome,
        );
    }

    /// A `pr_review` spawn failure must NOT demote the work item back to
    /// `todo`. The PrReview exception in the pane-spawn failure handler
    /// skips `demote_active_work_item_to_todo` so the kanban card stays
    /// in its current state (here: `active`, as it would be just after an
    /// implementation run that produced a PR). The symmetrical chore path
    /// (`pane_spawn_failure_raises_attention_item_and_dispatch_event`) DOES
    /// demote — this test pins the carve-out in the opposite direction.
    #[tokio::test]
    async fn pane_spawn_failure_for_pr_review_does_not_demote_work_item() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        // Create the chore with autostart=false so `rescan_active_dispatch`
        // never re-queues it after the PrReview execution fails. Only the
        // PrReview execution we inject below reaches the dispatcher.
        let chore = create_test_chore_manual(&db, product.id.clone(), "Reviewed chore");

        // Simulate the post-implementation state: the chore is `active`
        // (auto-advanced by `start_execution_run` when the implementation
        // run began) and a PrReview execution was just enqueued by the
        // completion handler. `autostart = 0` is already set, so the
        // rescan sweep skips this chore even after the review pool frees up.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'active', updated_at = '1' WHERE id = ?1",
                rusqlite::params![chore.id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO work_executions
                   (id, work_item_id, kind, status, repo_remote_url, priority, created_at)
                 VALUES (?1, ?2, ?3, 'ready', ?4, 0, '1')",
                rusqlite::params![
                    "exec-pr-review-1",
                    chore.id,
                    EXECUTION_KIND_PR_REVIEW,
                    "git@github.com:spinyfin/mono.git"
                ],
            )
            .unwrap();
        }

        let cube = Arc::new(FakeCubeClient::default());
        // fail=true simulates the pane-spawn failure path (libghostty IPC
        // error, prompt composition failure, etc.) for the pr_review
        // execution. The coordinator must NOT call demote_active_work_item_to_todo.
        let runner = Arc::new(FakeExecutionRunner {
            fail: true,
            ..FakeExecutionRunner::default()
        });
        // The coordinator already has a review pool (DEFAULT_REVIEW_POOL_SIZE
        // slots) by default — no extra setup needed; the PrReview execution
        // routes there automatically.
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), "exec-pr-review-1", ExecutionStatus::Failed).await;

        let item = db.get_work_item(&chore.id).unwrap();
        let status = match item {
            WorkItem::Chore(t) | WorkItem::Task(t) => t.status,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_ne!(
            status,
            TaskStatus::Todo,
            "pr_review spawn failure must not demote the work item to `todo`; \
             got `{status}` — the skip-demote guard for pr_review is absent or broken",
        );
    }

    /// Regression for the automation-pool dispatch stall (2026-06-03):
    /// an `automation_triage` execution must drive PAST `worker_claimed`
    /// — through host selection and the cube repo-ensure handoff — to the
    /// `cube_workspace_lease_attempted` stage, exactly like every other
    /// pool. The original symptom was the execution sitting silently at
    /// `worker_claimed` with no further dispatch event until the stall
    /// watchdog reaped it ~30s later. This test pins that the
    /// previously-silent gap now emits `host_selected:ok` and
    /// `cube_repo_ensure_attempted`, and that the lease stage is reached.
    #[tokio::test]
    async fn automation_triage_execution_advances_past_worker_claimed_to_lease() {
        use crate::work::CreateAutomationInput;
        use boss_protocol::AutomationTrigger;

        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let automation = db
            .create_automation(CreateAutomationInput {
                product_id: product.id.clone(),
                name: "Nightly check".to_owned(),
                repo_remote_url: None,
                trigger: AutomationTrigger::Schedule {
                    cron: "0 2 * * *".to_owned(),
                    timezone: "UTC".to_owned(),
                },
                standing_instruction: "audit the repo".to_owned(),
                open_task_limit: 1,
                catch_up_window_secs: None,
                enabled: true,
                created_via: None,
            })
            .unwrap();
        let triage_exec = db
            .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner::default());
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone());
        // Wire a 1-slot automation pool so the triage execution (which
        // targets the automation pool, not the main pool) is dispatched.
        coord.set_automation_pool(WorkerPool::new_automation(1));
        let coordinator = Arc::new(coord);
        coordinator.kick();

        // Poll the dispatch stream directly rather than a specific
        // execution status: the contract under test is "advances to the
        // lease stage", independent of the final run state.
        let mut reached_lease = false;
        for _ in 0..200 {
            let events = recording.events_for(&triage_exec.id).await;
            if events.iter().any(|e| e.stage == "cube_workspace_lease_attempted") {
                reached_lease = true;
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        let events = recording.events_for(&triage_exec.id).await;
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
        assert!(
            reached_lease,
            "automation execution never reached `cube_workspace_lease_attempted` \
             (stalled at worker_claimed?); timeline was {stages:?}",
        );

        // The previously-silent claimed -> repo-ensure handoff now emits
        // explicit milestones.
        for expected in [
            "worker_claimed",
            "host_selected",
            "cube_repo_ensure_attempted",
            "cube_workspace_lease_attempted",
        ] {
            assert!(
                stages.contains(&expected),
                "automation execution must advance through `{expected}`; got {stages:?}",
            );
        }

        // Host selection resolved successfully — it did not fail out.
        let host_selected = events
            .iter()
            .find(|e| e.stage == "host_selected")
            .expect("host_selected event present");
        assert_eq!(
            host_selected.outcome, "ok",
            "automation host selection must succeed; got {host_selected:?}",
        );

        // The watchdog signature we are fixing must be absent.
        assert!(
            !stages.contains(&"stage_stalled"),
            "automation execution must not stall; got {stages:?}",
        );
    }

    /// Regression for the regular-pool dispatch stall (T1849): a
    /// `revision_implementation` execution (main pool) must drive PAST
    /// `worker_claimed` — through host selection and the cube repo-ensure
    /// handoff — to `cube_workspace_lease_attempted`, exactly like the
    /// automation pool. The original symptom was the three early-exit guards
    /// in `schedule_execution` (redundant-spawn, chain-serializer,
    /// gating-prereqs) returning `Err` without emitting any dispatch event,
    /// so the timeline sat at `worker_claimed/ok` until the stall watchdog
    /// fired ~30s later and the orphan sweep abandoned the execution.
    #[tokio::test]
    async fn revision_implementation_execution_advances_past_worker_claimed_to_lease() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        // autostart=false so the reconcile sweep never enqueues a second
        // execution in parallel — only the one we inject reaches the dispatcher.
        let chore = create_test_chore_manual(&db, product.id.clone(), "Impl chore");
        let impl_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner::default());
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone()),
        );
        coordinator.kick();

        // Poll the dispatch stream directly — the contract is "advances to
        // the lease stage", independent of the final run state.
        let mut reached_lease = false;
        for _ in 0..200 {
            let events = recording.events_for(&impl_exec.id).await;
            if events.iter().any(|e| e.stage == "cube_workspace_lease_attempted") {
                reached_lease = true;
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        let events = recording.events_for(&impl_exec.id).await;
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
        assert!(
            reached_lease,
            "revision_implementation execution never reached `cube_workspace_lease_attempted` \
             (stalled at worker_claimed?); timeline was {stages:?}",
        );

        // The previously-silent gap must now emit explicit milestones.
        for expected in [
            "worker_claimed",
            "host_selected",
            "cube_repo_ensure_attempted",
            "cube_workspace_lease_attempted",
        ] {
            assert!(
                stages.contains(&expected),
                "revision_implementation execution must advance through `{expected}`; got {stages:?}",
            );
        }

        // Host selection resolved successfully — it must not have failed out.
        let host_selected = events
            .iter()
            .find(|e| e.stage == "host_selected")
            .expect("host_selected event present");
        assert_eq!(
            host_selected.outcome, "ok",
            "revision_implementation host selection must succeed; got {host_selected:?}",
        );

        // The stall-watchdog signature we are fixing must be absent.
        assert!(
            !stages.contains(&"stage_stalled"),
            "revision_implementation execution must not stall; got {stages:?}",
        );
    }

    /// The `pane_spawned: ok` event must carry the resolved spawn
    /// knobs (effort level, claude effort value, model) so
    /// `bossctl dispatch diagnose <exec-id>` can answer "what did
    /// this worker actually launch with" — design §Q2 ("surfaces the
    /// chosen model, effort value, and level on the dispatch
    /// instrumentation stream"). The fake runner reports a synthetic
    /// `SpawnConfig`; this test pins that the coordinator forwards
    /// it into the event's `details.spawn_config` field.
    #[tokio::test]
    async fn pane_spawned_event_carries_spawn_config_details() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Trivial chore")
                    .effort_level(crate::work::EffortLevel::Trivial)
                    .build(),
            )
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            slot_id: Some(1),
            spawn_config: Some(crate::effort::SpawnConfig {
                effort_level: Some(crate::work::EffortLevel::Trivial),
                claude_effort: Some("low"),
                model: "sonnet".to_owned(),
                driver: crate::effort::ENGINE_DEFAULT_DRIVER.to_owned(),
                prompt_addendum: None,
                model_floor: None,
            }),
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner)
                .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;

        let events = recording.events_for(&execution_id).await;
        let pane_event = events
            .iter()
            .find(|event| event.stage == "pane_spawned" && event.outcome == "ok")
            .unwrap_or_else(|| panic!("expected pane_spawned:ok event for {execution_id}; got {events:#?}"));
        let spawn = pane_event.details.get("spawn_config").unwrap_or_else(|| {
            panic!(
                "pane_spawned event missing spawn_config in details: {:?}",
                pane_event.details
            )
        });
        assert_eq!(spawn["effort_level"], "trivial");
        assert_eq!(spawn["claude_effort"], "low");
        assert_eq!(spawn["model"], "sonnet");
        assert_eq!(spawn["prompt_addendum_applied"], false);
    }

    /// Cube lease failures also need the loud-failure contract: a
    /// `WorkAttentionItem` AND a structured event. This pins both —
    /// the older `lease_failure_logs_cube_stderr_at_error_before_recording_failure`
    /// test only asserts the tracing log shape.
    #[tokio::test]
    async fn cube_lease_failure_raises_attention_item_and_dispatch_event() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        // No retries: go straight to permanent failure so the test does
        // not have to wait through exponential backoff delays.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![])
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "cube lease failure must raise exactly one attention item",
        );
        assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
        assert!(attention_items[0].body_markdown.contains("cube workspace lease failed"));

        let events = recording.events_for(&execution_id).await;
        // The lease attempt event is emitted before the call, so the
        // timeline pins what the engine *intended* to do even when
        // cube refuses.
        let attempt_event = events
            .iter()
            .find(|event| event.stage == "cube_workspace_lease_attempted")
            .expect("cube_workspace_lease_attempted event missing");
        assert_eq!(attempt_event.outcome, "ok");
        assert_eq!(
            attempt_event.details.get("attempt").and_then(|v| v.as_u64()),
            Some(1),
            "first attempt event should carry attempt=1; got {:?}",
            attempt_event.details,
        );

        let lease_failed = events
            .iter()
            .find(|event| event.stage == "cube_workspace_lease_failed")
            .expect("cube_workspace_lease_failed event missing");
        assert_eq!(lease_failed.outcome, "error");
        assert!(
            lease_failed
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("cube workspace lease failed")),
            "lease_failed event must carry the verbatim cube error; got {:?}",
            lease_failed.error_message,
        );
        assert_eq!(
            lease_failed.details.get("reason").and_then(|v| v.as_str()),
            Some("cube_error"),
            "lease_failed event must classify reason; got {:?}",
            lease_failed.details,
        );

        // The success event must NOT be emitted, and the timeline
        // must NOT include later stages — dispatch bailed at the
        // lease step.
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
        assert!(
            !stages.contains(&"cube_workspace_leased"),
            "cube_workspace_leased (success) must not appear when lease fails; got {stages:?}",
        );
        assert!(!stages.contains(&"cube_change_created"));
        assert!(!stages.contains(&"run_started"));
        assert!(!stages.contains(&"pane_spawned"));
    }

    /// The anaplian failure-mode A produced an opaque `reason: "cube_error"`
    /// even though the engine held the real cause (cube granted the lease,
    /// then a setup step exited non-zero). Pin that a typed `CubeCliError`
    /// now propagates its exit code + stderr into the `cube_workspace_lease_failed`
    /// event `details` so the failure is attributable in one read.
    #[tokio::test]
    async fn cube_lease_failure_surfaces_exit_code_and_stderr_in_details() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease_with_cube_cli_error: true,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![])
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

        let events = recording.events_for(&execution_id).await;
        let lease_failed = events
            .iter()
            .find(|event| event.stage == "cube_workspace_lease_failed")
            .expect("cube_workspace_lease_failed event missing");
        // The structured exit code is now attributable without parsing.
        assert_eq!(
            lease_failed.details.get("cube_exit_code").and_then(|v| v.as_i64()),
            Some(1),
            "lease_failed must carry the cube exit code; got {:?}",
            lease_failed.details,
        );
        // The real cause (the setup-step stderr) rides the event, not just
        // the flattened error_message.
        assert!(
            lease_failed
                .details
                .get("cube_stderr")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.contains("copy-config-secrets")),
            "lease_failed must carry the cube stderr; got {:?}",
            lease_failed.details,
        );
        assert_eq!(
            lease_failed.details.get("cube_host").and_then(|v| v.as_str()),
            Some("anaplian"),
        );
        // The verbatim message is still preserved for humans.
        assert!(
            lease_failed
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("copy-config-secrets")),
        );
    }

    /// `cube repo ensure` failures used to be recorded as
    /// `cube_repo_ensured` with `outcome=error` — a success-shaped stage
    /// name with an error attached, which is exactly how the anaplian
    /// incident's `command not found: cube` failure hid in plain sight in
    /// `dispatch.jsonl` for 12 consecutive attempts. Pin that the failure
    /// now emits its own terminal `cube_repo_ensure_failed:error` stage,
    /// and that the success-shaped `cube_repo_ensured` stage never
    /// appears at all when the ensure call fails.
    #[tokio::test]
    async fn cube_repo_ensure_failure_emits_dedicated_failed_stage() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = create_test_chore(&db, product.id.clone(), "Ensure Failure");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_ensure: true,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        // No retries: go straight to permanent failure so the test does
        // not have to wait through backoff delays.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![])
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

        let events = recording.events_for(&execution_id).await;
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();

        let failed_event = events
            .iter()
            .find(|event| event.stage == "cube_repo_ensure_failed")
            .unwrap_or_else(|| panic!("cube_repo_ensure_failed event missing; got {stages:?}"));
        assert_eq!(failed_event.outcome, "error");
        assert!(
            failed_event
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("cube repo ensure failed")),
            "failed event must carry the verbatim cube error; got {:?}",
            failed_event.error_message,
        );

        // The success-shaped stage must never appear for a failed attempt
        // — that ambiguity is exactly the bug this split fixes.
        assert!(
            !stages.contains(&"cube_repo_ensured"),
            "cube_repo_ensured (success-shaped) must not appear when ensure fails; got {stages:?}",
        );
        assert!(!stages.contains(&"cube_workspace_lease_attempted"));
        assert!(!stages.contains(&"run_started"));
        assert!(!stages.contains(&"pane_spawned"));
    }

    /// The "failing to start" vs. "waiting for a slot" ambiguity this
    /// bounce closes: a chore whose lease keeps failing (e.g. the
    /// `jj bookmark set pr/<n> … refusing to move backwards` incident)
    /// must not be left silently looping — the loop is over, and the
    /// operator must be able to see it's broken and why straight from
    /// the kanban card (`dispatch_failed_reason` / `dispatch_failed_error`),
    /// not just from a `WorkAttentionItem` a separate list call surfaces.
    #[tokio::test]
    async fn cube_lease_failure_bounces_work_item_to_backlog_with_error() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();
        assert!(
            match db.get_work_item(&chore.id).unwrap() {
                WorkItem::Chore(t) | WorkItem::Task(t) => t.autostart,
                other => panic!("expected chore, got {other:?}"),
            },
            "autostart must start true — otherwise the bounce assertion below is vacuous",
        );

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        // No retries: go straight to permanent failure.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![]),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

        let task = match db.get_work_item(&chore.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(
            task.status.as_str(),
            "todo",
            "a chore that fails to start must bounce back to Backlog, not strand in Doing",
        );
        assert!(
            !task.autostart,
            "autostart must be cleared so the card renders as parked in Backlog, \
             not as a phantom \"waiting for a slot\" card",
        );
        assert_eq!(
            task.dispatch_failed_reason.as_deref(),
            Some("cube_workspace_lease_failed"),
            "the failure reason must be stamped on the task for the kanban card to render",
        );
        assert_eq!(
            task.dispatch_failed_error.as_deref(),
            Some("cube workspace lease failed"),
            "the underlying cube error must be stamped on the task, not just buried in an attention item",
        );
        assert!(task.dispatch_failed_at.is_some());

        // A deliberate retry (mirroring a kanban drag or `bossctl work
        // start`) must clear the stale error — the card shouldn't keep
        // showing last time's failure once a fresh attempt is under way.
        db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();
        let retried_task = match db.get_work_item(&chore.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(retried_task.dispatch_failed_reason, None);
        assert_eq!(retried_task.dispatch_failed_error, None);
        assert_eq!(retried_task.dispatch_failed_at, None);
    }

    /// Pre-start failures (cube lease error, cube ensure error, etc.) should
    /// be retried automatically before surfacing to the operator.
    ///
    /// This test uses zero-length backoff delays and a single retry slot so
    /// it runs quickly. It verifies:
    /// 1. A single pre-start failure resets the execution to `ready` (not
    ///    `failed`) and `pre_start_failure_count` is incremented.
    /// 2. A second failure (after retry) permanently marks the execution
    ///    `failed` and surfaces an attention item.
    /// 3. Only one execution row exists (no sibling rows).
    #[tokio::test]
    async fn pre_start_failure_retries_then_permanently_fails() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Retry Chore");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        // One retry (two attempts total), immediate backoff.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![Duration::ZERO]),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        coordinator.kick();
        // Wait for permanent failure — after 1 retry (2 total attempts)
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Failed);
        assert_eq!(
            execution.pre_start_failure_count, 2,
            "expected 2 pre-start failures (initial + 1 retry); got {}",
            execution.pre_start_failure_count
        );

        let runs = db.list_runs(&execution_id).unwrap();
        assert_eq!(
            runs.len(),
            2,
            "expected 2 run rows (one per attempt); got {}",
            runs.len()
        );
        assert!(runs.iter().all(|r| r.status == "failed"));

        // Exactly one execution row — retries reuse the same row.
        let all_executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(
            all_executions.len(),
            1,
            "retries must not create sibling execution rows; got {}",
            all_executions.len()
        );

        // Permanent failure surfaces exactly one attention item.
        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "permanent pre-start failure must raise exactly one attention item"
        );
        assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
    }

    /// Pre-start retry: when the FIRST attempt fails but a second succeeds,
    /// the execution reaches `running` and only one execution row is created.
    #[tokio::test]
    async fn pre_start_failure_retries_and_succeeds_on_second_attempt() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Retry Then Succeed");
        db.reconcile_product_executions(&product.id).unwrap();

        // `lease_workspace_with_fallback` makes two `lease_workspace`
        // calls per dispatch attempt (primary + `any_free` fallback).
        // Fail both calls in the first attempt so the retry path
        // actually triggers; calls 3+ succeed.
        let cube = Arc::new(FakeCubeClient {
            fail_first_n_leases: 2,
            ..FakeCubeClient::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                // pending=true keeps the execution in `running` so we can
                // assert on it without racing against the WaitingHuman
                // transition.
                Arc::new(FakeExecutionRunner {
                    pending: true,
                    ..FakeExecutionRunner::default()
                }),
            )
            .with_pre_start_retry_delays(vec![Duration::ZERO]),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        coordinator.kick();
        // On the retry the lease succeeds → execution reaches `running`.
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;

        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Running);
        assert_eq!(
            execution.pre_start_failure_count, 1,
            "expected exactly 1 pre-start failure before the successful attempt; got {}",
            execution.pre_start_failure_count
        );

        // Only the one failed run row (from the initial attempt) + the active run.
        let runs = db.list_runs(&execution_id).unwrap();
        assert_eq!(
            runs.len(),
            2,
            "expected 1 failed run + 1 active run; got {}",
            runs.len()
        );

        // No attention items — the retry succeeded.
        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert!(
            attention_items.is_empty(),
            "successful retry must not surface an attention item"
        );

        // Exactly one execution row.
        let all_executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(all_executions.len(), 1);
    }

    /// When `preferred_workspace_id` is set and cube refuses that workspace,
    /// the engine must NOT fall back to any other workspace — doing so would
    /// silently lose state continuity (the resuming worker needs that specific
    /// workspace). The dispatch must fail so the scheduler can retry with
    /// the correct workspace later.
    #[tokio::test]
    async fn lease_with_prefer_set_does_not_fall_back_when_refused() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .preferred_workspace_id("mono-agent-003")
                .build(),
        )
        .unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease_when_prefer_set: true,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        // No retries: go straight to permanent failure to avoid backoff
        // delays and to keep the lease-call assertion at exactly 1.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![])
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

        // Exactly one cube lease invocation: the engine must not retry
        // with a different workspace when a preferred workspace is set.
        let calls = cube.lease_calls.lock().await;
        assert_eq!(
            calls.len(),
            1,
            "engine must not retry when prefer is set; got {:?}",
            calls
        );
        assert_eq!(calls[0].2.as_deref(), Some("mono-agent-003"));
        drop(calls);

        let events = recording.events_for(&execution_id).await;
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();

        let attempt_events: Vec<&crate::dispatch_events::DispatchEvent> = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_attempted")
            .collect();
        assert_eq!(
            attempt_events.len(),
            1,
            "expected exactly one lease_attempted event; got stages {stages:?}"
        );
        assert_eq!(
            attempt_events[0]
                .details
                .get("prefer_workspace_id")
                .and_then(|v| v.as_str()),
            Some("mono-agent-003"),
        );
        assert_eq!(
            attempt_events[0]
                .details
                .get("fallback_policy")
                .and_then(|v| v.as_str()),
            Some("none"),
            "policy must be none when prefer is set — no silent workspace swap",
        );

        // Execution must fail, not succeed on a different workspace.
        assert!(
            !stages.contains(&"cube_workspace_leased"),
            "cube_workspace_leased must not appear; engine must not land on a different workspace; got {stages:?}",
        );

        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "terminal lease failure must raise exactly one attention item",
        );
        assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
    }

    /// Issue #962 -- the UI-crash resume reclaims a stale lease.
    ///
    /// A prior worker (the dead execution) was leased into
    /// `mono-agent-003` and then orphaned by the startup reaper, which
    /// preserved its `cube_lease_id` / `cube_workspace_id`. Cube still
    /// reports that workspace `leased` to the dead `lease-dead`. When the
    /// hard-prefer resume dispatches, the coordinator must force-release
    /// the dead lease first so the `--prefer` re-lease can succeed and
    /// recover the in-flight checkout -- instead of failing the resume
    /// and stranding the local work.
    #[tokio::test]
    async fn hard_prefer_resume_reclaims_stale_lease_then_leases() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Resume me");
        db.reconcile_product_executions(&product.id).unwrap();
        // autostart=false means reconcile won't auto-create an execution;
        // request one explicitly to seed the dead-predecessor record.
        db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();

        // Dead predecessor: started a run on mono-agent-003 with
        // lease-dead, then orphaned (lease columns preserved).
        let dead_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        db.start_execution_run(
            &dead_id,
            "agent-dead",
            "mono",
            "lease-dead",
            "mono-agent-003",
            "/tmp/mono-agent-003",
        )
        .unwrap();
        db.mark_execution_orphaned(&dead_id, "ui crash").unwrap();

        // Resume execution: hard prefer back onto mono-agent-003.
        let resume = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .preferred_workspace_id("mono-agent-003")
                    .build(),
            )
            .unwrap();

        // Cube reports mono-agent-003 still leased to the dead lease.
        let cube = Arc::new(FakeCubeClient::default().with_list_workspaces(vec![
            CubeWorkspaceStatus::builder()
                .workspace_id("mono-agent-003")
                .workspace_path(PathBuf::from("/tmp/mono-agent-003"))
                .state("leased")
                .lease_id("lease-dead")
                .holder("dead@host:1")
                .task("resume")
                .leased_at_epoch_s(1_700_000_000)
                .build(),
        ]));
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
        let repo = CubeRepoHandle {
            repo_id: "mono".to_owned(),
        };

        let result = coordinator
            .lease_workspace_with_fallback(&resume, "worker-resume", &repo, "task", &coordinator.host_adapter)
            .await;
        assert!(result.is_ok(), "resume lease should succeed after reclaim");

        // The dead lease was force-released exactly once.
        let releases = cube.force_release_calls.lock().await;
        assert_eq!(releases.len(), 1, "stale lease must be reclaimed once");
        assert_eq!(releases[0].0, "lease-dead");
        drop(releases);

        // The prefer lease was then issued for the same workspace.
        let calls = cube.lease_calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2.as_deref(), Some("mono-agent-003"));
    }

    /// Safety: a hard-prefer resume must NOT force-release a lease that
    /// cube reports holding a workspace the engine has no terminal
    /// record for (e.g. a genuinely live worker in another slot). The
    /// reclaim probe runs but finds nothing eligible, so the lease
    /// attempt proceeds without any force-release.
    #[tokio::test]
    async fn hard_prefer_resume_does_not_reclaim_unowned_lease() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Resume me");
        db.reconcile_product_executions(&product.id).unwrap();
        let resume = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .preferred_workspace_id("mono-agent-007")
                    .build(),
            )
            .unwrap();

        // Cube reports the workspace leased to a lease the engine has no
        // terminal execution record for.
        let cube = Arc::new(FakeCubeClient::default().with_list_workspaces(vec![
            CubeWorkspaceStatus::builder()
                .workspace_id("mono-agent-007")
                .workspace_path(PathBuf::from("/tmp/mono-agent-007"))
                .state("leased")
                .lease_id("lease-unknown")
                .holder("someone@host:9")
                .task("other")
                .leased_at_epoch_s(1_700_000_000)
                .build(),
        ]));
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
        let repo = CubeRepoHandle {
            repo_id: "mono".to_owned(),
        };

        let _ = coordinator
            .lease_workspace_with_fallback(&resume, "worker-resume", &repo, "task", &coordinator.host_adapter)
            .await;

        let releases = cube.force_release_calls.lock().await;
        assert!(releases.is_empty(), "must not reclaim a lease the engine doesn't own",);
    }

    /// When `preferred_workspace_id=null` and cube fails the first workspace
    /// (e.g. because it has uncommitted work from a prior crashed lease),
    /// the engine must retry with `any_free` policy and land on the second
    /// workspace. This pins the fix for the 2026-05-12 dispatch failure
    /// where a single bad workspace blocked dispatch despite 12+ free ones.
    #[tokio::test]
    async fn lease_falls_back_when_no_prefer_and_first_workspace_refused() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();

        // First lease call fails (simulating a workspace with uncommitted
        // work refusing the reset); second call succeeds on a different
        // workspace.
        let cube = Arc::new(FakeCubeClient {
            fail_first_n_leases: 1,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;

        // Two cube lease invocations: first fails, second succeeds.
        let calls = cube.lease_calls.lock().await;
        assert_eq!(
            calls.len(),
            2,
            "engine must retry on any_free when no prefer set; got {:?}",
            calls
        );
        // Both calls have no --prefer (engine retries with same strategy).
        assert_eq!(calls[0].2, None);
        assert_eq!(calls[1].2, None);
        drop(calls);

        let events = recording.events_for(&execution_id).await;
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();

        // Timeline: attempted #1 → failed #1 → attempted #2 → leased.
        let attempt_events: Vec<&crate::dispatch_events::DispatchEvent> = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_attempted")
            .collect();
        assert_eq!(
            attempt_events.len(),
            2,
            "expected two lease_attempted events (initial + any_free retry); got stages {stages:?}"
        );
        assert_eq!(
            attempt_events[0]
                .details
                .get("fallback_policy")
                .and_then(|v| v.as_str()),
            Some("any_free"),
            "first attempt must carry any_free policy when no prefer set",
        );
        assert!(
            attempt_events[0]
                .details
                .get("prefer_workspace_id")
                .map(|v| v.is_null())
                .unwrap_or(false),
            "first attempt must have prefer_workspace_id=null; got {:?}",
            attempt_events[0].details,
        );
        assert_eq!(
            attempt_events[1]
                .details
                .get("fallback_policy")
                .and_then(|v| v.as_str()),
            Some("none"),
            "retry attempt has no further fallback",
        );

        let failed_events: Vec<&crate::dispatch_events::DispatchEvent> = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_failed")
            .collect();
        assert_eq!(
            failed_events.len(),
            1,
            "exactly one lease_failed event for the first attempt; got stages {stages:?}"
        );

        // Final state: a successful `cube_workspace_leased` event.
        let leased = events
            .iter()
            .find(|e| e.stage == "cube_workspace_leased")
            .expect("cube_workspace_leased event missing after any_free retry");
        assert_eq!(leased.outcome, "ok");

        // No attention item — the fallback succeeded.
        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert!(
            attention_items.iter().all(|a| a.kind != "cube_workspace_lease_failed"),
            "any_free success must not raise a lease-failure attention item; got {attention_items:?}",
        );
    }

    /// When `preferred_workspace_id=null` and both lease attempts fail, the
    /// execution must transition to `failed` with both
    /// `cube_workspace_lease_failed` events visible — silent wait is not OK.
    #[tokio::test]
    async fn lease_fallback_failure_transitions_execution_to_failed() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        // No retries: go straight to permanent failure to keep the event
        // count assertions (2 attempts, 2 failures) unambiguous.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![])
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

        let events = recording.events_for(&execution_id).await;
        let attempt_count = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_attempted")
            .count();
        let failed_count = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_failed")
            .count();
        assert_eq!(
            attempt_count, 2,
            "expected initial + any_free retry attempt events; got {events:?}"
        );
        assert_eq!(
            failed_count, 2,
            "expected one lease_failed event per attempt; got {events:?}"
        );

        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "terminal lease failure must raise exactly one attention item",
        );
        assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
    }

    #[tokio::test]
    async fn change_creation_failure_marks_execution_failed_and_releases_workspace() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_create: true,
            ..FakeCubeClient::default()
        });
        // No retries: go straight to permanent failure to keep the
        // release_calls assertion (exactly "lease-1") unambiguous.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![]),
        );
        coordinator.kick();
        wait_for_execution_status(
            db.as_ref(),
            &db.list_executions(Some(&chore.id)).unwrap()[0].id,
            ExecutionStatus::Failed,
        )
        .await;

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        assert_eq!(execution.status, ExecutionStatus::Failed);
        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.status, "failed");
        assert_eq!(run.error_text.as_deref(), Some("cube change create failed"));
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        assert_eq!(coordinator.worker_pool().idle_count().await, 1);
    }

    /// T981 regression — the coordinator's mid-spawn cancel handling.
    /// When the runner reports `CancelledDuringSpawn` (it reaped the
    /// just-spawned pane), the coordinator must release the cube lease
    /// the cancel path deliberately left held, and must NOT drive the
    /// row to `waiting_human` (the row is already terminal). This is the
    /// downstream half of "the lease is not released until the process
    /// exits": the in-flight run is the sole releaser for a mid-spawn
    /// cancel.
    #[tokio::test]
    async fn cancelled_during_spawn_releases_lease_and_skips_completion() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Sort struct definitions");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            cancelled_during_spawn: true,
            work_db: Some(db.clone()),
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner)
                .with_pre_start_retry_delays(vec![]),
        );
        coordinator.kick();

        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        // The runner cancels the row inside the spawn; wait for that
        // terminal status to settle.
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Cancelled).await;

        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            execution.status,
            ExecutionStatus::Cancelled,
            "the row stays cancelled — the coordinator must not move it to waiting_human",
        );
        // The deferred lease must have been released exactly once, and
        // the row's lease columns cleared (ownership claimed atomically).
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "the deferred cube lease must be released after the mid-spawn cancel",
        );
        assert!(
            execution.cube_lease_id.is_none(),
            "lease columns must be cleared once the deferred lease is released",
        );
        // The pool slot is returned so dispatch can proceed.
        assert_eq!(coordinator.worker_pool().idle_count().await, 1);
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
        // two pools occupy disjoint slot ranges: interactive 1..=16, automation 17..=22.
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
        // Automation pool: slots 17..=22 → "auto-worker-M" → back to the same slot.
        let automation_end = MAX_WORKER_POOL_SIZE as u8 + MAX_AUTOMATION_POOL_SIZE as u8;
        for slot in (MAX_WORKER_POOL_SIZE as u8 + 1)..=automation_end {
            let wid = worker_id_for_slot(slot);
            let expected_ordinal = slot as usize - MAX_WORKER_POOL_SIZE;
            assert_eq!(wid, format!("auto-worker-{expected_ordinal}"));
            assert_eq!(slot_id_from_worker_id(&wid), Some(slot));
        }
        // Review pool: slots 23..=30 → "review-M" → back to the same slot.
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
        // automation (6) ranges, so they occupy slots 23..=30 — disjoint
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
        // The slot IDs produced by review-N (23..=30) must not overlap
        // with any interactive-pool (1..=16) or automation-pool (17..=22) slot.
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
            worker_page_label(crate::worker_registry::REMOTE_SLOT_BASE),
            None,
            "remote virtual slot has no page"
        );
    }

    #[test]
    fn automation_pool_slots_are_disjoint_from_regular_pool() {
        // The slot IDs produced by auto-worker-N (17..=22) must not
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
    fn slot_busy_occupant_walks_the_with_context_wrapped_chain() {
        // The spawn flow always wraps `StartWorkerError` with
        // `.with_context(...)` before it reaches the coordinator (see
        // `runner.rs`'s `spawning worker pane for run {}` wrapper), so
        // a naive `err.downcast_ref::<StartWorkerError>()` on the
        // outermost error would never match. This pins the chain-walk
        // that makes extraction work anyway.
        let root = StartWorkerError::AppError(EngineToAppError::SlotBusy {
            occupying_run_id: Some("run-husk".to_owned()),
        });
        let wrapped: anyhow::Error = anyhow::Error::new(root).context("spawning worker pane for run exec-1");
        assert_eq!(slot_busy_occupant(&wrapped), Some(Some("run-husk".to_owned())));
    }

    #[test]
    fn slot_busy_occupant_handles_missing_occupying_run_id() {
        // Older apps predating the field send `SlotBusy` with no
        // payload — must decode as `Some(None)` (the error IS
        // SlotBusy, but the occupant is unknown), not `None`
        // (not-a-SlotBusy-error at all).
        let root = StartWorkerError::AppError(EngineToAppError::SlotBusy { occupying_run_id: None });
        let wrapped: anyhow::Error = anyhow::Error::new(root).context("spawning worker pane for run exec-2");
        assert_eq!(slot_busy_occupant(&wrapped), Some(None));
    }

    #[test]
    fn slot_busy_occupant_is_none_for_other_start_worker_errors() {
        let root = StartWorkerError::AppError(EngineToAppError::NoAvailableSlot);
        let wrapped: anyhow::Error = anyhow::Error::new(root).context("spawning worker pane for run exec-3");
        assert_eq!(slot_busy_occupant(&wrapped), None);
    }

    #[test]
    fn slot_busy_occupant_is_none_for_unrelated_errors() {
        let wrapped = anyhow::anyhow!("workspace lease failed");
        assert_eq!(slot_busy_occupant(&wrapped), None);
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
    fn pool_model_override_for_worker_id_returns_opus_for_review_and_automation() {
        // Review and automation pools always pin to Opus per the automated-reviewer
        // design §5. Main-pool workers have no override and fall through to the
        // effort-driven default.
        for ordinal in 1u8..=MAX_REVIEW_POOL_SIZE as u8 {
            let wid = format!("review-{ordinal}");
            assert_eq!(
                pool_model_override_for_worker_id(&wid),
                Some("opus"),
                "review pool worker {wid:?} must return opus override"
            );
        }
        for ordinal in 1u8..=MAX_AUTOMATION_POOL_SIZE as u8 {
            let wid = format!("auto-worker-{ordinal}");
            assert_eq!(
                pool_model_override_for_worker_id(&wid),
                Some("opus"),
                "automation pool worker {wid:?} must return opus override"
            );
        }
        for ordinal in 1u8..=MAX_WORKER_POOL_SIZE as u8 {
            let wid = format!("worker-{ordinal}");
            assert_eq!(
                pool_model_override_for_worker_id(&wid),
                None,
                "main pool worker {wid:?} must return no override"
            );
        }
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

    #[tokio::test]
    async fn higher_priority_executions_run_first() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let early = create_test_chore(&db, product.id.clone(), "Old");
        let late = create_test_chore(&db, product.id.clone(), "New");
        db.reconcile_product_executions(&product.id).unwrap();

        // Bump the later chore's priority — it should run first despite
        // the older one being in the queue first.
        db.request_execution(
            RequestExecutionInput::builder()
                .work_item_id(late.id.clone())
                .priority(10)
                .build(),
        )
        .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();

        for _ in 0..100 {
            let runs = runner.calls.lock().await;
            if !runs.is_empty() {
                break;
            }
            drop(runs);
            sleep(Duration::from_millis(10)).await;
        }

        let calls = runner.calls.lock().await;
        assert!(!calls.is_empty(), "scheduler did not start any run");
        let started_execution_id = &calls[0].1;
        let late_execution = db.list_executions(Some(&late.id)).unwrap().pop().unwrap();
        assert_eq!(
            started_execution_id, &late_execution.id,
            "expected the higher-priority chore to run first"
        );
        // Old chore should still be queued (and was NOT picked).
        let early_execution = db.list_executions(Some(&early.id)).unwrap().pop().unwrap();
        assert_eq!(early_execution.status, ExecutionStatus::Ready);
    }

    /// Dispatch-class acceptance test (operator directive: revisions before
    /// tasks/chores, ordered by revision kind): a merge-conflict-fixing
    /// revision (class 1) must claim a single free slot before an ordinary
    /// chore (class 5) that has been sitting in the ready queue longer —
    /// the exact opposite of what plain FIFO-by-creation-time would pick.
    #[tokio::test]
    async fn merge_conflict_revision_outranks_older_ready_chore_for_a_single_free_slot() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();

        // Older, ordinary chore — created (and thus ready) first.
        let chore = create_test_chore(&db, product.id.clone(), "Ordinary chore");
        db.reconcile_product_executions(&product.id).unwrap();

        // Newer merge-conflict-fixing revision — `created_at` is stamped
        // far in the future so a plain FIFO queue would place it dead last;
        // dispatch class must still put it first.
        let revision_id = "task_merge_conflict_outranks_test";
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, created_via)
                 VALUES (?1, ?2, 'revision', 'Fix merge conflict', '', 'todo', '2099-01-01T00:00:00Z', '2099-01-01T00:00:00Z', 'merge-conflict:crz_1')",
                rusqlite::params![revision_id, product.id],
            )
            .unwrap();
        }
        let revision_execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision_id)
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();

        for _ in 0..100 {
            let runs = runner.calls.lock().await;
            if !runs.is_empty() {
                break;
            }
            drop(runs);
            sleep(Duration::from_millis(10)).await;
        }

        let calls = runner.calls.lock().await;
        assert!(!calls.is_empty(), "scheduler did not start any run");
        assert_eq!(
            &calls[0].1, &revision_execution.id,
            "the merge-conflict revision must dispatch before the older ordinary chore",
        );
        drop(calls);

        let chore_execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        assert_eq!(
            chore_execution.status,
            ExecutionStatus::Ready,
            "the older chore must remain queued behind the higher-class revision",
        );
    }

    /// Selection-time ordering only — a higher dispatch class must never
    /// preempt a worker that already claimed the slot. Once a slot is
    /// running, a newly-arrived class-1 revision simply queues behind it
    /// like anything else and dispatches only when the slot frees.
    #[tokio::test]
    async fn running_worker_is_never_preempted_by_a_higher_dispatch_class_arrival() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();

        let _chore = create_test_chore(&db, product.id.clone(), "Ordinary chore");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();

        // Wait for the chore to actually claim the single slot and go
        // `running` before the higher-class revision even exists.
        for _ in 0..200 {
            let executions = db.list_executions(None).unwrap();
            if executions.iter().any(|e| e.status == ExecutionStatus::Running) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        {
            let calls = runner.calls.lock().await;
            assert_eq!(calls.len(), 1, "the chore must have claimed the single slot first");
        }

        // A class-1 merge-conflict revision arrives after the slot is gone.
        let revision_id = "task_merge_conflict_arrives_after_slot_claimed";
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, created_via)
                 VALUES (?1, ?2, 'revision', 'Fix merge conflict', '', 'todo', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z', 'merge-conflict:crz_2')",
                rusqlite::params![revision_id, product.id],
            )
            .unwrap();
        }
        let revision_execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision_id)
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        coordinator.kick();
        // Give the scheduler a window to (incorrectly) act, if it were
        // going to. There is no positive event to wait on here — the
        // assertion is that nothing changes.
        sleep(Duration::from_millis(100)).await;

        let calls = runner.calls.lock().await;
        assert_eq!(
            calls.len(),
            1,
            "the running worker must not be preempted by a newly-arrived higher-class execution",
        );
        drop(calls);

        let revision_status = db.get_execution(&revision_execution.id).unwrap().status;
        assert_eq!(
            revision_status,
            ExecutionStatus::Ready,
            "the higher-class revision must queue behind the running slot, not preempt it",
        );
    }

    #[tokio::test]
    async fn scheduler_passes_preferred_workspace_to_lease_and_records_affinity() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .preferred_workspace_id("mono-agent-007")
                .build(),
        )
        .unwrap();

        let cube = Arc::new(FakeCubeClient::default().with_next_workspace_id("mono-agent-007"));
        let runner = Arc::new(FakeExecutionRunner::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

        let calls = cube.lease_calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2.as_deref(), Some("mono-agent-007"));
        drop(calls);

        let execution = db.get_execution(&execution.id).unwrap();
        assert_eq!(execution.cube_workspace_id.as_deref(), Some("mono-agent-007"));
        assert_eq!(
            coordinator.worker_pool().worker_affinity("worker-1").await.as_deref(),
            Some("mono-agent-007")
        );
    }

    #[tokio::test]
    async fn coordinator_publishes_execution_topic_events() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let coordinator = Arc::new(ExecutionCoordinator::with_publisher(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
            publisher.clone(),
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

        let events = publisher.publish_calls.lock().await;
        let reasons: Vec<&str> = events.iter().map(|(_, _, _, reason)| reason.as_str()).collect();
        assert!(reasons.contains(&"execution_started"));
        assert!(reasons.contains(&"execution_run_completed"));
        let last_status = events
            .iter()
            .rev()
            .find(|(_, _, _, reason)| reason == "execution_run_completed")
            .map(|(_, _, status, _)| status.clone());
        assert_eq!(last_status.as_deref(), Some("waiting_human"));

        // The kanban activity-icon depends on a work-tree invalidation
        // on run completion, otherwise the card would stay stuck on
        // "active" after the agent moved to waiting_human. Confirm the
        // coordinator now fires the broadcast on the completion path
        // too — not just on execution-start auto-advance.
        let work_item_events = publisher.events.lock().await;
        assert!(
            work_item_events
                .iter()
                .any(|(_, _, reason)| { reason == "execution_run_completed" }),
            "expected execution_run_completed work-item invalidation, got: {:?}",
            *work_item_events,
        );
    }

    /// When `start_execution_run` auto-advances `tasks.status` to
    /// `'active'`, the coordinator must also publish a work-tree
    /// invalidation so kanban subscribers re-fetch the board. Without
    /// this, the DB has the right value but the GUI never refreshes
    /// — the bug surfaced manually that this test exists to prevent.
    #[tokio::test]
    async fn coordinator_publishes_work_item_changed_on_execution_start() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let coordinator = Arc::new(ExecutionCoordinator::with_publisher(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
            publisher.clone(),
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

        // Work-item invalidation should have fired with the chore's
        // product id and the chore's work-item id. Reason wording
        // isn't load-bearing but we assert it's there to confirm the
        // call site is the auto-advance one and not some unrelated
        // future broadcast.
        let work_item_events = publisher.events.lock().await;
        assert!(
            work_item_events.iter().any(|(product_id, work_item_id, reason)| {
                product_id == &product.id && work_item_id == &chore.id && reason == "execution_started_auto_advance"
            }),
            "expected execution_started_auto_advance event for chore {} on product {}, got: {:?}",
            chore.id,
            product.id,
            *work_item_events,
        );

        // And the DB-level auto-advance itself: the chore status must
        // have flipped from `todo` to `active` when the execution
        // started running.
        let advanced = db.get_work_item(&chore.id).unwrap();
        match advanced {
            WorkItem::Chore(t) | WorkItem::Task(t) => {
                assert_eq!(t.status, TaskStatus::Active, "chore should auto-advance to active");
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn scheduler_respects_worker_pool_capacity() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let first_project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Design A".to_owned(),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: false,
            })
            .unwrap();
        let second_project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Design B".to_owned(),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: false,
            })
            .unwrap();
        db.create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(first_project.id.clone())
                .name("A1")
                .build(),
        )
        .unwrap();
        db.create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(second_project.id.clone())
                .name("B1")
                .build(),
        )
        .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();
        for _ in 0..100 {
            let executions = db.list_executions(None).unwrap();
            if executions
                .iter()
                .filter(|execution| execution.status == ExecutionStatus::Running)
                .count()
                == 1
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let executions = db.list_executions(None).unwrap();
        assert_eq!(
            executions
                .iter()
                .filter(|execution| execution.status == ExecutionStatus::Running)
                .count(),
            1,
            "pool cap = 1 must keep exactly one execution `running`",
        );
        // Project design now lives on a per-project `kind = 'design'`
        // task at `ordinal = 0`, with the user's project_tasks at
        // `ordinal >= 1`. Only the design tasks are eligible for
        // `ready` until they complete; the user-tasks stay
        // `waiting_dependency` behind their project's design. So the
        // shape is: 1 running design, 1 ready design (gated on the
        // pool slot), 2 waiting_dependency project_tasks.
        assert_eq!(
            executions
                .iter()
                .filter(|execution| execution.status == ExecutionStatus::Ready)
                .count(),
            1,
        );
        assert_eq!(
            executions
                .iter()
                .filter(|execution| execution.status == ExecutionStatus::WaitingDependency)
                .count(),
            2,
        );
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
    }

    /// Ghost-active regression: when the worker pool is exhausted,
    /// chores that lost the dispatcher's claim race must NOT have
    /// `tasks.status` flipped to `'active'`. They stay in `todo` so
    /// `boss chore list --status active` and `bossctl agents list`
    /// agree on which chores actually have a worker.
    ///
    /// Setup: pool capped at 1, three autostart chores reconciled into
    /// `ready` executions back-to-back. Only one can be dispatched —
    /// the other two must remain `todo` with no run record. This is
    /// the test that would have caught the "6 active, 4 workers"
    /// observation in the bug report.
    #[tokio::test]
    async fn pool_exhaustion_does_not_ghost_activate_chores() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        let mut chore_ids = Vec::new();
        for index in 0..3 {
            let chore = create_test_chore(&db, product.id.clone(), format!("Chore {index}"));
            chore_ids.push(chore.id);
        }
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();

        // Wait for the dispatcher to settle on exactly one running
        // execution. With pool=1 and 3 ready chores the loop must
        // claim the first slot, then break on pool exhaustion.
        for _ in 0..200 {
            let executions = db.list_executions(None).unwrap();
            if executions
                .iter()
                .filter(|execution| execution.status == ExecutionStatus::Running)
                .count()
                == 1
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        // One chore active with a run, two stay todo with no run.
        let mut active_with_run = 0usize;
        let mut still_todo = 0usize;
        for chore_id in &chore_ids {
            let item = db.get_work_item(chore_id).unwrap();
            let status = match item {
                WorkItem::Chore(t) | WorkItem::Task(t) => t.status,
                other => panic!("expected chore/task, got {other:?}"),
            };
            let executions = db.list_executions(Some(chore_id)).unwrap();
            assert_eq!(executions.len(), 1, "exactly one execution per chore");
            let runs = db.list_runs(&executions[0].id).unwrap();
            match status.as_str() {
                "active" => {
                    assert_eq!(executions[0].status, ExecutionStatus::Running);
                    assert_eq!(runs.len(), 1, "active chore must have a run record");
                    assert_eq!(runs[0].status, "active");
                    active_with_run += 1;
                }
                "todo" => {
                    assert_eq!(executions[0].status, ExecutionStatus::Ready);
                    assert!(
                        runs.is_empty(),
                        "todo chore must not have a run record yet, got {runs:?}",
                    );
                    still_todo += 1;
                }
                other => panic!(
                    "chore {chore_id} unexpectedly in status `{other}` — \
                     `active` and `todo` are the only valid states for this \
                     pool-exhausted scenario",
                ),
            }
        }
        assert_eq!(
            active_with_run, 1,
            "exactly one chore should be active with a run; got {active_with_run}",
        );
        assert_eq!(
            still_todo, 2,
            "two chores should stay `todo` with no run; got {still_todo}",
        );
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
    }

    /// Root-cause regression (T2130, 2026-07-01): pool exhaustion is a
    /// transient capacity wait, not a failure. A chore that repeatedly loses
    /// the pool-claim race (`worker_claimed/skipped reason=pool_exhausted`,
    /// cycle after cycle across drain passes) must stay untouched — no
    /// execution ever marked `failed`, `autostart` never flipped — and must
    /// dispatch on its own the instant a slot frees, via the ordinary
    /// `release_worker_and_kick` re-scan. No `force_dispatch` / manual
    /// `bossctl work start` should ever be required to recover it.
    #[tokio::test]
    async fn pool_exhaustion_recovers_automatically_when_slot_frees_without_manual_intervention() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        let winner = create_test_chore(&db, product.id.clone(), "Winner");
        let waiter = create_test_chore(&db, product.id.clone(), "Waiter");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();

        // Settle: one chore claims the sole slot; the other is left `ready`
        // behind the exhausted pool.
        for _ in 0..200 {
            let running = db.list_executions(Some(&winner.id)).unwrap();
            let waiting = db.list_executions(Some(&waiter.id)).unwrap();
            if running.iter().any(|e| e.status == ExecutionStatus::Running)
                && waiting.len() == 1
                && waiting[0].status == ExecutionStatus::Ready
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        // Reproduce "repeated cycles" from the incident report: several more
        // drain passes while the pool stays full. None of these may touch
        // the waiting row.
        for _ in 0..5 {
            coordinator.kick();
            sleep(Duration::from_millis(10)).await;
        }

        let waiter_task = match db.get_work_item(&waiter.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(
            waiter_task.status.as_str(),
            "todo",
            "pool-exhausted chore must stay queued in Backlog, not be demoted or archived",
        );
        assert!(
            waiter_task.autostart,
            "pool exhaustion is a transient wait, not a failure — autostart must never be flipped off",
        );
        let waiter_executions = db.list_executions(Some(&waiter.id)).unwrap();
        assert_eq!(
            waiter_executions.len(),
            1,
            "no duplicate/extra execution should be created while waiting on the pool",
        );
        assert_eq!(
            waiter_executions[0].status,
            ExecutionStatus::Ready,
            "the waiting execution must stay `ready`, never `failed`, across pool_exhausted cycles",
        );

        // Free the slot exactly like a real completion would: every
        // completion path funnels through `release_worker_and_kick`.
        let winner_execution = db.list_executions(Some(&winner.id)).unwrap().remove(0);
        let claimed_worker_id = coordinator
            .worker_pool()
            .claims()
            .await
            .into_iter()
            .find(|claim| claim.execution_id == winner_execution.id)
            .map(|claim| claim.worker_id)
            .expect("winner's execution should hold a claimed worker slot");
        coordinator.release_worker_and_kick(&claimed_worker_id, None).await;

        // No manual intervention: the waiter must pick up the freed slot on
        // its own, driven purely by the release's kick.
        let mut waiter_running = false;
        for _ in 0..200 {
            let executions = db.list_executions(Some(&waiter.id)).unwrap();
            if executions.iter().any(|e| e.status == ExecutionStatus::Running) {
                waiter_running = true;
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert!(
            waiter_running,
            "pool-exhausted chore must auto-dispatch the instant a slot frees — no manual work-start needed",
        );
    }

    /// Boot-time heal: a `tasks.status = 'active'` row whose
    /// executions never produced a `work_runs` entry (e.g. previous
    /// engine crashed between the kanban drag and the dispatch claim,
    /// or a `RequestExecution` raced ahead of an exhausted pool) is
    /// demoted back to `todo` on startup. Items WITH run history are
    /// left alone — `reconcile_active_dispatch` is the right tool for
    /// those.
    #[tokio::test]
    async fn heal_ghost_active_demotes_chores_without_run_history() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        // Ghost A: dragged to Doing but no execution exists at all.
        let ghost_a = create_test_chore_manual(&db, product.id.clone(), "Ghost A");
        db.update_work_item(
            &ghost_a.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        // Ghost B: dragged to Doing, has a `ready` execution but no
        // run yet — the "RequestExecution raced an exhausted pool"
        // shape from the bug report.
        let ghost_b = create_test_chore_manual(&db, product.id.clone(), "Ghost B");
        db.update_work_item(
            &ghost_b.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.request_execution(
            RequestExecutionInput::builder()
                .work_item_id(ghost_b.id.clone())
                .build(),
        )
        .unwrap();

        // Real worker: started a run before the engine restarted,
        // mimicking a crashed-mid-flight chore. heal must NOT touch
        // this — `reconcile_active_dispatch` redispatches it.
        let real = create_test_chore_manual(&db, product.id.clone(), "Real worker");
        let real_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(real.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        db.start_execution_run(
            &real_exec.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

        let healed = db.heal_ghost_active_chores().unwrap();
        let mut healed_ids: Vec<String> = healed.iter().map(|h| h.work_item_id.clone()).collect();
        healed_ids.sort();
        let mut expected = vec![ghost_a.id.clone(), ghost_b.id.clone()];
        expected.sort();
        assert_eq!(healed_ids, expected, "healed only the ghost rows");
        // product_id rides along so the caller can publish a
        // work-item-changed event on the product's kanban topic.
        for h in &healed {
            assert_eq!(h.product_id, product.id, "healed row should carry its product_id");
        }

        // Demoted ghosts now sit in `todo` and are stamped as engine-
        // initiated so the kanban can attribute the move correctly
        // instead of blaming the human who last dragged the row.
        for id in &[&ghost_a.id, &ghost_b.id] {
            match db.get_work_item(id).unwrap() {
                WorkItem::Chore(t) | WorkItem::Task(t) => {
                    assert_eq!(t.status, TaskStatus::Todo);
                    assert_eq!(t.last_status_actor, "engine");
                }
                other => panic!("expected chore/task, got {other:?}"),
            }
        }

        // Ghost B's stranded `ready` execution was abandoned so the
        // dispatcher won't claim a slot for a chore that just got
        // pulled out of the Doing column.
        let ghost_b_execs = db.list_executions(Some(&ghost_b.id)).unwrap();
        assert_eq!(ghost_b_execs.len(), 1);
        assert_eq!(ghost_b_execs[0].status, ExecutionStatus::Abandoned);

        // The real chore stays `active` with its `running` execution
        // intact — heal is conservative.
        match db.get_work_item(&real.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
            other => panic!("expected chore/task, got {other:?}"),
        }
        let real_execs = db.list_executions(Some(&real.id)).unwrap();
        assert_eq!(real_execs.len(), 1);
        assert_eq!(real_execs[0].status, ExecutionStatus::Running);
    }

    /// Regression coverage for PR #228. Default-sized pool
    /// (`MAX_WORKER_POOL_SIZE` = 8) must dispatch all five chores when
    /// they autostart back-to-back — the original bug was a pool that
    /// silently capped at 1 (and an earlier-still incarnation that
    /// capped at 4), so `kick()` broke out of `run_scheduler` after
    /// claiming the first few workers and the rest stayed `ready`.
    /// This test would have caught that: it asserts every one of the
    /// five executions reaches `running`, and that the pool consumed
    /// five distinct worker slots (so dispatch fanned out into the
    /// 5..=8 range that the original bug had unreachable).
    #[tokio::test]
    async fn default_pool_dispatches_five_concurrent_autostart_chores() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        // Five autostart chores — the same shape `boss chore create`
        // produces when `--no-autostart` is omitted. Reconcile then
        // promotes each to a `ready` execution row.
        for index in 0..5 {
            create_test_chore(&db, product.id.clone(), format!("Chore {index}"));
        }
        db.reconcile_product_executions(&product.id).unwrap();

        // Use the default pool size so this test pins the contract
        // `WorkConfig::load_from_env` exposes to production.
        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(MAX_WORKER_POOL_SIZE),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();

        for _ in 0..200 {
            let executions = db.list_executions(None).unwrap();
            if executions
                .iter()
                .filter(|execution| execution.status == ExecutionStatus::Running)
                .count()
                == 5
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let executions = db.list_executions(None).unwrap();
        let running = executions
            .iter()
            .filter(|execution| execution.status == ExecutionStatus::Running)
            .count();
        assert_eq!(
            running, 5,
            "expected all 5 autostart chores to be dispatched concurrently, got {running} running",
        );
        // Five of the default pool's slots are now busy; the remainder stay
        // idle. Derive the expectation from the pool size so this keeps pinning
        // the contract as the interactive pool grows pages.
        assert_eq!(coordinator.worker_pool().idle_count().await, MAX_WORKER_POOL_SIZE - 5);
    }

    /// `bossctl agents launch` (Phase 7 of the v2 plan) must dispatch
    /// even when every configured slot is busy — the verb's whole point
    /// is to *skip the queue*. We mirror the cap test above
    /// (`scheduler_respects_worker_pool_capacity`) but with a smaller
    /// pool so we can sit under the hard cap, fill every slot, and
    /// then prove `force_dispatch` grows the pool by one slot and runs
    /// the launched item immediately rather than leaving it `ready`.
    #[tokio::test]
    async fn force_dispatch_bypasses_configured_pool_cap() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let busy = create_test_chore(&db, product.id.clone(), "Already running");
        // A second chore that will sit in `ready` because the
        // configured pool size is 1 and `busy` claimed it.
        let queued = create_test_chore_manual(&db, product.id.clone(), "Skip the queue");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();

        // Wait for the first chore to actually be claimed by the lone
        // worker slot — otherwise force_dispatch might race the
        // scheduler and grow the pool unnecessarily.
        for _ in 0..200 {
            let busy_exec = db.list_executions(Some(&busy.id)).unwrap().pop().unwrap();
            if busy_exec.status == ExecutionStatus::Running {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
        assert_eq!(coordinator.worker_pool().capacity().await, 1);

        // `bossctl agents launch <queued.id>` enters the engine via
        // `RequestExecution { force: true }`. Promote `queued` to a
        // `ready` execution (the auto-start opt-out kept it parked),
        // then call the same coordinator entry point that `app.rs`
        // hits when `force = true`.
        let queued_exec = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(queued.id.clone())
                    .force(true)
                    .build(),
            )
            .unwrap();
        let worker_id = coordinator
            .force_dispatch(&queued_exec.id)
            .await
            .expect("force_dispatch should bypass the cap and return a worker id");
        assert_eq!(
            worker_id, "worker-2",
            "expected force_dispatch to grow the pool with a new slot",
        );

        for _ in 0..200 {
            let queued_after = db.list_executions(Some(&queued.id)).unwrap().pop().unwrap();
            if queued_after.status == ExecutionStatus::Running {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let queued_after = db.list_executions(Some(&queued.id)).unwrap().pop().unwrap();
        assert_eq!(
            queued_after.status,
            ExecutionStatus::Running,
            "force-launched execution should be dispatched immediately",
        );
        assert_eq!(
            coordinator.worker_pool().capacity().await,
            2,
            "force_dispatch must grow the pool by one slot",
        );
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
    }

    /// The pool-grow path is hard-capped at `MAX_WORKER_POOL_SIZE`
    /// because the macOS app renders one pane per interactive slot. A
    /// force-launch request that arrives with every hard-cap slot busy must
    /// surface a real error instead of silently overcommitting.
    /// On-free rescan regression: a chore whose `tasks.status` is
    /// `active` but whose latest execution is terminal (worker died,
    /// cube lease errored, kanban-drag-while-pool-was-full) must be
    /// redispatched the next time a worker frees up. Without the
    /// rescan, `kick()` only sees `ready` executions and the stuck
    /// chore stays in Doing forever.
    #[tokio::test]
    async fn worker_release_redispatches_active_chore_with_terminal_execution() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        // Warm-up chore: gets a normal `ready` execution so the
        // dispatcher has something to consume the single pool slot.
        // Its run completes via FakeExecutionRunner (WaitingHuman), at
        // which point the pool worker is released and our rescan fires.
        let warm = create_test_chore(&db, product.id.clone(), "Warm-up");
        db.reconcile_product_executions(&product.id).unwrap();

        // Stuck chore: `active` with a `failed` execution row,
        // mimicking the bug — worker died, kanban card stayed in
        // Doing, and the create-time dispatch path won't ever look
        // at it again.
        let stuck = create_test_chore(&db, product.id.clone(), "Stuck");
        db.update_work_item(
            &stuck.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(stuck.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Failed)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
        coordinator.kick();

        // Wait for the stuck chore to reach a non-failed execution
        // — that means the rescan inserted a fresh `ready` row and
        // the post-release `kick()` claimed it.
        for _ in 0..400 {
            let executions = db.list_executions(Some(&stuck.id)).unwrap();
            if executions.iter().any(|exec| exec.status.is_live()) {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let warm_execs = db.list_executions(Some(&warm.id)).unwrap();
        let stuck_execs = db.list_executions(Some(&stuck.id)).unwrap();
        panic!(
            "stuck chore was never redispatched after warm-up release;\nwarm executions: {warm_execs:?}\nstuck executions: {stuck_execs:?}",
        );
    }

    /// Negative case for the rescan: an `autostart=false` chore that
    /// is parked in `active` with a terminal execution must remain
    /// untouched even after a worker frees up. The on-free rescan is
    /// recurring; without the autostart filter it would loop on a
    /// chore the user explicitly opted out of auto-handling.
    #[tokio::test]
    async fn worker_release_skips_no_autostart_active_chore() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        let warm = create_test_chore(&db, product.id.clone(), "Warm-up");
        db.reconcile_product_executions(&product.id).unwrap();

        let parked = create_test_chore_manual(&db, product.id.clone(), "Parked");
        db.update_work_item(
            &parked.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(parked.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Failed)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
        coordinator.kick();

        // Wait for the warm-up to settle (its run will finish on
        // WaitingHuman). After that the rescan has had its chance to
        // touch the parked chore — it must not have.
        wait_for_execution_status(
            db.as_ref(),
            &db.list_executions(Some(&warm.id)).unwrap()[0].id,
            ExecutionStatus::WaitingHuman,
        )
        .await;
        // Give the post-release rescan a clear window in which to
        // (incorrectly) redispatch the parked chore. 100ms is plenty
        // — the rescan is synchronous on the release path.
        sleep(Duration::from_millis(100)).await;

        let parked_execs = db.list_executions(Some(&parked.id)).unwrap();
        assert_eq!(
            parked_execs.len(),
            1,
            "autostart=false parked chore must not be redispatched, got {parked_execs:?}",
        );
        assert_eq!(parked_execs[0].status, ExecutionStatus::Failed);
    }

    #[tokio::test]
    async fn force_dispatch_errors_at_hard_cap() {
        let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE);
        for i in 0..MAX_WORKER_POOL_SIZE {
            pool.claim_worker(&format!("exec-{i}"), None)
                .await
                .expect("hard-cap pool should hand out one slot per claim");
        }
        assert_eq!(pool.idle_count().await, 0);
        assert!(
            pool.claim_worker_force("overflow", None).await.is_none(),
            "claim_worker_force must reject when the pool is already at the hard cap",
        );
        assert_eq!(
            pool.capacity().await,
            MAX_WORKER_POOL_SIZE,
            "rejected force-claim must not grow the pool past the hard cap",
        );
    }

    /// Regression for `task_18ae9d21044843b8_44` — `bossctl work start`
    /// returned `status: ready` but no scheduler ever ran, leaving the
    /// row stranded. Root cause was a TOCTOU between the scheduler's
    /// last `list_ready_executions()` call and dropping its
    /// `scheduling_active` guard: a `kick()` that landed in that
    /// window observed `active=true`, returned without spawning, and
    /// the guard then dropped to `false` with no scheduler running.
    ///
    /// The fix latches every `kick()` into `scheduling_pending` so the
    /// alive scheduler always notices the wakeup. This test pins the
    /// contract: a `kick()` that arrives while `scheduling_active` is
    /// already true MUST set `scheduling_pending` so the running
    /// scheduler can re-enter its drain loop.
    #[tokio::test]
    async fn kick_during_active_scheduler_latches_pending_wakeup() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        // Simulate "another scheduler is already running".
        coordinator.scheduling_active.store(true, Ordering::Release);
        coordinator.scheduling_pending.store(false, Ordering::Release);

        coordinator.kick();

        assert!(
            coordinator.scheduling_pending.load(Ordering::Acquire),
            "kick that lost the active-flag race must still latch pending so the alive \
             scheduler re-enters its drain loop instead of exiting on stale state",
        );
    }

    /// End-to-end regression for the same race: even when a `kick()`
    /// loses the active-flag race, the row it queued for must still
    /// reach a worker. We can't deterministically force the OS into
    /// the exact "scheduler just finished its drain" timing, but we
    /// can prove the contract works by simulating the surviving
    /// scheduler picking up the wakeup: the pending bit is the
    /// in-process signal; if the pending bit is honored on the next
    /// run_scheduler entry, the new row gets processed.
    #[tokio::test]
    async fn ready_row_added_during_active_window_still_dispatches() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Stranded by lost wakeup");
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        // Simulate the bug-trigger sequence:
        //   1. A previous scheduler is "alive" (active=true) but
        //      has already finished its drain.
        //   2. RequestExecution lands, inserts a ready row, calls
        //      kick(). With the old code: kick observes active=true,
        //      returns, and the (now-exiting) scheduler drops the
        //      guard without re-checking. New row stranded.
        //   3. With the fix: kick latches pending=true.
        coordinator.scheduling_active.store(true, Ordering::Release);
        coordinator.scheduling_pending.store(false, Ordering::Release);
        coordinator.kick(); // noop on `active`, but latches pending

        // Now simulate the previous scheduler exiting: it must
        // honour the pending bit. Drop `active` and re-enter
        // `run_scheduler` exactly as the lossless-wakeup logic
        // would on the post-drain re-check path.
        coordinator.scheduling_active.store(false, Ordering::Release);
        assert!(
            coordinator.scheduling_pending.load(Ordering::Acquire),
            "post-drain re-check must see pending=true so the new row is not lost",
        );

        // The fix re-claims `active` and re-enters the drain. Kick
        // again to simulate that re-entry (this is what the
        // post-drain block in `run_scheduler` does internally), and
        // assert the row reaches `waiting_human`.
        coordinator.kick();
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;
    }

    /// Regression for the 2026-05-12 "`@` got re-pointed mid-flight"
    /// incident (`mono-agent-001`, Worf's report). Pre-fix, the engine
    /// never called `cube_client.heartbeat_lease` from anywhere — the
    /// trait method had only stub implementations in test mocks. Any
    /// worker that ran longer than `DEFAULT_LEASE_TTL_SECS = 1800` had
    /// its lease silently age out, after which the next
    /// `cube workspace lease` call from another execution reclaimed
    /// the workspace and ran `jj new <main>` on the still-active
    /// worker's working copy.
    ///
    /// This test pins down the fix: while the guard is alive, the
    /// heartbeat fires at the configured interval; dropping the guard
    /// stops the heartbeat. The default 5-minute production interval
    /// is shortened to 50 ms here so the test stays fast.
    #[tokio::test]
    async fn heartbeat_guard_renews_lease_until_dropped() {
        use super::{HeartbeatGuard, LocalHostAdapter};
        use crate::host_adapter::HostAdapter;

        let cube = Arc::new(FakeCubeClient::default());
        // Thin shim: wrap the FakeCubeClient in a LocalHostAdapter so the
        // HostAdapter-typed HeartbeatGuard interface is satisfied. The test
        // still inspects heartbeat_calls on the inner FakeCubeClient.
        let adapter: Arc<dyn HostAdapter> = Arc::new(LocalHostAdapter::new(
            cube.clone() as Arc<dyn CubeClient>,
            Arc::new(FakeExecutionRunner::default()),
        ));
        let guard = HeartbeatGuard::spawn_with_interval(
            adapter,
            "lease-1".to_owned(),
            "exec-1".to_owned(),
            "run-1".to_owned(),
            "worker-1".to_owned(),
            Duration::from_millis(50),
        );

        // Three intervals: expect at least two heartbeats (the first
        // tick is consumed at startup so the timer measures gaps).
        sleep(Duration::from_millis(180)).await;
        let beats_during = cube.heartbeat_calls.lock().await.len();
        assert!(
            beats_during >= 2,
            "expected >= 2 heartbeats in ~180ms with a 50ms interval, got {beats_during}",
        );
        for (lease, ttl) in cube.heartbeat_calls.lock().await.iter() {
            assert_eq!(lease, "lease-1");
            assert!(ttl.is_none(), "engine heartbeats use cube's default TTL");
        }

        // Drop stops the task. Sleep through more intervals and
        // assert the count is frozen — proving the heartbeat is
        // scoped to the guard's lifetime and cannot extend a lease
        // the run has already finished with.
        drop(guard);
        sleep(Duration::from_millis(50)).await;
        let beats_after_drop_snapshot = cube.heartbeat_calls.lock().await.len();
        sleep(Duration::from_millis(200)).await;
        let beats_final = cube.heartbeat_calls.lock().await.len();
        assert_eq!(
            beats_final, beats_after_drop_snapshot,
            "heartbeat must stop firing after the guard is dropped",
        );
    }

    /// Regression for `exec_18af3ba5259d32a8_12` (2026-05-13): a `ready`
    /// execution row that misses its scheduler wakeup sits at
    /// `status_transition` until the 90s-age orphan-active reconciler
    /// rescues it. With the heartbeat installed, the same stranded row
    /// reaches a worker within one heartbeat interval — no abandon /
    /// redispatch needed.
    ///
    /// The test simulates the failure mode by inserting a `ready` row
    /// without calling `kick()`, then spawning the heartbeat with a
    /// short interval. The heartbeat must observe the stranded row
    /// (the "fail loudly" surface for operators) and re-kick so the
    /// scheduler drains it.
    #[tokio::test]
    async fn heartbeat_rekicks_when_ready_row_was_orphaned_by_a_dropped_kick() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Stranded by lost wakeup");
        // Inserts a `ready` execution row but does NOT call `kick()`.
        // This mirrors the post-mortem evidence: the row exists, the
        // status_transition event was written, but no scheduler ever
        // picked the row up.
        db.reconcile_product_executions(&product.id).unwrap();
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        // Confirm the precondition: the row is `ready` and no scheduler
        // is running. (No `kick()` has been called.)
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Ready,
            "precondition: row must be `ready` before the heartbeat fires",
        );

        // Install the heartbeat with a short interval so the test
        // doesn't have to sleep for 15s of production cadence. The
        // heartbeat's startup-stagger sleep also uses this interval.
        let _handle = coordinator.spawn_scheduler_heartbeat(Duration::from_millis(80));

        // Within a few intervals the heartbeat should kick the
        // scheduler, drain the row, and move it through to
        // `waiting_human` via the fake runner.
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;
    }

    /// `stranded_ready_executions` is the read-side helper the heartbeat
    /// uses to surface dropped-wakeup symptoms. This test pins its
    /// contract directly so the heartbeat's `warn!` line is asserted on
    /// without depending on timer behaviour: a row younger than the
    /// configured threshold is invisible to the helper; once the row
    /// crosses the threshold it appears with its actual age.
    #[tokio::test]
    async fn stranded_ready_executions_only_returns_rows_past_the_threshold() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Age boundary");
        db.reconcile_product_executions(&product.id).unwrap();
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        // Threshold far in the future: the freshly-inserted row is too
        // young to count as stranded.
        let fresh = coordinator.stranded_ready_executions(60_000);
        assert!(
            fresh.is_empty(),
            "row younger than the threshold must not be flagged as stranded: {fresh:?}",
        );

        // Threshold of zero: any ready row should appear. The
        // execution we just inserted is in the queue with age >= 0.
        let any = coordinator.stranded_ready_executions(0);
        assert!(
            any.iter().any(|(id, _)| id == &execution_id),
            "with min_age_ms=0 the helper must surface the freshly-inserted ready row; \
             got {any:?}",
        );
    }

    /// Automation-produced tasks (stamped with `source_automation_id`) must be
    /// routed to the automation pool, not the main pool.  A normal chore with no
    /// `source_automation_id` must continue to route to the main pool.
    #[tokio::test]
    async fn automation_produced_task_routes_to_automation_pool() {
        use crate::work::CreateAutomationInput;

        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        let automation = db
            .create_automation(CreateAutomationInput {
                product_id: product.id.clone(),
                name: "Test automation".to_owned(),
                repo_remote_url: None,
                trigger: boss_protocol::AutomationTrigger::Schedule {
                    cron: "0 14 * * 1-5".to_owned(),
                    timezone: "UTC".to_owned(),
                },
                standing_instruction: "do maintenance".to_owned(),
                open_task_limit: 1,
                catch_up_window_secs: None,
                enabled: true,
                created_via: None,
            })
            .unwrap();

        // Create an automation-produced chore and stamp source_automation_id.
        let auto_chore = create_test_chore(&db, product.id.clone(), "Automation chore");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
                rusqlite::params![automation.id, auto_chore.id],
            )
            .unwrap();
        }

        // Create a regular chore with no source_automation_id.
        let main_chore = create_test_chore(&db, product.id.clone(), "Regular chore");

        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let mut coord = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        );
        // Wire in a 1-slot automation pool so we can check idle counts.
        coord.set_automation_pool(WorkerPool::new_automation(1));
        let coordinator = Arc::new(coord);
        coordinator.kick();

        // Wait for both chores to be dispatched.
        for _ in 0..200 {
            let executions = db.list_executions(None).unwrap();
            if executions
                .iter()
                .filter(|e| e.status == ExecutionStatus::Running)
                .count()
                == 2
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let executions = db.list_executions(None).unwrap();
        let running: Vec<_> = executions
            .iter()
            .filter(|e| e.status == ExecutionStatus::Running)
            .collect();
        assert_eq!(running.len(), 2, "both chores must be running; got {running:?}");

        // The main pool slot should be claimed by the regular chore.
        assert_eq!(
            coordinator.worker_pool().idle_count().await,
            0,
            "main pool slot must be claimed by the regular chore"
        );
        // The automation pool slot should be claimed by the automation chore.
        assert_eq!(
            coordinator.automation_worker_pool().idle_count().await,
            0,
            "automation pool slot must be claimed by the automation-produced chore"
        );

        let _ = auto_chore;
        let _ = main_chore;
    }

    /// When the coordinator dispatches an automation-pool execution the
    /// `worker_id` passed to the runner must carry the `"auto-worker-"`
    /// prefix and decode (via `slot_id_from_worker_id`) to a slot id
    /// that is strictly greater than `MAX_WORKER_POOL_SIZE` — i.e. it
    /// must land in the automation-pool slot range (Kira/Dax/Bashir),
    /// not the regular-pool range (Riker … O'Brien). This is the pane-
    /// spawn correctness regression test for the T1104 incident where
    /// `auto-worker-1` was decoded as slot 1 (Riker) instead of slot
    /// 9 (Kira).
    #[tokio::test]
    async fn automation_dispatch_worker_id_maps_to_automation_pool_slot() {
        use crate::work::CreateAutomationInput;

        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        let automation = db
            .create_automation(CreateAutomationInput {
                product_id: product.id.clone(),
                name: "Slot-range test".to_owned(),
                repo_remote_url: None,
                trigger: boss_protocol::AutomationTrigger::Schedule {
                    cron: "0 14 * * 1-5".to_owned(),
                    timezone: "UTC".to_owned(),
                },
                standing_instruction: "do it".to_owned(),
                open_task_limit: 1,
                catch_up_window_secs: None,
                enabled: true,
                created_via: None,
            })
            .unwrap();

        let auto_chore = create_test_chore(&db, product.id.clone(), "Slot-range chore");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
                rusqlite::params![automation.id, auto_chore.id],
            )
            .unwrap();
        }
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(0), cube.clone(), runner.clone());
        coord.set_automation_pool(WorkerPool::new_automation(1));
        let coordinator = Arc::new(coord);
        coordinator.kick();

        // Wait for the execution to reach running.
        for _ in 0..200 {
            let execs = db.list_executions(None).unwrap();
            if execs.iter().any(|e| e.status == ExecutionStatus::Running) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let calls = runner.calls.lock().await;
        assert_eq!(calls.len(), 1, "exactly one run should have been dispatched");
        let (worker_id, _, _, _) = &calls[0];

        // The worker_id must carry the automation-pool prefix.
        assert!(
            worker_id.starts_with(AUTOMATION_WORKER_ID_PREFIX),
            "automation-pool execution must receive an auto-worker-N worker_id, got {worker_id:?}"
        );

        // Decoded slot must be in the automation-pool range (> MAX_WORKER_POOL_SIZE).
        let slot = slot_id_from_worker_id(worker_id)
            .unwrap_or_else(|| panic!("slot_id_from_worker_id failed for {worker_id:?}"));
        assert!(
            slot as usize > MAX_WORKER_POOL_SIZE,
            "automation slot_id {slot} must be > {MAX_WORKER_POOL_SIZE} (the regular-pool ceiling); \
             got slot {slot} — automation pane would land on a regular-pool pane (T1104 regression)"
        );
    }

    /// Automation pool exhaustion must not block main-pool dispatch.
    /// When the automation pool is full, regular chores continue to be
    /// dispatched on the main pool.
    #[tokio::test]
    async fn automation_pool_exhaustion_does_not_block_main_pool() {
        use crate::work::CreateAutomationInput;

        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        let automation = db
            .create_automation(CreateAutomationInput {
                product_id: product.id.clone(),
                name: "Test automation".to_owned(),
                repo_remote_url: None,
                trigger: boss_protocol::AutomationTrigger::Schedule {
                    cron: "0 14 * * 1-5".to_owned(),
                    timezone: "UTC".to_owned(),
                },
                standing_instruction: "do maintenance".to_owned(),
                open_task_limit: 5,
                catch_up_window_secs: None,
                enabled: true,
                created_via: None,
            })
            .unwrap();

        // Two automation-produced chores (pool size will be 1, so the second stays ready).
        for n in 0..2 {
            let chore = create_test_chore(&db, product.id.clone(), format!("Auto chore {n}"));
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
                rusqlite::params![automation.id, chore.id],
            )
            .unwrap();
        }

        // One regular chore — must still be dispatched even when the automation pool is full.
        create_test_chore(&db, product.id.clone(), "Regular chore");

        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        // Main pool: 1 slot; automation pool: 1 slot.
        let mut coord = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        );
        coord.set_automation_pool(WorkerPool::new_automation(1));
        let coordinator = Arc::new(coord);
        coordinator.kick();

        // Wait for at least 2 executions to be running (1 main + 1 automation).
        for _ in 0..200 {
            let executions = db.list_executions(None).unwrap();
            if executions
                .iter()
                .filter(|e| e.status == ExecutionStatus::Running)
                .count()
                >= 2
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let executions = db.list_executions(None).unwrap();
        let running = executions
            .iter()
            .filter(|e| e.status == ExecutionStatus::Running)
            .count();
        assert_eq!(
            running, 2,
            "exactly 2 executions must be running (1 per pool); got {running}"
        );
        // The third execution (second auto chore) must remain ready — automation pool full.
        let ready = executions.iter().filter(|e| e.status == ExecutionStatus::Ready).count();
        assert_eq!(
            ready, 1,
            "the second auto chore must be deferred (automation pool full); got {ready} ready"
        );
    }

    /// A `pr_review` execution must route to the review pool; a normal
    /// chore execution must continue to route to the main pool. Review is
    /// checked before automation so the reviewer of an automation-produced
    /// task still lands in the review pool.
    #[tokio::test]
    async fn pr_review_execution_routes_to_review_pool() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let cube = Arc::new(FakeCubeClient::default());
        let mut coord = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        );
        coord.set_review_pool(WorkerPool::new_review(1));
        coord.set_automation_pool(WorkerPool::new_automation(1));

        let review_exec = WorkExecution::builder()
            .id("exec-review")
            .work_item_id("task-under-review")
            .created_at("1")
            .kind(ExecutionKind::PrReview)
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .status(ExecutionStatus::Ready)
            .build();
        assert!(coord.execution_targets_review_pool(&review_exec));

        // pool_for_execution must hand back the review pool — claiming from
        // it yields a `review-` worker id.
        let wid = coord
            .pool_for_execution(&review_exec)
            .claim_worker("exec-review", None)
            .await
            .unwrap();
        assert!(
            wid.starts_with(REVIEW_WORKER_ID_PREFIX),
            "pr_review must route to the review pool, got {wid:?}"
        );

        // A normal chore execution must NOT target the review pool.
        let chore_exec = WorkExecution::builder()
            .id("exec-chore")
            .work_item_id("regular-task")
            .created_at("1")
            .kind(ExecutionKind::ChoreImplementation)
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .status(ExecutionStatus::Ready)
            .build();
        assert!(!coord.execution_targets_review_pool(&chore_exec));
        let wid2 = coord
            .pool_for_execution(&chore_exec)
            .claim_worker("exec-chore", None)
            .await
            .unwrap();
        assert!(
            wid2.starts_with("worker-"),
            "chore must route to the main pool, got {wid2:?}"
        );
    }

    /// Releasing a `review-` worker id must free a slot in the review pool
    /// (not the main or automation pool). This is the release-routing-by-
    /// prefix guarantee `release_worker_and_kick` relies on.
    #[tokio::test]
    async fn review_prefix_worker_id_releases_to_review_pool() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let cube = Arc::new(FakeCubeClient::default());
        let mut coord = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(2),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        );
        coord.set_review_pool(WorkerPool::new_review(2));
        coord.set_automation_pool(WorkerPool::new_automation(2));
        let coordinator = Arc::new(coord);

        let wid = coordinator
            .review_worker_pool()
            .claim_worker("exec-r", None)
            .await
            .unwrap();
        assert!(wid.starts_with(REVIEW_WORKER_ID_PREFIX));
        assert_eq!(coordinator.review_worker_pool().idle_count().await, 1);

        // Release routes by prefix → the review-pool slot is freed.
        coordinator.release_worker_and_kick(&wid, None).await;
        assert_eq!(
            coordinator.review_worker_pool().idle_count().await,
            2,
            "release must free the review-pool slot"
        );
        // The other pools must be untouched.
        assert_eq!(coordinator.worker_pool().idle_count().await, 2);
        assert_eq!(coordinator.automation_worker_pool().idle_count().await, 2);
    }

    /// Review pool exhaustion must not block main-pool dispatch. When the
    /// review pool is full, a regular chore continues to be dispatched on
    /// the main pool and the deferred `pr_review` stays `ready`.
    #[tokio::test]
    async fn review_pool_exhaustion_does_not_block_main_pool() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);

        // One regular chore — must still dispatch even when review is full.
        create_test_chore(&db, product.id.clone(), "Regular chore");
        db.reconcile_product_executions(&product.id).unwrap();

        // Insert a ready `pr_review` execution. It never reaches the
        // schedule path in this test — the review pool is pre-occupied, so
        // the claim fails first — so a synthetic work_item_id is fine.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO work_executions
                   (id, work_item_id, kind, status, repo_remote_url, priority, created_at)
                 VALUES (?1, ?2, ?3, 'ready', ?4, 0, '1')",
                rusqlite::params![
                    "exec-review-1",
                    "task-under-review",
                    EXECUTION_KIND_PR_REVIEW,
                    "git@github.com:spinyfin/mono.git"
                ],
            )
            .unwrap();
        }

        let cube = Arc::new(FakeCubeClient::default());
        // Main pool: 1 slot; review pool: 1 slot.
        let mut coord = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        );
        coord.set_review_pool(WorkerPool::new_review(1));
        let coordinator = Arc::new(coord);

        // Pre-occupy the review pool's only slot so the pr_review can't claim.
        let occupied = coordinator.review_worker_pool().claim_worker("occupied", None).await;
        assert!(occupied.is_some(), "review pool slot must be claimable");

        coordinator.kick();

        // Wait for the main chore to run.
        for _ in 0..200 {
            let execs = db.list_executions(None).unwrap();
            if execs
                .iter()
                .any(|e| e.status == ExecutionStatus::Running && e.kind != ExecutionKind::PrReview)
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let execs = db.list_executions(None).unwrap();
        let main_running = execs
            .iter()
            .filter(|e| e.status == ExecutionStatus::Running && e.kind != ExecutionKind::PrReview)
            .count();
        assert_eq!(
            main_running, 1,
            "the regular chore must run even when the review pool is full"
        );
        // The pr_review must stay ready — review pool was full.
        let review_ready = execs
            .iter()
            .filter(|e| e.kind == ExecutionKind::PrReview && e.status == ExecutionStatus::Ready)
            .count();
        assert_eq!(
            review_ready, 1,
            "the pr_review must be deferred while the review pool is full"
        );
    }

    // ── Reviewer workspace positioning tests ──────────────────────────────────

    use crate::work::WorkItemPatch;

    /// Helper: create a product + chore pair and return their ids. When
    /// `pr_url` is `Some`, the chore's `pr_url` field is also set so that
    /// `schedule_execution` picks it up for the reviewer positioning path.
    fn make_pr_review_fixture(db: &WorkDb, pr_url: Option<&str>) -> (String, String) {
        let product = create_test_product_named(db, "TestProduct");
        let chore = create_test_chore_manual(db, product.id.clone(), "Test chore");
        if let Some(url) = pr_url {
            db.update_work_item(
                &chore.id,
                WorkItemPatch {
                    pr_url: Some(url.to_owned()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        }
        (product.id, chore.id)
    }

    /// When a `pr_review` execution has a non-empty `pr_url` on its task,
    /// `schedule_execution` must call `cube workspace goto` after the lease to
    /// position the workspace on the PR head, and must NOT call `create_change`.
    #[tokio::test]
    async fn pr_review_with_pr_url_positions_via_goto_not_create_change() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/42";
        let (_, chore_id) = make_pr_review_fixture(&db, Some(pr_url));

        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });

        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        coord.set_review_pool(WorkerPool::new_review(1));
        let coordinator = Arc::new(coord);

        let worker_id = coordinator
            .pool_for_execution(&execution)
            .claim_worker(&execution.id, None)
            .await
            .expect("review pool slot available");

        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(result.is_ok(), "schedule_execution must succeed: {result:?}");

        // goto_workspace must have been called with pr=42.
        let goto_calls = cube.goto_calls.lock().await;
        assert_eq!(goto_calls.len(), 1, "goto_workspace must be called exactly once");
        assert_eq!(goto_calls[0].1, 42, "goto_workspace must receive pr=42 for PR #42");
        drop(goto_calls);

        // lease_workspace must NOT have received resume_pr (it no longer exists).
        let lease_calls = cube.lease_calls.lock().await;
        assert_eq!(lease_calls.len(), 1, "lease_workspace must be called exactly once");
        drop(lease_calls);

        // create_change must NOT have been called — positioning happened via goto.
        assert!(
            cube.create_calls.lock().await.is_empty(),
            "create_change must not be called for the reviewer positioning path"
        );
    }

    /// When the `cube workspace lease` call fails for a `pr_review` execution,
    /// `schedule_execution` must record a `cube_workspace_lease_failed` start
    /// failure and must not release a workspace (none was acquired).
    #[tokio::test]
    async fn pr_review_lease_failure_records_start_failure() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/7";
        let (_, chore_id) = make_pr_review_fixture(&db, Some(pr_url));

        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let runner = Arc::new(FakeExecutionRunner::default());

        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        coord.set_review_pool(WorkerPool::new_review(1));
        // Disable retries so the pre-start failure is terminal immediately.
        let coordinator = Arc::new(coord.with_pre_start_retry_delays(Vec::new()));

        let worker_id = coordinator
            .pool_for_execution(&execution)
            .claim_worker(&execution.id, None)
            .await
            .expect("review pool slot available");

        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(result.is_err(), "schedule_execution must fail when the lease fails");

        // No workspace was ever leased so there is nothing to release.
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "no release should occur when the lease itself failed"
        );

        // A cube_workspace_lease_failed attention item must exist.
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(
            items.iter().any(|i| i.kind == "cube_workspace_lease_failed"),
            "expected a cube_workspace_lease_failed attention item, got {:?}",
            items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
        );
    }

    /// When `cube workspace goto` fails for a `pr_review` execution, dispatch must
    /// fail loudly with a `cube_workspace_positioning_failed` attention item.
    #[tokio::test]
    async fn pr_review_goto_failure_records_positioning_failed() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/7";
        let (_, chore_id) = make_pr_review_fixture(&db, Some(pr_url));

        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_goto: true,
            ..FakeCubeClient::default()
        });
        let runner = Arc::new(FakeExecutionRunner::default());

        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        coord.set_review_pool(WorkerPool::new_review(1));
        let coordinator = Arc::new(coord.with_pre_start_retry_delays(Vec::new()));

        let worker_id = coordinator
            .pool_for_execution(&execution)
            .claim_worker(&execution.id, None)
            .await
            .expect("review pool slot available");

        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(result.is_err(), "schedule_execution must fail when goto fails");

        // The workspace was leased and must be released after goto failure.
        assert_eq!(
            cube.release_calls.lock().await.len(),
            1,
            "workspace must be released after goto failure"
        );

        // A cube_workspace_positioning_failed attention item must exist.
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(
            items.iter().any(|i| i.kind == "cube_workspace_positioning_failed"),
            "expected a cube_workspace_positioning_failed attention item, got {:?}",
            items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
        );
    }

    /// When a `pr_review` execution has no `pr_url` on its task, the normal
    /// `create_change` path must be used.
    #[tokio::test]
    async fn pr_review_without_pr_url_uses_create_change_path() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        // No pr_url on the chore.
        let (_, chore_id) = make_pr_review_fixture(&db, None);

        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });

        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        coord.set_review_pool(WorkerPool::new_review(1));
        let coordinator = Arc::new(coord);

        let worker_id = coordinator
            .pool_for_execution(&execution)
            .claim_worker(&execution.id, None)
            .await
            .expect("review pool slot available");

        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(
            result.is_ok(),
            "schedule_execution must succeed on the create_change path: {result:?}"
        );

        // goto_workspace must NOT have been called — no pr_url means no PR positioning.
        assert!(
            cube.goto_calls.lock().await.is_empty(),
            "goto_workspace must not be called when pr_url is absent"
        );

        // create_change must have been called once.
        assert_eq!(
            cube.create_calls.lock().await.len(),
            1,
            "create_change must be called when pr_url is absent"
        );
    }

    // ── Dispatch-pause review exemption tests ──────────────────────────────────

    /// An operator-originated pause (`bossctl dispatch pause`, the human
    /// toggle) must NOT hold `pr_review` executions: a review is the
    /// lifecycle of a change already in flight, not new work, so it keeps
    /// dispatching through `drain_ready_queue` while paused.
    #[tokio::test]
    async fn operator_pause_exempts_ready_pr_review_execution() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let (_, chore_id) = make_pr_review_fixture(&db, None);
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        coord.set_review_pool(WorkerPool::new_review(1));
        let coordinator = Arc::new(coord);

        coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Operator);
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

        assert_eq!(
            runner.calls.lock().await.len(),
            1,
            "the review execution must dispatch despite the operator pause"
        );
    }

    /// The same operator pause must still hold a main-pool (non-review)
    /// execution — only review rows are exempt. It stays `ready` until an
    /// explicit resume kicks the scheduler.
    #[tokio::test]
    async fn operator_pause_holds_main_pool_row_until_resume() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));

        coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Operator);
        coordinator.kick();

        // No positive event to wait on — the assertion is that nothing
        // changes while paused.
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runner.calls.lock().await.len(),
            0,
            "a main-pool row must be held, not dispatched, during an operator pause"
        );
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Ready,
            "the held execution must remain `ready` while paused"
        );

        // Resume mirrors `handle_set_dispatch_paused`: flip the flag, then
        // kick so the held row drains immediately.
        coordinator.set_dispatch_paused(false, 0, DispatchPauseOrigin::Operator);
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;
    }

    /// A breaker-originated pause (the spawn-capability circuit breaker —
    /// see `spawn_health.rs`) must hold `pr_review` executions too: the
    /// app's spawn path itself is broken, so exempting reviews would just
    /// burn another spawn attempt against the same dead path.
    #[tokio::test]
    async fn breaker_pause_holds_pr_review_execution_too() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let (_, chore_id) = make_pr_review_fixture(&db, None);
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        coord.set_review_pool(WorkerPool::new_review(1));
        let coordinator = Arc::new(coord);

        coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Breaker);
        coordinator.kick();

        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runner.calls.lock().await.len(),
            0,
            "a breaker-tripped pause must hold review executions, not exempt them"
        );
        assert_eq!(
            db.get_execution(&execution.id).unwrap().status,
            ExecutionStatus::Ready,
            "the held review execution must remain `ready` while breaker-paused"
        );
    }

    /// Rows held by an operator pause must drain exactly once on resume,
    /// even if the scheduler was kicked multiple times while paused (e.g.
    /// by unrelated work being created) — no double-dispatch of the same
    /// held row.
    #[tokio::test]
    async fn resume_kick_drains_held_row_exactly_once() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
        db.reconcile_product_executions(&product.id).unwrap();
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));

        coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Operator);
        // Multiple kicks while paused must not cause multiple dispatches once resumed.
        coordinator.kick();
        coordinator.kick();
        coordinator.kick();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runner.calls.lock().await.len(),
            0,
            "must stay held across repeated kicks"
        );

        coordinator.set_dispatch_paused(false, 0, DispatchPauseOrigin::Operator);
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;

        // Give any (incorrect) duplicate dispatch a window to land before asserting.
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runner.calls.lock().await.len(),
            1,
            "the held row must be dispatched exactly once after resume"
        );
    }

    /// When a `revision_implementation` execution has a non-empty `pr_url`,
    /// `schedule_execution` must call `cube workspace goto` after the lease to
    /// position the workspace on the PR head, and must NOT call `create_change`.
    #[tokio::test]
    async fn revision_with_pr_url_positions_via_goto_not_create_change() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/99";
        let (_, chore_id) = make_pr_review_fixture(&db, None);

        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });

        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));

        let worker_id = coordinator
            .pool_for_execution(&execution)
            .claim_worker(&execution.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(result.is_ok(), "schedule_execution must succeed: {result:?}");

        // goto_workspace must have been called with pr=99.
        let goto_calls = cube.goto_calls.lock().await;
        assert_eq!(goto_calls.len(), 1, "goto_workspace must be called exactly once");
        assert_eq!(goto_calls[0].1, 99, "goto_workspace must receive pr=99 for PR #99");
        drop(goto_calls);

        // create_change must NOT have been called — positioning happened via goto.
        assert!(
            cube.create_calls.lock().await.is_empty(),
            "create_change must not be called for the revision positioning path"
        );
    }

    /// Regression: a `revision_implementation` execution with `pr_url = None` (as
    /// produced by the orphan-sweep re-dispatch and `bossctl work start` paths) must
    /// still call `cube workspace goto` using the chain root's PR URL. Without the
    /// chain-root fallback the positioning gate is silently skipped and the worker
    /// lands on main instead of the PR head.
    #[tokio::test]
    async fn revision_without_pr_url_falls_back_to_chain_root_for_goto() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let root_pr_url = "https://github.com/spinyfin/mono/pull/88";
        // Chain root: a chore with a bound PR URL.
        let (_, root_id) = make_pr_review_fixture(&db, Some(root_pr_url));
        // Revision hanging off the chain root, created without an execution.pr_url.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_no_pr_url', product_id, 'revision', 'Fix review findings', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        // Execution with pr_url absent — simulates orphan-sweep re-dispatch path.
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_no_pr_url")
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        assert!(execution.pr_url.is_none(), "test precondition: pr_url must be absent");

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });

        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));

        let worker_id = coordinator
            .pool_for_execution(&execution)
            .claim_worker(&execution.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(result.is_ok(), "schedule_execution must succeed: {result:?}");

        // goto_workspace must have been called with pr=88 (from the chain root).
        let goto_calls = cube.goto_calls.lock().await;
        assert_eq!(
            goto_calls.len(),
            1,
            "goto_workspace must be called exactly once via chain-root fallback"
        );
        assert_eq!(
            goto_calls[0].1, 88,
            "goto_workspace must receive pr=88 from the chain root PR URL"
        );
        drop(goto_calls);

        // create_change must NOT have been called — positioning happened via goto.
        assert!(
            cube.create_calls.lock().await.is_empty(),
            "create_change must not be called when chain-root fallback positions via goto"
        );
    }

    /// When a `revision_implementation` lease fails (which now includes
    /// When the `cube workspace lease` call fails for a `revision_implementation`
    /// execution, `schedule_execution` must record a `cube_workspace_lease_failed`
    /// start failure and must not have acquired a workspace to release.
    #[tokio::test]
    async fn revision_lease_failure_records_start_failure() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/100";
        let (_, chore_id) = make_pr_review_fixture(&db, None);

        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let runner = Arc::new(FakeExecutionRunner::default());

        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .pool_for_execution(&execution)
            .claim_worker(&execution.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(result.is_err(), "schedule_execution must fail when the lease fails");

        // No workspace was ever leased so there is nothing to release.
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "no release should occur when the lease itself failed"
        );

        // A cube_workspace_lease_failed attention item must exist.
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(
            items.iter().any(|i| i.kind == "cube_workspace_lease_failed"),
            "expected a cube_workspace_lease_failed attention item, got {:?}",
            items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
        );
    }

    /// The soft-prefer fallback must succeed when the preferred workspace is held,
    /// and workspace positioning must use `goto_workspace` (not `create_change`)
    /// when a `pr_url` is present.
    #[tokio::test]
    async fn revision_soft_prefer_fallback_positions_via_goto() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/42";
        let (_, chore_id) = make_pr_review_fixture(&db, None);

        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .preferred_workspace_id("mono-agent-001")
                    .prefer_is_soft(true)
                    .build(),
            )
            .unwrap();

        // Simulate the preferred workspace being held by the parent chore's worker.
        // Attempt 1 (--prefer mono-agent-001 --resume-pr 42) fails; attempt 2
        // (no --prefer, --resume-pr 42) must succeed and position the workspace.
        let cube = Arc::new(FakeCubeClient {
            fail_lease_when_prefer_set: true,
            ..FakeCubeClient::default()
        });
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });

        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));

        let worker_id = coordinator
            .pool_for_execution(&execution)
            .claim_worker(&execution.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(
            result.is_ok(),
            "schedule_execution must succeed via soft-prefer fallback: {result:?}"
        );

        let calls = cube.lease_calls.lock().await;
        assert_eq!(
            calls.len(),
            2,
            "two lease attempts expected: prefer failed then fallback succeeded"
        );

        // Attempt 1 targeted the preferred workspace; attempt 2 did not.
        assert_eq!(
            calls[0].2,
            Some("mono-agent-001".to_owned()),
            "attempt 1 must pass the preferred workspace"
        );
        assert_eq!(calls[1].2, None, "attempt 2 must not specify a preferred workspace");
        drop(calls);

        // Positioning happens via goto_workspace (not create_change) when pr_url is set.
        assert!(
            cube.create_calls.lock().await.is_empty(),
            "create_change must not be called when goto_workspace positions the workspace"
        );
        assert_eq!(
            cube.goto_calls.lock().await.len(),
            1,
            "goto_workspace must be called once for the PR positioning"
        );
    }

    /// Fix (a) regression: `schedule_execution` must refuse to dispatch a
    /// `ready` execution when the work item is still gated by an unmet
    /// prerequisite. Concretely: a `ready` row created via a timing race
    /// (autostart flipped before the dep edge committed) must be downgraded
    /// back to `waiting_dependency` and must NOT result in a cube workspace
    /// lease or worker spawn.
    #[tokio::test]
    async fn schedule_execution_rejects_gated_ready_execution() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let product = create_test_product(&db);

        // prereq: a separate chore that has not yet completed.
        let prereq = create_test_chore(&db, product.id.clone(), "Prereq (still active)");

        // dependent: the gated chore. Created with autostart=false so no
        // execution is created automatically.
        let dep = create_test_chore_manual(&db, product.id.clone(), "Gated chore (should not dispatch)");

        // Wire the blocks edge: dep requires prereq to be done.
        db.add_dependency(AddDependencyInput {
            dependent: dep.id.clone(),
            prerequisite: prereq.id.clone(),
            relation: None,
        })
        .unwrap();

        // Simulate the race: directly insert a `ready` execution as if the
        // autostart flip and reconcile ran before the dep edge committed.
        let execution = create_ready_chore_execution(&db, dep.id.clone());

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner::default());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&execution.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&execution, &worker_id).await;
        assert!(result.is_err(), "gated execution must be refused by schedule_execution");

        // The execution must have been downgraded to waiting_dependency, not
        // abandoned — it can be re-promoted when the gate clears.
        let updated = db.get_execution(&execution.id).unwrap();
        assert_eq!(
            updated.status,
            ExecutionStatus::WaitingDependency,
            "gated execution must be downgraded to waiting_dependency, got {:?}",
            updated.status,
        );

        // No cube calls must have been made — no workspace was leased.
        assert!(
            cube.lease_calls.lock().await.is_empty(),
            "no cube lease must occur for a gated execution"
        );
        assert!(
            cube.ensure_calls.lock().await.is_empty(),
            "no cube ensure must occur for a gated execution"
        );
    }

    /// Per-PR single-writer guard (T1577 / T1815 incident). When an
    /// implementation execution on the chain root is live, dispatching a
    /// conflict-resolution revision (a DIFFERENT work item that targets the
    /// SAME PR via the chain) must be DEFERRED — not co-dispatched onto the
    /// shared jj backing store, and not abandoned. The revision execution
    /// must stay `ready` and must dispatch only once the live sibling reaps.
    #[tokio::test]
    async fn schedule_execution_defers_revision_behind_live_chain_sibling() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/1467";
        // Chain root: a chore in_review with a bound PR (the implementation task).
        let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            // A conflict-resolution revision hanging off the chain root.
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_serialize', product_id, 'revision', 'Resolve conflicts', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        // Live implementation resume on the chain root.
        let root_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root_id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();

        // Ready conflict-resolution revision execution targeting the same PR.
        let revision_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_serialize")
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&revision_exec.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&revision_exec, &worker_id).await;
        assert!(
            result.is_err(),
            "revision must be deferred while a chain sibling is live: {result:?}",
        );

        // Deferred, NOT abandoned: the execution stays `ready` so it can be
        // re-attempted when the live sibling reaps.
        let after_defer = db.get_execution(&revision_exec.id).unwrap();
        assert_eq!(
            after_defer.status,
            ExecutionStatus::Ready,
            "deferred revision must remain ready, not abandoned/waiting_dependency, got {:?}",
            after_defer.status,
        );
        assert!(
            cube.lease_calls.lock().await.is_empty(),
            "no cube workspace may be leased while serialized behind a live chain sibling",
        );

        // The live sibling reaps. Mirror the drain loop releasing the worker,
        // then re-attempt: the revision must now dispatch.
        coordinator.worker_pool().release_worker(&worker_id, None).await;
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = 'completed', finished_at = '1' WHERE id = ?1",
                rusqlite::params![root_exec.id],
            )
            .unwrap();
        }

        let worker_id2 = coordinator
            .worker_pool()
            .claim_worker(&revision_exec.id, None)
            .await
            .expect("worker pool slot available after release");
        let result_after = coordinator.schedule_execution(&revision_exec, &worker_id2).await;
        assert!(
            result_after.is_ok(),
            "revision must dispatch once the live chain sibling has reaped: {result_after:?}",
        );
        assert!(
            !cube.lease_calls.lock().await.is_empty(),
            "a cube workspace must be leased once the chain is clear",
        );
    }

    /// The chore_18c0da77bef326b0_840 fix: a live `pr_review` sibling is
    /// strictly read-only, so it must NOT chain-serialize a merge-conflict-
    /// fix revision behind it — the priority inversion where the fix waited
    /// the full length of a review run (T270/T258, 2026-07-10). Unlike
    /// [`schedule_execution_defers_revision_behind_live_chain_sibling`]
    /// (whose live sibling is a writer and must still block), this revision
    /// must dispatch immediately even though the review is still live.
    #[tokio::test]
    async fn schedule_execution_bypasses_live_review_sibling_for_merge_conflict_revision() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/1467";
        let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id, created_via)
                 SELECT 'task_rev_conflict_bypass', product_id, 'revision', 'Resolve conflicts', '', 'todo', '1', '1', ?1, 'merge-conflict:crz_bypass'
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        // Live PR review on the chain root — read-only, must not block.
        let _root_review_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();

        // Ready merge-conflict revision execution targeting the same PR.
        let revision_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_conflict_bypass")
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&revision_exec.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&revision_exec, &worker_id).await;
        assert!(
            result.is_ok(),
            "merge-conflict revision must bypass a live read-only pr_review sibling: {result:?}",
        );
        assert!(
            !cube.lease_calls.lock().await.is_empty(),
            "a cube workspace must be leased — the review sibling must not block the lease",
        );
    }

    /// The correctness gap this revision closes: a live `pr_review` on the
    /// chain root must NOT mask a live *writer* on a chain descendant. Sets
    /// up a root review PLUS a live descendant writer (another
    /// merge-conflict-fix revision execution, already running) and confirms
    /// a SECOND ready merge-conflict revision on the same chain is still
    /// `Blocked` (no workspace leased) — not `ReviewBypassed`. Before the
    /// `resolve_chain_hold`/`live_chain_siblings` fix, the root-first single-
    /// sibling walk would have returned only the review, bypassed, and
    /// co-dispatched a second writer alongside the still-live descendant
    /// writer — the exact T1577/T1815 two-writer hazard this guard exists to
    /// prevent.
    #[tokio::test]
    async fn schedule_execution_blocks_conflict_revision_when_review_masks_live_descendant_writer() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/1467";
        let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id, created_via)
                 SELECT 'task_rev_conflict_writer', product_id, 'revision', 'Resolve conflicts A', '', 'todo', '1', '1', ?1, 'merge-conflict:crz_a'
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id, created_via)
                 SELECT 'task_rev_conflict_second', product_id, 'revision', 'Resolve conflicts B', '', 'todo', '1', '1', ?1, 'merge-conflict:crz_b'
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        // Live PR review on the chain root — read-only.
        let _root_review_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();

        // Live WRITER on a chain descendant — the first conflict-fix
        // revision, already dispatched and still running.
        let _descendant_writer_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_conflict_writer".to_owned())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Running)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        // A second, ready merge-conflict revision on the same chain.
        let revision_exec_b = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_conflict_second".to_owned())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&revision_exec_b.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&revision_exec_b, &worker_id).await;
        assert!(
            result.is_err(),
            "a second conflict revision must NOT bypass when a live descendant writer is masked \
             behind the root review: {result:?}",
        );
        let after_defer = db.get_execution(&revision_exec_b.id).unwrap();
        assert_eq!(
            after_defer.status,
            ExecutionStatus::Ready,
            "deferred revision must remain ready, got {:?}",
            after_defer.status,
        );
        assert!(
            cube.lease_calls.lock().await.is_empty(),
            "no cube workspace may be leased while a live writer sibling exists elsewhere in the chain, \
             even if a review is also live",
        );
    }

    /// A non-conflict revision (no `merge-conflict:` `created_via` marker)
    /// gets none of the bypass above — it keeps serializing behind a live
    /// `pr_review` sibling exactly as before this fix. Only merge-conflict
    /// revisions are urgent enough, and safe enough (see the module docs on
    /// `ExecutionCoordinator::resolve_chain_hold`), to bypass a review.
    #[tokio::test]
    async fn schedule_execution_still_defers_non_conflict_revision_behind_live_review_sibling() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/1467";
        let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            // No `created_via` marker — an ordinary/operator-filed revision.
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_no_bypass', product_id, 'revision', 'Address feedback', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        let _root_review_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();

        let revision_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_no_bypass")
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&revision_exec.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&revision_exec, &worker_id).await;
        assert!(
            result.is_err(),
            "a non-conflict revision must still defer behind a live pr_review sibling: {result:?}",
        );
        let after_defer = db.get_execution(&revision_exec.id).unwrap();
        assert_eq!(
            after_defer.status,
            ExecutionStatus::Ready,
            "deferred revision must remain ready, got {:?}",
            after_defer.status,
        );
        assert!(
            cube.lease_calls.lock().await.is_empty(),
            "no cube workspace may be leased while serialized behind a live review sibling",
        );
    }

    /// The auto-dispatcher's pre-claim chain check must record a
    /// `dispatch_wait_reason` that distinguishes a review-held chain hold
    /// from a writer-held one — the trace line chore_18c0da77bef326b0_840
    /// asked for, so an operator (or the kanban card) can tell "waiting on
    /// a read-only review" from "waiting on another writer" without
    /// cross-referencing `engine-trace.jsonl` by hand.
    #[tokio::test]
    async fn drain_records_review_held_wait_reason_distinct_from_writer_held() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/9001";
        let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_wait_reason', product_id, 'revision', 'Address feedback', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        let _root_review_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();

        let revision_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_wait_reason")
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();

        let mut wait_reason = None;
        for _ in 0..200 {
            wait_reason = db.get_execution(&revision_exec.id).unwrap().dispatch_wait_reason;
            if wait_reason.is_some() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let wait_reason = wait_reason.expect("dispatch_wait_reason must be set");
        assert!(
            wait_reason.contains("an automated PR review runs at a time"),
            "a revision deferred behind a live pr_review sibling must record the review-held \
             reason, not the generic writer-held wording; got {wait_reason:?}",
        );
        assert!(
            !wait_reason.contains("revisions on the same PR run one at a time"),
            "must not use the writer-held phrasing for a review-held wait; got {wait_reason:?}",
        );
        assert!(
            !wait_reason.to_lowercase().contains("sibling"),
            "operator-facing wait reason must not use engine-internal \"sibling\" vocabulary; \
             got {wait_reason:?}",
        );
    }

    /// The `chain_serialized` (writer-held, not review-held) wait reason
    /// persisted into `dispatch_wait_reason` must name the concrete
    /// blocking task (`T<short_id>` + name) and PR — not the opaque
    /// "PR sibling" wording the T2469 incident (mono#1901) reported, which
    /// gave an operator no way to tell what a "sibling" was, which task
    /// was blocking, or which PR was involved.
    #[tokio::test]
    async fn drain_records_writer_held_wait_reason_names_blocking_task_and_pr() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/1901";
        let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_writer_held', product_id, 'revision', 'Fix failing CI', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        // Live writer execution on the chain root (blocks the revision below).
        let root_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root_id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();
        let root_short_label = match db.get_work_item(&root_id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => t.short_label(),
            _ => panic!("expected a task/chore work item"),
        };

        let revision_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_writer_held")
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();

        let mut wait_reason = None;
        for _ in 0..200 {
            wait_reason = db.get_execution(&revision_exec.id).unwrap().dispatch_wait_reason;
            if wait_reason.is_some() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let wait_reason = wait_reason.expect("dispatch_wait_reason must be set");
        assert!(
            wait_reason.contains(&root_short_label),
            "wait reason must name the blocking sibling's T-id; got {wait_reason:?}",
        );
        assert!(
            wait_reason.contains("Test chore"),
            "wait reason must name the blocking sibling's task title; got {wait_reason:?}",
        );
        assert!(
            wait_reason.contains("mono#1901"),
            "wait reason must name the blocking PR; got {wait_reason:?}",
        );
        assert!(
            wait_reason.contains("revisions on the same PR run one at a time"),
            "writer-held wait must not use the review-held phrasing; got {wait_reason:?}",
        );
        assert!(
            !wait_reason.to_lowercase().contains("sibling"),
            "operator-facing wait reason must not use engine-internal \"sibling\" vocabulary; \
             got {wait_reason:?}",
        );
        assert!(root_exec.id != revision_exec.id, "sanity: distinct executions");
    }

    /// Redundant-spawn guard emits `host_selected:error` with `reason=redundant_spawn`
    /// when a live execution already exists for the same work item. The execution is
    /// marked abandoned (not left ready), and the stall watchdog must not fire because
    /// the terminal `host_selected:error` event closes the stage immediately.
    #[tokio::test]
    async fn redundant_spawn_guard_emits_terminal_host_selected_error() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Redundant-spawn chore");

        // Simulate the race: a first execution is already live (running).
        let _live_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();

        // A second, redundant `ready` execution arrives (the one under test).
        let redundant_exec = create_ready_chore_execution(&db, chore.id.clone());

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&redundant_exec.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&redundant_exec, &worker_id).await;
        assert!(result.is_err(), "redundant spawn must be rejected: {result:?}");

        // Dispatch timeline: must carry a terminal host_selected:error with the right reason.
        let events = recording.events_for(&redundant_exec.id).await;
        let host_selected = events
            .iter()
            .find(|e| e.stage == "host_selected")
            .unwrap_or_else(|| panic!("expected host_selected event; got {events:#?}"));
        assert_eq!(
            host_selected.outcome, "error",
            "redundant_spawn must surface as host_selected:error; got {host_selected:?}",
        );
        assert_eq!(
            host_selected.details.get("reason").and_then(|v| v.as_str()),
            Some("redundant_spawn"),
            "host_selected:error must name redundant_spawn reason; got {:?}",
            host_selected.details,
        );
        assert!(
            crate::dispatch_reader::is_terminal_event(host_selected),
            "host_selected:error must be terminal so the stall watchdog never fires",
        );

        // No stage_stalled must appear — the terminal event closes the stage immediately.
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
        assert!(
            !stages.contains(&"stage_stalled"),
            "redundant_spawn must not produce a stage_stalled event; got {stages:?}",
        );

        // Post-condition: the redundant execution is abandoned, not left in a live state.
        let after = db.get_execution(&redundant_exec.id).unwrap();
        assert_eq!(
            after.status,
            ExecutionStatus::Abandoned,
            "redundant execution must be marked abandoned; got {:?}",
            after.status,
        );

        // No cube workspace was touched.
        assert!(
            cube.lease_calls.lock().await.is_empty(),
            "no cube workspace may be leased for a redundant spawn",
        );
    }

    /// Liveness gate (2026-06-14 waiting_human-zombie fix): when the "live"
    /// blocker the redundant-spawn guard finds is actually a zombie — a local
    /// execution whose recorded cube workspace directory has vanished (its
    /// pane is gone) — the guard must reconcile it to a terminal status and
    /// let the new spawn PROCEED, rather than rejecting it forever as
    /// redundant. This is the exact wedge that broke all automations for 17
    /// days: three triage rows stuck `waiting_human` after their workspaces
    /// were migrated away blocked every subsequent fire with `redundant_spawn`.
    #[tokio::test]
    async fn redundant_spawn_guard_reconciles_lost_workspace_zombie_and_proceeds() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Zombie-blocked chore");

        // The "live" blocker: a running execution with a real run on the local
        // host, but whose workspace directory no longer exists on disk — a
        // dead pane the engine never reaped (no Stop hook).
        let zombie = create_ready_chore_execution(&db, chore.id.clone());
        db.start_execution_run(
            &zombie.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "mono-agent-028",
            "/nonexistent/old-root/mono-agent-028",
        )
        .unwrap();
        // Park it in waiting_human, exactly like a just-spawned worker.
        let zrun = db
            .active_run_ids_for_execution(&zombie.id)
            .unwrap()
            .into_iter()
            .next()
            .expect("zombie has a run");
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&zombie.id)
                .run_id(&zrun)
                .execution_status(ExecutionStatus::WaitingHuman)
                .run_status("completed")
                .build(),
        )
        .unwrap();

        // The new execution the scheduler wants to spawn.
        let fresh = create_ready_chore_execution(&db, chore.id.clone());

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&fresh.id, None)
            .await
            .expect("worker pool slot available");

        let _ = coordinator.schedule_execution(&fresh, &worker_id).await;

        // The zombie was reconciled to a terminal status (orphaned) with a
        // lost_workspace_reconcile trace event naming its prior status.
        let zombie_after = db.get_execution(&zombie.id).unwrap();
        assert_eq!(
            zombie_after.status,
            ExecutionStatus::Orphaned,
            "the lost-workspace zombie must be finalized; got {:?}",
            zombie_after.status,
        );
        let zombie_events = recording.events_for(&zombie.id).await;
        let reconcile = zombie_events
            .iter()
            .find(|e| e.stage == "lost_workspace_reconcile")
            .unwrap_or_else(|| panic!("expected lost_workspace_reconcile event; got {zombie_events:#?}"));
        assert_eq!(
            reconcile.details.get("prior_status").and_then(|v| v.as_str()),
            Some("waiting_human"),
            "the trace must record the prior status; got {:?}",
            reconcile.details,
        );

        // The fresh execution was NOT rejected as redundant.
        let fresh_after = db.get_execution(&fresh.id).unwrap();
        assert_ne!(
            fresh_after.status,
            ExecutionStatus::Abandoned,
            "the new spawn must not be abandoned as redundant once the zombie is cleared",
        );
        let fresh_events = recording.events_for(&fresh.id).await;
        assert!(
            !fresh_events.iter().any(|e| e.stage == "host_selected"
                && e.details.get("reason").and_then(|v| v.as_str()) == Some("redundant_spawn")),
            "the new spawn must not emit a redundant_spawn host_selected:error; got {fresh_events:#?}",
        );
    }

    /// Chain-serialized backstop guard emits `host_selected:error` with
    /// `reason=chain_serialized_backstop` when a live execution exists on a
    /// different work item in the same revision chain. This guard is the backstop
    /// for `force_dispatch` (the auto-dispatcher pre-filters at the worker-claim
    /// stage). The deferred execution must stay `ready` so it can be re-dispatched
    /// once the live sibling reaps; it must NOT be abandoned. The stall watchdog
    /// must not fire because the terminal event closes the stage immediately.
    #[tokio::test]
    async fn chain_serialized_backstop_emits_terminal_host_selected_error() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/1849";
        let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            // Revision hanging off the chain root (same PR, different work item).
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_backstop', product_id, 'revision', 'Resolve conflicts backstop', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        // Live implementation resume on the chain root (blocks the revision).
        let _root_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root_id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();

        // Ready conflict-resolution revision execution targeting the same PR.
        let revision_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_backstop")
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        // Use force_dispatch to hit the schedule_execution backstop (the auto-dispatcher
        // pre-filters this case before claiming a worker, so force_dispatch is the path
        // that actually reaches this guard in production).
        let result = coordinator.force_dispatch(&revision_exec.id).await;
        assert!(
            result.is_err(),
            "chain-serialized revision must be deferred: {result:?}",
        );

        // Dispatch timeline: must carry a terminal host_selected:error with the right reason.
        let events = recording.events_for(&revision_exec.id).await;
        let host_selected = events
            .iter()
            .find(|e| e.stage == "host_selected")
            .unwrap_or_else(|| panic!("expected host_selected event; got {events:#?}"));
        assert_eq!(
            host_selected.outcome, "error",
            "chain_serialized_backstop must surface as host_selected:error; got {host_selected:?}",
        );
        assert_eq!(
            host_selected.details.get("reason").and_then(|v| v.as_str()),
            Some("chain_serialized_backstop"),
            "host_selected:error must name chain_serialized_backstop reason; got {:?}",
            host_selected.details,
        );
        assert!(
            crate::dispatch_reader::is_terminal_event(host_selected),
            "host_selected:error must be terminal so the stall watchdog never fires",
        );

        // No stage_stalled — the terminal event closes the stage immediately.
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
        assert!(
            !stages.contains(&"stage_stalled"),
            "chain_serialized_backstop must not produce stage_stalled; got {stages:?}",
        );

        // Post-condition: deferred, NOT abandoned — execution stays ready for re-dispatch.
        let after = db.get_execution(&revision_exec.id).unwrap();
        assert_eq!(
            after.status,
            ExecutionStatus::Ready,
            "chain-serialized execution must stay ready (deferred, not abandoned); got {:?}",
            after.status,
        );

        // No cube workspace was touched.
        assert!(
            cube.lease_calls.lock().await.is_empty(),
            "no cube workspace may be leased while serialized behind a live chain sibling",
        );
    }

    /// T251 incident (2026-07-09, `exec_18af40745c552070_26`): the chain
    /// single-writer guard must not treat a `waiting_human` chain sibling as
    /// live forever when its worker pane is actually dead (workspace
    /// directory gone, no `Stop` hook). The auto-dispatcher's pre-claim
    /// `chain_serialized` check — the path that actually looped every ~10s
    /// in the incident — must reconcile the zombie via the same
    /// `lost_workspace_sweep` logic the double-spawn guard already uses, and
    /// let the ready revision dispatch instead of deferring indefinitely.
    #[tokio::test]
    async fn chain_serialized_pre_claim_reconciles_lost_workspace_zombie_and_dispatches() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/1852";
        let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            // Revision hanging off the chain root (same PR, different work item).
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_t251', product_id, 'revision', 'Resolve merge conflict against main', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        // The chain root's execution: a `waiting_human` zombie whose recorded
        // workspace directory no longer exists — exactly the 56-day-old
        // `exec_18af40745c552070_26` from the incident (a dead pane, no `Stop`
        // hook, no live pane per `bossctl agents status`).
        let root_exec = create_ready_chore_execution(&db, root_id.clone());
        db.start_execution_run(
            &root_exec.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "mono-agent-t251",
            "/nonexistent/old-root/mono-agent-t251",
        )
        .unwrap();
        let root_run = db
            .active_run_ids_for_execution(&root_exec.id)
            .unwrap()
            .into_iter()
            .next()
            .expect("root has a run");
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&root_exec.id)
                .run_id(&root_run)
                .execution_status(ExecutionStatus::WaitingHuman)
                .run_status("completed")
                .build(),
        )
        .unwrap();

        // Ready conflict-resolution revision execution targeting the same PR —
        // this is the row that looped `chain_serialized` every ~10s in the
        // incident.
        let revision_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_t251")
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        // Drive the real auto-dispatch path (not `force_dispatch`), since the
        // incident's silent 10s loop was the pre-claim guard in
        // `drain_ready_queue`, not the `schedule_execution` backstop.
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &revision_exec.id, ExecutionStatus::Running).await;

        // The zombie chain root was reconciled to a terminal status, not left
        // wedging every future dispatch behind it.
        let root_after = db.get_execution(&root_exec.id).unwrap();
        assert_eq!(
            root_after.status,
            ExecutionStatus::Orphaned,
            "the lost-workspace zombie chain sibling must be finalized; got {:?}",
            root_after.status,
        );

        // No lingering `chain_serialized` deferrals once the zombie clears.
        let revision_events = recording.events_for(&revision_exec.id).await;
        assert!(
            !revision_events
                .iter()
                .any(
                    |e| e.details.get("reason").and_then(|v| v.as_str()) == Some("chain_serialized")
                        && e.stage == "worker_claimed"
                ),
            "the revision must not be permanently deferred behind a dead sibling; got {revision_events:#?}",
        );
    }

    /// Part (c) of the T251 fix: once a `ready` execution has sat
    /// chain-serialized behind a genuinely live sibling for longer than
    /// [`CHAIN_SERIALIZED_STALL_THRESHOLD_SECS`], a durable, user-visible
    /// `chain_serialized_stall` attention must be raised on its work item —
    /// the incident sat in this exact state for ~20 minutes with the only
    /// signal being a `engine-trace.jsonl` line repeating every ~10s.
    #[tokio::test]
    async fn chain_serialized_stall_raises_durable_attention_after_threshold() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let pr_url = "https://github.com/spinyfin/mono/pull/1853";
        let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_stall', product_id, 'revision', 'Resolve merge conflict', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        }

        // Genuinely live root execution: no run attached, so the zombie
        // reconcilers have zero evidence either way and correctly leave it
        // alone (this must stay `chain_serialized`, not get reconciled away).
        let _root_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root_id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();

        let revision_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id("task_rev_stall")
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .pr_url(pr_url.to_owned())
                    .build(),
            )
            .unwrap();

        // Backdate `created_at` well past the stall threshold — simulates the
        // T251 incident's ~20 minutes of silent re-defers without a real sleep.
        {
            let conn = db.connect().unwrap();
            let stale_created_at =
                (crate::run_reconcile::current_epoch_s() - CHAIN_SERIALIZED_STALL_THRESHOLD_SECS - 60).to_string();
            conn.execute(
                "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
                rusqlite::params![revision_exec.id, stale_created_at],
            )
            .unwrap();
        }

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        coordinator.kick();

        let mut items = Vec::new();
        for _ in 0..100 {
            items = db.list_attention_items_for_work_item("task_rev_stall").unwrap();
            if !items.is_empty() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(items.len(), 1, "expected exactly one attention item; got {items:#?}");
        assert_eq!(items[0].kind, CHAIN_SERIALIZED_STALL_ATTENTION_KIND);
        assert_eq!(items[0].status, "open");

        // Still `ready` (deferred, not abandoned) — the root is genuinely alive.
        let after = db.get_execution(&revision_exec.id).unwrap();
        assert_eq!(
            after.status,
            ExecutionStatus::Ready,
            "a stall attention must not itself change the execution's status; got {:?}",
            after.status,
        );
    }

    /// Gating-prereqs guard emits `host_selected:error` with
    /// `reason=gating_prereqs_blocked` when the work item has an unmet
    /// prerequisite at dispatch time. The execution must be downgraded to
    /// `waiting_dependency` (not abandoned) so it can be re-promoted when the
    /// gate clears. The stall watchdog must not fire because the terminal event
    /// closes the stage immediately.
    #[tokio::test]
    async fn gating_prereqs_guard_emits_terminal_host_selected_error() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let product = create_test_product(&db);

        // prereq: a chore that has not yet completed.
        let prereq = create_test_chore(&db, product.id.clone(), "Prereq (still active)");

        // dependent: the gated chore.
        let dep = create_test_chore_manual(&db, product.id.clone(), "Gated chore");

        // Wire the blocks edge: dep requires prereq to be done first.
        db.add_dependency(AddDependencyInput {
            dependent: dep.id.clone(),
            prerequisite: prereq.id.clone(),
            relation: None,
        })
        .unwrap();

        // Simulate the timing race: a `ready` execution created before the dep edge committed.
        let gated_exec = create_ready_chore_execution(&db, dep.id.clone());

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&gated_exec.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&gated_exec, &worker_id).await;
        assert!(result.is_err(), "gated execution must be refused: {result:?}");

        // Dispatch timeline: must carry a terminal host_selected:error with the right reason.
        let events = recording.events_for(&gated_exec.id).await;
        let host_selected = events
            .iter()
            .find(|e| e.stage == "host_selected")
            .unwrap_or_else(|| panic!("expected host_selected event; got {events:#?}"));
        assert_eq!(
            host_selected.outcome, "error",
            "gating_prereqs_blocked must surface as host_selected:error; got {host_selected:?}",
        );
        assert_eq!(
            host_selected.details.get("reason").and_then(|v| v.as_str()),
            Some("gating_prereqs_blocked"),
            "host_selected:error must name gating_prereqs_blocked reason; got {:?}",
            host_selected.details,
        );
        assert!(
            crate::dispatch_reader::is_terminal_event(host_selected),
            "host_selected:error must be terminal so the stall watchdog never fires",
        );

        // No stage_stalled — the terminal event closes the stage immediately.
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
        assert!(
            !stages.contains(&"stage_stalled"),
            "gating_prereqs_blocked must not produce stage_stalled; got {stages:?}",
        );

        // Post-condition: downgraded to waiting_dependency (not abandoned) so it can
        // be re-promoted when the prerequisite clears.
        let after = db.get_execution(&gated_exec.id).unwrap();
        assert_eq!(
            after.status,
            ExecutionStatus::WaitingDependency,
            "gated execution must be downgraded to waiting_dependency; got {:?}",
            after.status,
        );

        // No cube workspace was touched.
        assert!(
            cube.lease_calls.lock().await.is_empty(),
            "no cube workspace may be leased for a gated execution",
        );
    }

    /// 2026-07-03 incident (exec_18be836b10baae8_35 / T2154): a merge-conflict
    /// revision can sit `ready` behind worker-pool contention while the
    /// periodic merge-poller sweep (`conflict_watch::on_resolved`) notices the
    /// bound PR is mergeable again and independently retires the linked
    /// `conflict_resolutions` ledger row to `succeeded`. Dispatching a worker
    /// for this now-unnecessary revision would just have it discover "nothing
    /// to do" and churn the produce-a-PR nudge loop. The dispatch-time guard
    /// must catch this before ever leasing a workspace: retire the execution
    /// and advance the revision task to `in_review` without spawning anyone.
    #[tokio::test]
    async fn merge_conflict_already_resolved_guard_short_circuits_dispatch() {
        use crate::work::{ConflictResolutionInsertInput, FakePrStateChecker, PrOpenState};
        use boss_protocol::CreateRevisionInput;

        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/1709";
        let parent = create_test_chore_manual(&db, product.id.clone(), "Parent chore");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
                rusqlite::params![parent.id, parent_pr_url],
            )
            .unwrap();
        }
        let attempt = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: product.id.clone(),
                work_item_id: parent.id.clone(),
                pr_url: parent_pr_url.to_owned(),
                pr_number: 1709,
                head_branch: "my-feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base_sha_1".into()),
                head_sha_before: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            })
            .unwrap()
            .unwrap();
        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(parent.id.clone())
                    .description("Resolve merge conflict against main")
                    .created_via(format!("merge-conflict:{}", attempt.id))
                    .build(),
                &checker,
            )
            .unwrap();
        db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id)
            .unwrap();
        // The periodic merge-poller sweep already found the PR mergeable and
        // retired the ledger row independent of any worker.
        db.mark_conflict_resolution_succeeded(&attempt.id, None).unwrap();

        let exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .pr_url(parent_pr_url)
                    .build(),
            )
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone())
                .with_pre_start_retry_delays(Vec::new()),
        );

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&exec.id, None)
            .await
            .expect("worker pool slot available");

        let result = coordinator.schedule_execution(&exec, &worker_id).await;
        assert!(
            result.is_err(),
            "an already-resolved merge-conflict revision must not be dispatched: {result:?}"
        );

        let events = recording.events_for(&exec.id).await;
        let host_selected = events
            .iter()
            .find(|e| e.stage == "host_selected")
            .unwrap_or_else(|| panic!("expected host_selected event; got {events:#?}"));
        assert_eq!(
            host_selected.outcome, "error",
            "already-resolved guard must surface as host_selected:error; got {host_selected:?}",
        );
        assert_eq!(
            host_selected.details.get("reason").and_then(|v| v.as_str()),
            Some("merge_conflict_already_resolved"),
            "host_selected:error must name merge_conflict_already_resolved; got {:?}",
            host_selected.details,
        );

        // Execution retired without ever leasing a workspace.
        let after_exec = db.get_execution(&exec.id).unwrap();
        assert_eq!(
            after_exec.status,
            ExecutionStatus::Abandoned,
            "execution must be abandoned, not dispatched; got {:?}",
            after_exec.status,
        );
        assert!(
            cube.lease_calls.lock().await.is_empty(),
            "no cube workspace may be leased for an already-resolved revision",
        );

        // Revision task advances to in_review — same terminal state a normal
        // completed revision reaches — instead of stranding in `active`.
        match db.get_work_item(&revision.id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(
                t.status,
                TaskStatus::InReview,
                "revision task must leave `active` when the conflict resolved before dispatch",
            ),
            other => panic!("expected task, got {other:?}"),
        }
    }

    /// Occupancy-guard livelock regression (T1769). When cube keeps handing
    /// back the same occupied workspace, the engine must exclude it on the
    /// next lease call and land on a different free workspace.
    ///
    /// Setup:
    ///   - mono-agent-037 is returned first by cube, and the live-worker
    ///     registry says exec-live occupies it (live pid = current test
    ///     process, so probe_pid sees it as alive).
    ///   - mono-agent-014 is returned on subsequent calls (simulating cube
    ///     respecting --exclude mono-agent-037).
    ///
    /// Expected: the execution lands on mono-agent-014, not loops forever
    /// on mono-agent-037; and the second lease call carries --exclude
    /// mono-agent-037.
    #[tokio::test]
    async fn occupancy_refusal_excludes_workspace_on_retry() {
        use crate::live_worker_state::LiveWorkerStateRegistry;
        use boss_protocol::WorkerEvent;

        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Anti-livelock task");
        db.reconcile_product_executions(&product.id).unwrap();

        // Create a second product and chore to serve as the "occupied" execution.
        // We need a separate DB row for exec-live so get_execution(exec_live.id)
        // returns cube_workspace_id = "mono-agent-037".
        let other_product = create_test_product_named(&db, "OtherProduct");
        let other_chore = create_test_chore(&db, other_product.id.clone(), "Live worker chore");
        db.reconcile_product_executions(&other_product.id).unwrap();
        let exec_live = db.list_executions(Some(&other_chore.id)).unwrap().pop().unwrap();
        // Transition it to running so start_execution_run can set cube_workspace_id.
        db.start_execution_run(
            &exec_live.id,
            "worker-0",
            "mono",
            "lease-live",
            "mono-agent-037",
            "/workspaces/mono-agent-037",
        )
        .unwrap();

        // First lease returns the occupied workspace; subsequent calls
        // return the free one (simulating cube respecting --exclude).
        let cube = Arc::new(FakeCubeClient::default().with_workspace_id_queue(["mono-agent-037", "mono-agent-014"]));

        // Wire the live-worker registry: exec-live is Working and alive
        // (pid = current test process → probe_pid sees it as alive).
        let live_pid = std::process::id() as i32;
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        live_states.register_spawn(0, &exec_live.id, "sonnet", live_pid, None);
        // Advance the slot to Working so the occupancy guard doesn't see Spawning.
        live_states.apply_event(
            0,
            &WorkerEvent::UserPromptSubmit {
                session_id: "s".to_owned(),
                prompt: "do the thing".to_owned(),
            },
        );

        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });

        let mut coordinator_inner =
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        coordinator_inner.set_live_worker_states(live_states);
        // Zero-delay retry so the test doesn't sleep.
        let coordinator = Arc::new(coordinator_inner.with_pre_start_retry_delays(vec![Duration::ZERO]));

        coordinator.kick();

        // Wait for the anti-livelock task execution to reach Running.
        let our_execution_id = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap().id;

        wait_for_execution_status(db.as_ref(), &our_execution_id, ExecutionStatus::Running).await;

        let calls = cube.lease_calls.lock().await;
        // At minimum two cube lease calls: first returning mono-agent-037
        // (refused by occupancy guard), then returning mono-agent-014 (accepted).
        assert!(
            calls.len() >= 2,
            "expected at least 2 lease calls (refused + accepted); got {}",
            calls.len()
        );
        // The second call must exclude mono-agent-037.
        assert!(
            calls[1].4.iter().any(|id| id == "mono-agent-037"),
            "second lease call must pass --exclude mono-agent-037; got {:?}",
            calls[1].4
        );
        drop(calls);

        // The execution must have landed on mono-agent-014, not 037.
        let execution = db.get_execution(&our_execution_id).unwrap();
        assert_eq!(
            execution.cube_workspace_id.as_deref(),
            Some("mono-agent-014"),
            "execution must land on the free workspace, not the occupied one"
        );
    }
}
