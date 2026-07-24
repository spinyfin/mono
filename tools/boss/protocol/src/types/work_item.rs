//! The `WorkItem` sum type over tasks and projects, its patch and
//! external-ref types, and worktree metadata.

use super::dependency::WorkItemDependency;
use super::product::Product;
use super::project::Project;
use super::task::{Task, TaskRuntime};
use serde::{Deserialize, Serialize};

/// Input to `LinkWorkItemExternalRef`: manually bind a work item to a
/// specific upstream issue. The engine stores `kind`/`canonical_id` in
/// the `tasks.external_ref_*` columns so the reconciler can start
/// mirroring state for the row on its next tick. The `raw` blob and
/// `web_url` are populated by the engine from the tracker's
/// `fetch_item` response; the caller does not supply them here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkExternalRefInput {
    /// Stable tracker-specific id (`"spinyfin/mono#560"` for GitHub).
    pub canonical_id: String,

    pub work_item_id: String,
    /// Tracker discriminator matching `products.external_tracker_kind`
    /// for the work item's product.
    pub kind: String,
}

/// One work item bound to a given PR number, together with the
/// revisions in that PR's chain. Returned by
/// [`crate::wire::FrontendRequest::FindWorkItemsByPr`].
///
/// `owner` is the row that owns the `pr_url` — the chain root, which
/// may be any kind (`project_task`, `chore`, `design`,
/// `investigation`). `revisions` are the `kind = 'revision'`
/// descendants that committed to the same PR branch without owning a
/// `pr_url` of their own; they carry `revision_seq` /
/// `revision_parent_pr_url` projections and are ordered by sequence
/// (R1, R2, …). Empty when the owner has no revisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrWorkItemMatch {
    pub owner: Task,
    #[serde(default)]
    pub revisions: Vec<Task>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "item_type", rename_all = "snake_case")]
pub enum WorkItem {
    Product(Product),
    Project(Project),
    Task(Task),
    Chore(Task),
}

impl WorkItem {
    /// The primary id of any `WorkItem` variant, for callers that just need a
    /// string to compare/print regardless of kind.
    pub fn primary_id(&self) -> &str {
        match self {
            WorkItem::Product(p) => &p.id,
            WorkItem::Project(p) => &p.id,
            WorkItem::Task(t) | WorkItem::Chore(t) => &t.id,
        }
    }

    /// The owning product's id — the product's own id for a `Product`,
    /// otherwise the `product_id` foreign key every other variant carries.
    pub fn product_id(&self) -> &str {
        match self {
            WorkItem::Product(p) => &p.id,
            WorkItem::Project(p) => &p.product_id,
            WorkItem::Task(t) | WorkItem::Chore(t) => &t.product_id,
        }
    }
}

/// Stable upstream pointer stored on a work item that has been linked to
/// an external tracker issue. All three `kind`/`canonical_id`/`raw` fields
/// mirror the corresponding `tasks.external_ref_*` columns; `web_url` is
/// the canonical browser URL for the upstream issue (derived by the engine
/// at read time, not stored). See the external-tracker sync design.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkItemExternalRef {
    /// Stable opaque id used as the reconciler's lookup key.
    /// For GitHub: `"spinyfin/mono#560"`.
    pub canonical_id: String,

    /// Tracker discriminator (`"github"`, eventually `"jira"`, etc.).
    pub kind: String,

    /// Tracker-specific extras opaque to the engine. For GitHub: the
    /// `project_item_id` needed for status-field reads/writes.
    pub raw: serde_json::Value,

    /// Canonical browser URL for the upstream issue. Derived at read
    /// time by the engine; not stored in the DB.
    pub web_url: String,

    /// Unix-seconds string of the last successful upstream→Boss
    /// reconcile. `None` until the first reconcile completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<String>,

    /// Unix-seconds string when the binding was cleared because the
    /// upstream item disappeared from the product's configured scope.
    /// `None` while the binding is active. Retained so the reconciler
    /// can re-bind automatically if the item reappears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unbound_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkItemPatch {
    /// Flip the `autostart` flag. `None` → leave unchanged.
    /// `Some(true)` → enable auto-dispatch; `Some(false)` → disable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart: Option<bool>,

    /// Flip the `deferred` (future-scope) classification. `None` → leave
    /// unchanged. `Some(false)` approves the item (pulls it into scope so
    /// it can be dispatched); `Some(true)` re-defers it. See
    /// [`crate::Task::deferred`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<bool>,

    /// Set or clear the `blocked_reason` field. `None` → leave unchanged.
    /// `Some("")` → clear (write NULL). Any non-empty string is stored verbatim
    /// (e.g. `"merge_conflict"`, `"ci_failure"`). Manual escape hatch for
    /// clearing stale reasons the automated sweepers missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,

    /// Set or clear the `blocked_detail` field — the verbatim, untruncated
    /// long-form explanation of `blocked_reason`, rendered as a tooltip on
    /// the pill. `None` → leave unchanged. `Some("")` → clear (write NULL).
    /// The engine clears this alongside `blocked_reason` whenever the
    /// latter is cleared or the status leaves `blocked` — a detail can't
    /// outlive its label. Unlike `blocked_reason`, this field has no
    /// length limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_detail: Option<String>,

    /// Product-level default model. Only honoured on
    /// product-targeted updates; ignored when patching a task/chore/
    /// project. `None` → leave unchanged. `Some("")` → clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    /// Product-level default driver. Only honoured on product-targeted
    /// updates. `None` → leave unchanged. `Some("")` → clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_driver: Option<String>,

    pub description: Option<String>,
    /// Product-level design-task repo override. Only honoured on
    /// product-targeted updates; ignored when patching a task /
    /// chore / project. `None` → leave unchanged. `Some("")` →
    /// clear (write NULL). Stored canonicalised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_repo: Option<String>,

    /// Product-level dispatch preamble. Only honoured on
    /// product-targeted updates. `None` → leave unchanged.
    /// `Some("")` → clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_preamble: Option<String>,

    /// Product-level investigation-task ("docs") repo override. Only
    /// honoured on product-targeted updates; ignored when patching a
    /// task / chore / project. `None` → leave unchanged. `Some("")` →
    /// clear (write NULL → fall through to `BOSS_USER_DOCS_REPO`).
    /// Stored canonicalised. See [`Product::docs_repo`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_repo: Option<String>,

    /// Effort estimate to apply on this update. `None` → leave the
    /// existing column value alone. `Some("")` → clear the column
    /// (write NULL). Any other string is validated against the
    /// [`EffortLevel`] enum at the engine boundary; invalid values
    /// reject the entire patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<String>,

    pub goal: Option<String>,
    /// Model slug override. `None` → leave unchanged. `Some("")` →
    /// clear the column. Any other string is stored verbatim (no
    /// validation — `claude` is the source of truth on slugs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    /// Driver override. `None` → leave unchanged. `Some("")` → clear.
    /// Any other string is stored verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,

    pub name: Option<String>,
    pub ordinal: Option<i64>,
    pub pr_url: Option<String>,
    pub priority: Option<String>,
    pub repo_remote_url: Option<String>,
    pub status: Option<String>,
    /// Product-level worker branch-name prefix. Only honoured on
    /// product-targeted updates; ignored when patching a task / chore /
    /// project. `None` → leave unchanged. `Some("")` → clear (write
    /// NULL → engine default `boss/`). Stored canonicalised with a
    /// trailing `/`. See [`Product::worker_branch_prefix`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_branch_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkTree {
    pub chores: Vec<Task>,
    /// Every `work_item_dependencies` edge whose dependent belongs to
    /// this product. Lets the kanban resolve "blocked by <prereq>"
    /// labels (and any future dep affordance) without an N+1 round
    /// trip — clients already have every task/chore/project name.
    #[serde(default)]
    pub dependencies: Vec<WorkItemDependency>,

    pub product: Product,
    pub projects: Vec<Project>,
    #[serde(default)]
    pub task_runtimes: Vec<TaskRuntime>,

    pub tasks: Vec<Task>,
}
