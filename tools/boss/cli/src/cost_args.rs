//! `boss cost …` clap command / argument definitions.
//!
//! Split out of `commands.rs` to keep that file under the repo's
//! file-size limit (mirrors `idea_args.rs`).

use crate::*;

#[derive(Debug, Subcommand)]
pub(crate) enum CostCommand {
    /// Token cost for one work item, aggregated across every execution
    /// and run it has ever had. The natural companion to `boss task
    /// show` / `boss chore show`.
    Task(CostTaskArgs),
    /// Token cost recorded in a time window, broken down by execution
    /// kind, model, reasoning mode, and effort level — the groupings
    /// that answer "where did the budget go."
    Window(CostWindowArgs),
    /// The highest-token-cost runs recorded in a time window, ranked
    /// descending.
    Top(CostTopArgs),
}

#[derive(Debug, Clone, Args)]
pub(crate) struct CostTaskArgs {
    /// Task/chore id. Accepts: primary id (`task_…`), friendly short id
    /// (case-insensitive `t`-prefixed form, `42`, or `#42`), or
    /// cross-product form (`boss/42` or `boss/#42`). Globally unique
    /// short ids resolve without `--product`.
    #[arg(value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) id: String,

    /// Resolve a friendly short id (`42` or `#42`) against this product
    /// (slug or id). Optional when the short id is globally unique.
    /// Ignored when the selector already embeds a product slug or is a
    /// primary id.
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Include an estimated USD figure alongside the token totals,
    /// priced from `boss-engine`'s human-maintained pricing table
    /// (`tools/boss/engine/core/src/cost_pricing.rs`). Always labelled
    /// as an estimate in the output — never billing truth.
    #[arg(long)]
    pub(crate) usd: bool,

    /// Print report timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct CostWindowArgs {
    /// Start of the window: an RFC3339 timestamp (e.g.
    /// `2026-07-01T00:00:00Z`) or a relative duration ago (`24h`, `7d`,
    /// `2w`).
    #[arg(long)]
    pub(crate) since: String,

    /// End of the window, same accepted formats as `--since`. Defaults
    /// to now.
    #[arg(long)]
    pub(crate) until: Option<String>,

    /// Include an estimated USD figure alongside the token totals. See
    /// `boss cost task --usd`'s help for the pricing-table caveat.
    #[arg(long)]
    pub(crate) usd: bool,

    /// Print report timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct CostTopArgs {
    /// Start of the window: an RFC3339 timestamp (e.g.
    /// `2026-07-01T00:00:00Z`) or a relative duration ago (`24h`, `7d`,
    /// `2w`).
    #[arg(long)]
    pub(crate) since: String,

    /// End of the window, same accepted formats as `--since`. Defaults
    /// to now.
    #[arg(long)]
    pub(crate) until: Option<String>,

    /// How many of the highest-token-cost runs to return.
    #[arg(long, default_value_t = 20)]
    pub(crate) limit: u32,

    /// Include an estimated USD figure alongside the token totals. See
    /// `boss cost task --usd`'s help for the pricing-table caveat.
    #[arg(long)]
    pub(crate) usd: bool,

    /// Print report timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}
