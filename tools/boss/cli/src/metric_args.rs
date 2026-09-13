//! `boss metrics catalog|series` clap command / argument definitions.
//!
//! Split out of `commands.rs` to keep that file under the repo's
//! file-size limit (mirrors `idea_args.rs`). These are operator `boss`
//! verbs alongside `boss cost`, not `bossctl metrics` verbs.

use crate::*;

#[derive(Debug, Subcommand)]
pub(crate) enum MetricCommand {
    /// List every engine-defined series, the dimensions it supports, its
    /// default group-by, named presets, observed dimension values, and
    /// coverage. `--json` renders the wire [`boss_protocol::MetricCatalog`].
    Catalog(MetricCatalogArgs),
    /// One catalog series bucketed over a time window. `--json` renders
    /// the wire [`boss_protocol::MetricSeriesReport`].
    Series(MetricSeriesArgs),
}

#[derive(Debug, Clone, Args)]
pub(crate) struct MetricCatalogArgs {
    /// Print timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct MetricSeriesArgs {
    /// Catalog series id (`review_duration`, `execution_duration`,
    /// `task_lead_time`, `execution_outcomes`, `prs_generated`).
    pub(crate) series: String,

    /// Start of the window: an RFC3339 timestamp (e.g.
    /// `2026-07-01T00:00:00Z`) or a relative duration ago (`24h`, `7d`,
    /// `2w`).
    #[arg(long)]
    pub(crate) since: String,

    /// End of the window, same accepted formats as `--since`. Defaults
    /// to now.
    #[arg(long)]
    pub(crate) until: Option<String>,

    /// Bucket width: `hour`, `day`, `week`, or `month`. Omitted, the
    /// engine picks from the range (hour up to three days, day up to
    /// 200 days, week up to four years, month beyond).
    #[arg(long)]
    pub(crate) bucket: Option<String>,

    /// Catalog dimension to group by (`driver`, `model`, `kind`,
    /// `status`, `effort_level`, `repo`, `product`, `reasoning`).
    /// Omitted, the reply is a single group.
    #[arg(long)]
    pub(crate) group_by: Option<String>,

    /// Dimension filter `dim=a,b`. Repeatable; AND across flags, OR
    /// within a flag's values. Example: `--filter status=failed,orphaned`.
    #[arg(long = "filter", value_name = "DIM=VALUE[,VALUE...]")]
    pub(crate) filters: Vec<String>,

    /// Print report timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}
