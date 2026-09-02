//! `boss task update` / `boss chore update` argument struct. Split out of
//! `commands.rs` (which sits at the repo's file-size ceiling) rather than
//! grown in place — `TaskUpdateArgs` is a self-contained leaf shared only
//! by `run_update_leaf`.

use crate::*;

#[derive(Debug, Clone, Args)]
pub(crate) struct TaskUpdateArgs {
    /// Task/chore id. Accepts primary id, friendly short id, or
    /// cross-product form. Globally unique short ids resolve without
    /// `--product`; ambiguous ones error listing every candidate.
    #[arg(value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) id: String,

    /// Resolve a friendly short id (`T42`, `42`, `#42`) against this product
    /// (slug or id). Optional when the short id is globally unique.
    /// Ignored when the selector already embeds a product slug
    /// (`boss/42`) or when the selector is a primary id.
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Resolve a friendly short id against the product that owns this project.
    /// Accepts a typed project id (`project_…`) to infer the product
    /// automatically. Combined with `--product` when passing a slug; ignored
    /// for primary ids.
    #[arg(long, value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) project: Option<String>,

    /// Move into this project (resolved against the item's own product): `chore` becomes `project_task` with a
    /// fresh ordinal. Refused for other kinds. Mutates membership, unlike `--project` above. Conflicts with `--unset-project`.
    #[arg(
        long = "set-project",
        value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME,
        conflicts_with = "unset_project"
    )]
    pub(crate) set_project: Option<String>,

    /// Move out to the no-project state: `project_task` becomes `chore`, ordinal cleared. Conflicts with `--set-project`.
    #[arg(long = "unset-project", conflicts_with = "set_project")]
    pub(crate) unset_project: bool,

    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    #[arg(long)]
    pub(crate) status: Option<TaskStatusArg>,

    #[arg(long)]
    pub(crate) priority: Option<TaskPriority>,

    #[arg(long)]
    pub(crate) ordinal: Option<i64>,

    /// Escape hatch for backfilling `pr_url` when the engine's
    /// auto-detection couldn't pick it up. With the on-Stop +
    /// merge-poller pair installed in the engine you should rarely
    /// need this; hidden from `-h` short help to keep the common
    /// path clean while still surfacing it in `--help` and via
    /// `boss chore update --help`.
    #[arg(long = "pr-url", hide_short_help = true)]
    pub(crate) pr_url: Option<String>,

    /// Set or clear this item's repo override. `--repo <url>` sets
    /// the override; `--repo ""` clears it so the item inherits
    /// from the product default. Same shape as `--pr-url ""`.
    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    pub(crate) repo_remote_url: Option<String>,

    /// Set the effort level (`trivial`/`small`/`medium`/`large`/`max`).
    /// Mutually exclusive with `--unset-effort`.
    #[arg(long, value_enum, conflicts_with = "unset_effort")]
    pub(crate) effort: Option<EffortLevelArg>,

    /// Clear the effort level so the row falls through to the
    /// dispatcher's product / engine default again (design §Q3).
    #[arg(long = "unset-effort")]
    pub(crate) unset_effort: bool,

    /// Set or clear the heuristic matched-rule provenance for
    /// `--effort` (e.g. `rule 3 (multi-subsystem)`). Pass
    /// `--effort-matched-rule ""` to clear. When `--effort` is set
    /// without provenance, the engine clears both provenance columns
    /// so a hand-set level is detectable.
    #[arg(long = "effort-matched-rule", value_name = "RULE", allow_hyphen_values = true)]
    pub(crate) effort_matched_rule: Option<String>,

    /// Set or clear the heuristic reasons provenance for `--effort`.
    /// Pass `--effort-reasons ""` to clear. See
    /// `--effort-matched-rule`.
    #[arg(long = "effort-reasons", value_name = "REASONS", allow_hyphen_values = true)]
    pub(crate) effort_reasons: Option<String>,

    /// Model slug for the resolved driver. Stored verbatim. Mutually
    /// exclusive with `--unset-model`.
    #[arg(long, value_name = "SLUG", conflicts_with = "unset_model")]
    pub(crate) model: Option<String>,

    /// Clear the per-row model override so the dispatcher falls
    /// through per design §Q3 precedence.
    #[arg(long = "unset-model")]
    pub(crate) unset_model: bool,

    /// Set the reasoning mode (`standard`/`investigation`) — what kind of
    /// thinking this needs, independent of `--effort`'s size. Mutually
    /// exclusive with `--unset-reasoning`.
    #[arg(long, value_enum, conflicts_with = "unset_reasoning")]
    pub(crate) reasoning: Option<ReasoningArg>,

    /// Clear the reasoning mode so the row falls back to the dispatcher's
    /// legacy kind-floor / effort-table model resolution.
    #[arg(long = "unset-reasoning")]
    pub(crate) unset_reasoning: bool,

    /// Agent driver override (e.g. `claude`, `copilot`, `codex`).
    /// Mutually exclusive with `--unset-driver`.
    #[arg(long, value_name = "DRIVER", conflicts_with = "unset_driver")]
    pub(crate) driver: Option<String>,

    /// Clear the per-row driver override so the dispatcher falls
    /// through to the product / engine default.
    #[arg(long = "unset-driver")]
    pub(crate) unset_driver: bool,

    /// Enable or disable auto-dispatch for this item. `--autostart true`
    /// lets the engine auto-dispatch the item when a worker slot is free;
    /// `--autostart false` parks it in the backlog until you re-enable it.
    #[arg(long, value_name = "BOOL")]
    pub(crate) autostart: Option<bool>,

    /// Mark or unmark this item as deferred / future scope. `--deferred
    /// false` approves it — pulls it into scope so the engine can dispatch
    /// it again; `--deferred true` parks it as future scope (auto-unblocked
    /// and visible, but never auto-dispatched until approved). Distinct
    /// from `--autostart`, which is a one-shot dispatch-timing pause.
    #[arg(long, value_name = "BOOL")]
    pub(crate) deferred: Option<bool>,

    /// Mark or unmark this item as human-driven. `--human-driven true`
    /// means a person does the work (no agent worker, close only via
    /// `boss task complete --summary`); `--human-driven false` clears the
    /// flag so the row behaves as ordinary agent work again.
    #[arg(long = "human-driven", value_name = "BOOL")]
    pub(crate) human_driven: Option<bool>,

    /// Set the design-only driver reasoning-effort escalation. This is not the
    /// work-size `--effort` estimate: it is an explicit coordinator judgement
    /// for a complex `kind=design` row. The engine rejects every other kind.
    #[arg(long = "design-reasoning-effort-xhigh", value_name = "BOOL")]
    pub(crate) design_reasoning_effort_xhigh: Option<bool>,

    /// Set or clear the blocked reason on this item. Accepts any engine
    /// reason value (`merge_conflict`, `ci_failure`, `ci_failure_exhausted`,
    /// `dependency`, `review_feedback`) or an empty string to clear.
    /// Pass `--blocked-reason ""` to wipe a stale reason the automated
    /// sweepers left behind. This is the manual escape hatch; automated
    /// clearing happens when the engine transitions a row away from `blocked`.
    #[arg(long = "blocked-reason", value_name = "REASON", allow_hyphen_values = true)]
    pub(crate) blocked_reason: Option<String>,

    /// Set or clear the long-form, verbatim explanation of the blocked
    /// reason. Rendered as a tooltip on the pill instead of the short
    /// label: no title-casing, no truncation, no length limit — put the
    /// full prose (identifiers, punctuation, sentences) here instead of
    /// cramming it into --blocked-reason. Pass `--blocked-detail ""` to
    /// clear. Requires an accompanying (or already-set) --blocked-reason;
    /// the engine rejects a detail with no reason to attach it to.
    #[arg(long = "blocked-detail", value_name = "DETAIL", allow_hyphen_values = true)]
    pub(crate) blocked_detail: Option<String>,

    /// Set or clear the archival reason. Required when an agent (or other
    /// automated actor) moves the row to `archived`; optional for a human
    /// archive. Pass `--archived-reason ""` to clear.
    #[arg(long = "archived-reason", value_name = "REASON", allow_hyphen_values = true)]
    pub(crate) archived_reason: Option<String>,

    /// Replace the full free-form tag set on this work item. Comma-separated
    /// list (e.g. `--tags needs-human,ci-flake`). Empty string clears all
    /// tags (same as `--clear-tags`). Mutually exclusive with `--clear-tags`.
    /// Caps: 24 chars per tag, 5 tags per item (engine-enforced).
    #[arg(long = "tags", value_name = "TAGS", conflicts_with = "clear_tags")]
    pub(crate) tags: Option<String>,

    /// Append one free-form tag (repeatable). De-duplicated against the
    /// current set. May be combined with `--remove-tag` and/or `--tags`.
    #[arg(long = "add-tag", value_name = "TAG", action = clap::ArgAction::Append)]
    pub(crate) add_tags: Vec<String>,

    /// Remove one free-form tag (repeatable, exact match). Unknown names
    /// are ignored. May be combined with `--add-tag` and/or `--tags`.
    #[arg(long = "remove-tag", value_name = "TAG", action = clap::ArgAction::Append)]
    pub(crate) remove_tags: Vec<String>,

    /// Clear every free-form tag on this work item. Mutually exclusive
    /// with `--tags`.
    #[arg(long = "clear-tags", conflicts_with = "tags")]
    pub(crate) clear_tags: bool,
}
