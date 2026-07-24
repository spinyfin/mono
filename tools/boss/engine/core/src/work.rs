use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

/// How long sqlite will internally retry on `SQLITE_BUSY` before
/// surfacing the error to the caller. We funnel concurrent CLI writes
/// against the same `state.db` (multiple `boss chore bind-pr` etc.
/// landing in the engine in parallel) — without this the second writer
/// would fail with "database is locked" even though the first writer
/// finishes in microseconds. Five seconds is overkill for the in-engine
/// case (writes are tiny) but cheap when uncontended.
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// SQL fragment for the set of task kinds that behave like chores: they own
/// their own PR and follow the active → in_review → done lifecycle. Used in
/// every `kind IN (...)` filter that drives the merge-poller and blocking
/// sweeps so a new kind only needs to be added here to be wired in everywhere.
pub(crate) const CHORE_LIKE_KINDS_SQL: &str =
    "'chore', 'project_task', 'design', 'investigation', 'followup', 'design_postmortem'";

/// Sliding window for the merge-conflict churn-guard heuristic
/// (`merge-conflict-handling-in-review.md` Q6 / Phase 6 #16): the
/// 4th `conflict_resolutions` row for a given work item inside one
/// hour is created as `abandoned` instead of `pending`.
pub const CHURN_GUARD_WINDOW_SECS: i64 = 60 * 60;
/// Trailing-window count at which the next attempt is pre-abandoned.
/// The first 3 attempts inside `CHURN_GUARD_WINDOW_SECS` go live; the
/// 4th trips the guard.
pub const CHURN_GUARD_THRESHOLD: i64 = 3;
/// `failure_reason` stamped on the pre-abandoned row.
pub const CHURN_GUARD_REASON: &str = "churn_threshold_exceeded";

/// Sliding window for the orphan-active redispatch churn guard: the
/// 4th orphan-redispatch for a given work item inside one hour is
/// skipped and a warning is logged instead.
pub const ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS: i64 = 60 * 60;
/// Trailing-window terminal-execution count at which the next
/// orphan-redispatch is skipped. The first 3 cycles inside the window
/// go live; the 4th trips the guard.
pub const ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD: i64 = 3;

/// `work_attention_items.kind` filed when [`crate::orphan_sweep`] or
/// [`crate::pr_review_recovery`] trips the churn guard above and parks a
/// work item instead of auto-redispatching it. Before this existed the
/// trip was only a `tracing::warn!` in the engine trace — the work item
/// stayed `active` with no live execution and nothing in `boss task show`,
/// `bossctl agents status`, or the kanban card said why. Resolved
/// automatically the next time [`WorkDb::request_execution_with_live_check`]
/// is called for the work item (either the sweep succeeding once the
/// trailing window drains, or an operator running `bossctl work start`,
/// which bypasses the guard entirely since it only lives in the sweeps).
pub const CHURN_GUARD_PARKED_ATTENTION_KIND: &str = "churn_guard_parked";

/// `work_attention_items.kind` raised by [`crate::dispatch_stall_escalation`]
/// when a dispatch timeline sits stuck in one stage past
/// [`crate::dispatch_stall_escalation::PERSISTENT_STALL_THRESHOLD`]. The
/// underlying `stage_stalled` dispatch event (`dispatch_events.rs`) is
/// deliberately write-only telemetry with no alert behind it; this kind
/// escalates a *persistent* stall (minutes, not the ~30-120s per-stage
/// thresholds that drive `stage_stalled` itself) onto the operator-visible
/// attention surface. Resolved the next time the execution successfully
/// claims a worker slot (`Coordinator::dispatch_claimed_execution`) —
/// mirrors how [`CHURN_GUARD_PARKED_ATTENTION_KIND`] resolves on the work
/// item's next successful dispatch attempt rather than being proactively
/// cleared by the sweep that raised it.
pub const DISPATCH_STAGE_STALLED_ATTENTION_KIND: &str = "dispatch_stage_stalled";

/// Outcome of [`WorkDb::classify_dispatch_stall`] — whether a persistently
/// stalled execution the escalation sweep detected should actually be
/// escalated onto the work-item attention surface, and if not, why.
///
/// The sweep's detector ([`crate::dispatch_reader::persistently_stalled`])
/// is a pure scan of the per-execution `dispatch.jsonl` mirrors with no
/// view of the DB — deliberately, so it still works when the engine RPC is
/// wedged. That blind spot is exactly why it over-reports: a mirror can
/// outlive its execution row, describe an already-terminal execution, or
/// belong to an execution whose `work_item_id` is not a product/project/task
/// at all. An `automation_triage` execution carries the `automations.id`
/// (an `auto_…` id) as its `work_item_id`
/// (`WorkDb::create_automation_triage_execution`), and
/// [`crate::work::WorkDb::file_dispatch_stage_stalled_attention`] cannot
/// target that — `product_id_for_work_item` rejects the id with
/// `unknown work item id format: auto_…`. Only [`Self::Escalate`] is safe to
/// file; the other variants are the sweep's cue to skip (and count) the
/// stall rather than hammer the trace with a per-item failure every 60s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StallEscalation {
    /// The execution is live (non-terminal) and bound to a real work item.
    /// Carries the row's authoritative `work_item_id` to file/refresh the
    /// attention item against.
    Escalate { work_item_id: String },
    /// The execution's `work_item_id` is not a product/project/task — e.g.
    /// an `automation_triage` execution whose id is an `auto_…` automation
    /// id. Such executions have no kanban card to surface a stall on; their
    /// stalls remain visible via the `stage_stalled` dispatch event and
    /// `bossctl dispatch ghost-active --include-stalled`.
    NotWorkItem,
    /// The execution row is already terminal
    /// (`completed`/`failed`/`abandoned`/`cancelled`/`orphaned`) — a dead
    /// timeline whose mirror simply stopped, not a live stall.
    Terminal,
    /// No execution row exists for the mirror's id — retention-swept, or an
    /// on-disk mirror that outlived its DB row. Nothing live to escalate.
    Missing,
}

/// Attention-item `kind` raised when the engine stops auto-resuming a
/// worker because its Claude API error is non-retryable (permanent or
/// unrecognised). See [`crate::transient_recovery`].
pub const ATTENTION_KIND_RECOVERY_PERMANENT: &str = "worker_recovery_permanent_error";
/// Attention-item `kind` raised when the engine stops auto-resuming a
/// worker because the transient-error retry cap was reached.
pub const ATTENTION_KIND_RECOVERY_EXHAUSTED: &str = "worker_recovery_exhausted";

/// Cooldown after a pre-spawn dispatch failure exhausts
/// `PRE_START_RETRY_DELAYS` and `bounce_dispatch_failed_to_backlog` parks
/// the work item in Backlog (`autostart = 0`, `dispatch_failed_reason`
/// set) before [`crate::dispatch_failure_recovery_sweep`] gives it
/// another shot. Long relative to the ~65s in-process retry window —
/// the point of the bounce was to stop hammering a broken host/repo
/// immediately, so the recovery sweep's cadence is "try again later, in
/// case the world changed," not a tighter retry.
pub const DISPATCH_FAILURE_RECOVERY_MIN_AGE_SECS: i64 = 10 * 60;
/// Sliding window for the dispatch-failure-recovery churn guard: once a
/// work item has produced this many terminal executions inside the
/// window, the recovery sweep stops re-enqueueing it and leaves it
/// parked for a human. The attention item `record_pre_start_failure`
/// raised at the original bounce is the escalation; the sweep does not
/// raise a second one on every skip.
pub const DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_WINDOW_SECS: i64 = 24 * 60 * 60;
/// Trailing-window terminal-execution count at which the recovery sweep
/// stops auto-re-enqueueing a work item.
pub const DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_THRESHOLD: i64 = 5;

/// Sliding window for the duplicate-create guard: a non-deleted task/chore
/// in the same product with the same name created within this many seconds
/// of the attempted insert causes a `DuplicateTaskError` unless
/// `force_duplicate` is set on the input.
pub const DUPLICATE_GUARD_WINDOW_SECS: i64 = 60;

/// Phase 12 #40 — sliding window for the CI-retry churn guard. The
/// engine counts `ci_remediations` created in the last
/// `CI_CHURN_WINDOW_SECS` for the work item; once the count crosses
/// [`CI_CHURN_LIMIT`], the next manual `boss engine ci retry`
/// invocation is rate-limited and requires `--force` to proceed.
pub const CI_CHURN_WINDOW_SECS: i64 = 60 * 60;
/// Threshold for [`CI_CHURN_WINDOW_SECS`]. Set well above the default
/// 3-attempt budget so the natural failure → fix → green cycle never
/// triggers; the only realistic path to 5 within an hour is repeated
/// manual retries against a flaky / fundamentally-broken PR.
pub const CI_CHURN_LIMIT: i64 = 5;

pub use boss_protocol::{
    ANSWER_AGENT_RUN_STATUS_FAILED, ANSWER_AGENT_RUN_STATUS_REPLIED, ANSWER_AGENT_RUN_STATUS_RUNNING,
    ANSWER_AGENT_RUN_STATUS_SUPERSEDED, AddDependencyInput, AnswerAgentRun, Attention, AttentionGroup, AttentionMerge,
    Automation, AutomationPatch, AutomationRun, AutomationTrigger, BOOTHBY_REVERSIBILITY_IRREVERSIBLE,
    BOOTHBY_TARGET_ATTENTION, BOOTHBY_TARGET_ATTENTION_ITEM, BOOTHBY_TARGET_PROJECT, BOOTHBY_TARGET_TASK,
    BlockedSignal, BoothbyAction, BoothbyCursor, BoothbyFinding, BoothbyPass, BranchNaming, COMMENT_STATUS_ACTIVE,
    COMMENT_STATUS_ANSWERED, COMMENT_STATUS_ANSWERING, COMMENT_STATUS_AWAITING_FOLLOWUP, COMMENT_STATUS_DISMISSED,
    COMMENT_STATUS_IN_REVISION, COMMENT_STATUS_ORPHANED, COMMENT_STATUS_RESOLVED, CREATED_VIA_ATTENTION,
    CREATED_VIA_BOOTHBY_PREFIX, CREATED_VIA_CI_FIX_PREFIX, CREATED_VIA_DOC_COMMENT_PREFIX, CREATED_VIA_ENGINE_AUTO,
    CREATED_VIA_MERGE_CONFLICT_PREFIX, CREATED_VIA_PR_REVIEW_PREFIX, CREATED_VIA_UNKNOWN, CiBudgetSnapshot,
    CiRemediation, CommentAnchor, CommentResolution, CommentThreadEntry, CommentWithThread, CommentsBannerState,
    ConflictClassCount, ConflictFileFrequency, ConflictFilePairFrequency, ConflictHotspotReport, ConflictResolution,
    CreateAttentionInput, CreateAttentionItemInput, CreateAutomationInput, CreateChoreInput, CreateCommentInput,
    CreateExecutionInput, CreateManyChoresInput, CreateManyTasksInput, CreateProductInput, CreateProjectInput,
    CreateRevisionInput, CreateRunInput, CreateTaskInput, DeferredScopeAttention, DependencyDirection, DependencyEdge,
    DependencyFilter, DocOwner, DocOwnerPrLifecycle, EditorialAction, EditorialRules, EffortLevel,
    EngineAttemptListEntry, ExecutionKind, ExecutionReconcileResult, ExecutionStatus, FinishExecutionRunInput,
    INTENT_DIRECTIVE, INTENT_LARGER_CHANGE, INTENT_QUESTION, LAST_STATUS_ACTOR_BOOTHBY, LAST_STATUS_ACTOR_HUMAN,
    ListDependenciesInput, PrWorkItemMatch, Product, Project, ProjectDesignDocState, ProjectStatus,
    RESOLVED_WITH_EXACT, RESOLVED_WITH_FUZZY, RESOLVED_WITH_ORPHAN, RemoveDependencyInput, RequestExecutionInput,
    ResolveProjectDesignDocOutput, ResolvedComment, ResolvedDesignDoc, ResolvedDesignDocKind, ReviseDocInput,
    ReviseDocOutcome, SetProjectDesignDocInput, StatusActor, THREAD_ENTRY_AUTHOR_ENGINE, THREAD_ENTRY_KIND_ANSWER,
    THREAD_ENTRY_KIND_NUDGE, THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP, Task, TaskKind, TaskRuntime, TaskStatus,
    WorkAttentionItem, WorkComment, WorkExecution, WorkItem, WorkItemDependency, WorkItemDependencyDetail,
    WorkItemDependencyView, WorkItemExternalRef, WorkItemPatch, WorkRun, WorkTree, is_known_created_via,
};

/// Outcome of `WorkDb::record_pre_start_failure`. The coordinator uses
/// this to decide whether to schedule a delayed kick (retry) or surface
/// a permanent failure to the operator.
#[derive(Debug, Clone, PartialEq)]
pub enum PreStartFailureOutcome {
    /// The execution has been reset to `ready` with a `dispatch_not_before`
    /// delay. The coordinator should kick the scheduler after `delay`.
    Retry { delay: Duration },
    /// All retry attempts exhausted. The execution is now `failed`.
    /// The coordinator should surface an attention item.
    PermanentFail,
}

/// Returned by `insert_task_in_tx` / `insert_chore_in_tx` when the
/// duplicate guard fires. Carried as an `anyhow::Error` so `app.rs` can
/// downcast and send a structured `WorkItemDuplicateBlocked` event.
#[derive(Debug)]
pub struct DuplicateTaskError {
    pub existing_id: String,
    pub existing_short_id: i64,
    pub name: String,
    pub age_secs: i64,
}

impl std::fmt::Display for DuplicateTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "A task/chore named {:?} was created {} seconds ago (id: {}, short_id: T{}); \
             pass --force-duplicate to create another",
            self.name, self.age_secs, self.existing_id, self.existing_short_id,
        )
    }
}

impl std::error::Error for DuplicateTaskError {}

/// Returned by [`WorkDb::create_automation_task`] when the dedup gate
/// finds an open sibling of the same automation already tracking this
/// finding.
///
/// This is not the same condition as [`DuplicateTaskError`], which is the
/// 60-second same-name guard on human/agent task creation. That guard is
/// useless against automations: a cron re-fire lands half a day later and
/// re-words the title, so neither the window nor the exact-name match
/// applies. See `boss_engine_automation_dedup` for what is compared.
///
/// Surfaces to the triage agent as the error text of its
/// `boss task create --automation` call, which is the only channel that
/// reaches it — hence the [`Display`](std::fmt::Display) impl spelling out
/// the marker it should emit instead. A triage agent that got this far
/// believes it found real work, so telling it merely "rejected" would
/// leave it retrying or stopping with no marker at all.
#[derive(Debug)]
pub struct AutomationDuplicateTaskError {
    /// Canonical id of the open sibling that already tracks this finding.
    pub existing_id: String,
    /// Friendly `T<n>` number of that sibling, for the agent's marker.
    pub existing_short_id: i64,
    /// That sibling's title, so the agent can sanity-check the match.
    pub existing_name: String,
    /// Which dedup signal fired (`file_target` / `module_target` /
    /// `normalized_title`).
    pub matched_on: &'static str,
    /// The shared value that fired it — a path, a module name, or the
    /// normalized title.
    pub match_key: String,
}

impl std::fmt::Display for AutomationDuplicateTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "duplicate suppressed: this automation already has an open task \
             covering {:?} — T{} {:?} (matched on {}). Do NOT file another. \
             End your run with the marker: `automation: skip — already tracked as T{}`",
            self.match_key, self.existing_short_id, self.existing_name, self.matched_on, self.existing_short_id,
        )
    }
}

impl std::error::Error for AutomationDuplicateTaskError {}

/// One row demoted by [`WorkDb::heal_ghost_active_chores`]. Carries the
/// owning `product_id` so the caller can publish a `work_item_changed`
/// invalidation on the product's topic — the kanban view subscribes on
/// product topics, not task ids, so the topic must be the product to
/// reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealedGhostActive {
    pub work_item_id: String,
    pub product_id: String,
}

/// One execution row abandoned by
/// [`WorkDb::abandon_stranded_executions_on_closed_work_items`] — a
/// `queued`/`ready`/`waiting_dependency` execution that pointed at a
/// work item already in a terminal status or soft-deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedStrandedExecution {
    pub execution_id: String,
    pub work_item_id: String,
}

/// One task recovered by
/// [`WorkDb::reconcile_orphaned_conflict_ladder_attempts`] at engine
/// startup — a `blocked: merge_conflict` parent whose `blocked_attempt_id`
/// pointed at a non-terminal `conflict_resolutions` attempt that no live
/// driver owns (an inline mechanical-rung attempt killed mid-flight by a
/// restart, or a missing row). The reconciler abandoned the orphaned
/// attempt, freed its idempotency slot, and flipped the parent back to
/// `in_review` so the merge poller re-detects the still-open conflict and
/// re-enters the ladder cleanly. Carries the owning `product_id` so the
/// caller can publish a `work_item_changed` invalidation on the product
/// topic, and the `rung`/`attempt_id` the death happened on for the
/// observability trace line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredConflictLadderAttempt {
    pub work_item_id: String,
    pub product_id: String,
    pub pr_url: String,
    /// The orphaned attempt id, or `None` when `blocked_attempt_id`
    /// pointed at a row that no longer exists at all.
    pub attempt_id: Option<String>,
    /// The mechanical rung (0/1) the attempt was mid-flight on when it was
    /// orphaned, when known (`mechanical_rung_in_flight`); `None` for a
    /// pre-marker orphan or a missing row.
    pub rung: Option<i64>,
}

/// One work item returned by
/// [`WorkDb::list_dead_pr_review_candidates`] — a non-terminal work item
/// whose latest execution is a `pr_review` that reached a terminal state
/// (`orphaned`/`abandoned`/`failed`/`cancelled`) WITHOUT ever reaching
/// `finalize_pr_review_pass` (the only path that produces `completed`).
/// The review died silently — host failure, cube-lease reap, crash — and
/// the PR it was reviewing may now merge with no automated review and no
/// visible signal that one is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadPrReviewCandidate {
    pub work_item_id: String,
    pub execution_id: String,
    pub execution_status: ExecutionStatus,
}

/// A task returned by
/// [`WorkDb::list_in_review_tasks_for_doc_branch_backfill`] — an
/// `in_review` task whose doc-branch pointer is `NULL` and that has a
/// `pr_url` the design detector can scan.
///
/// - `project_id = Some(id)` → project-design path; call
///   [`crate::design_detector::on_design_pr_detected`].
/// - `project_id = None` → per-task-doc path (investigation or
///   project-less design); call
///   [`crate::design_detector::on_task_doc_pr_detected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocBranchBackfillCandidate {
    pub task_id: String,
    pub product_id: String,
    pub project_id: Option<String>,
    pub pr_url: String,
}

use crate::work_dependencies::{self as deps, EdgeInsertOutcome, RELATION_BLOCKS};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MEM_DB_ID: AtomicU64 = AtomicU64::new(1);

/// Identifies a named shared-cache in-memory SQLite database. Kept for
/// `is_in_memory()` and error messages; the database itself is kept alive
/// by `WorkDb::conn` (below) holding the one live connection to it, not by
/// this struct.
#[derive(Clone)]
struct InMemoryAnchor {
    uri: String,
}

/// A connection handle borrowed from [`WorkDb::connect`]'s pool. Derefs
/// transparently to [`Connection`], so existing call sites (`conn.execute`,
/// `conn.prepare`, `conn.transaction()` on a `mut` binding, …) are unaffected
/// by the switch from "fresh connection per call" to "one shared, cached
/// connection guarded by a mutex."
pub(crate) type PooledConnection<'a> = std::sync::MutexGuard<'a, Connection>;

pub struct WorkDb {
    path: PathBuf,
    /// Present only when the database is in-memory (path == ":memory:").
    memory: Option<InMemoryAnchor>,
    /// The single connection this `WorkDb` (and every clone of it) uses for
    /// every operation. Opening a fresh `rusqlite::Connection` used to run
    /// 3 PRAGMAs and re-parse the entire schema on every `connect()` call —
    /// 337+ call sites deep, this was most of the SQLite mutex/malloc
    /// contention seen under parallel test load and adds needless overhead
    /// to production cold paths too. `Mutex` serializes access the same way
    /// SQLite's own connection locking already effectively did; `Arc` lets
    /// `WorkDb::clone` share the one connection across copies (`WorkDb` is
    /// cloned freely across the engine).
    conn: Arc<Mutex<Connection>>,
    /// The Boothby action the executor is currently performing, if any.
    /// Set by [`WorkDb::arm_boothby_action`] and read by the mutation layer
    /// when a write arrives with `actor = "boothby"`, which is how the
    /// executor's `verb` / `rationale` / `reversibility` reach the journal
    /// without threading them through every mutation signature.
    ///
    /// `Arc` so it is shared across clones rather than copied: `WorkDb` is
    /// cloned freely across the engine, and an arm on one handle must be
    /// visible to the mutation that runs through another.
    boothby_action: Arc<Mutex<Option<boothby::BoothbyActionContext>>>,
}

impl Clone for WorkDb {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            memory: self.memory.clone(),
            conn: Arc::clone(&self.conn),
            boothby_action: Arc::clone(&self.boothby_action),
        }
    }
}

// ---- module tree (see PR description for the split rationale) ----
mod answer_agent_runs;
mod attentions;
mod audit_misc;
mod automations;
mod blocking;
mod boothby;
mod chain_helpers;
mod comment_thread_entries;
mod comments;
mod conflict_res;
mod create_entities;
mod dep_helpers;
mod design_postmortem;
mod dispatch;
mod dispatch_class;
mod dispatch_helpers;
mod editorial;
mod exec_status_helpers;
mod exec_tail;
mod execution_retention;
mod executions_runs;
mod host_reconcile_queries;
mod insert_helpers;
mod list_filter;
mod mappers;
mod metrics_db;
mod migrations_a;
mod migrations_b;
mod migrations_boothby;
mod output_types;
mod planner_runs;
mod pr_flow;
mod pr_state;
mod products_design;
mod proposal_apply;
mod proposals;
mod query_ensure;
mod revise_doc;
mod revision_helpers;
mod schema_init;
mod task_targets;
#[cfg(test)]
mod tests;
mod trunk_merge_intents;
mod updates;
mod workitems;

// Private on purpose: only the create-path gate that needs to file an
// attention item inside its own transaction (`WorkDb::create_automation_task`)
// reaches for the in-tx variant; every other caller uses the public,
// tx-owning `WorkDb::create_attention`.
use attentions::create_attention_in_tx;
pub(crate) use audit_misc::*;
pub(crate) use chain_helpers::*;
pub(crate) use dep_helpers::*;
pub(crate) use design_postmortem::TriggerTaskSnapshot;
pub(crate) use dispatch_class::DispatchClass;
pub(crate) use dispatch_helpers::*;
pub(crate) use exec_status_helpers::*;
pub(crate) use exec_tail::content_checksum;
pub(crate) use insert_helpers::*;
// Private on purpose: only the list-read submodules under `work` build
// these queries, so it stays visible to `work` and its children only.
use list_filter::ListFilterQuery;
pub(crate) use mappers::*;
pub(crate) use migrations_a::*;
pub(crate) use migrations_b::*;
pub(crate) use migrations_boothby::*;
pub(crate) use products_design::{parse_pr_doc_artifact_id, resolve_task_doc_pointer};
pub(crate) use proposal_apply::*;
pub(crate) use query_ensure::*;
pub(crate) use revision_helpers::*;
pub(crate) use task_targets::*;

pub use attentions::ActionedAttentionGroup;
pub use audit_misc::AUDIT_ACTOR_DESIGN_DETECTOR;
pub use audit_misc::AUDIT_ACTOR_HUMAN;
pub use audit_misc::ProjectPropertyAuditEntry;
pub use audit_misc::canonicalize_repo_remote_url;
pub use audit_misc::canonicalize_worker_branch_prefix;
pub use automations::AutomationFireRecord;
pub use boothby::{BoothbyActionContext, BoothbyActionGuard};
pub use execution_retention::{
    DEFAULT_RETENTION_KEEP_PER_WORK_ITEM, DEFAULT_RETENTION_MAX_AGE_SECS, ExecutionPruneOutcome,
    ExecutionRetentionPolicy,
};
pub use mappers::CiInFlightObservation;
pub use mappers::CiRemediationInsertInput;
pub use mappers::ConflictResolutionInsertInput;
pub use mappers::ProducerConflictInsertInput;
pub use mappers::SpeculativeConflictInsertInput;
// Metrics vocabulary, owned by the metrics framework rather than by
// `work`. Re-exported here so the `WorkDb::metrics_*` signatures read
// the same as every other row type in this module.
pub use boss_metrics::MetricsCounterRow;
pub use boss_metrics::MetricsGaugeRow;
pub use output_types::AutomationDedupSuppression;
pub use output_types::AutomationSiblingTask;
pub use output_types::HostBoundExecution;
pub use output_types::IdleAbandonmentCompletion;
pub use output_types::LatePrCandidate;
pub use output_types::PendingMergeCheck;
pub use output_types::RemoteRunHandle;
pub use output_types::SetRunTranscriptPathOutcome;
pub use output_types::StoredExternalRef;
pub use output_types::StrandedCiRemediationAttempt;
pub use output_types::WorkerPrCompletion;
pub use output_types::WorkerPrCompletionTarget;
pub use planner_runs::ClaimPlannerRunInput;
pub use planner_runs::PlannerRunPatch;
pub use pr_flow::PrPollStateInput;
pub use pr_flow::QueuedMergeQueueMember;
#[cfg(test)]
pub use pr_state::FakePrStateChecker;
pub use pr_state::GhPrStateChecker;
pub use pr_state::PrMergeClass;
pub use pr_state::PrOpenState;
pub use pr_state::PrStateChecker;
pub use pr_state::RevisionGateError;
pub use pr_state::StaticPrStateChecker;
pub use pr_state::classify_pr_merge_state;
pub use proposals::SubmitWorkerProposalInput;
pub use proposals::SubmitWorkerProposalOutcome;
pub use revision_helpers::normalize_priority;
pub use trunk_merge_intents::{ActiveTrunkMergeIntent, TrunkMergeIntent, TrunkMergeIntentInsertInput};
