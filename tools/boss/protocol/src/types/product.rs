//! Products, along with their editorial rules and external-tracker
//! configuration.

use super::common::{BranchNaming, RedactionRule, TemplatePolicy, TrailerPolicy};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateProductInput {
    pub name: String,
    pub description: Option<String>,
    /// See [`Product::design_repo`]. `None` → no override; design
    /// tasks resolve through the standard chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_repo: Option<String>,

    /// See [`Product::docs_repo`]. `None` → fall through to
    /// `BOSS_USER_DOCS_REPO` for investigation deliverables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_repo: Option<String>,

    pub repo_remote_url: Option<String>,
    /// See [`Product::worker_branch_prefix`]. `None` → the engine
    /// default `boss/`. Stored canonicalised with a trailing `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_branch_prefix: Option<String>,
}

/// One recorded enforcement action taken by the editorial-rules hook
/// against a `gh` command invocation. Stored in `editorial_actions`
/// for audit and debugging; surfaced via `ListEditorialActions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct EditorialAction {
    pub id: String,
    pub execution_id: String,
    pub product_id: String,
    /// What the hook did: `"redact"` (body was rewritten in place),
    /// `"block"` (invocation was rejected), or `"advise"` (warning
    /// prepended to the prompt but the call was allowed through).
    pub action: String,

    pub created_at: String,
    /// Human-readable explanation produced by the hook (the matched
    /// pattern name or the template section that was missing).
    pub reason: String,

    /// Verbatim command the worker attempted (e.g. `gh pr create
    /// --title "…" --body "…"`), truncated to 4 KiB for storage.
    pub tool_command: String,

    /// The PR URL the action was taken on, when known at hook time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

/// Per-product editorial rules constraining what workers write into
/// GitHub-visible surfaces.
///
/// All fields carry `#[serde(default)]` so an absent or `null` JSON
/// object deserialises to the identity value that preserves today's
/// behaviour. The `Default` impl is therefore the "no rules configured"
/// state and matches what unconfigured products experience.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EditorialRules {
    /// Branch-naming strategy for worker branches on this product.
    #[serde(default)]
    pub branch_naming: BranchNaming,

    /// Whether AI co-author trailers should be stripped from commit
    /// messages authored by workers on this product.
    #[serde(default)]
    pub commit_trailer_policy: TrailerPolicy,

    /// Ordered redaction rules applied to every `gh pr|issue
    /// {create,edit,comment}` body before the call goes through.
    /// Empty → no redaction pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redactions: Vec<RedactionRule>,

    /// How strictly `.github/PULL_REQUEST_TEMPLATE.md` conformance is
    /// enforced on PR bodies.
    #[serde(default)]
    pub template_policy: TemplatePolicy,

    /// Free-text instructions injected verbatim into the worker's
    /// initial prompt beneath a `[editorial-rules]` header. `None` →
    /// no free-text block injected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct Product {
    pub id: String,
    pub created_at: String,
    pub description: String,
    pub name: String,
    pub slug: String,
    pub status: String,
    pub updated_at: String,
    /// Per-product default model slug used when a task/chore on this
    /// product has no `model_override` set. `None` → fall through to
    /// the effort-level default / engine default (per the design's Q3
    /// precedence). Stored verbatim — the engine does not validate the
    /// slug, so a future Claude release can ship without a Boss
    /// migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    /// Per-product default agent driver. `None` → fall through to the
    /// engine default (`"claude"`). Stored verbatim — the engine does
    /// not validate the slug. Precedence: `task.driver` →
    /// `product.default_driver` → `"claude"` (design §Mix-and-match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_driver: Option<String>,

    /// Optional override repo for `kind = 'design'` tasks on this
    /// product. When set, design tasks resolve to this repo (the docs
    /// site) instead of `repo_remote_url`. Implementation-kind tasks
    /// (`task`, `chore`, `project_task`) are unaffected. Per-task
    /// `--repo` overrides still win — this is a new middle layer in
    /// the existing precedence chain. Stored canonicalised, same as
    /// `repo_remote_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_repo: Option<String>,

    /// Optional preamble prepended to every worker's initial context
    /// at spawn time, visibly bracketed so humans reading transcripts
    /// know what was injected and by whom. `None` / empty → today's
    /// behaviour (no injection). Intended for per-product runtime
    /// guidance such as test-runner preferences that workers should
    /// see on every spawn rather than only when they read AGENTS.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_preamble: Option<String>,

    /// Optional repo where `kind = 'investigation'` task deliverables
    /// (markdown docs) are filed. When set, investigation workers open
    /// PRs against this repo instead of the user-level fallback
    /// (`BOSS_USER_DOCS_REPO`). Stored canonicalised, same as
    /// `repo_remote_url`. `None` → fall through to user-level default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_repo: Option<String>,

    /// Per-product editorial rules that constrain what workers write
    /// into GitHub-visible surfaces (PR bodies, comments, branch name,
    /// commit messages). `None` means no rules are configured; the
    /// engine uses its built-in defaults (strip known Boss identifier
    /// patterns). See `editorial-controls-for-agent-authored-prs-and-github-comments.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editorial_rules: Option<EditorialRules>,

    /// Kind-specific config blob for the bound external tracker.
    /// JSON shape is validated by the tracker impl's `validate_config`
    /// at write time; the protocol type carries it opaquely so new
    /// tracker kinds can ship without a protocol version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_tracker_config: Option<serde_json::Value>,

    /// Discriminator for the external tracker bound to this product.
    /// `None` means no tracker is bound and the reconciler skips this
    /// product. When set (e.g. `"github"`), `external_tracker_config`
    /// carries the kind-specific JSON config. See the external-tracker
    /// sync design (`external-issue-tracker-sync-github-projects.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_tracker_kind: Option<String>,

    pub repo_remote_url: Option<String>,
    /// Leading prefix for worker branch names on this product. The
    /// engine names a worker's branch `<worker_branch_prefix>exec_<id>`;
    /// the `exec_<id>` suffix is the stable identifier every subsystem
    /// keys off (PR-to-execution linking, the kanban Review lane, lease
    /// lookups), so only this leading literal is configurable. `None`
    /// (or empty) → the engine default `boss/`. Set it to satisfy orgs
    /// that enforce per-developer branch prefixes via local hooks (e.g.
    /// `bduff/`). Stored canonicalised with a guaranteed trailing `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_branch_prefix: Option<String>,
}

/// Input for `SetProductEditorialRules`: replace or clear a product's
/// editorial-rules blob. `rules = None` clears the column and reverts
/// the product to the engine defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetProductEditorialRulesInput {
    pub product_id: String,
    /// The new rules to store. `None` clears the column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<EditorialRules>,
}

/// Input to `SetProductExternalTracker`: bind (or unbind) an external
/// tracker on a product. When `unset` is `true`, the engine clears both
/// `external_tracker_kind` and `external_tracker_config` regardless of the
/// other fields. When `unset` is `false`, both `kind` and `config` must be
/// `Some`; the engine passes `config` through the tracker's
/// `validate_config` before persisting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetProductExternalTrackerInput {
    pub product_id: String,
    /// When `true`, clear the tracker binding. All other fields are
    /// ignored.
    #[serde(default)]
    pub unset: bool,

    /// Kind-specific JSON config. `None` only when `unset = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,

    /// Tracker discriminator (`"github"`, etc.). `None` only when
    /// `unset = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}
