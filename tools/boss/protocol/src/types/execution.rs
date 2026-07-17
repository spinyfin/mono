//! Executions: the kind and status vocabulary, the `WorkExecution` row,
//! and the runs an execution is made of.

use super::attention::CreateAttentionItemInput;
use super::common::BranchNaming;
use serde::{Deserialize, Serialize};

/// Discriminator for the `work_executions.kind` column.  Exhaustive
/// match enforces that every callsite handles new variants explicitly —
/// adding a new kind here produces a compile error at every kind-keyed
/// branch that must reason about it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    /// Read-only "mini-coordinator" answer agent (P3a of
    /// `comment-triggered-document-revisions.md`). Spawned to answer a
    /// `question`-classified doc comment in its thread. Distinct from every
    /// implementation/design kind: it runs under a **capability-restricted
    /// dispatch surface** (allowlist, not blocklist) — read-only DB queries,
    /// a read-only workspace checkout, and the post-thread-reply command
    /// only; every mutating tool/RPC is omitted. See the `answer_agent_runs`
    /// tracking table and the engine's `answer_agent` module for the
    /// enforced tool table.
    AnswerAgent,
    AutomationTriage,
    ChoreImplementation,
    CiRemediation,
    ConflictResolution,
    InvestigationImplementation,
    PrReview,
    ProductDesign,
    ProjectDesign,
    RevisionImplementation,
    TaskImplementation,
}

impl ExecutionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AnswerAgent => "answer_agent",
            Self::AutomationTriage => "automation_triage",
            Self::ChoreImplementation => "chore_implementation",
            Self::CiRemediation => "ci_remediation",
            Self::ConflictResolution => "conflict_resolution",
            Self::InvestigationImplementation => "investigation_implementation",
            Self::PrReview => "pr_review",
            Self::ProductDesign => "product_design",
            Self::ProjectDesign => "project_design",
            Self::RevisionImplementation => "revision_implementation",
            Self::TaskImplementation => "task_implementation",
        }
    }
}

impl std::fmt::Display for ExecutionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ExecutionKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "answer_agent" => Ok(Self::AnswerAgent),
            "automation_triage" => Ok(Self::AutomationTriage),
            "chore_implementation" => Ok(Self::ChoreImplementation),
            "ci_remediation" => Ok(Self::CiRemediation),
            "conflict_resolution" => Ok(Self::ConflictResolution),
            "investigation_implementation" => Ok(Self::InvestigationImplementation),
            "pr_review" => Ok(Self::PrReview),
            "product_design" => Ok(Self::ProductDesign),
            "project_design" => Ok(Self::ProjectDesign),
            "revision_implementation" => Ok(Self::RevisionImplementation),
            "task_implementation" => Ok(Self::TaskImplementation),
            other => Err(format!(
                "unknown execution kind: `{other}`; expected one of: \
                 answer_agent, automation_triage, chore_implementation, ci_remediation, \
                 conflict_resolution, investigation_implementation, pr_review, \
                 product_design, project_design, revision_implementation, task_implementation"
            )),
        }
    }
}

/// Discriminator for the `work_executions.status` column. Exhaustive
/// match enforces that every callsite handles new variants explicitly —
/// adding a new status here produces a compile error at every status-keyed
/// branch that must reason about it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    #[default]
    Queued,
    Ready,
    WaitingDependency,
    Running,
    WaitingHuman,
    WaitingReview,
    WaitingMerge,
    Completed,
    Failed,
    Abandoned,
    Cancelled,
    Orphaned,
}

impl ExecutionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Ready => "ready",
            Self::WaitingDependency => "waiting_dependency",
            Self::Running => "running",
            Self::WaitingHuman => "waiting_human",
            Self::WaitingReview => "waiting_review",
            Self::WaitingMerge => "waiting_merge",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
            Self::Cancelled => "cancelled",
            Self::Orphaned => "orphaned",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Abandoned | Self::Cancelled | Self::Orphaned
        )
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Running | Self::WaitingHuman)
    }

    pub fn can_reconcile(&self) -> bool {
        matches!(self, Self::Queued | Self::Ready | Self::WaitingDependency)
    }
}

impl std::fmt::Display for ExecutionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ExecutionStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(Self::Queued),
            "ready" => Ok(Self::Ready),
            "waiting_dependency" => Ok(Self::WaitingDependency),
            "running" => Ok(Self::Running),
            "waiting_human" => Ok(Self::WaitingHuman),
            "waiting_review" => Ok(Self::WaitingReview),
            "waiting_merge" => Ok(Self::WaitingMerge),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "abandoned" => Ok(Self::Abandoned),
            "cancelled" => Ok(Self::Cancelled),
            "orphaned" => Ok(Self::Orphaned),
            other => Err(format!(
                "unknown execution status: `{other}`; expected one of: \
                 queued, ready, waiting_dependency, running, waiting_human, \
                 waiting_review, waiting_merge, completed, failed, abandoned, \
                 cancelled, orphaned"
            )),
        }
    }
}

/// `work_executions.kind` discriminator for an automation triage execution
/// (Maint task 6). Kept as a constant alias; prefer `ExecutionKind::AutomationTriage`
/// in new code.
pub const EXECUTION_KIND_AUTOMATION_TRIAGE: &str = "automation_triage";

/// Execution kind for an independent reviewer agent. Kept as a constant
/// alias; prefer `ExecutionKind::PrReview` in new code.
pub const EXECUTION_KIND_PR_REVIEW: &str = "pr_review";

/// `work_executions.kind` discriminator for a read-only answer-agent run
/// (P3b of `comment-triggered-document-revisions.md`). Kept as a constant
/// alias, mirroring [`EXECUTION_KIND_AUTOMATION_TRIAGE`]; prefer
/// `ExecutionKind::AnswerAgent` in new code.
pub const EXECUTION_KIND_ANSWER_AGENT: &str = "answer_agent";

/// `task_blocked_signals.reason` literal stamped when a CI-remediation worker
/// classifies a failure as flaky/infra and re-triggers the failing job rather
/// than pushing a code change (`boss engine ci mark-retriggered`). Unlike the
/// `ci_failure` reasons this signal does NOT move the parent to
/// `status='blocked'`: the verdict is "the agent attributed the failure to
/// infra, re-ran CI, and there is nothing to push." It surfaces a flake tag on
/// the task card and tells the completion path to park the worker (awaiting the
/// CI retry / a human decision) instead of probing it for a diff that will
/// never exist. Cleared when the PR's CI resolves or a fresh remediation
/// attempt supersedes the verdict.
pub const BLOCKED_REASON_CI_FLAKY_RETRIGGERED: &str = "ci_flaky_retriggered";

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateExecutionInput {
    pub work_item_id: String,
    /// When `true`, cube will be invoked with `--allow-dirty` so it
    /// reclaims the preferred workspace with uncommitted work intact.
    /// Only set on the orphan recovery re-dispatch path.
    #[serde(default)]
    #[builder(default)]
    pub allow_dirty: bool,

    pub kind: ExecutionKind,
    /// When true, the cube lease fallback degrades silently to any free
    /// workspace if the preferred workspace is gone or leased. Used for
    /// `revision_implementation` executions where warmth is a hint only.
    #[serde(default)]
    #[builder(default)]
    pub prefer_is_soft: bool,

    pub cube_lease_id: Option<String>,
    pub cube_repo_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    pub finished_at: Option<String>,
    /// PR URL to bind to this execution row at creation time. For
    /// `revision_implementation` this is the chain root's `pr_url` so
    /// the SHA-delta gate can snapshot and verify the parent PR HEAD.
    #[serde(default)]
    pub pr_url: Option<String>,

    pub preferred_workspace_id: Option<String>,
    pub priority: Option<i64>,
    pub repo_remote_url: Option<String>,
    pub started_at: Option<String>,
    pub status: Option<ExecutionStatus>,
    pub workspace_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateRunInput {
    pub agent_id: String,
    pub execution_id: String,
    pub artifacts_path: Option<String>,
    pub error_text: Option<String>,
    pub finished_at: Option<String>,
    pub result_summary: Option<String>,
    pub started_at: Option<String>,
    pub status: Option<String>,
    pub transcript_path: Option<String>,
}

/// Everything needed to finish the active run of an execution in one atomic
/// DB transaction (see `WorkDb::finish_execution_run`): the execution's new
/// status, the run's terminal status and result/error text, whether to clear
/// the workspace lease columns, and an optional execution-scoped attention
/// item to file. Bundled into a builder-constructed input so the DB method
/// stays under clippy's argument-count threshold, mirroring `CreateRunInput`.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct FinishExecutionRunInput {
    pub execution_id: String,
    pub run_id: String,
    pub execution_status: ExecutionStatus,
    pub run_status: String,
    pub result_summary: Option<String>,
    pub error_text: Option<String>,
    /// Null the `cube_lease_id` / `cube_workspace_id` / `workspace_path`
    /// columns as part of finishing. Defaults to `false` (leave the lease
    /// intact) — only terminal/failure paths that release the workspace set
    /// this.
    #[builder(default)]
    pub clear_workspace_lease: bool,
    pub attention: Option<CreateAttentionItemInput>,
}

/// One row in the unified `boss engine attempts list` v2 result —
/// design Phase 11 #36. A small projection across three attempt
/// subsystems (`conflict_resolutions`, `rebase_attempts`,
/// `ci_remediations`) with a `kind` discriminator. The full per-row
/// state still lives on its origin table; this view is the columns the
/// shared list view needs (id, kind, status, work item, PR, reason,
/// timestamps) — callers fetching deeper detail switch to the
/// kind-specific `show` verb.
///
/// `kind` is one of:
/// - `"conflict"`  — `conflict_resolutions` row (merge-conflict flow)
/// - `"rebase"`    — `rebase_attempts` row (auto-rebase flow)
/// - `"ci"`        — `ci_remediations` row (CI-failure flow)
///
/// `work_item_id` is the parent's id where the kind has one;
/// `rebase_attempts` is keyed on `dependent_pr_url`, so its
/// `work_item_id` may be `None` (depending on schema as it lands).
///
/// `extra` carries kind-specific scalar values that are useful in the
/// shared list view but don't justify a column — currently
/// `attempt_kind` for `ci` rows. The contract is "stringly typed
/// extras"; consumers index by key and tolerate absence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct EngineAttemptListEntry {
    pub id: String,
    pub product_id: String,
    pub created_at: String,
    /// Kind-specific scalar columns the consumer may want to render
    /// (e.g. `attempt_kind` for `ci`). Stringly-typed; consumers
    /// fall back to the kind-specific `show` verb for deep detail.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extra: std::collections::BTreeMap<String, String>,

    pub kind: String,
    pub pr_url: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionReconcileResult {
    pub created: Vec<WorkExecution>,
    pub updated: Vec<WorkExecution>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct RequestExecutionInput {
    pub work_item_id: String,
    /// Request cube to reclaim the preferred workspace with its dirty
    /// working copy intact (passed through to `CreateExecutionInput`
    /// and stored on the `work_executions` row). Set `true` on the
    /// orphan recovery re-dispatch path; `false` everywhere else.
    #[serde(default)]
    #[builder(default)]
    pub allow_dirty: bool,

    /// Skip the dispatcher's pool-cap deferral. With `force = false`
    /// (the default), `RequestExecution` is the soft "queue this and
    /// dispatch when a slot frees up" verb. With `force = true`
    /// (`bossctl agents launch`), the engine grows the worker pool by
    /// one slot — bounded by the hard cap `MAX_WORKER_POOL_SIZE` — so
    /// the work item starts immediately even when every configured
    /// slot is busy.
    #[serde(default)]
    #[builder(default)]
    pub force: bool,

    pub preferred_workspace_id: Option<String>,
    pub priority: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkExecution {
    pub id: String,
    pub work_item_id: String,
    /// When `true`, the cube lease call for this execution will include
    /// `--allow-dirty`, causing cube to reclaim the named preferred workspace
    /// with its uncommitted working copy intact rather than resetting it.
    /// Set on the orphan recovery re-dispatch path (when the predecessor
    /// execution was `orphaned`) and unconditionally by the
    /// transient-recovery auto-resume path (`WorkDb::request_resume_execution`
    /// in `boss-engine`) so the recovering worker lands on the exact dirty
    /// workspace its predecessor left behind instead of cube silently
    /// wiping it. Normal first-time dispatches leave this `false`.
    #[serde(default)]
    #[builder(default)]
    pub allow_dirty: bool,

    /// Branch-naming strategy snapshotted from the owning product's
    /// `editorial_rules.branch_naming` at execution spawn time. Frozen
    /// here so that the engine can reconstruct the expected branch name
    /// from `state.db` alone, even after the product's rule changes.
    /// `NULL` in the DB (pre-migration rows) deserialises as the default
    /// [`BranchNaming::BossExecPrefix`], preserving historical behaviour.
    #[serde(default)]
    #[builder(default)]
    pub branch_naming: BranchNaming,

    pub created_at: String,
    pub kind: ExecutionKind,
    /// Number of pre-start failures (cube_repo_ensure, workspace_lease,
    /// change_create, run_start) accumulated on this execution row. The
    /// engine retries up to N times before marking the execution `failed`
    /// permanently. Reset to 0 on a fresh `ready` execution.
    #[serde(default)]
    #[builder(default)]
    pub pre_start_failure_count: i64,

    /// When `true`, the cube workspace preference (`preferred_workspace_id`)
    /// is treated as a warmth hint only: if the preferred workspace is
    /// unavailable or busy, the coordinator falls back silently to any free
    /// workspace rather than failing terminally. Set `true` for
    /// `revision_implementation` executions (warmth ≠ correctness; the
    /// branch is always recoverable via `jj git fetch`). Pre-revision rows
    /// default to `false`, preserving the existing hard-prefer semantics
    /// used by orphan-resume.
    #[serde(default)]
    #[builder(default)]
    pub prefer_is_soft: bool,

    #[serde(default)]
    #[builder(default)]
    pub priority: i64,

    pub repo_remote_url: String,
    pub status: ExecutionStatus,
    /// Number of times the engine has auto-resumed this work item's
    /// chain of executions because a worker stalled or died on a
    /// *transient* Claude API error (socket closed, connection reset,
    /// 5xx, `overloaded_error`, `rate_limit`/429, request timeout).
    /// Carried forward onto each fresh resume execution by
    /// [`crate::WorkExecution`]'s recovery path so the engine can cap
    /// retries and back off — distinct from
    /// [`Self::pre_start_failure_count`], which counts failures that
    /// happen *before* a worker ever runs. Reset to 0 on a human-
    /// initiated or first dispatch.
    #[serde(default)]
    #[builder(default)]
    pub transient_failure_count: i64,

    pub cube_lease_id: Option<String>,
    pub cube_repo_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    /// Unix epoch seconds (as a string) before which this `ready`
    /// execution must not be dispatched. `None` means dispatchable
    /// immediately. Set during pre-start retry backoff windows.
    #[serde(default)]
    pub dispatch_not_before: Option<String>,

    /// The dispatcher's current defer reason for this `ready`
    /// execution (`chain_serialized`, `pool_exhausted`, ...) — i.e. why
    /// it hasn't claimed a worker slot yet, distinct from a pool-capacity
    /// wait vs. a serialization/gating wait. `None` when the execution
    /// isn't currently deferred (never attempted yet, or already
    /// claimed a slot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_wait_reason: Option<String>,

    /// Unix epoch seconds (as a string) when `dispatch_wait_reason` was
    /// first set to its current value — the start of the *current*
    /// wait, not the most recent poll. `None` whenever
    /// `dispatch_wait_reason` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_wait_since: Option<String>,

    pub finished_at: Option<String>,
    /// SHA of the bound chore PR's head ref at the moment this
    /// execution started running. Captured once at run start when
    /// `Task.pr_url` is already populated (i.e. this is a resume /
    /// bounce-back of an already-bound chore). Used by the Stop
    /// boundary's SHA-delta gate to decide whether the run actually
    /// contributed to the bound PR before falling through to the
    /// `PROBE_NO_PR` nudge — fixes the runtime-nudge-loop bug where
    /// resume runs that pushed a fix commit got re-nudged forever.
    /// `None` when `Task.pr_url` was empty at run start (new-PR
    /// flow), when the snapshot fetch failed, or on rows that
    /// predate this column.
    #[serde(default)]
    pub pr_head_before: Option<String>,

    /// The PR URL captured at the end of this execution's run, if any.
    /// Set when the worker successfully opens a PR and the engine
    /// records the `completed` transition for this execution.
    #[serde(default)]
    pub pr_url: Option<String>,

    pub preferred_workspace_id: Option<String>,
    pub started_at: Option<String>,
    /// Worker branch-name prefix frozen onto this execution at creation
    /// time, denormalised from the owning product's
    /// `worker_branch_prefix` (same pattern as `repo_remote_url`).
    /// Freezing it here keeps the engine-supplied branch name
    /// reconstructible from `state.db` alone and immune to a product
    /// prefix change between spawn and PR detection. `None` → the
    /// engine default `boss/`. The branch name is
    /// `<worker_branch_prefix>exec_<id>`; only the prefix varies.
    #[serde(default)]
    pub worker_branch_prefix: Option<String>,

    pub workspace_path: Option<String>,
}

impl WorkExecution {
    /// `started_at` parsed as Unix epoch seconds. The column stores the
    /// epoch as a string; this encapsulates the
    /// `as_deref().and_then(parse::<i64>)` dance shared by every sweep that
    /// applies a grace window against the worker's start time. `None` when
    /// `started_at` is unset or unparseable.
    pub fn started_epoch(&self) -> Option<i64> {
        self.started_at.as_deref().and_then(|s| s.parse::<i64>().ok())
    }

    /// `finished_at` parsed as Unix epoch seconds, mirroring
    /// [`Self::started_epoch`]. `None` when `finished_at` is unset or
    /// unparseable.
    pub fn finished_epoch(&self) -> Option<i64> {
        self.finished_at.as_deref().and_then(|s| s.parse::<i64>().ok())
    }

    /// `created_at` parsed as Unix epoch seconds. Unlike `started_at` /
    /// `finished_at`, `created_at` is a non-optional column, so this is
    /// `None` only when the stored value is unparseable.
    pub fn created_epoch(&self) -> Option<i64> {
        self.created_at.parse::<i64>().ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkRun {
    pub id: String,
    pub agent_id: String,
    pub execution_id: String,
    pub created_at: String,
    pub status: String,
    pub artifacts_path: Option<String>,
    pub error_text: Option<String>,
    pub finished_at: Option<String>,
    pub result_summary: Option<String>,
    pub started_at: Option<String>,
    pub transcript_path: Option<String>,
}
