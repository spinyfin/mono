//! Projects, their status vocabulary, and the design-doc resolution and
//! revision types that hang off them.

use super::common::{default_human_actor, default_priority, default_true};
use super::task::TaskKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateProjectInput {
    pub product_id: String,
    /// Project creation auto-creates a `kind = 'design'` task as the
    /// first row under the project so the design phase shows up on
    /// the kanban like any other task. With `autostart = false` that
    /// design task is created in `todo` but the engine will NOT
    /// dispatch a worker against it until something explicitly
    /// schedules it (CLI `work start`, kanban drag-to-Doing, etc.).
    /// Mirrors the chore/task `autostart` semantics — same gate,
    /// applied at the moment the design task is born.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,

    pub name: String,
    /// When `true`, skip creation of the auto-generated `kind=design`
    /// seed task entirely. The project is filed alone with zero child
    /// tasks. Useful for non-design-shaped projects (postmortems,
    /// milestone aggregators, checklists) where the seed task is dead
    /// weight. Defaults to `false` to preserve existing behaviour.
    #[serde(default)]
    #[builder(default)]
    pub no_design_task: bool,

    pub description: Option<String>,
    pub goal: Option<String>,
}

/// Typed status for a [`Project`]. The five values mirror the
/// free-form strings that were stored before this enum was introduced
/// (`planned`, `active`, `blocked`, `done`, `archived`); the mapping
/// is 1:1 and round-trips through serde and `FromStr`/`Display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectStatus {
    Planned,
    Active,
    Blocked,
    Done,
    Archived,
}

impl ProjectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectStatus::Planned => "planned",
            ProjectStatus::Active => "active",
            ProjectStatus::Blocked => "blocked",
            ProjectStatus::Done => "done",
            ProjectStatus::Archived => "archived",
        }
    }
}

impl std::fmt::Display for ProjectStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProjectStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "planned" => Ok(ProjectStatus::Planned),
            "active" => Ok(ProjectStatus::Active),
            "blocked" => Ok(ProjectStatus::Blocked),
            "done" => Ok(ProjectStatus::Done),
            "archived" => Ok(ProjectStatus::Archived),
            other => Err(format!(
                "unknown project status `{other}`; expected one of: planned, active, blocked, done, archived"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct Project {
    pub id: String,
    /// Per-product short id allocated at insert time. Always `Some` after the
    /// schema migration runs; `None` only on rows predating it (which the
    /// migration backfills, so in practice this is never `None` at runtime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    pub created_at: String,
    pub description: String,
    pub goal: String,
    /// Who made the most recent status change. Four values, parseable as
    /// [`StatusActor`]:
    /// - `'human'` (default) — a CLI / app caller with no registered
    ///   Boss-session ancestry, or a drag-drop gesture in the macOS app.
    /// - `'boss'` — the caller's process ancestry traces back to the
    ///   registered Boss-coordinator session pid (the libghostty pane
    ///   where Claude Code runs as coordinator).
    /// - `'engine'` — the engine wrote the status itself (dependency
    ///   auto-block/unblock, merge poller, CI watch, etc.).
    /// - `'boothby'` — the autonomous groundskeeper decided this row was
    ///   stale / empty / wedged during a maintenance pass. Audited with
    ///   pre/post images in `boothby_actions`, and undoable from there.
    ///
    /// The auto-unblock path only flips a `blocked` row back to `todo`
    /// when this is `'engine'` — manual, Boss-driven and Boothby blocks
    /// all stick, because each is a deliberate decision about this row
    /// rather than cascade bookkeeping the engine owns reversing. See
    /// [`StatusActor::is_engine_cascade`], which states that rule once.
    #[serde(default = "default_human_actor")]
    #[builder(default = default_human_actor())]
    pub last_status_actor: String,

    pub name: String,
    #[builder(default = default_priority())]
    pub priority: String,

    pub slug: String,
    pub status: ProjectStatus,
    pub updated_at: String,
    /// Branch the design doc lives on. `None` → inherit from the
    /// product's docs branch (or `"main"` if no per-product default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_branch: Option<String>,

    /// Repo-relative path to the design doc (e.g.
    /// `"tools/boss/docs/designs/foo.md"`). `None` → no pointer set;
    /// UI affordance is hidden. This is the load-bearing field — when
    /// `None` the other two are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_path: Option<String>,

    /// Repo URL the project's design doc lives in. `None` → inherit
    /// from the project's product (`products.repo_remote_url`). Set
    /// explicitly when the doc lives in a different repo (the
    /// separate-doc-repo case at work).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_repo_remote_url: Option<String>,
}

/// Wire-level state returned by `ResolveProjectDesignDoc`. The UI
/// uses this directly to pick the right affordance (hidden, plain
/// icon, warning glyph) without re-implementing the resolution
/// logic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProjectDesignDocState {
    /// The project has no design-doc pointer set. UI hides the
    /// affordance entirely.
    NotSet,
    /// The pointer resolved cleanly. Carries the structured triple,
    /// the absolute path of a leased cube workspace for the resolved
    /// repo (so the open dispatcher can pick the filesystem fast
    /// path), and a pre-rendered GitHub web URL for the kanban
    /// tooltip / right-click "copy link."
    Resolved {
        resolved: ResolvedDesignDoc,
        /// Absolute path to a cube workspace leased for
        /// `resolved.repo_remote_url`, if any. `Some(path)` means the
        /// open dispatcher can hand `<workspace_path>/<resolved.path>`
        /// to `$EDITOR` / the in-app renderer; `None` means no
        /// workspace is currently leased so the affordance falls back
        /// to `raw_content_url` or the GitHub web URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_path: Option<String>,
        /// `https://github.com/<owner>/<repo>/blob/<branch>/<path>`,
        /// pre-built so consumers don't have to re-parse the repo
        /// URL.
        web_url: String,
        /// `https://raw.githubusercontent.com/<owner>/<repo>/<branch>/<path>`,
        /// present when `resolved.repo_remote_url` is a github.com URL.
        /// Used by the macOS app to fetch and render the doc inline when
        /// no workspace fast-path is available — in particular when the
        /// design task is `in_review` and the file lives on the PR head
        /// branch rather than `main`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_content_url: Option<String>,
    },
    /// The pointer is set but cannot be resolved (e.g. path with no
    /// repo to resolve against). The UI surfaces a warning glyph
    /// linking to the set-design-doc form.
    Broken { reason: String },
}

/// Result of resolving a project's design-doc pointer. Carries the
/// concrete `(repo, branch, path)` triple plus a discriminator that
/// tells the open affordance which fast path it can take.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedDesignDoc {
    pub branch: String,
    pub kind: ResolvedDesignDocKind,
    pub path: String,
    pub repo_remote_url: String,
}

/// Where the resolved design doc actually lives relative to the
/// project's product. Drives the open affordance: `SameProduct` and
/// `OtherProduct` can open in the leased workspace's filesystem when
/// cube has a workspace for the repo; `External` always falls back
/// to the GitHub web URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResolvedDesignDocKind {
    /// Doc lives in the project's own product's repo. Fast path: read
    /// the file straight from a leased workspace.
    SameProduct { product_id: String },
    /// Doc lives in a Boss-tracked product different from the
    /// project's. If cube has a workspace for that repo, the same
    /// fast path applies; otherwise fall through to web.
    OtherProduct { product_id: String },
    /// Doc lives in a repo Boss does not track as a Product. Open
    /// always goes through the GitHub web URL.
    External,
}

/// Output of the `ResolveProjectDesignDoc` RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveProjectDesignDocOutput {
    pub project_id: String,
    pub state: ProjectDesignDocState,
}

/// The task that owns a `pr_doc:*` comment artifact, and that task's current
/// PR lifecycle. Returned by the engine's `resolve_doc_owner` reverse
/// resolver (`tools/boss/engine/core`), which gates both classifier
/// eligibility (only `Design`/`Investigation`-owned docs are classified) and
/// the directive/larger-change revision-vs-chore routing decision. Design:
/// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// §"The revision-vs-general-task decision".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocOwner {
    /// The task the resolver matched. Always `kind ∈ {Design, Investigation}`
    /// — the resolver returns `None` for any other kind (the scope guard).
    pub task_id: String,
    pub task_kind: TaskKind,
    /// The root of `task_id`'s revision chain. In practice this always
    /// equals `task_id`: a revision task never owns its own PR branch (it
    /// commits onto its chain root's existing branch), so `resolve_doc_owner`
    /// never matches a `Revision`-kind task. Carried anyway so callers have
    /// the id `create_revision`'s `parent` argument expects without
    /// re-deriving it.
    pub chain_root_id: String,
    pub pr_url: Option<String>,
    pub pr_lifecycle: DocOwnerPrLifecycle,
}

/// Read-only `[Revise]`-banner summary for a `pr_doc` artifact, returned by
/// `WorkDb::comments_banner_state`. A small companion read to `CommentsList`
/// so a client can render the banner without loading every comment. Design:
/// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// §"2d. Banner state on the comment read path".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommentsBannerState {
    /// True iff `doc_kind` is `Some` (a design/investigation-owned doc)
    /// and `unresolved_count > 0`.
    pub revisable: bool,
    /// `active` comments with `intent ∈ {directive, larger_change}` —
    /// the same candidate set `[Revise]` itself batches.
    pub unresolved_count: i64,
    /// Comments currently claimed by an in-flight revision/chore
    /// (`status = 'in_revision'`).
    pub in_revision_count: i64,
    /// The doc owner's kind (always `Design`/`Investigation` when
    /// present); `None` when `resolve_doc_owner` found no owner.
    pub doc_kind: Option<TaskKind>,
}

/// A coarse, DB-only summary of a doc-owning task's PR lifecycle — derived
/// from `tasks.pr_url` and `tasks.status` alone, no GitHub round trip.
/// `resolve_doc_owner` must stay cheap enough to run on every comment
/// create, so it reads only what the row already has cached; contrast with
/// the engine's live merge-poller `PrLifecycleState`, which comes from a
/// `gh pr view` probe. `ClosedUnmerged` has no stored terminal signal yet
/// (`chore-lifecycle-pr-closed-unmerged` is unimplemented) and is not
/// distinguishable from `Open` here — a present `pr_url` with no terminal
/// marker reads as `Open`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DocOwnerPrLifecycle {
    /// `pr_url` is set and the task has not reached the terminal `Done`
    /// status — covers genuinely-open PRs and (today, indistinguishably)
    /// closed-unmerged ones.
    Open,
    /// The task reached `Done`, which the engine only sets via
    /// `mark_chore_pr_merged` — the PR merged.
    Merged,
    /// `pr_url` is `NULL` — the doc's owning task never opened a PR.
    NoPr,
}

/// Input to the `CommentsReviseDoc` RPC: batch-address every unaddressed
/// `directive`/`larger_change` comment on a `pr_doc` artifact. Design:
/// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// §"Engine RPC surface".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviseDocInput {
    /// `"pr_doc"` (v1). Any other value resolves to `NotApplicable` —
    /// `resolve_doc_owner`'s scope guard returns `None` for it.
    pub artifact_kind: String,
    /// `pr_doc:<repo_remote_url>:<branch>:<path>`.
    pub artifact_id: String,
    /// `None` (v1 default) addresses every `active` comment on the artifact
    /// classified `directive`/`larger_change`. Reserved for a future subset
    /// selection (design §"Batch scope"); a caller-supplied id outside that
    /// set is silently excluded, never an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_ids: Option<Vec<String>>,
}

/// Outcome of `CommentsReviseDoc`. Design:
/// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// §"Engine RPC surface".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReviseDocOutcome {
    /// A revision (open PR) or chore (merged/closed/no-PR) was created and
    /// the addressed comments were flipped to `in_revision`.
    Created {
        /// The revision or chore that now owns the addressed comments.
        task_id: String,
        /// `"revision"` | `"chore"`.
        task_kind: String,
        addressed_comment_ids: Vec<String>,
        /// Comments the operator can see badged `directive`/`larger_change`
        /// on this artifact that this batch did **not** address, because
        /// their `status` disqualified them (`in_revision` — already claimed
        /// by an earlier batch; `orphaned` — the anchor no longer resolves;
        /// `answering` — a live answer-agent run). The badge renders
        /// `intent` alone, so without this the operator sees a plain success
        /// toast for a batch that silently dropped comments they believe are
        /// revisable. Empty in the common case.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        excluded_comment_ids: Vec<String>,
        /// The chain root's PR (revision path), or `None` for a fresh
        /// chore that has not opened a PR yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pr_url: Option<String>,
    },
    /// No `active` comment on the artifact carries a `directive`/
    /// `larger_change` intent — idempotent no-op.
    NoUnresolvedComments,
    /// A prior `CommentsReviseDoc` call already claimed every candidate
    /// comment between this call's read and its guarded update.
    AlreadyInFlight { task_id: String },
    /// `resolve_doc_owner` found no design/investigation-owned task for
    /// this artifact — not eligible for classification/routing at all.
    NotApplicable { reason: String },
}

/// Input to the `SetProjectDesignDoc` RPC: point a project at its
/// design doc. Three optional fields (mirroring the three columns on
/// `projects`), plus an `unset` switch that clears the pointer.
///
/// Resolution semantics (also enforced engine-side):
/// - `design_doc_path = Some(p)` with non-empty `p` → set the
///   pointer; `repo_remote_url` / `branch` are best-effort overrides
///   (any `None` falls back to the product's defaults).
/// - `design_doc_path = None` with `unset = false` → only the
///   non-path fields are updated; the existing path stays put.
/// - `unset = true` → clear all three columns. Any explicit field
///   values supplied alongside are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetProjectDesignDocInput {
    pub project_id: String,
    /// When `true`, clear the pointer entirely (all three columns set
    /// to NULL). Takes precedence over any explicit field values.
    #[serde(default)]
    pub unset: bool,

    /// `None` means "inherit from `product.docs_branch`, falling back
    /// to `"main"`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_branch: Option<String>,

    /// Repo-relative path. Setting `Some("")` is rejected by the
    /// engine (use `unset = true` to clear). `None` leaves the
    /// existing path unchanged unless `unset` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_path: Option<String>,

    /// `None` means "inherit from `product.repo_remote_url`" (the
    /// in-repo case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_repo_remote_url: Option<String>,
}
