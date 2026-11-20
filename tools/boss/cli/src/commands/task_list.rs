//! Task-list command arguments.

use super::*;

/// Args for `boss task list`. Returns every leaf work-item kind for the
/// selected products (project tasks, design, investigation, design_postmortem,
/// followup, chore, revision). Use --all-products to search across products.
/// With --match or --ids, omitting both --product and --project searches across
/// products. Narrow with `--project` (excludes
/// project-less chores) or use `boss chore list` / `boss task list-revisions`
/// for single-kind views.
#[derive(Debug, Clone, Args)]
pub(crate) struct TaskListArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Search every product. Implicit with --match or --ids without a product or project scope.
    #[arg(long, conflicts_with_all = ["product", "project"])]
    pub(crate) all_products: bool,

    /// Filter by work-item kind. Repeat or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub(crate) kind: Vec<boss_protocol::TaskKind>,

    /// Created at or after an RFC3339 timestamp or a relative age (24h, 7d, 2w).
    #[arg(long)]
    pub(crate) created_since: Option<String>,

    #[arg(long, value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) project: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    pub(crate) with_primary_id: bool,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub(crate) status: Vec<TaskStatusArg>,

    /// Filter by priority. Repeat the flag or use a comma-separated list.
    /// e.g. `--priority high` shows only high-priority work.
    #[arg(long, value_delimiter = ',')]
    pub(crate) priority: Vec<TaskPriority>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    pub(crate) match_term: Option<String>,

    /// Cap rows after filtering; cross-product results are newest first, then descending primary id.
    #[arg(long)]
    pub(crate) limit: Option<usize>,

    /// Filter to specific id(s). Comma-separated, or repeat the flag.
    /// Accepts the same forms as `boss task show`: friendly short ids
    /// (`T<n>`, `n`, `#n`), primary ids (`task_…`), and the
    /// cross-product form (`boss/42`). An id that names no row in the
    /// selected scope errors rather than being silently dropped. Without
    /// --product, ids may span products; bare numbers must be unambiguous
    /// across live and soft-deleted rows, even without --deleted.
    /// An explicit --product restricts every id to that product. Composes with
    /// the other filters below as an AND — a row excluded by e.g.
    /// `--status` is not an error, only an id that doesn't exist at
    /// all is. `--id` is accepted as a legacy alias for existing
    /// scripts.
    #[arg(long = "ids", alias = "id", value_delimiter = ',')]
    pub(crate) ids: Vec<String>,

    /// Include soft-deleted (tombstoned) tasks in the listing. Use this
    /// to find a `deleted_at` row to `boss task restore`. The DELETED
    /// column appears whenever any listed row carries a tombstone.
    #[arg(long = "deleted", alias = "include-deleted")]
    pub(crate) include_deleted: bool,

    /// Include archived tasks in the listing. Archived rows are hidden from
    /// the default view and kanban board, but unlike deleted rows they retain
    /// their history and can be reopened.
    #[arg(long = "include-archived")]
    pub(crate) include_archived: bool,

    #[command(flatten)]
    pub(crate) parked: ParkedListArgs,

    /// Filter by resolved repo. Accepts a full URL or a short
    /// name (basename of the URL minus `.git`). Resolution falls
    /// back to the parent product's `repo_remote_url` when the
    /// task carries no override, so `--repo nimbus` finds inherited
    /// matches too. Short-name match is case-insensitive prefix;
    /// selectors shorter than 2 chars are rejected.
    #[arg(long = "repo")]
    pub(crate) repo: Option<String>,

    /// Restrict to rows whose `parent_task_id` matches this task,
    /// canonicalized to its revision chain root (so a mid-chain revision id
    /// and its root return the same rows, matching `list-revisions
    /// --parent`). Accepts the same id forms as `boss task show`: friendly
    /// short ids (`T<n>`, bare `n`, `#n`), product-scoped `slug/n`, or a
    /// primary `task_…` id. Bare and `#` forms resolve globally; a short
    /// id that exists in more than one product hard-errors with the
    /// candidate list — pass `slug/n` or the primary id to disambiguate.
    /// Use this to enumerate children / revisions of a known parent in one
    /// call (e.g. "did this chore ever spawn a CI-fix revision?"). An id
    /// that names no row errors rather than returning empty. Composes with
    /// the other filters as an AND.
    #[arg(long, value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) parent: Option<String>,

    #[command(flatten)]
    pub(crate) dep: DependencyFilterArgs,
}
