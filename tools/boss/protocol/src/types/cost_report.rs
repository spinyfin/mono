//! Read-only token-cost reporting types — backs the `boss cost` CLI
//! surface (`boss cost task`, `boss cost window`, `boss cost top`).
//!
//! These aggregate `work_runs`' recorded per-run token columns
//! (`input_tokens`, `output_tokens`, `cache_creation_tokens`,
//! `cache_read_tokens`, `agent_active_ms`). Token counts are the
//! measured, exact figures the system recorded; any USD figure
//! derived from them is an *estimate* against a human-maintained
//! pricing table (`boss-engine`'s `cost_pricing` module) and is
//! always optional and clearly separated from the token totals.
//!
//! ## NULL vs. zero
//!
//! Per-run token capture did not exist before 2026-07-27T09:23:00Z
//! (2026-07-27 04:23 America/Chicago). Runs before that instant have
//! NULL token columns — the cost is unknown, not zero. A run that
//! failed before ever reaching a model legitimately has zero tokens
//! recorded. [`CostMeasurement`] keeps these distinct:
//! `runs_unmeasured` counts rows with no recorded token category
//! (excluded from every token sum); `runs_zero` counts rows with a
//! real, recorded zero. Token sums are never computed by coalescing
//! NULL to zero.

use serde::{Deserialize, Serialize};

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// Aggregate token/cost figures for a set of `work_runs` rows. Shared
/// shape used at every granularity — a single bucket, a task's
/// overall total, a report's grand total.
///
/// `runs_total == runs_measured + runs_unmeasured`. `runs_zero` is a
/// subset of `runs_measured` (a measured run whose recorded tokens
/// happen to be exactly zero, e.g. a run that failed before calling
/// a model). A run is measured when any token category is present.
/// Unmeasured runs are excluded from category sums and do not make a
/// measured category unknown. A category is NULL when no run was
/// measured, or when any measured run lacks that category.
/// `total_tokens` sums available categories on measured runs; when
/// `tokens_partial` is true it is a lower bound, not a complete total.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct CostMeasurement {
    /// Reasons recorded on runs without complete provider usage.
    #[serde(default)]
    #[builder(default)]
    pub usage_unavailable: Vec<String>,
    /// Totals are a lower bound when a measured run lacks a category.
    /// Unmeasured runs are counted in `runs_unmeasured` and
    /// `usage_unavailable` instead; they do not set this flag.
    #[serde(default)]
    #[builder(default)]
    pub tokens_partial: bool,
    pub agent_active_ms: i64,
    pub cache_creation_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    /// `true` when at least one run folded into this measurement used
    /// a model with unverified rates or incomplete categories, meaning `estimated_usd`
    /// (when present) understates the true total. `false` when either
    /// every priced-eligible run had a known price, or no USD
    /// estimate was requested at all.
    pub estimated_usd_partial: bool,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    /// Runs with at least one recorded token category — includes
    /// `runs_zero`, whose recorded tokens are all exactly zero.
    pub runs_measured: u32,
    pub runs_total: u32,
    /// Runs with no recorded token category — cost unknown, excluded
    /// from every sum in this measurement. Never conflated with a
    /// recorded-zero run.
    pub runs_unmeasured: u32,
    /// Runs whose tokens were recorded and are all exactly zero (a
    /// real, measured zero — e.g. a run that failed pre-model-call).
    pub runs_zero: u32,
    pub total_tokens: i64,
    /// USD estimate against the pricing table in `boss-engine`'s
    /// `cost_pricing` module. `None` unless the caller explicitly
    /// asked for one (`boss cost ... --usd`). Always labelled as an
    /// estimate in CLI output — never treated as billing truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_usd: Option<f64>,
    /// Number of priced runs folded into this measurement whose non-zero
    /// cache-write tokens used the single 5-minute `cache_write` fallback
    /// rate (either because no 5m/1h split was recorded or because the model
    /// entry has no separate 1h rate). `0` when no USD estimate was requested
    /// or every non-zero cache write was priced at its own lifetime rate.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    #[builder(default)]
    pub unsplit_cache_write_runs: u32,
}

/// One labelled slice of a [`WindowCostReport`] breakdown (by
/// execution kind, model, reasoning mode, or effort level).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostBucket {
    /// Grouping key for this slice, e.g. `"chore_implementation"` (kind),
    /// a model slug, `"standard"` (reasoning), `"medium"` (effort level),
    /// or `"(none)"` when the underlying column was NULL. Reasoning and
    /// effort labels reflect the work item's current setting because their
    /// historical per-run values are not recorded.
    pub label: String,
    pub measurement: CostMeasurement,
}

/// Per-execution slice of a [`TaskCostReport`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionCostRow {
    pub execution_id: String,
    pub kind: String,
    /// Distinct models observed across this execution's runs, sorted.
    /// Usually one entry; more than one means a retry ran under a
    /// different model.
    pub models: Vec<String>,
    pub status: String,
    pub measurement: CostMeasurement,
}

/// Output shape for `boss cost task <id>` — cost for one work item,
/// aggregated across every execution and run it has ever had, including
/// the review guides generated for its PR (their `ExecutionCostRow::kind`
/// is `pr_review_guide`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct TaskCostReport {
    pub work_item_id: String,
    pub short_id: Option<i64>,
    pub by_execution: Vec<ExecutionCostRow>,
    pub generated_at_epoch_s: i64,
    pub name: String,
    pub overall: CostMeasurement,
    /// Distinct models observed on this task's runs that had no entry
    /// in the pricing table, present only when a USD estimate was
    /// requested. Empty otherwise.
    pub pricing_gaps: Vec<String>,
}

/// Output shape for `boss cost window` — cost recorded in
/// `[since_epoch_s, until_epoch_s)`, broken down by the dimensions
/// that proved decision-relevant in practice: execution kind, model,
/// reasoning mode, and effort level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct WindowCostReport {
    #[serde(default)]
    #[builder(default)]
    pub by_driver: Vec<CostBucket>,
    pub by_effort_level: Vec<CostBucket>,
    pub by_kind: Vec<CostBucket>,
    pub by_model: Vec<CostBucket>,
    pub by_reasoning: Vec<CostBucket>,
    /// Fixed boundary: 2026-07-27T09:23:00Z, before which no run has
    /// token data. Echoed back so a client doesn't have to hardcode it.
    pub capture_start_epoch_s: i64,
    pub generated_at_epoch_s: i64,
    pub overall: CostMeasurement,
    pub pricing_gaps: Vec<String>,
    pub since_epoch_s: i64,
    /// `true` when `since_epoch_s < capture_start_epoch_s` — part of
    /// the requested window predates token capture entirely, so
    /// `overall` (and every bucket) is a floor, not a complete total.
    pub spans_capture_boundary: bool,
    pub until_epoch_s: i64,
}

/// One row of `boss cost top` — a single run, identified well enough
/// to know what it was. `work_item_id`, `short_id` and `name` describe the
/// task the run's spend belongs to; for a review guide that is the task that
/// owns the PR. A run whose execution has no task (answer agent, automation
/// triage) carries what its execution is bound to as `work_item_id`, no
/// `short_id`, and the name `(no task)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct CostTopRow {
    #[serde(default)]
    #[builder(default)]
    pub tokens_partial: bool,
    pub execution_id: String,
    pub run_id: String,
    pub work_item_id: String,
    pub short_id: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub kind: String,
    pub name: String,
    pub output_tokens: Option<i64>,
    pub status: String,
    pub total_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_usd: Option<f64>,
    /// `true` when `estimated_usd` priced non-zero cache-write tokens at the
    /// single 5-minute `cache_write` fallback rate rather than distinct 5m/1h
    /// rates.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[builder(default)]
    pub unsplit_cache_write: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_epoch_s: Option<i64>,
}

/// A coordinator session in a `boss cost top` window whose usage could not be
/// read. It is never ranked and never scored as zero: it is listed so the
/// missing coordinator is visible as unknown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct CostUnmeasuredCoordinatorRow {
    pub execution_id: String,
    /// Display name of the coordinator session.
    pub label: String,
    pub status: String,
    /// Why the usage is unknown, verbatim from capture.
    pub reason: Option<String>,
}

/// How many unmeasured runs of one execution kind a `boss cost top` window
/// excluded from its ranking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnmeasuredKindCount {
    pub kind: String,
    pub count: u32,
}
/// Output shape for `boss cost top` — the highest-token-cost runs
/// recorded in `[since_epoch_s, until_epoch_s)`, ranked descending by
/// `total_tokens`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct TopCostReport {
    pub rows: Vec<CostTopRow>,
    /// Runs in the window with NULL token columns — they cannot be
    /// ranked by cost, so they are excluded from `rows` entirely
    /// rather than sorted as if they cost zero. Counted here so the
    /// omission is visible instead of silent.
    pub excluded_unmeasured_count: u32,
    /// Coordinator sessions in the window whose usage is unknown (also
    /// counted in `excluded_unmeasured_count`), so an unreadable coordinator
    /// is shown as unknown rather than silently missing.
    #[serde(default)]
    #[builder(default)]
    pub unmeasured_coordinator: Vec<CostUnmeasuredCoordinatorRow>,
    /// `excluded_unmeasured_count` broken down by execution kind, so an
    /// unmeasured run (a review guide, say) is still identifiable by kind.
    /// Sorted by kind; counts sum to `excluded_unmeasured_count`.
    #[serde(default)]
    #[builder(default)]
    pub excluded_unmeasured_by_kind: Vec<UnmeasuredKindCount>,
    pub generated_at_epoch_s: i64,
    pub limit: u32,
    pub pricing_gaps: Vec<String>,
    pub since_epoch_s: i64,
    /// `true` when more measured runs matched the window than `limit`
    /// allowed — `rows` is the top slice, not the full set.
    pub truncated: bool,
    pub until_epoch_s: i64,
}

/// Eligible Codex outcomes grouped by the original PR-family assignment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct SolArmReport {
    pub arm: String,
    pub ci_observed_prs: u64,
    pub first_attempt_ci_passes: u64,
    pub merged_prs: u64,
    pub prs: u64,
    pub review_findings: u64,
    pub revisions_before_merge: u64,
    pub merged_pr_cost: super::CostMeasurement,
    pub first_attempt_ci_pass_rate: Option<f64>,
    pub findings_per_pr: Option<f64>,
    pub revisions_per_merged_pr: Option<f64>,
    /// Mean tokens over merged families whose every run was measured; NULL
    /// when no merged family is fully measured.
    pub tokens_per_merged_pr: Option<f64>,
    /// Merged families left out of `tokens_per_merged_pr` because at least one
    /// of their runs has no token measurement.
    #[serde(default)]
    #[builder(default)]
    pub merged_prs_tokens_unmeasured: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SolComparisonReport {
    pub arms: Vec<SolArmReport>,
    pub notes: Vec<String>,
}
