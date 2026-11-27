//! Run-grain token usage rows and bucketed usage series — the wire shapes
//! behind `boss cost rows`, `boss cost series`, and the Token Usage window.
//!
//! [`crate::FrontendRequest::GetTokenUsageRows`] returns the task → execution →
//! run tree for a time window with every figure already computed by the engine:
//! direct and inclusive totals, per-group sums, coverage and caveats.
//! [`crate::FrontendRequest::GetTokenUsageSeries`] returns bucketed totals and
//! normalizer ratios (tokens per run, per completed execution, per active hour,
//! per completed task, per distinct PR) with honest range coverage. A client
//! lays out what it is sent and never adds, divides or compares two token
//! counts itself, so the honesty rules below exist in exactly one place.
//!
//! ## Honesty rules every figure obeys
//!
//! * **NULL is never zero.** A token class the provider did not report is
//!   `None`, per class. A run with no class at all is `unmeasured`
//!   ([`RunCoverage`]); a run missing some classes is `partial`; only a
//!   recorded zero is `zero`. Sums are made with [`CostMeasurement`], whose
//!   own contract (`types/cost_report.rs`) is reused unchanged.
//! * **Direct versus inclusive.** [`TaskUsageNode::direct`] is the task's own
//!   runs. [`TaskUsageNode::inclusive`] exists only on a chain root and adds
//!   the `direct` of every revision in the chain. Every list total
//!   ([`TokenUsageRowsReport::overall`], [`UsageGroup::measurement`]) sums
//!   direct only, so nothing is counted twice.
//! * **Caveats are data.** [`UsageCaveat::code`] is an open string so an
//!   engine ahead of its client still says something true.
//! * **A ratio needs a denominator.** [`CallStats`] ratios are `None`, never
//!   zero, when no run contributes to them, and each carries the count of runs
//!   it was computed over so a figure over two of forty runs reads as such.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use super::{CostBucket, CostMeasurement, default_true};

/// A way to slice usage: filter by it, group by it, or list its observed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDimension {
    Product,
    Project,
    TaskKind,
    ExecutionKind,
    SpendClass,
    Driver,
    /// The run's observed model, falling back to the execution's launched model.
    Model,
    /// The execution's launched effort, falling back to the task's current setting.
    Effort,
    /// The task's *current* reasoning setting; the launch tuple does not record it.
    Reasoning,
    TaskStatus,
    ExecutionStatus,
    Coverage,
    Host,
}

impl UsageDimension {
    pub const ALL: [UsageDimension; 13] = [
        Self::Product,
        Self::Project,
        Self::TaskKind,
        Self::ExecutionKind,
        Self::SpendClass,
        Self::Driver,
        Self::Model,
        Self::Effort,
        Self::Reasoning,
        Self::TaskStatus,
        Self::ExecutionStatus,
        Self::Coverage,
        Self::Host,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Product => "product",
            Self::Project => "project",
            Self::TaskKind => "task_kind",
            Self::ExecutionKind => "execution_kind",
            Self::SpendClass => "spend_class",
            Self::Driver => "driver",
            Self::Model => "model",
            Self::Effort => "effort",
            Self::Reasoning => "reasoning",
            Self::TaskStatus => "task_status",
            Self::ExecutionStatus => "execution_status",
            Self::Coverage => "coverage",
            Self::Host => "host",
        }
    }
}

impl fmt::Display for UsageDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UsageDimension {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|dimension| dimension.as_str() == s)
            .ok_or_else(|| {
                let known: Vec<_> = Self::ALL.iter().map(|dimension| dimension.as_str()).collect();
                format!("unknown usage dimension {s:?}; expected one of {}", known.join(", "))
            })
    }
}

/// Keep runs whose value for `dimension` is one of `values`. Filters AND
/// across dimensions and OR within one; a filter with no values restricts
/// nothing, so a client can send a row per dimension without special-casing
/// an empty selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageFilter {
    pub dimension: UsageDimension,
    pub values: Vec<String>,
}

/// The payload of [`crate::FrontendRequest::GetTokenUsageRows`]: the window
/// (half-open on `work_runs.created_at`), the filters, the grouping (zero, one
/// or two levels, outermost first), whether to nest each revision under its
/// chain root (the default) or list it as a top-level sibling, and whether to
/// add the optional pricing-table USD estimate. Serialised flat beside the
/// request's `type` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
pub struct TokenUsageRowsRequest {
    pub since_epoch_s: i64,
    pub until_epoch_s: i64,
    #[serde(default)]
    #[builder(default)]
    pub filters: Vec<UsageFilter>,
    #[serde(default)]
    #[builder(default)]
    pub group_by: Vec<UsageDimension>,
    #[serde(default = "default_true")]
    #[builder(default = default_true())]
    pub roll_up_revisions: bool,
    #[serde(default)]
    #[builder(default)]
    pub estimate_usd: bool,
}

/// Spend classes from the cost-attribution design: what a run's execution
/// kind says the tokens were spent on. Reported beside the total, never blended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendClass {
    Implementation,
    Design,
    Overhead,
    /// An execution kind this engine build does not classify. Shown as its
    /// own class rather than folded into one of the three.
    Unclassified,
}

impl SpendClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Implementation => "implementation",
            Self::Design => "design",
            Self::Overhead => "overhead",
            Self::Unclassified => "unclassified",
        }
    }
}

/// Whether a driver's reported `input` already excludes cache reads (`net`)
/// or contains them (`gross`). Totals that mix conventions are not comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputConvention {
    Net,
    Gross,
    Unknown,
}

/// The per-run measurement state, matching [`CostMeasurement::tokens_partial`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunCoverage {
    /// Every token class was reported and at least one is non-zero.
    Measured,
    /// At least one class was reported and at least one was not (Grok and
    /// Antigravity never report cache creation). Known classes are a floor.
    Partial,
    /// No class was reported: unknown, never zero.
    Unmeasured,
    /// Every class was reported and every one is exactly zero.
    Zero,
}

impl RunCoverage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Partial => "partial",
            Self::Unmeasured => "unmeasured",
            Self::Zero => "zero",
        }
    }
}

/// Where an execution's driver came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverSource {
    /// Recorded at spawn on `work_executions.driver`.
    LaunchTuple,
    /// The launch tuple was NULL; filled from the same chain
    /// `driver_transcript::resolve_execution_driver_slug` uses (`tasks.driver`
    /// → product default → pool policy → engine default). This is the task's
    /// current routing, not a frozen record of what ran.
    Resolved,
    /// Not recorded and not resolvable. Shown as "not recorded", never
    /// assumed to be Claude.
    Unknown,
}

/// Per-class token counts of one measured run. Each class is independently
/// `None` when the provider did not report it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TokenClasses {
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub cache_creation: Option<i64>,
    pub cache_creation_5m: Option<i64>,
    pub cache_creation_1h: Option<i64>,
    pub cache_read: Option<i64>,
}

/// One model's share of a run. One element today (the observed model with
/// the run's classes); per-model rows from the cost-attribution project add
/// more elements without a shape change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub tokens: Option<TokenClasses>,
}

/// A reason a run's figure should be read with care. `code` is open on
/// purpose: a client labels the codes it knows and shows an unknown code
/// verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCaveat {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Engine-computed call statistics for one set of runs: how many model calls
/// they made, how large their prompts were, and how often they compacted.
///
/// Every figure is taken only over the runs that recorded it, and says how many
/// those were (`runs_with_*`), because a driver whose transcript does not expose
/// a figure leaves it NULL on the run and a NULL is never folded in as zero.
/// A ratio is `None` when no run contributes to it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, bon::Builder)]
pub struct CallStats {
    /// Runs whose round count is recorded; a recorded zero counts.
    #[serde(default)]
    #[builder(default)]
    pub runs_with_rounds: u32,
    /// Rounds (model calls) summed over `runs_with_rounds`.
    #[serde(default)]
    #[builder(default)]
    pub rounds: i64,
    /// `rounds / runs_with_rounds`; `None` when no run recorded rounds.
    pub rounds_per_run: Option<f64>,
    /// The token classes each run reported, summed, over those rounds, taken
    /// over the runs that have both tokens and rounds. A floor when any of those
    /// runs is `partial`. `None` when no such run exists or they made no rounds.
    pub tokens_per_round: Option<f64>,
    /// Runs whose first call's prompt size is recorded.
    #[serde(default)]
    #[builder(default)]
    pub runs_with_first_call_context: u32,
    /// Median of the first call's prompt size over `runs_with_first_call_context`.
    pub median_first_call_context: Option<f64>,
    /// Runs whose peak prompt size is recorded.
    #[serde(default)]
    #[builder(default)]
    pub runs_with_peak_context: u32,
    /// Median of the peak prompt size over `runs_with_peak_context`.
    pub median_peak_context: Option<f64>,
    /// The largest peak prompt size among `runs_with_peak_context`.
    pub max_peak_context: Option<i64>,
    /// Runs whose compaction count is recorded; a recorded zero counts, and a
    /// driver that records none contributes no run here.
    #[serde(default)]
    #[builder(default)]
    pub runs_with_compactions_recorded: u32,
    /// Of those, the runs that compacted at least once.
    #[serde(default)]
    #[builder(default)]
    pub runs_compacted: u32,
}

/// One `work_runs` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct RunUsageRow {
    pub run_id: String,
    pub coverage: RunCoverage,
    pub created_at_epoch_s: i64,
    pub host_id: String,
    pub input_convention: InputConvention,
    pub status: String,
    #[serde(default)]
    #[builder(default)]
    pub caveats: Vec<UsageCaveat>,
    #[serde(default)]
    #[builder(default)]
    pub models: Vec<ModelUsage>,
    /// `None` when the run is unmeasured.
    pub tokens: Option<TokenClasses>,
    pub agent_active_ms: Option<i64>,
    pub finished_at_epoch_s: Option<i64>,
    /// Tokens the first model call sent as its prompt: fresh input, cache
    /// writes and cache reads. `None` where the driver's transcript does not
    /// expose a single call's prompt size (never zero), and for every run that
    /// predates the capture.
    #[serde(default)]
    pub first_call_context_tokens: Option<i64>,
    /// Tokens the first model call wrote to the prompt cache. `None` where the
    /// driver does not report cache writes.
    #[serde(default)]
    pub first_call_cache_write_tokens: Option<i64>,
    /// The largest prompt any model call sent. `None` unless every call's
    /// prompt size is known.
    #[serde(default)]
    pub peak_context_tokens: Option<i64>,
    /// Context compactions the transcript records. `None` where the driver
    /// records none, which is not a recorded zero.
    #[serde(default)]
    pub compactions: Option<i64>,
    /// `work_runs.model` verbatim, `<synthetic>` included.
    pub model_observed: Option<String>,
    pub rounds: Option<i64>,
    pub started_at_epoch_s: Option<i64>,
    /// The sum of the token classes the provider reported, so a client never
    /// adds classes itself. A floor when `coverage` is `partial`; `None` when
    /// the run is unmeasured.
    pub total_tokens: Option<i64>,
    /// Why the run has no (or incomplete) provider usage, verbatim from
    /// `work_runs.usage_unavailable`. `None` means usage was observed.
    pub usage_unavailable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_usd: Option<f64>,
    /// Why no USD estimate exists for this run, when one was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpriceable_cause: Option<String>,
}

/// One `work_executions` row with its runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ExecutionUsageNode {
    pub execution_id: String,
    /// The execution's `work_item_id`; for a taskless execution this is a
    /// comment, comparison or automation id, not a task.
    pub work_item_id: String,
    pub created_at_epoch_s: i64,
    pub driver_source: DriverSource,
    pub kind: String,
    pub measurement: CostMeasurement,
    pub spend_class: SpendClass,
    pub status: String,
    /// Call statistics over the same runs as `measurement`.
    #[serde(default)]
    #[builder(default)]
    pub calls: CallStats,
    #[serde(default)]
    #[builder(default)]
    pub runs: Vec<RunUsageRow>,
    pub driver: Option<String>,
    pub effort_launched: Option<String>,
    pub finished_at_epoch_s: Option<i64>,
    pub host_id: Option<String>,
    pub model_launched: Option<String>,
    pub pr_url: Option<String>,
    pub started_at_epoch_s: Option<i64>,
}

/// One task with its own executions and, on a chain root, its revisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct TaskUsageNode {
    pub task_id: String,
    pub chain_root_id: String,
    pub direct: CostMeasurement,
    /// Call statistics over the same runs as `direct`.
    #[serde(default)]
    #[builder(default)]
    pub calls: CallStats,
    pub is_chain_root: bool,
    pub kind: String,
    pub name: String,
    pub product_id: String,
    /// Runs this task has outside the requested window. When non-zero the
    /// figures here are a slice of the task's life, not its total.
    pub runs_outside_window: u32,
    pub status: String,
    #[serde(default)]
    #[builder(default)]
    pub by_spend_class: Vec<CostBucket>,
    #[serde(default)]
    #[builder(default)]
    pub executions: Vec<ExecutionUsageNode>,
    /// Revisions of this chain root, oldest first. Empty on a revision, and
    /// empty everywhere when the request did not roll revisions up (they are
    /// then top-level siblings).
    #[serde(default)]
    #[builder(default)]
    pub revisions: Vec<TaskUsageNode>,
    pub completed_at_epoch_s: Option<i64>,
    /// The task's *current* effort setting, not necessarily the launched one.
    pub effort_current: Option<String>,
    /// Chain roots only: `direct` plus the `direct` of every revision in the
    /// chain. Never summed across a list.
    pub inclusive: Option<CostMeasurement>,
    /// Chain roots only: call statistics over the same runs as `inclusive`.
    #[serde(default)]
    pub inclusive_calls: Option<CallStats>,
    pub pr_url: Option<String>,
    pub project_id: Option<String>,
    /// The task's *current* reasoning setting; historical values are not recorded.
    pub reasoning_current: Option<String>,
    pub short_id: Option<i64>,
}

/// One level of a grouping path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageGroupKey {
    pub dimension: UsageDimension,
    /// The machine value filters match on.
    pub value: String,
    /// The display label (a product name, "not recorded" for an absent driver).
    pub label: String,
}

/// A group of runs. The tree under a group holds only that group's runs, so
/// a task with a Claude and a Codex execution appears under both driver groups.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct UsageGroup {
    /// The path from the outermost grouping level down to this group.
    pub key: Vec<UsageGroupKey>,
    /// Direct sum of the runs in this group.
    pub measurement: CostMeasurement,
    /// Call statistics over the same runs as `measurement`.
    #[serde(default)]
    #[builder(default)]
    pub calls: CallStats,
    /// Next grouping level; empty at the leaf level.
    #[serde(default)]
    #[builder(default)]
    pub subgroups: Vec<UsageGroup>,
    /// The task tree for this group's runs; leaf groups only.
    #[serde(default)]
    #[builder(default)]
    pub tasks: Vec<TaskUsageNode>,
    /// Executions with no task row; leaf groups only.
    #[serde(default)]
    #[builder(default)]
    pub taskless: Vec<ExecutionUsageNode>,
}

/// One observed value of a dimension and how many runs carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageDimensionValue {
    pub value: String,
    pub label: String,
    pub runs: u32,
}

/// Observed values of one dimension, for building filter chips from data.
/// Counts apply every active filter except this dimension's own, so a chip
/// for a value that is currently filtered out still shows what selecting it
/// would add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageDimensionValues {
    pub dimension: UsageDimension,
    pub values: Vec<UsageDimensionValue>,
}

/// How many runs carry one caveat code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCaveatCount {
    pub code: String,
    pub runs: u32,
}

/// A report-level coverage note (`capture_started`, `retention_bounded`,
/// `pricing_gaps`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageNote {
    pub code: String,
    pub detail: String,
}

/// Coverage of the runs in a reply. The four state counts are exclusive and
/// sum to `runs_total`; this differs from `CostMeasurement::runs_measured`,
/// which counts every run with any class present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct UsageCoverage {
    /// Runs in state [`RunCoverage::Measured`].
    pub runs_complete: u32,
    pub runs_partial: u32,
    pub runs_total: u32,
    pub runs_unmeasured: u32,
    pub runs_zero: u32,
    #[serde(default)]
    #[builder(default)]
    pub by_caveat: Vec<UsageCaveatCount>,
    #[serde(default)]
    #[builder(default)]
    pub notes: Vec<UsageNote>,
    /// Runs a USD estimate was produced for. `None` unless an estimate was requested.
    pub runs_priceable: Option<u32>,
}

/// Reply to [`crate::FrontendRequest::GetTokenUsageRows`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct TokenUsageRowsReport {
    /// Fixed boundary before which no run has token data, echoed so a client
    /// does not hardcode it.
    pub capture_start_epoch_s: i64,
    pub coverage: UsageCoverage,
    pub generated_at_epoch_s: i64,
    /// Direct sum of every run in this reply, taskless included.
    pub overall: CostMeasurement,
    /// Call statistics over the same runs as `overall`.
    #[serde(default)]
    #[builder(default)]
    pub overall_calls: CallStats,
    /// The engine's cap on runs in one reply.
    pub run_cap: u32,
    pub since_epoch_s: i64,
    pub spans_capture_boundary: bool,
    /// More runs matched than `run_cap`; the most recent were kept.
    pub truncated: bool,
    pub until_epoch_s: i64,
    #[serde(default)]
    #[builder(default)]
    pub dimension_values: Vec<UsageDimensionValues>,
    #[serde(default)]
    #[builder(default)]
    pub group_by: Vec<UsageDimension>,
    #[serde(default)]
    #[builder(default)]
    pub groups: Vec<UsageGroup>,
    #[serde(default)]
    #[builder(default)]
    pub pricing_gaps: Vec<String>,
    /// Executions with no `tasks` row (answer agent, review guide, automation
    /// triage); empty when grouped.
    #[serde(default)]
    #[builder(default)]
    pub taskless: Vec<ExecutionUsageNode>,
    /// The task tree; empty when grouped.
    #[serde(default)]
    #[builder(default)]
    pub tasks: Vec<TaskUsageNode>,
}

// ── Bucketed usage series (`GetTokenUsageSeries`) ─────────────────────────

/// Bucket granularity for [`TokenUsageSeriesRequest`]. When omitted on the
/// request, the engine chooses a width from the requested time span and
/// coarsens through this ladder if the reply would exceed the cell cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageBucketWidth {
    Hour,
    Day,
    Week,
    Month,
}

impl UsageBucketWidth {
    pub const ALL: [UsageBucketWidth; 4] = [Self::Hour, Self::Day, Self::Week, Self::Month];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    /// Nominal width of one bucket in seconds. Hour, day and week are exact
    /// UTC alignments; `month` is the mean 30-day length used for the cell-cap
    /// heuristic and `bucket_secs` on the wire. Actual month bucket starts are
    /// UTC calendar-month midnights (see `bucket_start_epoch_s`).
    pub const fn bucket_secs(self) -> i64 {
        match self {
            Self::Hour => 3_600,
            Self::Day => 86_400,
            Self::Week => 604_800,
            Self::Month => 2_592_000,
        }
    }

    /// The next coarser width on the ladder, or `None` at [`Self::Month`].
    pub const fn coarser(self) -> Option<Self> {
        match self {
            Self::Hour => Some(Self::Day),
            Self::Day => Some(Self::Week),
            Self::Week => Some(Self::Month),
            Self::Month => None,
        }
    }
}

impl fmt::Display for UsageBucketWidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UsageBucketWidth {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|width| width.as_str() == s).ok_or_else(|| {
            let known: Vec<_> = Self::ALL.iter().map(|width| width.as_str()).collect();
            format!("unknown bucket width {s:?}; expected one of {}", known.join(", "))
        })
    }
}

/// The payload of [`crate::FrontendRequest::GetTokenUsageSeries`]: the window
/// (half-open `[since_epoch_s, until_epoch_s)`), an optional bucket width
/// (`None` lets the engine choose from the window span and coarsen at the cell
/// cap), an optional single-level `group_by`, filters (AND across dimensions,
/// OR within one), and whether to add the pricing-table USD estimate.
/// Serialised flat beside the request's `type` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
pub struct TokenUsageSeriesRequest {
    pub since_epoch_s: i64,
    pub until_epoch_s: i64,
    #[serde(default)]
    #[builder(default)]
    pub estimate_usd: bool,
    #[serde(default)]
    #[builder(default)]
    pub filters: Vec<UsageFilter>,
    #[serde(default)]
    pub bucket: Option<UsageBucketWidth>,
    #[serde(default)]
    pub group_by: Option<UsageDimension>,
}

/// Engine-computed normalizer ratios for one `(bucket, group)` cell. Every
/// ratio is `None` when its denominator is zero or when its numerator has no
/// measured runs; clients draw a gap, never a zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, bon::Builder)]
pub struct UsageRatios {
    /// Measured tokens over runs that record `agent_active_ms`, divided by
    /// those runs' `agent_active_ms / 3_600_000.0`. Both numerator and
    /// denominator exclude measured runs with unknown active time (such as
    /// Grok), which are counted in [`UsageSeriesCoverage::notes`].
    pub tokens_per_active_hour: Option<f64>,
    /// Bucket's measured `total_tokens / executions_completed`.
    pub tokens_per_completed_execution: Option<f64>,
    /// Attributed inclusive chain cost (`tasks_completed_cost.total_tokens`)
    /// divided by `tasks_completed`. May include runs outside the window.
    pub tokens_per_completed_task: Option<f64>,
    /// Bucket's measured `total_tokens / distinct_prs` (approximation: runs in
    /// a bucket may belong to PRs that finished in another bucket).
    pub tokens_per_pr: Option<f64>,
    /// Bucket's measured `total_tokens / runs_measured`.
    pub tokens_per_run: Option<f64>,
}

/// One `(bucket, group)` cell in [`UsageBucket::cells`]. Emitted only when the
/// group has at least one fact in the bucket; a client never fabricates zeros.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct UsageCell {
    /// Distinct non-empty `pr_url` values over executions finished in this bucket.
    pub distinct_prs: u32,
    /// Executions with `status = 'completed'` and `finished_at` in this bucket.
    pub executions_completed: u32,
    /// Group key (`"(all)"` when ungrouped, otherwise the dimension value).
    pub group: String,
    /// Direct sum of runs created in this bucket for this group, preserving
    /// per-class NULL semantics (`tokens_partial` when any measured run lacks
    /// a class, never zero).
    pub measurement: CostMeasurement,
    /// Engine-computed normalizer ratios for this cell.
    pub ratios: UsageRatios,
    /// Chain-root tasks with `tasks.completed_at` in this bucket.
    pub tasks_completed: u32,
    /// Inclusive chain cost of those `tasks_completed` tasks, whenever spent
    /// (including runs outside the requested window).
    pub tasks_completed_cost: CostMeasurement,
    /// This group's measured `total_tokens` divided by the bucket's measured
    /// `total_tokens` across all groups (`None` when the cell has no measured
    /// runs or the bucket's measured total is zero).
    pub share_of_bucket_total_tokens: Option<f64>,
}

/// One time bucket in [`TokenUsageSeriesReport::buckets`], ascending by
/// `start_epoch_s`. A bucket with no fact is absent from the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageBucket {
    pub start_epoch_s: i64,
    pub cells: Vec<UsageCell>,
}

/// A coverage note on [`UsageSeriesCoverage::notes`]: carries the note code
/// (`capture_started`, `dimension_started`, `retention_bounded`,
/// `pricing_gaps`, `bucket_coarsened`, `cell_cap_exceeded`,
/// `active_time_missing`), the optional boundary instant (`epoch_s`) so a
/// chart can shade before it, and a human-readable explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSeriesNote {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_s: Option<i64>,
    pub detail: String,
}

/// Range and data-quality coverage for [`TokenUsageSeriesReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct UsageSeriesCoverage {
    /// Caveat counts across the filtered in-window runs (e.g. `gross_input` so
    /// a driver-mix shift reveals when comparability changes).
    #[serde(default)]
    #[builder(default)]
    pub by_caveat: Vec<UsageCaveatCount>,
    #[serde(default)]
    #[builder(default)]
    pub notes: Vec<UsageSeriesNote>,
    /// Measured in-window runs excluded from both numerator and denominator of
    /// `tokens_per_active_hour` because `agent_active_ms` is unknown.
    #[serde(default)]
    #[builder(default)]
    pub runs_excluded_from_active_hour: u32,
    /// Later of the earliest fact in the reply and `capture_start_epoch_s`;
    /// `None` when the reply has no facts.
    pub data_from_epoch_s: Option<i64>,
    /// Earliest fact carrying a recorded value for the `group_by` dimension;
    /// `None` when ungrouped or when no fact carries a recorded value.
    pub dimension_from_epoch_s: Option<i64>,
}

/// Reply to [`crate::FrontendRequest::GetTokenUsageSeries`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct TokenUsageSeriesReport {
    /// Nominal width of one bucket in seconds (`3_600` for hour, `86_400` for
    /// day, `604_800` for week, nominal `2_592_000` for month). For
    /// [`UsageBucketWidth::Month`], buckets are UTC calendar months (28–31
    /// days), so `bucket_secs` is nominal only: each month bucket ends where
    /// the next UTC calendar month starts, not at `start_epoch_s + bucket_secs`.
    pub bucket_secs: i64,
    /// Effective bucket width (requested or engine-chosen, possibly coarsened
    /// to stay under the cell cap). Hour, day and week buckets have fixed
    /// length `bucket_secs`; `month` buckets are UTC calendar months, each
    /// ending where the next UTC calendar month starts.
    pub bucket_width: UsageBucketWidth,
    pub capture_start_epoch_s: i64,
    pub coverage: UsageSeriesCoverage,
    pub generated_at_epoch_s: i64,
    pub since_epoch_s: i64,
    pub spans_capture_boundary: bool,
    pub until_epoch_s: i64,
    #[serde(default)]
    #[builder(default)]
    pub buckets: Vec<UsageBucket>,
    /// Display labels for each key in `groups` (for product/project names and
    /// `"not recorded"`), keyed by the machine group value.
    #[serde(default)]
    #[builder(default)]
    pub group_labels: std::collections::BTreeMap<String, String>,
    /// Group keys in render order (`["(all)"]` when ungrouped).
    #[serde(default)]
    #[builder(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    #[builder(default)]
    pub pricing_gaps: Vec<String>,
    #[serde(default)]
    pub group_by: Option<UsageDimension>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FrontendRequest;

    #[test]
    fn the_series_request_is_flat_beside_its_tag_and_defaults_bucket_and_grouping_to_none() {
        let request: FrontendRequest = serde_json::from_value(serde_json::json!({
            "type": "get_token_usage_series",
            "since_epoch_s": 10,
            "until_epoch_s": 20,
        }))
        .unwrap();
        let FrontendRequest::GetTokenUsageSeries(request) = request else {
            panic!("expected GetTokenUsageSeries");
        };
        assert_eq!(
            request,
            TokenUsageSeriesRequest::builder()
                .since_epoch_s(10)
                .until_epoch_s(20)
                .build()
        );
        assert!(request.bucket.is_none() && request.group_by.is_none());
        assert!(request.filters.is_empty() && !request.estimate_usd);
    }

    #[test]
    fn the_series_request_round_trips_with_bucket_grouping_and_filters() {
        let request = FrontendRequest::GetTokenUsageSeries(
            TokenUsageSeriesRequest::builder()
                .since_epoch_s(100)
                .until_epoch_s(200)
                .bucket(UsageBucketWidth::Day)
                .group_by(UsageDimension::Driver)
                .filters(vec![UsageFilter {
                    dimension: UsageDimension::Driver,
                    values: vec!["claude".into(), "grok".into()],
                }])
                .estimate_usd(true)
                .build(),
        );
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["type"], "get_token_usage_series");
        assert_eq!(json["bucket"], "day");
        assert_eq!(json["group_by"], "driver");
        let decoded: FrontendRequest = serde_json::from_value(json).unwrap();
        let FrontendRequest::GetTokenUsageSeries(decoded) = decoded else {
            panic!("expected GetTokenUsageSeries");
        };
        assert_eq!(decoded.bucket, Some(UsageBucketWidth::Day));
        assert_eq!(decoded.group_by, Some(UsageDimension::Driver));
        assert!(decoded.estimate_usd);
    }

    #[test]
    fn bucket_width_parses_and_coarsens_through_the_ladder() {
        for width in UsageBucketWidth::ALL {
            assert_eq!(width.as_str().parse::<UsageBucketWidth>(), Ok(width));
        }
        assert_eq!(UsageBucketWidth::Hour.coarser(), Some(UsageBucketWidth::Day));
        assert_eq!(UsageBucketWidth::Day.coarser(), Some(UsageBucketWidth::Week));
        assert_eq!(UsageBucketWidth::Week.coarser(), Some(UsageBucketWidth::Month));
        assert_eq!(UsageBucketWidth::Month.coarser(), None);
        let error = "decade".parse::<UsageBucketWidth>().unwrap_err();
        assert!(error.contains("hour") && error.contains("decade"), "{error}");
    }

    #[test]
    fn the_request_is_flat_beside_its_tag_and_defaults_to_rolling_revisions_up() {
        let request: FrontendRequest = serde_json::from_value(serde_json::json!({
            "type": "get_token_usage_rows",
            "since_epoch_s": 10,
            "until_epoch_s": 20,
        }))
        .unwrap();
        let FrontendRequest::GetTokenUsageRows(request) = request else {
            panic!("expected GetTokenUsageRows");
        };
        assert_eq!(
            request,
            TokenUsageRowsRequest::builder()
                .since_epoch_s(10)
                .until_epoch_s(20)
                .build()
        );
        assert!(
            request.roll_up_revisions,
            "a revision chain rolls up unless asked not to"
        );
        assert!(request.filters.is_empty() && request.group_by.is_empty() && !request.estimate_usd);
    }

    #[test]
    fn the_request_round_trips_with_filters_and_grouping() {
        let request = FrontendRequest::GetTokenUsageRows(
            TokenUsageRowsRequest::builder()
                .since_epoch_s(1)
                .until_epoch_s(2)
                .filters(vec![UsageFilter {
                    dimension: UsageDimension::Driver,
                    values: vec!["claude".into(), "codex".into()],
                }])
                .group_by(vec![UsageDimension::Driver, UsageDimension::TaskKind])
                .roll_up_revisions(false)
                .estimate_usd(true)
                .build(),
        );
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["type"], "get_token_usage_rows");
        assert_eq!(json["group_by"], serde_json::json!(["driver", "task_kind"]));
        assert_eq!(json["filters"][0]["dimension"], "driver");
        let decoded: FrontendRequest = serde_json::from_value(json).unwrap();
        let FrontendRequest::GetTokenUsageRows(decoded) = decoded else {
            panic!("expected GetTokenUsageRows");
        };
        assert!(!decoded.roll_up_revisions && decoded.estimate_usd);
        assert_eq!(decoded.group_by.len(), 2);
    }

    #[test]
    fn a_dimension_parses_from_its_wire_name_and_unknown_names_are_listed() {
        for dimension in UsageDimension::ALL {
            assert_eq!(dimension.as_str().parse::<UsageDimension>(), Ok(dimension));
        }
        let error = "flavour".parse::<UsageDimension>().unwrap_err();
        assert!(error.contains("task_kind") && error.contains("flavour"), "{error}");
    }
}
