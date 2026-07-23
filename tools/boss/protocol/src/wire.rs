use serde::{Deserialize, Serialize};

use crate::engine_app::{EngineToAppRequest, EngineToAppResponse, HostedPaneEntry};
use crate::health_wire::EngineHealthReport;
use crate::host_registry_wire::HostSnapshot;
use crate::live_worker_state::LiveWorkerState;
use crate::metrics_wire::MetricLiveEntry;
use crate::types::{
    AddDependencyInput, Attention, AttentionGroup, AttentionMerge, Automation, AutomationDedupSuppression,
    AutomationPatch, AutomationRun, CiBudgetSnapshot, CiRemediation, CommentAnchor, CommentWithThread,
    CommentsBannerState, ConflictHotspotReport, ConflictResolution, CreateAttentionInput, CreateAttentionItemInput,
    CreateAutomationInput, CreateChoreInput, CreateCommentInput, CreateExecutionInput, CreateInvestigationInput,
    CreateManyChoresInput, CreateManyTasksInput, CreateProductInput, CreateProjectInput, CreateRevisionInput,
    CreateRunInput, CreateTaskInput, DeferredScopeAttention, DependencyFilter, EditorialAction, EngineAttemptListEntry,
    GitHubAuthStateDto, LinkExternalRefInput, ListDependenciesInput, PrWorkItemMatch, Product, Project, ProposalKind,
    ProposalState, ProposalSubmissionError, RemoveDependencyInput, RequestExecutionInput,
    ResolveProjectDesignDocOutput, ResolvedComment, ReviseDocInput, ReviseDocOutcome, SetProductEditorialRulesInput,
    SetProductExternalTrackerInput, SetProjectDesignDocInput, Task, TaskRuntime, TranscriptSegment, WorkAttentionItem,
    WorkComment, WorkExecution, WorkItem, WorkItemDependency, WorkItemDependencyDetail, WorkItemDependencyView,
    WorkItemPatch, WorkRun, WorkerContextBundle, WorkerProposal, WorkerTierDenial,
};

/// Outcome of the live `getQueue` smoke check `boss engine trunk status`
/// runs against a `trunk_queue`-mechanism product's queue, once one exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrunkQueueCheckDto {
    pub ok: bool,
    pub detail: String,
}

pub const TOPIC_WORK_PRODUCTS: &str = "work.products";

/// Global topic carrying GitHub OAuth auth-state pushes
/// ([`FrontendEvent::GitHubAuthState`]). Unlike work topics this is not
/// per-product: the engine owns a single per-host (github.com) auth state and
/// fans every transition out on this one topic. The macOS app subscribes to it
/// to render the issue-sync "GitHub account" section as the device flow
/// advances. See the OAuth device-flow design (§3 state machine, §4 RPC).
pub const TOPIC_GITHUB_AUTH: &str = "github.auth";

/// Global topic carrying engine-health pushes
/// ([`FrontendEvent::EngineHealthResult`]). The engine fans every
/// health-state change (dispatch pause/resume, API key changes, etc.)
/// out on this topic so subscribed frontends update without polling.
pub const TOPIC_ENGINE_HEALTH: &str = "engine.health";

pub fn work_product_topic(product_id: &str) -> String {
    format!("work.product.{product_id}")
}

pub fn execution_topic(execution_id: &str) -> String {
    format!("executions.{execution_id}")
}

/// Per-run topic that carries probe lifecycle pushes for `run_id`.
/// Subscribers (e.g. a `bossctl probe` invocation that wants to wait
/// for the worker's reply) join this topic on the run they care about
/// and observe [`FrontendEvent::ProbeReplied`] when the engine pops a
/// queued probe and watches the next Stop boundary land.
pub fn probe_topic(run_id: &str) -> String {
    format!("probes.{run_id}")
}

/// Per-artifact comment topic. Fires whenever any comment row on the
/// artifact changes (create / status flip / re-anchor); subscribers
/// refetch via `comments_list` / `comments_resolve`. Grammar:
/// `comments.artifact.<artifact_kind>:<artifact_id>`.
pub fn comment_topic(artifact_kind: &str, artifact_id: &str) -> String {
    format!("comments.artifact.{artifact_kind}:{artifact_id}")
}

/// Per-product editorial-actions topic. The engine pushes a
/// [`TopicEventPayload::WorkEditorialAction`] on this topic after every
/// PreToolUse hook decision so the UI can badge product cards.
pub fn editorial_actions_topic(product_id: &str) -> String {
    format!("editorial_actions.{product_id}")
}

fn default_pool_size() -> i64 {
    1
}

/// One task preserved (not deleted) by [`FrontendRequest::UnpopulateProject`]
/// because it already had an execution — i.e. it was released and
/// dispatched, so undoing the populate would destroy in-flight work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnpopulatePreservedTask {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendRequestEnvelope {
    pub request_id: String,
    pub payload: FrontendRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendEventEnvelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub payload: FrontendEvent,
}

impl FrontendEventEnvelope {
    pub fn response(request_id: impl Into<String>, payload: FrontendEvent) -> Self {
        Self {
            request_id: Some(request_id.into()),
            revision: None,
            payload,
        }
    }

    pub fn push(payload: FrontendEvent) -> Self {
        Self {
            request_id: None,
            revision: None,
            payload,
        }
    }

    pub fn response_with_revision(request_id: impl Into<String>, revision: u64, payload: FrontendEvent) -> Self {
        Self {
            request_id: Some(request_id.into()),
            revision: Some(revision),
            payload,
        }
    }

    pub fn push_with_revision(revision: u64, payload: FrontendEvent) -> Self {
        Self {
            request_id: None,
            revision: Some(revision),
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendRequest {
    /// Engine-side abandon for a non-terminal `ci_remediations`
    /// attempt. Distinct from `MarkCiRemediationFailed` (the
    /// worker-facing "I gave up" surface) in that the caller is
    /// explicitly stepping away (manual override). Idempotent on rows
    /// already terminal — returns [`FrontendEvent::WorkError`].
    AbandonCiRemediation {
        attempt_id: String,
        reason: String,
    },

    /// Engine-side abandon: flip a non-terminal attempt to `abandoned`
    /// with the supplied reason. Distinct from `mark-failed` in that the
    /// caller is explicitly stepping away (PR closed, parent merged
    /// externally, manual override) rather than declaring the worker
    /// gave up. Idempotent; rows already terminal yield a WorkError.
    AbandonConflictResolution {
        attempt_id: String,
        reason: String,
    },

    /// Accept an open `deferred_scope` item with no followup task.
    AcceptDeferredScopeAttention {
        id: String,
    },

    /// Action an open/partially-answered attention group — produce the
    /// downstream artifact (one revision, one design task, or a batch
    /// of new tasks) and transition the group to `actioned`. Replies
    /// with [`FrontendEvent::AttentionGroupActioned`].
    ActionAttentionGroup {
        /// Group id (`atg_…`) or `A<n>` short id.
        id: String,
        /// When `true`, mark every unanswered member as `skipped`
        /// before actioning so the caller doesn't need to touch every
        /// row explicitly.
        #[serde(default)]
        skip_unanswered: bool,
    },

    /// Declare a `blocks` edge from `dependent` to `prerequisite`.
    /// Idempotent: re-adding an existing edge is a no-op. Cycles are
    /// rejected at the engine before insert.
    AddDependency {
        #[serde(flatten)]
        input: AddDependencyInput,
    },

    /// Register a new remote SSH host. The engine stores the row,
    /// then eagerly pushes the `boss-remote-run` wrapper and runs
    /// capability discovery — identical to `bossctl hosts add`. On
    /// success replies with [`FrontendEvent::HostResult`]. On failure
    /// replies with [`FrontendEvent::Error`] with a human-readable
    /// message. The host row is created before the push; if the push
    /// fails the host is left disabled (same policy as the CLI).
    AddHost {
        id: String,
        ssh_target: String,
        #[serde(default = "default_pool_size")]
        pool_size: i64,
        #[serde(default)]
        tags: Vec<String>,
    },

    /// Add one user-defined capability tag to a registered host.
    /// Replies with [`FrontendEvent::HostUpdated`] on success.
    AddHostTag {
        host_id: String,
        tag: String,
    },

    /// Record the human's answer for one attention member (`atn_…`).
    /// Replies with [`FrontendEvent::AttentionGroupUpdated`] carrying
    /// the group's updated state.
    AnswerAttention {
        /// The individual attention id (`atn_…`).
        id: String,
        /// The captured answer. Shape depends on the attention's
        /// `question_type`: `"yes"`/`"no"` for `yes_no`; the chosen
        /// value for `multiple_choice`; free text for `prompt`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        /// Mark this member `skipped` rather than `answered`.
        #[serde(default)]
        skip: bool,
        /// Mark this member `dismissed` without answering.
        #[serde(default)]
        dismiss: bool,
    },

    /// Heuristic feedback-loop audit (design §Q4 follow-up, PR #370).
    /// Aggregates recorded escalation events for `product_id`
    /// against the §Q4 marker corpus and returns a snapshot report
    /// of under-classification rates per marker. Read-only; backs
    /// the `boss product audit-effort` CLI verb. `window_days`
    /// trims the event set to a rolling window (events older than
    /// `now - window` are excluded); `None` means "all recorded
    /// events." Replies with [`FrontendEvent::EffortAuditReport`].
    AuditProductEffort {
        product_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window_days: Option<u32>,
    },

    /// Cancel a queued or running execution. Marks the execution row
    /// `cancelled`, releases any cube workspace lease it still holds,
    /// and tears down the libghostty pane (if one was allocated).
    /// Idempotent on already-terminal rows (returns `WorkError`).
    CancelExecution {
        execution_id: String,
    },

    /// Worker → engine marker (Phase 9 #30): record the worker's
    /// post-log triage decision on a `ci_remediations` attempt.
    /// Canonical values: `tractable`, `flaky_or_infra`, `unfixable`.
    /// Pure metadata column on the attempt row; no state-machine
    /// effect — the worker still calls `mark-failed` (`unfixable` /
    /// give up), `mark-retriggered` (`flaky_or_infra` → re-ran),
    /// or simply pushes (`tractable` → fix landed) to drive the
    /// terminal status.
    ClassifyCiRemediation {
        attempt_id: String,
        triage_class: String,
    },

    /// Read-only summary of the `[Revise]` banner's state for an
    /// artifact: `{ revisable, unresolved_count, in_revision_count,
    /// doc_kind }`. A small companion read to `CommentsList` that lets a
    /// client render the banner without loading every comment. Design:
    /// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
    /// §"2d. Banner state on the comment read path".
    CommentsBannerState {
        artifact_kind: String,
        artifact_id: String,
    },

    /// Create an `active` comment on an artifact. Returns the row.
    CommentsCreate {
        #[serde(flatten)]
        input: CreateCommentInput,
    },

    /// Soft-dismiss: transition a comment to `resolved`.
    CommentsDismiss {
        comment_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
    },

    /// List comments for an artifact. Excludes `resolved` / `dismissed`
    /// (and `orphaned` unless `include_resolved`) by default.
    CommentsList {
        artifact_kind: String,
        artifact_id: String,
        #[serde(default)]
        include_resolved: bool,
    },

    /// Worker-callable: post the read-only answer agent's reply (P3b of
    /// `comment-triggered-document-revisions.md`). The engine resolves
    /// `run_id` (the caller's own `BOSS_RUN_ID`, i.e. `work_executions.id`) to
    /// its bound comment and the comment's `running` `answer_agent_runs`
    /// row — the caller cannot target any other comment or run, by
    /// construction (see `boss comment reply`'s security note). On success:
    /// completes the run (`replied`), appends an `entry_kind = 'answer'`
    /// `comment_thread_entries` row, and transitions the comment
    /// `answering → answered`. Errors (unknown run, run not
    /// `answer_agent`-kind, no `running` row for its comment, or a duplicate
    /// call after the run already completed) surface as `WorkError` so the
    /// agent sees it failed rather than silently no-op-ing.
    CommentsPostAnswer {
        run_id: String,
        body: String,
    },

    /// Operator-authored reply in a bucket-2 comment's thread (P3c "Follow-up
    /// reclassification loop" of `comment-triggered-document-revisions.md`).
    /// Only valid while the comment is `answered` — replying to any other
    /// status is a `WorkError` (in particular, a comment still `answering`
    /// rejects a second follow-up rather than queuing it; design
    /// §"Concurrency/idempotency" describes queuing as the eventual UX, not
    /// yet implemented). On success: appends an `entry_kind =
    /// 'operator_followup'` `comment_thread_entries` row, transitions the
    /// comment `answered → awaiting_followup`, and — off the request's
    /// critical path, mirroring `CommentsCreate`'s classifier dispatch —
    /// reclassifies the follow-up with the accumulated thread as context.
    /// `question` re-enters bucket 2 (`awaiting_followup → answering`,
    /// answer agent runs again); `directive`/`larger_change` bridges into
    /// the bucket-1&3 path (`awaiting_followup → active`), carrying the
    /// thread's answer-agent reply into the next `[Revise]` batch's
    /// directive.
    CommentsPostFollowup {
        comment_id: String,
        body: String,
        author: String,
    },

    /// Resolve every active comment on an artifact against the renderer's
    /// current plain-text projection. The engine runs the
    /// `TextQuoteSelector` resolver, persists fuzzy re-anchors (setting
    /// `last_resolved_with = 'fuzzy'`) and flips unresolvable comments to
    /// `orphaned`, then returns each comment with its [`CommentResolution`].
    CommentsResolve {
        artifact_kind: String,
        artifact_id: String,
        /// The doc's current rendered plain-text projection.
        plain_text: String,
        #[serde(default)]
        plain_text_projection_version: i64,
    },

    /// Batch-address every unaddressed `directive`/`larger_change` comment
    /// on a design/investigation-owned `pr_doc` artifact: creates a
    /// revision (open PR) or chore (merged/closed/no-PR) — the
    /// `[Revise]`-banner action. App-or-Boss tier. Replies with
    /// [`FrontendEvent::CommentsReviseDocResult`]. Design:
    /// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
    /// §"Buckets 1 & 3".
    CommentsReviseDoc {
        #[serde(flatten)]
        input: ReviseDocInput,
    },

    /// Manually reclassify a comment's intent (sidebar intent badge).
    /// Sets `intent_overridden_by = 'user'` and re-runs routing from the new
    /// intent's entry point — the override doubles as the classification,
    /// so no re-classification LLM call is triggered. Comment-intent-
    /// classification design § "Misclassification / override".
    CommentsSetIntent {
        comment_id: String,
        /// `directive` | `question` | `larger_change`.
        intent: String,
    },

    /// General status transition (`active` / `resolved` / `orphaned`).
    /// Re-activation is accepted; hard delete is not exposed.
    CommentsSetStatus {
        comment_id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
    },

    /// Renderer callback after a fuzzy re-resolve: persists the new anchor
    /// coordinates so subsequent loads exact-match. Records the fuzzy
    /// outcome on the row.
    CommentsUpdateAnchor {
        comment_id: String,
        anchor: CommentAnchor,
        new_doc_version: String,
        #[serde(default)]
        plain_text_projection_version: i64,
    },

    /// Create a new attention (question or followup member). The engine
    /// finds or creates the owning group based on the association and
    /// source fields in the input. Replies with
    /// [`FrontendEvent::AttentionCreated`].
    CreateAttention {
        #[serde(flatten)]
        input: CreateAttentionInput,
    },

    CreateAttentionItem {
        #[serde(flatten)]
        input: CreateAttentionItemInput,
    },

    /// Create a new automation for a product. Replies with
    /// [`FrontendEvent::AutomationCreated`] on success.
    CreateAutomation {
        #[serde(flatten)]
        input: CreateAutomationInput,
    },

    /// Create a single maintenance task produced by an automation's triage
    /// phase (Maint task 6). Called by the triage agent via
    /// `boss task create --automation <A-id>`. The engine resolves the
    /// automation, transactionally re-checks its open-task cap (the
    /// backstop against a misbehaving agent fanning out multiple tasks),
    /// stamps `tasks.source_automation_id`, inherits the automation's repo,
    /// and — because the produced task is `autostart` — requests its
    /// execution (which the dispatcher routes to the automations pool).
    /// Replies with [`FrontendEvent::WorkItemCreated`] on success, or
    /// [`FrontendEvent::WorkError`] when the automation is unknown, the
    /// open-task cap is already reached, or the pre-file dedup gate
    /// (investigation `automation-duplicate-work-2026-07-14.md` §4 Layer 1)
    /// suppresses the create as a likely duplicate of an already-open
    /// automation-sourced task.
    ///
    /// `target_files` / `target_symbols` are the files/symbols the triage
    /// agent declares this task will touch (`--target-file`/
    /// `--target-symbol`, repeatable). Stored in `task_targets` and used by
    /// the dedup gate; empty is allowed (an undeclared candidate is never
    /// gated) but weakens the gate for every automation on this product.
    CreateAutomationTask {
        automation_id: String,
        name: String,
        description: Option<String>,
        #[serde(default)]
        target_files: Vec<String>,
        #[serde(default)]
        target_symbols: Vec<String>,
    },

    CreateChore {
        #[serde(flatten)]
        input: CreateChoreInput,
    },

    CreateExecution {
        #[serde(flatten)]
        input: CreateExecutionInput,
    },

    /// Create a `kind = 'investigation'` task. Parallel to
    /// `CreateChore` but uses `investigation` kind and supports an
    /// optional `project_id`. Workers dispatched against investigation
    /// tasks receive a doc-output prelude and open PRs against the
    /// product's `docs_repo` or `BOSS_USER_DOCS_REPO`.
    CreateInvestigation {
        #[serde(flatten)]
        input: CreateInvestigationInput,
    },

    /// Batch create N chores in one engine round-trip. See
    /// `CreateManyTasks` for atomicity semantics.
    CreateManyChores {
        #[serde(flatten)]
        input: CreateManyChoresInput,
    },

    /// Batch create N tasks in one engine round-trip. Atomic: the
    /// whole batch is wrapped in a single sqlite transaction and
    /// rolled back on the first per-item failure. Replies with
    /// `WorkItemsCreated` carrying the full list of inserted rows.
    CreateManyTasks {
        #[serde(flatten)]
        input: CreateManyTasksInput,
    },

    CreateProduct {
        #[serde(flatten)]
        input: CreateProductInput,
    },

    CreateProject {
        #[serde(flatten)]
        input: CreateProjectInput,
    },

    /// Create a `kind = 'revision'` task bound to an existing parent task
    /// whose PR is open and unmerged. The worker's deliverable is a new
    /// commit on the parent's PR branch — no new PR is opened. Mirrors
    /// `CreateInvestigation` in structure; gate enforcement and dispatch
    /// are implemented in Phase 2 and Phase 3 respectively. Ships dark in
    /// Phase 1: the wire type is parseable but no kind is dispatchable yet.
    CreateRevision {
        #[serde(flatten)]
        input: CreateRevisionInput,
    },

    CreateRun {
        #[serde(flatten)]
        input: CreateRunInput,
    },

    CreateTask {
        #[serde(flatten)]
        input: CreateTaskInput,
    },

    /// File a followup task from an open `deferred_scope` item, prefilled
    /// from its `summary`/`reason` plus source-task/PR provenance.
    CreateTaskFromDeferredScopeAttention {
        attention_id: String,
    },

    /// One-shot diagnostic snapshot of the live-status pipeline.
    /// Returns the engine build SHA, ANTHROPIC_API_KEY presence, and
    /// per-slot detail covering trigger / outcome / transcript path —
    /// see [`crate::LiveStatusDebugReport`]. Wired through to
    /// `bossctl live-status debug`. Read-only; no side effects.
    DebugLiveStatusPipeline,

    /// Permanently delete an automation and its run history.
    /// Replies with [`FrontendEvent::AutomationDeleted`].
    DeleteAutomation {
        id: String,
    },

    DeleteWorkItem {
        id: String,
    },

    /// Set `enabled = false` on an automation. Idempotent.
    /// Replies with [`FrontendEvent::AutomationUpdated`].
    DisableAutomation {
        id: String,
    },

    /// Dismiss an attention group or a single member without producing a
    /// downstream artifact. Accepts both `atg_…` group ids and `atn_…`
    /// member ids; the engine discriminates by prefix. Replies with
    /// [`FrontendEvent::AttentionGroupUpdated`].
    DismissAttention {
        /// `atg_…` to dismiss the whole group; `atn_…` to dismiss one
        /// member.
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Set `enabled = true` on an automation. Idempotent.
    /// Replies with [`FrontendEvent::AutomationUpdated`].
    EnableAutomation {
        id: String,
    },

    /// App's reply to a previous `FrontendEvent::EngineRequest`.
    /// `request_id` echoes the value the engine sent.
    EngineResponse {
        request_id: String,
        response: EngineToAppResponse,
    },

    /// Evaluate a product's editorial rules against a candidate PR body
    /// (and optional title) without touching GitHub. Returns
    /// [`FrontendEvent::EditorialRulesEvaluated`] with the decision,
    /// per-finding descriptions, and the rewritten body when applicable.
    /// Mirrors `boss editorial test --body-file` but over the IPC socket
    /// so the macOS app can present the result inline.
    EvaluateEditorialRules {
        product_id: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },

    /// Resolve and render the full transcript for a completed or
    /// in-progress execution, keyed on the durable execution id.
    ///
    /// The engine reads from `work_runs.transcript_path` (via the
    /// durable `work_executions` row) rather than the live supervisor
    /// — this sidesteps the "unknown run" divergence and makes
    /// historical executions, retries, and remediations all reachable.
    ///
    /// Returns [`FrontendEvent::ExecutionTranscriptResult`] on success
    /// or [`FrontendEvent::ExecutionTranscriptUnavailable`] when the
    /// transcript file is absent or the execution never recorded one.
    ExecutionTranscript {
        execution_id: String,
    },

    /// Look up the work item(s) bound to a GitHub PR number, spanning
    /// the *entire* `tasks` table — every kind (`project_task`,
    /// `chore`, `design`, `investigation`, `revision`) across every
    /// product. Unlike `ListTasks` / `ListChores` there is no kind or
    /// product partition, so a chore- or revision-backed PR is just as
    /// findable as a project task. The PR number is parsed from each
    /// row's stored `pr_url`.
    ///
    /// Replies with [`FrontendEvent::WorkItemsByPrResult`] carrying one
    /// [`PrWorkItemMatch`] per owning row (a row whose `pr_url` resolves
    /// to `pr_number`), each bundling the revisions in that PR's chain.
    /// More than one match means the same PR number exists in more than
    /// one repo — the caller disambiguates by repo. An empty list means
    /// no work item is bound to the PR. Repo filtering and ambiguity
    /// messaging are left to the caller, which has the repo-selector
    /// vocabulary.
    FindWorkItemsByPr {
        pr_number: i64,
    },

    /// Boss-tier RPC: bring the worker pane hosting `run_id` to the
    /// front in the macOS app. Resolves `run_id → slot_id` via the
    /// engine's worker registry and forwards a `FocusWorkerPane`
    /// engine→app request. Used by `bossctl agents focus`. Returns a
    /// `WorkError` if the run is unknown or has no allocated pane.
    FocusWorkerPane {
        run_id: String,
    },

    /// Fetch one attention group by id (`atg_…` or `A<n>` short id).
    /// Replies with [`FrontendEvent::AttentionGroupResult`].
    GetAttentionGroup {
        id: String,
    },

    GetAttentionItem {
        id: String,
    },

    /// Fetch a single automation by its canonical `auto_…` id.
    /// Replies with [`FrontendEvent::AutomationResult`] or
    /// [`FrontendEvent::WorkError`] when not found.
    GetAutomation {
        id: String,
    },

    /// Return the count of open tasks produced by an automation.
    /// "Open" = any non-terminal status: `todo`, `ready`, `active` (doing),
    /// `in_review`, `blocked`. Note: the kanban label "doing" maps to the DB
    /// value `active`; the query uses the stored value.
    /// Replies with [`FrontendEvent::AutomationOpenTaskCount`].
    GetAutomationOpenTaskCount {
        automation_id: String,
    },

    /// Query the current automation-pause state without changing it.
    /// Independent of [`FrontendRequest::GetDispatchState`] — see
    /// [`FrontendRequest::SetAutomationPaused`] for the scope of what
    /// this flag holds. Replies with [`FrontendEvent::AutomationStateResult`].
    GetAutomationState,

    /// Read-only: snapshot a work item's CI attempt budget — the
    /// `tasks.ci_attempt_budget` override, the product's default, the
    /// effective value the engine uses, and the live
    /// `tasks.ci_attempts_used` counter. Backs the
    /// `boss engine ci budget show <work-item-id>` verb.
    GetCiBudget {
        work_item_id: String,
    },

    /// Read-only: fetch a single `ci_remediations` row by id. Returns
    /// [`FrontendEvent::CiRemediation`] on success and
    /// [`FrontendEvent::WorkError`] when the id is unknown.
    GetCiRemediation {
        attempt_id: String,
    },

    /// Read-only: aggregate `conflict_diagnosis` for one product into a
    /// hotspot report (`boss engine conflicts hotspots`, Layer 0 / T5):
    /// per-file frequency, per-file-pair co-conflict frequency, per-class
    /// counts. `top` caps each ranked list. Replies with
    /// [`FrontendEvent::ConflictHotspots`].
    GetConflictHotspots {
        product_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        top: Option<u32>,
    },

    /// Read-only: fetch a single attempt row by id. Returns
    /// [`FrontendEvent::ConflictResolution`] on success and
    /// [`FrontendEvent::WorkError`] when the id is unknown.
    GetConflictResolution {
        attempt_id: String,
    },

    /// Query the current dispatch-pause state without changing it.
    /// Replies with [`FrontendEvent::DispatchStateResult`].
    GetDispatchState,

    /// One-shot snapshot of the engine's user-visible configuration
    /// health — currently a single ANTHROPIC_API_KEY presence bit plus
    /// any rendered [`EngineHealthIssue`]s the UI should surface as a
    /// banner / settings-pane warning. Read-only; no side effects.
    /// Replies with [`FrontendEvent::EngineHealthResult`]. The macOS
    /// app calls this on session start so a missing API key cannot
    /// silently break summarization the way it did before #699.
    GetEngineHealth,

    /// Ask the engine to identify itself. Replies with
    /// [`FrontendEvent::EngineVersionResult`] carrying the build SHA,
    /// build time, and binary-content fingerprint of the running
    /// engine binary. Used by the macOS app on attach to detect
    /// whether the running engine matches the app's bundled engine; if
    /// they differ the app stops the old engine and spawns the new one
    /// from the bundle, ensuring the user always gets the version that
    /// shipped with the app they launched.
    GetEngineVersion,

    GetExecution {
        id: String,
    },

    /// Full details for one registered host, including all capabilities.
    /// Replies with [`FrontendEvent::HostResult`] or
    /// [`FrontendEvent::Error`] when the id is unknown.
    GetHost {
        id: String,
    },

    GetRun {
        id: String,
    },

    /// Snapshot of every registered per-installation setting and its
    /// current value. Used by the macOS Settings window to render the
    /// current state on open. Replies with
    /// [`FrontendEvent::SettingsList`]. Read-only; no side effects.
    GetSettings,

    /// Per-work-item runtime snapshot — single-item flavour of the
    /// `task_runtimes` block carried in `WorkTree`. Used by
    /// `boss chore show` / `boss task show` to enrich the rendered work
    /// item with the active execution and run ids without re-fetching
    /// the entire product tree. Replies with
    /// [`FrontendEvent::TaskRuntimeResult`]. Returns the same
    /// `TaskRuntime` shape `WorkTree` uses; every `Option` is `None`
    /// when the work item has no executions yet.
    GetTaskRuntime {
        work_item_id: String,
    },

    /// Worker → engine, read-only: the sanitized one-call context bundle —
    /// own task + project + product, sibling tasks in the project (each
    /// with dependency edges), edges touching the caller's own task, open
    /// attention groups on its work item, and its work item's proposals
    /// across executions with dispositions.
    ///
    /// No work-item argument, deliberately — exactly like
    /// [`Self::ListProposals`], scope is derived from the socket peer's
    /// attributed execution, never from a caller-supplied id. `run_id` is
    /// the caller's own `BOSS_RUN_ID`, a cross-check rather than a
    /// credential.
    ///
    /// Replies with [`FrontendEvent::WorkerContextResult`], or
    /// [`FrontendEvent::ProposalRejected`] when attribution fails — reused
    /// from the proposal API, which faces the identical attribution
    /// failure modes (see `engine/core/src/app/context.rs`).
    GetWorkerContext {
        run_id: String,
    },

    GetWorkItem {
        id: String,
    },

    /// Look up a work item by its per-product short_id (the friendly
    /// numeric id, e.g. 42 for `#42`). Searches both `tasks` and
    /// `projects` tables. Replies with `WorkItemResult` on success or
    /// `WorkError` when no match exists.
    GetWorkItemByShortId {
        product_id: String,
        short_id: i64,
    },

    GetWorkTree {
        product_id: String,
        /// App-side per-product population-fetch sequence number (T2101
        /// R1). Purely a correlation id: the macOS app mints a 1-based
        /// per-product `fetch_seq` for every `GetWorkTree` it issues and
        /// stamps it on its `population-timing-*.jsonl` lines. Propagating
        /// it here lets the engine stamp the same value on its
        /// `engine-population-timing-*.jsonl` segment events, so the two
        /// sides can be joined on `(product_id, fetch_seq)` for one fetch.
        /// Optional for backward compatibility: older app builds omit it
        /// and the engine falls back to its own envelope `request_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fetch_seq: Option<i64>,
    },

    /// Abort an in-progress device-flow authorization. The engine
    /// stops the poll loop and transitions to `Disconnected`. No-op
    /// if no flow is in progress.
    GitHubAuthCancel,

    /// Delete the stored OAuth token and return to `Disconnected`.
    /// The deletion is local only (no server-side revocation).
    GitHubAuthDisconnect,

    /// Begin the GitHub OAuth device-flow authorization for github.com.
    /// The engine starts the flow, requests a device code, and pushes
    /// [`FrontendEvent::GitHubAuthState`] events as the flow advances.
    /// If a flow is already in progress this is a no-op.
    GitHubAuthStart,

    /// Request the current GitHub auth state. The engine replies
    /// immediately with a [`FrontendEvent::GitHubAuthState`] push
    /// reflecting the latest known state.
    GitHubAuthStatus,

    /// Boss-tier RPC: interrupt the worker pane hosting `run_id` —
    /// equivalent to the human pressing Esc inside that pane.
    /// Resolves `run_id → slot_id` and forwards an
    /// `InterruptWorkerPane` engine→app request. Cancels the worker's
    /// in-flight turn without killing the run. Used by `bossctl
    /// agents interrupt`. Returns a `WorkError` if the run is unknown
    /// or has no allocated pane.
    InterruptWorkerPane {
        run_id: String,
    },

    /// App sends this when its window becomes active (user switching back
    /// from another app, e.g. after reviewing a PR on GitHub). The engine
    /// schedules an immediate pass of every PR-state reconciler so the
    /// kanban reflects upstream changes without waiting for the next
    /// periodic tick. Engine-side quiescing (15 s window) prevents
    /// repeated GitHub API calls on rapid focus-toggle events.
    /// Replies with [`FrontendEvent::PrReconcilersKicked`].
    KickPrReconcilers,

    /// Manually link a work item to a specific upstream tracker issue.
    /// The engine stores `kind`/`canonical_id` on the row; `raw` and
    /// `web_url` are populated on the next reconcile tick via
    /// `fetch_item`. Replies with [`FrontendEvent::WorkItemUpdated`]
    /// carrying the updated row, or [`FrontendEvent::WorkError`] if the
    /// work item or tracker kind is not found.
    LinkWorkItemExternalRef {
        #[serde(flatten)]
        input: LinkExternalRefInput,
    },

    /// List attention groups for a product, with optional filters.
    /// Replies with [`FrontendEvent::AttentionGroupsList`].
    ListAttentionGroups {
        product_id: String,
        /// Filter to groups associated with this project.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        /// Filter to groups associated with this task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        /// Filter by kind (`"question"` | `"followup"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Filter by state. Defaults to open + partially_answered when
        /// `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
    },

    ListAttentionItems {
        execution_id: String,
    },

    ListAttentionItemsForWorkItem {
        work_item_id: String,
    },

    /// List the `attention_merges` provenance rows recorded against one
    /// canonical `Attention` id — every fold that was reconciled into it.
    /// Feeds the Notifications UI's merge-provenance affordance ("edited by
    /// merge" marker + "folded N duplicate reports" detail). Replies with
    /// [`FrontendEvent::AttentionMergesList`]. Empty until the dedup
    /// creation/sweep paths (P1203 tasks 5/7) start writing rows.
    ListAttentionMerges {
        attention_id: String,
    },

    /// List the `automation_dedup_suppressions` trace for an automation,
    /// newest first — every candidate task the dedup gate refused because
    /// an open sibling already tracked the finding. Replies with
    /// [`FrontendEvent::AutomationDedupSuppressionsList`].
    ListAutomationDedupSuppressions {
        automation_id: String,
    },

    /// List the `automation_runs` history for an automation, newest first.
    /// Replies with [`FrontendEvent::AutomationRunsList`].
    ListAutomationRuns {
        automation_id: String,
    },

    /// List all automations for a product, ordered `created_at ASC`.
    /// Replies with [`FrontendEvent::AutomationsList`].
    ListAutomations {
        product_id: String,
    },

    /// List tasks that were produced by a specific automation
    /// (`tasks.source_automation_id = automation_id`), ordered by
    /// `created_at DESC`. Replies with [`FrontendEvent::AutomationTasksList`].
    ListAutomationTasks {
        automation_id: String,
    },

    ListChores {
        product_id: String,
        /// Phase 3 dep filter (Q6). See [`Self::ListProjects`].
        #[serde(default)]
        dep_filter: Option<DependencyFilter>,
        /// See [`Self::ListTasks::include_deleted`].
        #[serde(default)]
        include_deleted: bool,
    },

    /// Read-only: list `ci_remediations` rows. The CLI surface is
    /// `boss engine ci list` (design Phase 11 #35). Filters are AND-ed;
    /// an empty `status` list matches every status. Ordering is
    /// `created_at DESC, id DESC` so the freshest attempt is first.
    ListCiRemediations {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        product_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        status: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work_item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    /// Read-only: list `conflict_resolutions` rows. The CLI surface is
    /// `boss engine conflicts list` (design Phase 5 / #13). Filters are
    /// AND-ed; an empty `status` list matches every status. Ordering is
    /// `created_at DESC, id DESC` so the freshest attempt is first.
    ListConflictResolutions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        product_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        status: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work_item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    /// List every open `deferred_scope` item across a product, paired with
    /// the id of the work item whose execution recorded it.
    ListDeferredScopeAttentions {
        product_id: String,
    },

    /// Return the prerequisite and/or dependent edges for one work
    /// item. `direction` defaults to `both`.
    ListDependencies {
        #[serde(flatten)]
        input: ListDependenciesInput,
    },

    /// Resolved counterpart of [`Self::ListDependencies`]: returns
    /// the same incoming / outgoing split, but each entry carries the
    /// peer's status and name already joined in. Used by `boss
    /// <kind> show` so the human / JSON renderer needs one round-trip
    /// instead of N+1.
    ListDependenciesDetailed {
        #[serde(flatten)]
        input: ListDependenciesInput,
    },

    /// List recorded editorial-action audit rows for a product, ordered
    /// `created_at DESC` (freshest first). `limit` caps the result set;
    /// defaults to 50 when absent. Returns
    /// [`FrontendEvent::EditorialActionsList`].
    ListEditorialActions {
        product_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    /// Read-only: list rows from any of the three attempt subsystems
    /// (`conflict_resolutions`, `rebase_attempts`, `ci_remediations`),
    /// projected through the [`EngineAttemptListEntry`] shape with a
    /// `kind` discriminator. Design Phase 11 #36.
    ///
    /// `kinds` is the set of `kind` values to include; an empty vec
    /// matches all three. `status` is AND-ed across all included
    /// kinds (each row is filtered by its own table's `status`
    /// column). Ordering: `created_at DESC` across the merged set.
    ListEngineAttempts {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        kinds: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        product_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        status: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work_item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    ListExecutions {
        work_item_id: Option<String>,
        /// When `true` and `work_item_id` names a chain root, the
        /// response includes executions from every revision task in
        /// the chain as well as the root's own executions.
        #[serde(default)]
        include_revision_chain: bool,
    },

    /// Snapshot of every registered engine feature flag and its current
    /// value. Backs the macOS app's Feature Flags debug pane (incident
    /// 001 AI #5). Replies with [`FrontendEvent::FeatureFlagsList`].
    /// Read-only; no side effects.
    ListFeatureFlags,

    /// All registered hosts with their enabled state and capabilities.
    /// Includes the built-in `local` host. Replies with
    /// [`FrontendEvent::HostsList`].
    ListHosts,

    /// Read-only query: which slots does the app currently host a
    /// session in that the engine has NO live-tracked run for
    /// ("husk" panes)? Powers `bossctl agents list --all`, which is
    /// otherwise structurally blind to husks — `ListWorkerLiveStates`
    /// only reflects the engine's own `LiveWorkerStateRegistry`, which
    /// by definition has no entry for a husk. Replies with
    /// [`FrontendEvent::HuskPanesList`]; an empty list (not an error)
    /// when no app session is registered or the app reports nothing
    /// the engine doesn't already track.
    ListHuskPanes,

    /// Snapshot of which slots currently have the live-status
    /// summarizer disabled. The UI uses this to render the toggle
    /// state on the Agents-tab worker row.
    ListLiveStatusDisabledSlots,

    /// All `planner_runs` audit rows for a project, newest first. Backs
    /// `boss project plan-runs <project>` — the operator's after-the-fact
    /// window into auto-populate invocations they could not watch. See
    /// `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`
    /// §"Durable audit trail". Replies with [`FrontendEvent::PlannerRunsList`].
    ListPlannerRuns {
        project_id: String,
    },

    ListProducts,

    ListProjects {
        product_id: String,
        /// Phase 3 dep filter (Q6). Restricts the returned list to
        /// rows that match a dependency-graph predicate before any
        /// CLI-side filters (status / match / id). Backwards-
        /// compatible: pre-Phase-3 callers omit the field and get the
        /// historical behaviour.
        #[serde(default)]
        dep_filter: Option<DependencyFilter>,
    },

    /// Worker → engine, read-only: every `worker_proposals` row filed
    /// against the caller's own work item, **across executions**, with its
    /// current disposition (`state`, `decision_reason`, `applied_ref`).
    ///
    /// Scope is the work item, not the execution, deliberately: a resumed or
    /// successor run must be able to see "rejected: duplicate of an existing
    /// task" from a prior execution and adjust, rather than re-proposing
    /// into the same wall (dispositions must be visible across executions,
    /// not just in-run — see
    /// `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`).
    /// The caller cannot widen the scope — the work item is derived from
    /// the socket peer's attributed execution, never from a field on this
    /// request.
    ///
    /// `run_id` is the caller's own `BOSS_RUN_ID` (i.e. `work_executions.id`)
    /// and is a **cross-check, not a credential** — see [`Self::SubmitProposal`].
    /// Replies with [`FrontendEvent::ProposalsList`], or
    /// [`FrontendEvent::ProposalRejected`] when attribution fails.
    ListProposals {
        run_id: String,
        /// Restrict to one kind. `None` returns every kind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<ProposalKind>,
        /// Restrict to one disposition. `None` returns every state,
        /// including `rejected` / `expired` history — which is the point of
        /// the verb.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<ProposalState>,
    },

    /// Read-only: enumerate `kind = 'revision'` rows for a product. Revisions
    /// are excluded from `ListTasks` and `ListChores` by design; this request
    /// is the only way to list them in bulk. Replies with
    /// [`FrontendEvent::RevisionsList`].
    ListRevisions {
        product_id: String,
        /// Phase 3 dep filter (Q6). See [`Self::ListProjects`].
        #[serde(default)]
        dep_filter: Option<DependencyFilter>,
        /// Restrict results to revisions whose `parent_task_id` matches this
        /// canonical task id. When absent, all revisions in the product are
        /// returned.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// See [`Self::ListTasks::include_deleted`].
        #[serde(default)]
        include_deleted: bool,
    },

    ListRuns {
        execution_id: String,
    },

    ListTasks {
        product_id: String,
        project_id: Option<String>,
        /// Phase 3 dep filter (Q6). See [`Self::ListProjects`].
        #[serde(default)]
        dep_filter: Option<DependencyFilter>,
        /// When `true`, soft-deleted rows (`deleted_at IS NOT NULL`) are
        /// included in the result so the operator can find a tombstoned
        /// task to `restore` it. Defaults to `false` — pre-existing
        /// callers omit the field and keep the historical live-only view.
        #[serde(default)]
        include_deleted: bool,
    },

    /// Snapshot of every allocated worker slot's live state — what
    /// model it's running, what activity (working / waiting / idle /
    /// errored / terminated), most recent tool, etc. Source of truth
    /// for the kanban Doing-icon and the per-pane titlebar pill.
    /// Subscribers can also listen on the `worker.live_states` topic
    /// for push updates whenever any slot's state changes.
    ListWorkerLiveStates,

    /// Worker → engine marker (Phase 9 #30): flip a non-terminal
    /// `ci_remediations` attempt to `failed` with a reason. Mirrors
    /// [`Self::MarkConflictResolutionFailed`] — the worker calls
    /// this when it classifies the failure as `unfixable` (or
    /// otherwise gives up without pushing). The parent stays
    /// `blocked: ci_failure`.
    MarkCiRemediationFailed {
        attempt_id: String,
        reason: String,
    },

    /// Worker → engine, *validated* terminal signal: "there is no CI
    /// to fix — the PR's required checks are already green." CLI:
    /// `boss engine ci mark-noop --attempt-id <cir_…> [--observed-sha
    /// <sha>] [--reason <r>]`. Unlike other `Mark*` verbs, the engine
    /// does NOT take the worker's word: it re-probes LIVE CI for the
    /// PR's CURRENT head SHA (the merge-poller's `gh pr view …
    /// statusCheckRollup` source) and only honors the claim when
    /// every required check is verified passing on that exact SHA.
    /// Verified-green retires the attempt and unblocks the parent
    /// ([`FrontendEvent::CiRemediationNoopValidated`]); red/pending
    /// (or a moved SHA) rejects and keeps the row actionable
    /// ([`FrontendEvent::CiRemediationNoopRejected`]).
    ///
    /// `observed_sha` is advisory only — the verdict always re-derives
    /// from the live head SHA. `reason` is free-form (default `already_green`).
    MarkCiRemediationNoop {
        attempt_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_sha: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Worker → engine marker (Phase 9 #30): record that the worker
    /// re-triggered the failing build via the per-provider CLI
    /// (`bk build retry` / `gh run rerun --failed`). `new_id` is the
    /// provider-emitted identifier for the new run/build (Buildkite
    /// returns a fresh build id; GHA reuses the original run id).
    /// The engine stamps it as a debug breadcrumb; the merge-poller's
    /// CI probe observes the re-run's outcome on the next sweep.
    /// `retrigger`-kind attempts stay `running` after this call
    /// because their terminal status is set by the next probe; no
    /// status flip happens here.
    MarkCiRemediationRetriggered {
        attempt_id: String,
        new_id: String,
    },

    /// Worker → engine, *validated* terminal signal (Phase 9 #30,
    /// reconciled 2026-05-17 layered design call; re-validated per the
    /// T2764 postmortem, PR spinyfin/mono#2023): the worker claims it
    /// rebased onto base HEAD, force-pushed, and CI came back green
    /// without changing any code. Unlike a trust-the-worker marker, the
    /// engine does NOT honor this claim on say-so: it independently
    /// re-probes LIVE CI for the PR's CURRENT head SHA (the same gate
    /// [`Self::MarkCiRemediationNoop`] uses) and only flips the attempt
    /// to `succeeded` — with `consumes_budget = 0`, refunding the
    /// detection-side `ci_attempts_used` bump, and publishing
    /// `CiRemediationSucceeded` — when every required check is verified
    /// passing on that exact SHA
    /// ([`FrontendEvent::CiRemediationSucceededViaRebase`]). A claim made
    /// before the re-run has settled (still pending) or on a head that is
    /// actually red is rejected with the live status and the row stays
    /// actionable — no budget refund, no auto-merge re-arm, no
    /// `CiRemediationSucceeded` event
    /// ([`FrontendEvent::CiRemediationSucceededViaRebaseRejected`]).
    /// Idempotent — a second call on the already-`succeeded` row echoes
    /// the existing receipt without re-probing.
    MarkCiRemediationSucceededViaRebase {
        attempt_id: String,
    },

    /// Worker-facing escape hatch for the merge-conflict resolution
    /// flow: flip a `conflict_resolutions` attempt to `failed` with a
    /// reason. The CLI surface is `boss engine conflicts mark-failed
    /// <attempt-id> --reason <r>` — workers call it when they hit one
    /// of the stop conditions (semantic obsolescence, product decision
    /// required, architectural mismatch) and decide not to push. See
    /// `tools/boss/docs/designs/merge-conflict-handling-in-review.md`
    /// Q4 / Q11.
    MarkConflictResolutionFailed {
        attempt_id: String,
        reason: String,
    },

    /// User-initiated "Merge When Ready" for a Review-lane task's PR.
    /// The engine resolves the task's PR and its product's merge
    /// mechanism, then fires the appropriate operation:
    /// - `direct` (default; also covers a GitHub-native merge queue):
    ///   - repo has a merge queue → enqueue the PR
    ///   - no merge queue, checks passing → merge directly
    ///   - no merge queue, checks pending → enable auto-merge
    /// - `trunk_queue`: submit the PR to the product's Trunk merge queue
    ///   (`POST submitPullRequest`) and record a standing merge intent the
    ///   queue poller tracks to a terminal state. A missing/rejected Trunk
    ///   API token surfaces as a loud [`FrontendEvent::WorkError`] — never
    ///   a silent fallback to the `direct` verb. A second click while an
    ///   intent is already active is a no-op that re-reports success
    ///   without re-submitting.
    ///
    /// Pre-flight guards: the task must be a task/chore (not a project),
    /// have `status == "in_review"`, and carry a non-empty `pr_url`.
    /// Any failure returns [`FrontendEvent::WorkError`]. On success,
    /// replies with [`FrontendEvent::MergeWhenReadyAccepted`] and kicks
    /// the PR-reconciler so the kanban reflects the new state promptly.
    MergeWhenReady {
        work_item_id: String,
    },

    /// Bulk snapshot of every registered counter and gauge, bypassing
    /// the 30s flush-staleness window. Used by the macOS app's Metrics
    /// debug pane to render a full listing in one round-trip instead of
    /// one `MetricsShowLive` call per metric. Replies with
    /// [`FrontendEvent::MetricsListLiveResult`]. Includes stale
    /// (rehydrated from `state.db` but no current handle) entries so
    /// the pane can surface historical counters that no longer exist in
    /// the running binary.
    MetricsListLive,

    /// Reset one or all counter / gauge values to zero — both
    /// in-memory and in `state.db` — in a single atomic step.
    /// `name = None` means "reset everything". Routes through engine
    /// RPC so the in-memory atomic and the database row are cleared in
    /// lockstep; a direct SQLite write would leave the atomic stale
    /// until the next flush. Replies with
    /// [`FrontendEvent::MetricsResetDone`].
    MetricsReset {
        name: Option<String>,
    },

    /// Read a single metric's current in-memory value, bypassing the
    /// 30s flush-staleness window. Used by `bossctl metrics show
    /// --live`. The engine replies with
    /// [`FrontendEvent::MetricsShowLiveResult`]; `entry` is `None`
    /// when no counter or gauge with `name` is registered.
    MetricsShowLive {
        name: String,
    },

    /// App asks the engine for a terminal into the workspace of a work
    /// item's already-live execution (Doing-column debugging affordance).
    /// Unlike [`Self::OpenReviewTerminal`], this does NOT lease a new
    /// workspace — it reads the `workspace_path` already held by the
    /// running worker's execution row, so the terminal opens alongside
    /// the worker without disturbing its session. The engine replies with
    /// [`FrontendEvent::LiveWorkspaceTerminalReady`] on success or
    /// [`FrontendEvent::WorkError`] if the work item has no live
    /// execution / leased workspace.
    OpenLiveWorkspaceTerminal {
        work_item_id: String,
    },

    /// App asks the engine to lease a workspace for the given Review-
    /// column work item, fetch the PR branch, and create a fresh jj
    /// commit off `<branch>@origin`. The engine replies with
    /// [`FrontendEvent::ReviewTerminalReady`] on success or
    /// [`FrontendEvent::WorkError`] on failure.
    OpenReviewTerminal {
        work_item_id: String,
    },

    /// Operator entry point for the auto-populate Planner/Materializer
    /// (design P783 §2 "Reusability" #2 — "plan this project now" /
    /// replan). Builds the same `PlannerInput` the design-PR-merge
    /// trigger builds — from the project's stored `design_doc_path`,
    /// fetched live from GitHub — and runs the identical claim →
    /// pre-seeded check → fetch → plan → validate → apply → audit →
    /// surface pipeline, stamping `planner_runs.caller = "operator"`.
    ///
    /// `dry_run` runs infer + validate and returns the proposal without
    /// materializing or claiming the per-project idempotency gate — a
    /// preview of what a real run would do. `force` bypasses the
    /// pre-seeded refusal (a project that already has implementation
    /// tasks); the Materializer's `(name, project_id)` dedup makes a
    /// forced re-populate additive, never destructive. Replies with
    /// [`FrontendEvent::PlanProjectResult`].
    PlanProject {
        project_id: String,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        dry_run: bool,
    },

    /// Boss-tier RPC: queue a probe prompt for `run_id`. By default
    /// the engine holds the text until the next `Stop` hook event for
    /// that run, then writes it into the worker's pty as if it were
    /// typed by the user. When `urgent` is `true`, the engine delivers
    /// the probe at the next `PostToolUse` boundary instead — after the
    /// current tool call finishes (so no in-flight Bash is cancelled)
    /// but before the worker starts its next tool call. Urgent probes
    /// are pushed to the front of the per-run queue so they always
    /// land before any queued non-urgent probes. Returns immediately
    /// with a `ProbeQueued` event carrying the engine-minted `probe_id`;
    /// the worker's reply is surfaced asynchronously via
    /// [`FrontendEvent::ProbeReplied`] on the [`probe_topic`] for
    /// `run_id`. Urgent probes are prefixed with `[coordinator-nudge]`
    /// in the transcript so the worker and human readers can identify
    /// coordinator-injected text.
    ProbeRun {
        run_id: String,
        text: String,
        /// When `true`, deliver at the next tool-call boundary
        /// (PostToolUse) rather than the next Stop boundary. The
        /// engine waits for any in-flight tool call to return before
        /// injecting, so no work is discarded. Omit or set to `false`
        /// for the original queue-for-Stop behaviour.
        #[serde(default)]
        urgent: bool,
    },

    /// Boss-only RPC: mark the execution backing `run_id` as the
    /// terminal `orphaned` status and preserve its cube workspace
    /// lease so a fresh execution can resume against the same branch.
    /// Used by `bossctl agents reap` for orphans that the engine
    /// startup heuristics missed — e.g. when the cube lease is still
    /// within its TTL because the previous app crash was recent.
    /// Returns `WorkError` if the run id is unknown or already
    /// terminal.
    ReapRun {
        run_id: String,
    },

    /// Append an effort-level escalation event (design §Q5, PR
    /// #370 follow-up). Wire surface used by the sibling
    /// escalation-handler task; this task ships the row format and
    /// the read path. Engine assigns `id` and `created_at`; the
    /// caller passes the row's original / new level and the §Q4
    /// markers the heuristic recorded against the row at creation.
    /// Replies with [`FrontendEvent::EffortEscalationRecorded`].
    RecordEffortEscalation {
        work_item_id: String,
        original_level: crate::EffortLevel,
        new_level: crate::EffortLevel,
        markers: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule_id: Option<String>,
    },

    /// Worker-facing telemetry surface closing the producer-side
    /// blind spot in `conflict_resolutions`
    /// (`merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`
    /// T1): a normal (non-conflict-resolution) worker's own `cube
    /// workspace rebase` reported `REBASED_WITH_CONFLICTS` mid-task —
    /// resolved inline and never surfaced to `conflict_watch`. The CLI
    /// surface is `boss engine conflicts record-producer`. The engine
    /// resolves `product_id` / `work_item_id` / any existing `pr_url`
    /// from `execution_id`; the row is inserted already terminal
    /// (`status = 'succeeded'`, `event_source = 'producer_rebase'`) —
    /// by the time a worker calls this it has already resolved the
    /// conflict, so there is no separate pending/running lifecycle to
    /// track. Replies with [`FrontendEvent::ConflictResolution`].
    RecordProducerSideConflict {
        execution_id: String,
        head_branch: String,
        base_branch: String,
        conflicted_files: Vec<String>,
    },

    /// App self-identifies as the singleton app session. The engine
    /// rejects this unless `LOCAL_PEERPID` matches the app's pid (the
    /// engine's parent). After registration, `EngineRequest` events
    /// flow to this session only.
    RegisterAppSession,

    /// App tells the engine which pid is the Boss session's shell.
    /// Used to populate the second trust root for Boss-only RPCs.
    /// Only the registered app session may call this.
    RegisterBossSession {
        shell_pid: i32,
    },

    /// App reports the capability IDs compiled into this build. The
    /// engine updates its in-memory capability registry so the flag
    /// system can detect when a flag is enabled but its backing
    /// capability is absent. Sent once per session immediately after
    /// [`Self::RegisterAppSession`] is acknowledged. Replies with
    /// [`FrontendEvent::FeatureFlagsList`] so the flag pane
    /// immediately reflects accurate `capability_present` state.
    RegisterCapabilities {
        capability_ids: Vec<String>,
    },

    /// Release a project's staged auto-populate batch (design
    /// §"Operator review checkpoint"): flips `autostart = true` on every
    /// non-deleted task tagged with the project's live `planner_runs`
    /// row (`outcome = 'staged'`), so the normal dispatcher picks them up
    /// on its next pass. Errors if there is no staged run for the
    /// project (nothing to release, or it was already released).
    /// Replies with [`FrontendEvent::ReleaseProjectResult`].
    ReleaseProject {
        project_id: String,
    },

    /// App notifies the engine that a review terminal window has closed
    /// and the associated workspace lease should be released. This is
    /// fire-and-forget: the engine logs failures but does not reply.
    ReleaseReviewTerminal {
        lease_id: String,
    },

    /// Drop the `(dependent, prerequisite, relation)` edge. No-op if
    /// the edge does not exist (mirrors `boss <kind> delete` on an
    /// already-archived row).
    RemoveDependency {
        #[serde(flatten)]
        input: RemoveDependencyInput,
    },

    /// Deregister a remote host. Fails for the built-in `local` host
    /// (matching the `bossctl hosts remove` invariant). Replies with
    /// [`FrontendEvent::HostRemoved`] on success or
    /// [`FrontendEvent::Error`] on failure.
    RemoveHost {
        id: String,
    },

    /// Remove one user-defined capability tag from a host. Only user
    /// tags can be removed; auto-discovered tags are managed by the
    /// engine heartbeat. Replies with [`FrontendEvent::HostUpdated`].
    RemoveHostTag {
        host_id: String,
        tag: String,
    },

    ReorderProjectTasks {
        project_id: String,
        task_ids: Vec<String>,
    },

    /// App reports that a worker pane's shell never came up — the
    /// libghostty surface failed to create (typically `ghostty_surface_new`
    /// returning NULL when there is no active display after sleep/wake,
    /// the #800 condition). This is the proactive NACK for the false-live
    /// spawn: the spawn RPC was already answered `Ok(shell_pid: 0)`
    /// synchronously because the surface is created asynchronously, so the
    /// only way the engine learns the shell never started — short of the
    /// 60s `spawn_ack_sweep` timeout — is this message. The engine reaps
    /// the execution immediately (mirroring the sweep) and feeds its
    /// spawn-capability circuit breaker, so a systemic post-wake failure
    /// is caught in seconds instead of churning for hours. `reason` is a
    /// short human-readable cause for the orphan record and diagnostics.
    /// Fire-and-forget; no response expected. Only the registered app
    /// session may call this.
    ReportWorkerSpawnFailed {
        run_id: String,
        reason: String,
    },

    RequestExecution {
        #[serde(flatten)]
        input: RequestExecutionInput,
    },

    /// Read-only: resolve a project's design-doc pointer into the
    /// structured [`ResolveProjectDesignDocOutput`] the UI consumes.
    /// Engine-side this is `WorkDb::resolve_project_design_doc`
    /// composed with a cheap check against the engine's in-flight
    /// execution list to populate
    /// [`ProjectDesignDocState::Resolved::workspace_path`].
    /// No DB writes; no topic events.
    ResolveProjectDesignDoc {
        project_id: String,
    },

    /// Inverse of [`Self::DeleteWorkItem`]: clear the `deleted_at`
    /// tombstone on a soft-deleted task, making it visible again. The
    /// `id` accepts a canonical `task_…` id or a friendly short id
    /// (`T43`); the engine resolves the friendly form against
    /// soft-deleted rows too, so a tombstoned task is still findable.
    /// Idempotent — restoring an already-live row succeeds as a no-op.
    /// Replies with [`FrontendEvent::WorkItemRestored`] on success.
    RestoreWorkItem {
        id: String,
    },

    /// Boss-only break-glass RPC: instruct the app to tear down
    /// whatever pane is hosted in `slot_id` and free the slot in the
    /// app's own bookkeeping — WITHOUT resolving through a run id.
    /// Exists for "husk" panes: a pane the app still hosts but the
    /// engine has no live-tracked run for (crash, terminal-fail path
    /// bug, spawn-ack timeout). Neither `StopRun` nor `ReapRun` can
    /// reach this case — both key off a run id the engine no longer
    /// has a slot mapping for, so `bossctl agents stop` fails with "no
    /// live worker matches" before it ever reaches the engine. Used by
    /// `bossctl agents retire-pane <slot>`.
    ///
    /// Refuses with `WorkError` when the engine's own
    /// `LiveWorkerStateRegistry` still shows a live (non-terminal) run
    /// in `slot_id` — that pane is not a husk, and retiring it would
    /// tear down a pane the engine still considers active; the caller
    /// must use `agents stop` (or `agents reap`) instead. Idempotent
    /// otherwise: a slot the app doesn't recognise (already released,
    /// never allocated) still replies `PaneRetired`.
    RetirePane {
        slot_id: u8,
    },

    /// User-facing reset for an `in_review` PR that has been blocked
    /// by the CI auto-fix flow. Accepts either a work-item id or a
    /// `ci_remediations` attempt id (the engine resolves an attempt id
    /// to its parent's `work_item_id`). Resets `tasks.ci_attempts_used`
    /// to 0 and, when the parent is `blocked: ci_failure_exhausted`,
    /// flips it back to `in_review` so the next merge-poller sweep
    /// re-fires the auto-fix path. Design Phase 11 #35; see also Q11.
    RetryCiRemediation {
        /// Either a `ci_remediations` attempt id (`cir_…`) or a
        /// work-item id. The engine handles both shapes.
        selector: String,
    },

    /// Reset a terminal-failure attempt back to `pending` so the
    /// dispatcher re-spawns a worker. Only valid for rows whose status
    /// is `failed` or `abandoned`; calling on a non-terminal row
    /// (`pending` / `running`) is rejected. The parent work item is
    /// re-flipped to `blocked: merge_conflict` and the new
    /// `blocked_attempt_id` points at the reset row. See Phase 5 #13.
    RetryConflictResolution {
        attempt_id: String,
    },

    /// Boss-tier RPC: ask the macOS app to scroll the kanban to a
    /// specific work item's card and play a short transient highlight.
    /// `id` accepts a canonical id (`task_…`, `proj_…`) or a
    /// short-id form (`T607`). Idempotent — repeat calls re-pulse
    /// without animation overlap. Replies with [`FrontendEvent::WorkItemRevealed`]
    /// on success or [`FrontendEvent::WorkError`] on failure (item not
    /// found, item deleted, app not running, unknown id format).
    RevealWorkItem {
        id: String,
    },

    /// Enqueue an out-of-schedule triage fire for an automation.
    /// `force = true` bypasses the open-task cap. Replies with
    /// [`FrontendEvent::AutomationRunEnqueued`] when the fire was accepted,
    /// or [`FrontendEvent::WorkError`] if the cap gate blocks it (and
    /// `force` was not set).
    RunAutomation {
        automation_id: String,
        force: bool,
    },

    /// Boss-tier RPC: write `text` into the worker pane hosting
    /// `run_id` as if the user typed it. Resolves `run_id → slot_id`
    /// via the worker registry and forwards a `SendToPane` engine→app
    /// request, which the app routes through the same libghostty
    /// surface a real keystroke takes. Used by `bossctl agents send`.
    /// Returns `WorkError` if the run is unknown, has no allocated
    /// pane, or the app rejects the injection.
    SendInputToWorker {
        run_id: String,
        text: String,
    },

    /// Pause or resume automation-originated activity. When `paused = true`
    /// the engine holds NEW automation activity: the automation scheduler
    /// (and `bossctl` / `boss automation run`'s manual fire) stop starting
    /// triage passes, and `drain_ready_queue` stops claiming executions
    /// bound for the automation pool (both fresh triage executions and
    /// tasks a triage worker produces) until a subsequent
    /// `SetAutomationPaused { paused: false }` call. Already-running
    /// automation workers are NOT interrupted — they complete normally,
    /// including recording whatever task their triage decision produces.
    /// The flag is persisted to `state.db` so it survives an engine
    /// restart. Idempotent: pausing while already paused (or resuming
    /// while already running) is a no-op.
    ///
    /// Independent of [`FrontendRequest::SetDispatchPaused`]: a dispatch
    /// pause already holds automation-pool *spawns* (automation rows are
    /// not exempt from it, same as main-pool rows) but does not stop the
    /// automation scheduler from creating new triage executions — they
    /// just queue `ready` until dispatch resumes. This flag additionally
    /// stops those triage passes from starting in the first place, which
    /// matters when the goal is curbing runaway automation-produced work
    /// items rather than just throttling dispatch. Toggling one flag never
    /// changes the other. Replies with [`FrontendEvent::AutomationStateResult`].
    SetAutomationPaused {
        paused: bool,
    },

    /// Set (or clear) a work item's per-PR `tasks.ci_attempt_budget`
    /// override. Pass `Some(n)` (clamped server-side to `0..=10`) or
    /// `None` (clear → product default applies). Backs
    /// `boss engine ci budget set`.
    SetCiBudget {
        work_item_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<i64>,
    },

    /// Pause or resume global dispatch. When `paused = true` the engine
    /// stops dispatching new executions from every source (auto-dispatch,
    /// reconciliation, dependency-gate-clear, manual start) until a
    /// subsequent `SetDispatchPaused { paused: false }` call. Already-running
    /// executions are NOT interrupted — they complete normally. The flag is
    /// persisted to `state.db` so it survives an engine restart. Idempotent:
    /// pausing while already paused (or resuming while already running) is a
    /// no-op.
    ///
    /// Independent of [`FrontendRequest::SetAutomationPaused`]: this pause
    /// already holds automation-pool executions from claiming a slot (they
    /// are not exempt, same as main-pool rows) but does NOT stop the
    /// automation scheduler from creating new triage executions or a
    /// running triage worker from recording a produced task — those keep
    /// queueing and drain once dispatch resumes. Pausing dispatch never
    /// sets or implies the automation-pause flag, and vice versa. Replies
    /// with [`FrontendEvent::DispatchStateResult`].
    SetDispatchPaused {
        paused: bool,
    },

    /// Toggle one feature flag on or off. The engine updates the
    /// in-memory map and rewrites the on-disk file atomically; the
    /// new value is visible to consumer-side `is_enabled` calls the
    /// moment this request returns. The reply
    /// ([`FrontendEvent::FeatureFlagSet`]) confirms the persisted
    /// state and is the round-trip "the engine has reloaded" signal
    /// the debug pane uses to render the toggle as committed.
    SetFeatureFlag {
        name: String,
        enabled: bool,
    },

    /// Enable or disable a registered host. Disabled hosts receive no
    /// new work dispatches. Replies with [`FrontendEvent::HostUpdated`]
    /// on success or [`FrontendEvent::Error`] when the id is unknown.
    SetHostEnabled {
        id: String,
        enabled: bool,
    },

    /// Per-slot toggle for the live-status summarizer. When
    /// `enabled = false`, the engine stops calling the summarizer for
    /// `slot_id` and clears any existing `live_status`; the UI falls
    /// back to the static pane_summary. Persisted in the engine
    /// metadata table so the choice survives engine restarts.
    /// Idempotent — toggling to the current state is a benign no-op.
    SetLiveStatusEnabled {
        slot_id: u8,
        enabled: bool,
    },

    /// Set (or clear) a product's `default_driver`. `driver` is a
    /// driver slug stored verbatim; `None` clears the column (the
    /// engine resolves `NULL` to `"claude"`). The engine does NOT
    /// validate the slug — driver registration is engine-side.
    /// Returns the updated product wrapped in `WorkItemUpdated`.
    SetProductDefaultDriver {
        product_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        driver: Option<String>,
    },

    /// Set (or clear) a product's `default_model` per the
    /// effort-and-model-estimation design (PR #370). `model` is a
    /// claude model slug stored verbatim; `None` clears the column.
    /// The engine does NOT validate the slug — claude is the source
    /// of truth on what `--model` accepts (design §Q3). Returns the
    /// updated product wrapped in `WorkItemUpdated`.
    SetProductDefaultModel {
        product_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },

    /// Set (or clear) a product's editorial rules. `rules = None` in
    /// the input clears the stored blob; the product reverts to the
    /// engine defaults (strip known Boss identifiers, no template
    /// enforcement). Returns [`FrontendEvent::WorkItemUpdated`] carrying
    /// the updated product row, or [`FrontendEvent::WorkError`] if the
    /// product is not found.
    SetProductEditorialRules {
        #[serde(flatten)]
        input: SetProductEditorialRulesInput,
    },

    /// Bind (or unbind) an external tracker on a product. When `unset`
    /// is `true`, both `external_tracker_kind` and
    /// `external_tracker_config` are cleared. Otherwise both `kind` and
    /// `config` must be present; the engine passes `config` through the
    /// tracker impl's `validate_config` before persisting. Replies with
    /// [`FrontendEvent::WorkItemUpdated`] carrying the updated product
    /// row, or [`FrontendEvent::WorkError`] on validation failure.
    SetProductExternalTracker {
        #[serde(flatten)]
        input: SetProductExternalTrackerInput,
    },

    /// Set (or clear) a product's `merge_mechanism` (per the Trunk
    /// merge-queue integration design's "Per-product merge mechanism"
    /// setting). `mechanism` is the raw setting string (`"direct"` /
    /// `"trunk_queue"`); `None` clears the column, which resolves to
    /// `direct`. The engine validates the value against
    /// `MergeMechanism::parse` and rejects unknown values. Returns the
    /// updated product wrapped in `WorkItemUpdated`.
    SetProductMergeMechanism {
        product_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mechanism: Option<String>,
    },

    /// Set (or clear) a project's design-doc pointer. Persists the
    /// three `projects.design_doc_*` columns per
    /// [`SetProjectDesignDocInput`]'s semantics and replies with the
    /// updated `Project` row wrapped in a `WorkItemUpdated` event —
    /// same shape `UpdateWorkItem` returns for any other property
    /// edit, so existing kanban subscribers refresh without special
    /// casing. Publishes a `work_invalidated` topic event on the
    /// project's product so other connected clients see the change.
    SetProjectDesignDoc {
        #[serde(flatten)]
        input: SetProjectDesignDocInput,
    },

    /// Set one per-installation setting. The engine persists to
    /// `settings.toml` atomically; consumer-side reads see the new
    /// value the moment this returns. The reply
    /// ([`FrontendEvent::SettingSet`]) confirms the persisted value.
    SetSetting {
        key: String,
        enabled: bool,
    },

    /// Token-authenticated shutdown. The engine writes a random token
    /// to `~/Library/Application Support/Boss/engine-control.token`
    /// (mode 0600) at startup; callers that want to ask the engine to
    /// exit cleanly read the token file and send it here. The engine
    /// validates the token verbatim and, on match, replies with
    /// [`FrontendEvent::ShutdownAccepted`] then triggers the same
    /// graceful-shutdown path SIGTERM takes. A mismatch replies with
    /// [`FrontendEvent::ShutdownRejected`] and records an audit entry.
    ///
    /// Replaces SIGTERM as the everyday "restart engine" gesture — see
    /// issue #705. The macOS app, `boss engine stop`, and bossctl all
    /// take this route; SIGTERM remains the OS-shutdown fallback.
    Shutdown {
        token: String,
    },

    /// App reports that it can once again host worker panes — sent after
    /// `GhosttyRuntime` observes `NSWorkspace.didWakeNotification` /
    /// `screensDidWakeNotification` and confirms an active display is
    /// present. Without this, a sleep/wake cycle that briefly stranded a
    /// spawn (`ghostty_surface_new` returning NULL for the #800
    /// no-active-display condition, or an orphaned execution reported via
    /// `WorkerPaneDied`) only gets redispatched on the next periodic
    /// sweep/heartbeat tick, which can lag the wake by up to a minute.
    /// The engine reacts by kicking the scheduler immediately so any work
    /// stranded by the sleep is redispatched as soon as the app can host
    /// it again. Fire-and-forget; no response expected.
    SpawnCapabilityRestored,

    /// Boss-tier RPC: tear down the libghostty pane hosting `run_id`
    /// and release the cube workspace its execution still holds.
    /// Used by `bossctl agents stop`. Idempotent — duplicate requests
    /// (or one racing with completion-detection) collapse to a no-op
    /// on the second pass.
    StopRun {
        run_id: String,
    },

    /// Worker → engine: submit one typed proposal, synchronously validated
    /// and persisted before the reply.
    ///
    /// This is the mediated replacement for the marker/fenced-block seams.
    /// Its defining property is that a malformed submission comes back as a
    /// typed, field-level [`FrontendEvent::ProposalRejected`] *while the
    /// worker is still running*, so it can fix and retry — rather than
    /// failing a transcript scrape at Stop, when nothing can be done about it.
    ///
    /// **Attribution is verified identity.** The engine derives which
    /// execution is proposing from the socket peer's pid (walked up to a
    /// registered worker run), never from a field on this request.
    /// `run_id` — the caller's own `BOSS_RUN_ID`, i.e. `work_executions.id` —
    /// is a **cross-check, not a credential**: the engine rejects when it
    /// disagrees with the peer-resolved run, which catches a misconfigured
    /// env or a command copy-pasted between worker panes. A connection with
    /// no local peer pid (a remote SSH worker) is rejected outright in v1.
    ///
    /// **Idempotent** on `(execution_id, idempotency_key)`: a resubmission
    /// returns the existing row with `already_submitted: true` rather than
    /// erroring or duplicating, so a retried command and a resumed run are
    /// both safe. Omit `idempotency_key` and the engine derives one from the
    /// execution id, kind, and a content hash of the canonicalised payload.
    ///
    /// Rate-capped per execution (32 total, 8 per kind) — runaway-loop
    /// protection, not scarcity. Replies with
    /// [`FrontendEvent::ProposalSubmitted`] or
    /// [`FrontendEvent::ProposalRejected`].
    SubmitProposal {
        run_id: String,
        kind: ProposalKind,
        /// The kind-matched payload object (see the per-kind payload structs
        /// in `boss_protocol::types::proposal`). Carried as raw JSON rather
        /// than a typed enum so validation happens in the engine, where a
        /// missing/misspelled/mistyped key becomes a per-field complaint
        /// instead of one positional serde error at the wire boundary.
        payload: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },

    Subscribe {
        topics: Vec<String>,
    },

    /// Trigger an immediate reconcile pass for a single product's external
    /// tracker binding. Runs the same per-product logic as the periodic
    /// loop but synchronously in response to an explicit request. Useful
    /// for `boss product sync-external-tracker <selector>`. Replies with
    /// [`FrontendEvent::ExternalTrackerSyncStarted`] when the pass starts,
    /// or [`FrontendEvent::WorkError`] if the product has no binding.
    SyncProductExternalTracker {
        product_id: String,
    },

    /// Tail the most recent transcript chunk for `run_id`. `run_id`
    /// may be either an `exec_*` (execution) or `run_*` (work_runs)
    /// id — `bossctl agents transcript` passes the execution id (the
    /// alias the live registry uses); programmatic callers may pass
    /// the work_runs id.
    ///
    /// The engine resolves the transcript path via the dispatcher's
    /// in-memory cache, falling back to either DB namespace, and
    /// returns the trailing `lines` lines (raw JSONL — the caller
    /// decides how to render).
    ///
    /// Error shapes (all `WorkError`, distinguishable by message
    /// prefix so callers can branch):
    /// - `transcript not yet available for run <id>: …` — the run is
    ///   known and live, but no hook has carried a `transcript_path`
    ///   yet. Transient; retry in a few seconds. (Use this prefix to
    ///   distinguish a still-buffering live worker from a genuinely
    ///   unknown id — pre-fix the engine reported both as `unknown
    ///   run`, which masked the live-vs-stale distinction.)
    /// - `run <id> has no transcript path recorded` — the run/execution
    ///   is known but terminal and never persisted a transcript path.
    /// - `unknown run: <id>` — no live entry, no DB row matches.
    TailRunTranscript {
        run_id: String,
        lines: usize,
    },

    /// Re-fire the automated review pipeline for a PR on demand —
    /// `bossctl review start --pr <n>`. Enqueues a fresh `pr_review`
    /// execution against the work item bound to `pr_number`, the same
    /// dispatch path the engine's dead-review auto-recovery sweep uses
    /// (see `pr_review_recovery`). Useful for post-hoc review after an
    /// incident (a prior reviewer died without producing findings) or a
    /// deliberate re-review after significant new commits.
    ///
    /// `repo` disambiguates when the same PR number exists in more than
    /// one repo (mirrors `FindWorkItemsByPr`'s ambiguity handling).
    ///
    /// Replies with [`FrontendEvent::PrReviewTriggered`] on success, or
    /// `WorkError` when: no work item is bound to the PR (or the match
    /// is ambiguous and `repo` doesn't narrow it to one), the item has
    /// no open PR, the item is terminal, or a live execution already
    /// claims the item.
    TriggerPrReview {
        pr_number: i64,
        #[serde(default)]
        repo: Option<String>,
    },

    /// Store the Trunk org API token, provisioned by `boss engine trunk
    /// set-token` (read from stdin/prompt on the CLI side — never argv).
    /// Persisted to the OS keychain; never logged, never written to the
    /// DB or repo. Replies with [`FrontendEvent::TrunkStatus`] reflecting
    /// the newly stored token.
    TrunkSetToken {
        token: String,
    },

    /// Report whether a Trunk org API token is configured (env override or
    /// keychain). Once a `trunk_queue`-mechanism product exists, the reply
    /// will also carry a live `getQueue` smoke-check result in
    /// `queue_check`; until then that field is always unset. Replies with
    /// [`FrontendEvent::TrunkStatus`].
    TrunkStatus,

    /// Remove the external-tracker binding from a work item. Clears the
    /// `external_ref_*` columns without touching other fields. Replies with
    /// [`FrontendEvent::WorkItemUpdated`] carrying the updated row, or
    /// [`FrontendEvent::WorkError`] if the work item is not found.
    UnlinkWorkItemExternalRef {
        work_item_id: String,
    },

    /// Undo an auto-populate batch (design §"Undo / rollback"):
    /// soft-deletes every task tagged with `run_id`'s `planner_runs.id`
    /// that has no execution yet (i.e. was never released and
    /// dispatched), and deletes the `planner_runs` row itself — clearing
    /// the per-project idempotency gate so a corrected re-plan can run.
    /// Tasks that already have an execution are preserved, not deleted,
    /// and reported back so the operator decides what to do with them.
    /// Replies with [`FrontendEvent::UnpopulateProjectResult`].
    UnpopulateProject {
        project_id: String,
        /// The `planner_runs.id` (`run_…`) to undo.
        run_id: String,
    },

    Unsubscribe {
        topics: Vec<String>,
    },

    /// Apply an `AutomationPatch` to an automation. `None` fields are
    /// left unchanged. Replies with [`FrontendEvent::AutomationUpdated`].
    UpdateAutomation {
        id: String,
        patch: AutomationPatch,
    },

    /// App reports the real shell pid for a worker pane after the
    /// libghostty surface initializes. The engine stores this in
    /// `WorkerRegistry` and `LiveWorkerStateRegistry` so process
    /// tracking, dead-pid sweep, and `bossctl agents stop` work for
    /// reviewer and other shell_pid-0 spawns. Fire-and-forget; no
    /// response expected.
    UpdateWorkerShellPid {
        run_id: String,
        shell_pid: i32,
    },

    UpdateWorkItem {
        id: String,
        patch: WorkItemPatch,
    },

    /// App reports that a worker pane died before the engine could
    /// observe it any other way — either `ghostty_surface_new` returned
    /// NULL (surface never attached) or the pane's child process exited
    /// and the app has no restart-on-exit handler wired up for worker
    /// panes (only the Boss pane restarts itself). Without this, the
    /// engine only learns of the dead pane on the next 60s
    /// `dead_pid_sweep` pass or on app restart. The engine
    /// reaps the backing execution immediately using the same DB/pool
    /// effects as the periodic sweep, skipping its grace period and
    /// PID-liveness probe since the app's report is a direct
    /// observation, not a speculative signal. Fire-and-forget; no
    /// response expected.
    WorkerPaneDied {
        run_id: String,
    },

    /// Snapshot every engine worker pool's (main, automation, review)
    /// own claimed-slot bookkeeping — capacity, idle count, and each
    /// held slot's `worker_id → execution_id` mapping — cross-referenced
    /// against `LiveWorkerStateRegistry` so a claim with no backing live
    /// worker is flagged. Read-only. This is the diagnostic surface for
    /// "pool reports N/M busy but `bossctl agents list` shows fewer live
    /// workers": before this existed, diagnosing a leaked claim required
    /// manually diffing `agents list` against `pool_capacity` from
    /// `dispatch.jsonl` rejections.
    WorkerPoolSummary,

    /// Snapshot the cube workspace pool. Proxies to
    /// `cube --json workspace list`; the engine adds no editorial — the
    /// returned vector mirrors cube's view, optionally annotated with
    /// the engine's own knowledge of which leases back which executions.
    WorkspacePoolSummary,
}

mod events;
pub use events::FrontendEvent;

// Feature-flag / setting snapshots and workspace / worker pool summary
// rows live in the `snapshot_wire` submodule (extracted to keep this file
// under the size limit) and are re-exported so external
// `boss_protocol::{..}` paths are unchanged.
mod snapshot_wire;
pub use snapshot_wire::{
    FeatureFlagSnapshot, SettingSnapshot, WorkerPoolClaimEntry, WorkerPoolEntry, WorkspacePoolEntry,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TopicEventPayload {
    WorkInvalidated {
        reason: String,
        product_id: Option<String>,
        item_ids: Vec<String>,
    },
    ExecutionInvalidated {
        reason: String,
        execution_id: String,
        work_item_id: String,
        status: String,
    },
    /// An editorial hook decision was recorded for a product's execution.
    /// Emitted to the `"editorial_actions.<product_id>"` topic so the UI
    /// can badge product cards when actions accumulate.
    WorkEditorialAction { action: EditorialAction },
    /// The engine dropped one or more pending invalidations for this
    /// session's outbound queue while riding out a publish burst (a
    /// merge-poller sweep across many products/executions, for example)
    /// rather than disconnecting a client that was draining fine, just not
    /// fast enough for an instant. The app should treat this the same way
    /// it treats reconnecting — refetch its subscribed topics — without
    /// tearing down or reporting the connection as lost, since it never
    /// was.
    ResyncRequired,
}

#[cfg(test)]
mod editorial_controls_tests;

#[cfg(test)]
mod frontend_event_wire_tests;

#[cfg(test)]
mod feature_flags_wire_tests;

#[cfg(test)]
mod topic_and_envelope_tests;

#[cfg(test)]
mod sorted_request_variants_test;
