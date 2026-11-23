//! Run-grain token usage rows — the wire shapes behind `boss cost rows`
//! and the Token Usage window.
//!
//! One request ([`crate::FrontendRequest::GetTokenUsageRows`]) returns the
//! task → execution → run tree for a time window with every figure already
//! computed by the engine: direct and inclusive totals, per-group sums,
//! coverage and caveats. A client lays out what it is sent and never adds,
//! divides or compares two token counts itself, so the honesty rules below
//! exist in exactly one place.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FrontendRequest;

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
