//! Tasks: the status and kind vocabulary, the `Task` row itself, its
//! runtime projection, and the inputs used to create tasks and chores.

use super::attention::BlockedSignal;
use super::common::{
    EffortLevel, default_human_actor, default_priority, default_true, default_unknown_created_via, is_false,
    short_id_label,
};
use super::execution::ExecutionStatus;
use super::project::ProjectDesignDocState;
use super::work_item::WorkItemExternalRef;
use serde::{Deserialize, Serialize};

/// Discriminator for the `tasks.status` column. Exhaustive match enforces
/// that every callsite handles new variants explicitly — adding a new status
/// here produces a compile error at every status-keyed branch that must
/// reason about it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Todo,
    Active,
    Blocked,
    InReview,
    Done,
    Archived,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::InReview => "in_review",
            Self::Done => "done",
            Self::Archived => "archived",
            Self::Cancelled => "cancelled",
        }
    }

    /// True for statuses that represent terminal/closed states where no further
    /// engine action is expected.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Archived | Self::Cancelled)
    }

    /// True for statuses that represent work in progress (engine-owned dispatch
    /// slot is live or a PR is open awaiting review).
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Active | Self::InReview)
    }

    /// Board (UI) display label — the human-facing name shown on the kanban
    /// board. Distinct from [`as_str`] for the three statuses whose UI name
    /// differs from the stored name (`todo` → `backlog`, `active` → `doing`,
    /// `in_review` → `review`). Use this method at CLI display boundaries;
    /// use [`as_str`] for DB writes and wire comparisons.
    pub fn display_label(&self) -> &'static str {
        match self {
            Self::Todo => "backlog",
            Self::Active => "doing",
            Self::Blocked => "blocked",
            Self::InReview => "review",
            Self::Done => "done",
            Self::Archived => "archived",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "todo" => Ok(Self::Todo),
            "active" => Ok(Self::Active),
            "blocked" => Ok(Self::Blocked),
            "in_review" => Ok(Self::InReview),
            "done" => Ok(Self::Done),
            "archived" => Ok(Self::Archived),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(format!(
                "unknown task status: `{other}`; expected one of: \
                 todo, active, blocked, in_review, done, archived, cancelled"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateChoreInput {
    pub product_id: String,
    /// When `false`, the engine creates the chore in `todo` but does
    /// NOT spin up a `ready` execution for the auto-dispatcher to pick
    /// up. The chore stays parked until something explicitly schedules
    /// it (`bossctl work start <id>` or a kanban drag-to-Doing). Older
    /// clients that omit this field get the historical behavior
    /// (`autostart = true`).
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,

    /// See [`CreateTaskInput::force_duplicate`].
    #[serde(default)]
    #[builder(default)]
    pub force_duplicate: bool,

    /// Canonical ids of work items this chore must wait on. Each id
    /// becomes a `blocks` prerequisite edge declared **atomically with
    /// the row insert**, so the chore is born `blocked` (and never
    /// dispatched) while any prerequisite is unsatisfied. This closes
    /// the create→`depend add` race: there is no window where the chore
    /// autostarts before its gate exists. The caller (CLI) is
    /// responsible for resolving selectors (`T42`) to canonical ids
    /// before sending — mirrors [`AddDependencyInput`]. Cross-product
    /// edges and cycles are rejected at insert time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub depends_on: Vec<String>,

    pub name: String,
    /// See `CreateTaskInput::created_via`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    pub description: Option<String>,
    /// See [`CreateTaskInput::effort_level`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    /// See [`CreateTaskInput::model_override`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    /// See [`CreateTaskInput::driver`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,

    /// One of `low` / `medium` / `high`. Omitted → engine default
    /// (`medium`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,

    /// Per-work-item repo override. `None` → the chore inherits from
    /// its product. Canonical remote URL form (engine canonicalises
    /// caller-supplied URLs at write time). A bare registered cube repo
    /// slug (e.g. `bduff`) is also accepted and resolved to its origin
    /// URL at write time so the stored row is always dispatchable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,

    /// Optional kind override. `None` → `'chore'` (the historical default).
    /// Pass `TaskKind::Followup` when creating a review-finding follow-up
    /// task; the engine uses this to write the correct `kind` column and
    /// to store provenance in `origin_task_short_id` / `origin_pr_number`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind_override: Option<TaskKind>,

    /// Short id of the task whose PR-review produced this follow-up.
    /// Set only when `kind_override = Followup`; ignored for plain chores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_task_short_id: Option<i64>,

    /// GitHub PR number that was under review when the findings were filed.
    /// Set only when `kind_override = Followup`; ignored for plain chores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_pr_number: Option<i64>,
}

/// Input for `boss task create --kind investigation`. Parallel to
/// [`CreateChoreInput`] but adds `project_id` (investigation tasks
/// are product-level work items optionally scoped to a project) and
/// uses `kind = 'investigation'` on insert.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateInvestigationInput {
    pub product_id: String,
    /// See [`CreateChoreInput::autostart`].
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,

    #[serde(default)]
    #[builder(default)]
    pub force_duplicate: bool,

    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    /// See [`CreateTaskInput::driver`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,

    /// Optional project scope. When set, the investigation appears
    /// under the project on the kanban. `None` → product-level only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,

    /// Per-task repo override for the investigation deliverable. `None`
    /// → resolve from product `docs_repo`, then `BOSS_USER_DOCS_REPO`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}

/// Batch counterpart of [`CreateChoreInput`]. See
/// [`CreateManyTasksInput`] for atomicity / event semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateManyChoresInput {
    pub items: Vec<CreateChoreInput>,
}

/// Batch counterpart of [`CreateTaskInput`]. Items are fully resolved
/// inputs — the CLI merges any top-level `--product` / `--project` /
/// `--no-autostart` defaults into each entry before sending. The
/// engine inserts every item in one sqlite transaction and emits one
/// `WorkItemsCreated` response carrying the full list. On any
/// per-item validation failure the entire transaction is rolled back
/// — there is no partial state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateManyTasksInput {
    pub items: Vec<CreateTaskInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateTaskInput {
    pub product_id: String,
    pub project_id: String,
    /// See `CreateChoreInput::autostart`. Project tasks honour the
    /// same flag, but the kanban already serialises them via
    /// `waiting_dependency` so only the first incomplete task is ever
    /// `ready`. Defaults to `true`.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,

    /// Bypass the recent-duplicate guard. When `true`, the engine skips
    /// the 60-second same-name/same-product duplicate check and inserts
    /// a second row unconditionally. Intended as a CLI escape hatch for
    /// operators who explicitly want a second task with the same name.
    #[serde(default)]
    #[builder(default)]
    pub force_duplicate: bool,

    /// Canonical ids of work items this task must wait on. See
    /// [`CreateChoreInput::depends_on`] — same atomic-gate semantics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub depends_on: Vec<String>,

    pub name: String,
    /// Surface that filed this task — `cli`, `bossctl`, `mac_app`,
    /// `engine_auto`. Documented callers always set it explicitly;
    /// when omitted, the engine falls back to a transport-layer hint
    /// so the row is never silently labeled `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    pub description: Option<String>,
    /// Effort estimate. `None` → leave NULL on the row; dispatcher
    /// falls through to product / engine default per design §Q3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    /// Explicit model slug override. `None` → no override; dispatcher
    /// resolves per design §Q3 precedence. Stored verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    /// Explicit driver override. `None` → resolve via
    /// `product.default_driver` → `"claude"`. Stored verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,

    /// One of `low` / `medium` / `high`. Omitted → engine default
    /// (`medium`), which is the right answer for the vast majority
    /// of tasks; only callers who care should set this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,

    /// Per-work-item repo override. `None` → the task inherits from
    /// its product. Canonical remote URL form (engine canonicalises
    /// caller-supplied URLs at write time). A bare registered cube repo
    /// slug (e.g. `bduff`) is also accepted and resolved to its origin
    /// URL at write time so the stored row is always dispatchable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}

/// Discriminator for the `tasks.kind` column.  Exhaustive match
/// enforces that every callsite handles new variants explicitly —
/// adding a new kind here produces a compile error at every kind-keyed
/// branch that must reason about it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Chore,
    Design,
    /// A review-finding follow-up task created when a PR-review revision's
    /// parent PR merges before all findings are addressed. Behaviorally
    /// identical to `Chore` for dispatch/execution purposes; distinct for
    /// UI rendering and provenance tracking.
    Followup,
    Investigation,
    ProjectTask,
    Revision,
    Task,
}

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chore => "chore",
            Self::Design => "design",
            Self::Followup => "followup",
            Self::Investigation => "investigation",
            Self::ProjectTask => "project_task",
            Self::Revision => "revision",
            Self::Task => "task",
        }
    }
}

impl std::fmt::Display for TaskKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TaskKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "chore" => Ok(Self::Chore),
            "design" => Ok(Self::Design),
            "followup" => Ok(Self::Followup),
            "investigation" => Ok(Self::Investigation),
            "project_task" => Ok(Self::ProjectTask),
            "revision" => Ok(Self::Revision),
            "task" => Ok(Self::Task),
            other => Err(format!(
                "unknown task kind: `{other}`; expected one of: \
                 chore, design, followup, investigation, project_task, revision, task"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct Task {
    pub id: String,
    /// Per-product short id allocated at insert time. Always `Some` after the
    /// schema migration runs; `None` only on rows predating it (which the
    /// migration backfills, so in practice this is never `None` at runtime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    /// When `false`, the engine's auto-dispatcher will not turn this
    /// work item into a `ready` execution while it sits in `todo`.
    /// Existing rows from before this column was introduced default
    /// to `true` so legacy callers keep their old auto-start behavior.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,

    /// Every active block reason currently in flight on this work
    /// item — the multi-signal companion to the scalar
    /// `blocked_reason` cache. Mirrors the `task_blocked_signals`
    /// side table. Empty when the row is not blocked. The scalar
    /// `blocked_reason` / `blocked_attempt_id` fields above remain the
    /// denormalised "primary reason" cache for UI rendering and resolve
    /// to the highest-priority entry in this list per the design's
    /// §Q2 priority order. Existing rows from before this column was
    /// introduced default to an empty list.
    #[serde(default)]
    #[builder(default)]
    pub blocked_signals: Vec<BlockedSignal>,

    /// Number of CI fix attempts the engine has already consumed for
    /// the current cycle. Reset to 0 when the parent transitions back
    /// to `in_review` after a successful auto-fix (or when the user
    /// runs `boss engine ci retry`). Only `attempt_kind = 'fix'`
    /// attempts that progressed past the worker's go/no-go decision
    /// count. Existing rows from before this column was introduced
    /// default to 0.
    #[serde(default)]
    #[builder(default)]
    pub ci_attempts_used: i64,

    pub created_at: String,
    /// The surface that filed this row — `cli`, `bossctl`, `mac_app`,
    /// `engine_auto`, or `unknown`. Stamped at insert time and never
    /// rewritten. `unknown` only appears on rows that predate this
    /// column (the migration default); fresh writes always carry one
    /// of the other values.
    #[serde(default = "default_unknown_created_via")]
    #[builder(default = default_unknown_created_via())]
    pub created_via: String,

    pub description: String,
    /// `true` when any descendant revision task in the chain has status
    /// `todo` or `active` — new commits are still incoming, so the PR is
    /// not safe to merge yet. Derived projection, not stored. Only
    /// meaningful on chain-root tasks that carry a `pr_url`.
    #[serde(default, skip_serializing_if = "is_false")]
    #[builder(default)]
    pub has_in_progress_revision: bool,

    pub kind: TaskKind,
    /// Who made the most recent status change — `'human'`, `'boss'`,
    /// `'engine'`, or `'boothby'`. See `Project.last_status_actor` for
    /// full semantics and [`StatusActor`] for the parsed vocabulary.
    #[serde(default = "default_human_actor")]
    #[builder(default = default_human_actor())]
    pub last_status_actor: String,

    pub name: String,
    /// One of `low` / `medium` / `high`. Mirrors `Project.priority`
    /// exactly so kanban surfaces can render every work-item kind with
    /// the same vocabulary. Existing rows from before this column was
    /// introduced default to `medium`.
    #[serde(default = "default_priority")]
    #[builder(default = default_priority())]
    pub priority: String,

    pub status: TaskStatus,
    pub updated_at: String,

    /// Human-readable reason the *engine* (never a human) transitioned this
    /// row to `status = 'archived'`, e.g. `"parent PR merged: revision moot
    /// (created_via=merge-conflict:crz_123)"` or `"parent PR merged:
    /// superseded by chore task_456"`. Set only by the revision-chain
    /// reconciliation paths (`block_pending_revisions_on_parent_close`,
    /// `reconcile_revision_execution`'s dispatch-time catch-up gate) so an
    /// operator running `boss task show` can see *why* a row disappeared
    /// from the board instead of reconstructing it from engine logs.
    /// `None` for manually archived rows and for every non-archived status;
    /// cleared whenever the row leaves `archived` via any path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_reason: Option<String>,

    /// Soft FK to the attempt row currently trying to clear the block —
    /// `conflict_resolutions.id` when `blocked_reason = 'merge_conflict'`,
    /// the review-iteration table's id when `blocked_reason = 'review_feedback'`,
    /// etc. `None` for `'dependency'` (the prereqs are queried via
    /// `work_item_dependencies` instead) and for any block without an
    /// engine-managed attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_attempt_id: Option<String>,

    /// When `status = 'blocked'`, an open-ended discriminator
    /// explaining *why*. Documented values: `'dependency'` (gated by a
    /// `work_item_dependencies` prereq), `'merge_conflict'` (an
    /// `in_review` PR's branch conflicts with `main`), `'review_feedback'`
    /// (a reviewer requested changes), `'ci_failure'` / `'ci_failure_exhausted'`
    /// (CI on the PR went red). `None` for non-`blocked` rows and for
    /// legacy `blocked` rows whose reason wasn't tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,

    /// Per-PR override of the CI auto-fix attempt budget. `None` →
    /// inherit the product default (`products.ci_attempt_budget`,
    /// default 3). `Some(0)` means "notify only" (no auto-fix on this
    /// PR). See `merge-conflict-handling-in-review.md` §Q3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_attempt_budget: Option<i64>,

    /// Structured detail for the CI indicator tooltip. JSON-encoded list of
    /// objects with `name` and `conclusion` keys, one per failing required
    /// check. `None` when `ci_required_state` is not `"fail"` or when no
    /// detail is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_required_detail: Option<String>,

    /// Aggregate state of required CI checks at last poll. Three terminal
    /// values: `"in_progress"` (at least one required check is still
    /// running), `"success"` (all required checks passed), `"fail"` (at
    /// least one required check failed). `"unknown"` means the repo has no
    /// branch protection or the first poll hasn't run yet. `None` until the
    /// merge poller has performed at least one successful probe. Only
    /// meaningful when `status = "in_review"` and `pr_url` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_required_state: Option<String>,

    /// Unix epoch seconds (decimal string) at which this task last
    /// transitioned into a terminal status (`done`, `archived`, or
    /// `cancelled`). Set once on transition; cleared when the task is
    /// re-opened. `None` for non-terminal rows. Existing terminal rows
    /// are backfilled with `created_at` by `migrate_tasks_completed_at`
    /// (NOT `updated_at`, which is re-stamped by any mutation).
    ///
    /// The kanban Done-lane date bucketing groups by this field so a
    /// bulk mutation that re-stamps `updated_at` on many done rows does
    /// not mis-count them as completed today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,

    pub deleted_at: Option<String>,
    /// Effort estimate for the work item. `None` means "no level set;
    /// dispatcher falls through to product / engine default per design
    /// §Q3." Set by the coordinator's heuristic at creation, or by an
    /// explicit `--effort` flag on `boss task/chore create|edit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    /// Stable pointer to the upstream tracker issue linked to this work item.
    /// `None` when no external tracker binding exists. Populated by the
    /// reconciler on import or manual link; cleared (with `unbound_at` set)
    /// when the upstream item leaves the product's configured scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<WorkItemExternalRef>,

    /// Merge-queue / auto-merge state at last poll. `Some("queued")` when
    /// the PR is currently in GitHub's merge queue; `Some("auto_merge_enabled")`
    /// when GitHub auto-merge is armed (the "Merge When Ready" request is
    /// still waiting on required checks, or the repo has no merge queue at
    /// all) but the PR hasn't reached a queue; `None` when neither. Either
    /// `Some` value moves the task into the macOS kanban's "Merging"
    /// section (rendered above "Today" in the Done column) — the task's
    /// own `status` stays `"in_review"` the whole time, so a drop-out (the
    /// PR leaves the queue/auto-merge without merging) needs no special
    /// transition: this field just reverts to `None` on the next poll and
    /// the task falls back to rendering under Review. It is also forced to
    /// `None` (regardless of what GitHub still reports) whenever the same
    /// poll observes `ci_required_state == "fail"` while this field was
    /// `Some("auto_merge_enabled")` (mono#2023 / T2675): GitHub keeps
    /// auto-merge armed on red required checks — that's what "merge when
    /// ready" means — so without this override a card can sit in Merging
    /// with a simultaneous red CI chip. This override is deliberately
    /// scoped to the `auto_merge_enabled` bucket only — a `Some("queued")`
    /// row is left untouched by a failing check, per the invariant
    /// documented on `renumber_merge_queue`. GitHub's own arming is left
    /// untouched; the demotion is purely this field, and it lifts
    /// automatically on the next poll once CI reads
    /// `"success"`/`"in_progress"` again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_state: Option<String>,

    /// Structured sub-state for the merge-queue/auto-merge indicator,
    /// JSON-encoded as `{"position": <i64>, "state": "<GitHub
    /// mergeQueueEntry.state>", "enqueued_at": "<RFC3339>", "section_order":
    /// <i64>}`. `Some` whenever `merge_queue_state` is `Some`; cleared
    /// (`None`) the moment the probe no longer sees a `mergeQueueEntry` or
    /// an armed `autoMergeRequest` for the PR (merged, or dropped out), or
    /// whenever `merge_queue_state` is cleared by the `auto_merge_enabled`
    /// CI-fail override described on that field's doc comment.
    /// `state` is GitHub's raw `mergeQueueEntry.state` enum value
    /// (`AWAITING_CHECKS`, `MERGEABLE`, `LOCKED`, `QUEUED`, `UNMERGEABLE`) —
    /// `None` while `merge_queue_state == Some("auto_merge_enabled")`, since
    /// that state has no queue entry. The client maps `state` to display
    /// text; `position` and `enqueued_at` are omitted from the JSON when
    /// GitHub didn't report them. `section_order` is the engine-computed
    /// sort key for the Merging section — ascending order matches the real
    /// merge-queue order, with `auto_merge_enabled` cards (no queue
    /// position) always sorting below every queued card; the client
    /// renders it as-is rather than deriving its own ordering rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_detail: Option<String>,

    /// Explicit model slug override. `None` → resolve via the design's
    /// Q3 precedence (effort default → product default → engine default).
    /// Stored verbatim — the engine does not validate the slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    /// Explicit agent driver override. `None` → resolve via
    /// `product.default_driver` → engine default (`"claude"`).
    /// Stored verbatim (design §Mix-and-match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,

    pub ordinal: Option<i64>,
    /// Soft FK to the `tasks.id` whose PR this revision targets. `None`
    /// for every non-`revision` row. Required (app-enforced) when
    /// `kind = 'revision'`; never set by `ALTER TABLE … ADD COLUMN`
    /// backfill, so pre-revision rows carry `NULL` as expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,

    /// RFC 3339 timestamp of the most recent successful poll that wrote
    /// `ci_required_state` / `review_required_state`. `None` until the first
    /// probe completes. The UI uses this to render "last checked: N ago".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_state_polled_at: Option<String>,

    pub pr_url: Option<String>,
    pub project_id: Option<String>,
    /// Per-work-item repo override. `None` → inherit from the parent
    /// `Product.repo_remote_url`. Stored as a canonical remote URL
    /// (e.g. `git@github.com:myorg/repo.git` or
    /// `https://github.com/myorg/repo.git`); short-name display is
    /// derived on the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,

    /// Reviewer names for the review indicator tooltip. JSON-encoded list of
    /// login strings. For `"approved"`: the approving reviewers. For
    /// `"changes_requested"`: the requesting reviewers. `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_required_detail: Option<String>,

    /// State of required-review gating at last poll. Values:
    /// `"required"` (awaiting at least one required review),
    /// `"approved"` (all required reviews approved),
    /// `"changes_requested"` (at least one reviewer requested changes),
    /// `"unknown"` (review state could not be determined). `None` until the
    /// merge poller has performed at least one successful probe. Only
    /// meaningful when `status = "in_review"` and `pr_url` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_required_state: Option<String>,

    /// How many automated reviewer passes have completed for this PR.
    /// Starts at 0; incremented after each `pr_review` execution finishes.
    /// The engine skips enqueueing a new reviewer pass when this value
    /// reaches `max_review_cycles` (config knob; default 3). Only
    /// meaningful on tasks that carry a `pr_url`. P992 design §7.
    #[serde(default)]
    #[builder(default)]
    pub review_cycle: i64,

    /// HEAD SHA of the PR at the time the most recent reviewer pass
    /// completed. Used by the no-op skip gate (P992 design §8, task 10)
    /// to detect pure rebases and skip re-review when nothing meaningful
    /// changed since the last pass. `None` until at least one reviewer
    /// pass has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reviewed_sha: Option<String>,

    /// Denormalised PR URL of the chain-root task for fast revision-card
    /// rendering. `None` for non-revision rows and for revisions whose chain
    /// root has no PR yet (rare — the create gate normally blocks that).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_parent_pr_url: Option<String>,

    /// Engine-computed R-number for revision tasks. 1-based, chain-root-scoped,
    /// creation-ordered: the N-th revision filed against a given chain root
    /// gets `revision_seq = N`. `None` for every non-`revision` row. This is
    /// a derived projection — not a stored column — computed fresh on every
    /// `get_work_tree` call so deletions and soft-deletes stay consistent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_seq: Option<i64>,

    /// FK to the `automations.id` that produced this task via the triage
    /// phase. `None` for every task not produced by an automation. When set:
    /// (1) links the task back to its automation for per-automation task
    /// listing, (2) drives backlog/kanban exclusion, (3) routes the
    /// execution to the automations pool, (4) is the denominator for the
    /// automation's open-task limit. `None` on all pre-automation rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_automation_id: Option<String>,

    /// `true` while an independent `pr_review` reviewer execution is
    /// running for this task. The task is held in the Doing column
    /// (`status = "active"`) until the reviewer finalises (or a timeout
    /// forces the advance). Surfaces as a "Reviewing (AI)" badge on the
    /// kanban card so the user can tell the hold is intentional.
    ///
    /// This is a derived projection set by the engine's `get_work_tree`
    /// path (not a stored DB column). It is always `false` for tasks
    /// not currently undergoing an AI review pass.
    #[serde(default, skip_serializing_if = "is_false")]
    #[builder(default)]
    pub ai_reviewing: bool,

    /// Resolved doc-link state for a **project-less** docs-backed work
    /// item — chiefly `kind = 'investigation'`. Parity with the design
    /// card's doc-link icon, which is resolved from the parent
    /// *project's* `design_doc_*` columns; an investigation has no
    /// project, so the engine resolves the task's own `doc_*` columns
    /// (populated by the doc detector from the PR's changed files) into
    /// the same `ProjectDesignDocState` the kanban already renders.
    ///
    /// This is a derived projection set by the engine's `get_work_tree`
    /// path (not a stored DB column, and never set for design tasks —
    /// those keep using the per-project resolution path). `None` when
    /// the item has no per-task pointer (the common case), which hides
    /// the affordance exactly like `ProjectDesignDocState::NotSet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_link_state: Option<ProjectDesignDocState>,

    /// Per-product short id of the task whose PR-review produced this
    /// follow-up. Set only on `kind = 'followup'` rows created by
    /// `block_pending_revisions_on_parent_close` when the reviewed PR
    /// merges before all findings are addressed. `None` for every other
    /// task kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_task_short_id: Option<i64>,

    /// GitHub PR number that was under review when the findings were
    /// filed. Set only on `kind = 'followup'` rows; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_pr_number: Option<i64>,

    /// Machine discriminator for a dispatch failure the engine gave up
    /// retrying — e.g. `"cube_workspace_lease_failed"`, matching the
    /// `WorkAttentionItem.kind` raised for the same failure. Set by
    /// `WorkDb::bounce_dispatch_failed_to_backlog` when a pre-start
    /// dispatch attempt (cube repo ensure, workspace lease, change
    /// create, run start, …) fails non-transiently; distinguishes this
    /// from a task that is merely queued behind a full worker pool
    /// (which never sets this field). `None` when there is no
    /// unresolved dispatch failure. Cleared the next time a fresh
    /// dispatch is requested for this work item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_failed_reason: Option<String>,

    /// Human-readable error text for `dispatch_failed_reason` (e.g. the
    /// underlying cube lease error message), rendered directly on the
    /// kanban card so the operator can see why dispatch is stuck
    /// without digging into dispatch logs. `None` whenever
    /// `dispatch_failed_reason` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_failed_error: Option<String>,

    /// RFC 3339 timestamp of the dispatch failure recorded in
    /// `dispatch_failed_reason`. `None` whenever that field is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_failed_at: Option<String>,
}

impl Task {
    /// Operator-facing short form (`"T2344"`) for embedding in human-readable
    /// text — never the canonical `task_*` id, which is an internal
    /// implementation detail. Falls back to a generic, non-identifying label
    /// on the (in-practice-unreachable) `short_id = None` case rather than
    /// leaking the canonical id.
    pub fn short_label(&self) -> String {
        short_id_label(self.short_id).unwrap_or_else(|| "a task".to_owned())
    }
}

/// Live runtime status for a single task/chore — the current execution
/// and most recent run, summarized for the kanban view. `None` fields
/// mean no execution (or no run) exists yet for the work item.
///
/// `execution_id` is the active or most recent execution row; the
/// engine uses the same value as `run_id` when registering live
/// worker state, so UI consumers can join `task → execution_id →
/// LiveWorkerState`. `current_run_id` is the latest `work_runs` row
/// attached to that execution (`None` until the dispatch loop has
/// progressed past the cube-workspace-lease stage and called
/// `start_execution_run`).
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct TaskRuntime {
    pub work_item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,

    pub execution_status: Option<ExecutionStatus>,
    pub run_status: Option<String>,

    /// Unix epoch seconds (decimal string), set only while the current
    /// execution is `ready` but withheld from dispatch by the engine's
    /// in-process backoff after a pre-spawn dispatch failure. `None`
    /// otherwise — including the ordinary "no failure yet, no free slot"
    /// queue wait, which is a genuinely different state. Distinguishes
    /// "dispatch is retrying after a failure" from "waiting for a slot"
    /// so the kanban card doesn't render the misleading "Waiting for a
    /// slot" label during the retry backoff window. The *post*-give-up
    /// bounced state was already correctly labeled (clears `autostart`,
    /// surfaces `dispatch_failed_reason` instead) before this field
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_retry_at: Option<String>,

    /// The dispatcher's current defer reason for this `ready` execution
    /// (`chain_serialized`, `pool_exhausted`, ...) — mirrors
    /// `WorkExecution::dispatch_wait_reason`. `None` when the execution
    /// isn't currently deferred (never attempted yet, or already claimed
    /// a slot). Distinct from [`Self::dispatch_retry_at`], which is the
    /// post-failure in-process backoff window, not a capacity/serialization
    /// wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_wait_reason: Option<String>,

    /// Unix epoch seconds (as a string) since `dispatch_wait_reason` took
    /// its current value. `None` whenever `dispatch_wait_reason` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_wait_since: Option<String>,
}
