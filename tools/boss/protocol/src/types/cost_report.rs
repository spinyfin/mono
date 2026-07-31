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
//! `runs_unmeasured` counts NULL rows (excluded from every token
//! sum); `runs_zero` counts rows with a real, recorded zero. Token
//! sums are never computed by coalescing NULL to zero.

use serde::{Deserialize, Serialize};

/// Aggregate token/cost figures for a set of `work_runs` rows. Shared
/// shape used at every granularity — a single bucket, a task's
/// overall total, a report's grand total.
///
/// `runs_total == runs_measured + runs_unmeasured`. `runs_zero` is a
/// subset of `runs_measured` (a measured run whose recorded tokens
/// happen to be exactly zero, e.g. a run that failed before calling
/// a model). Every token field sums only `runs_measured` rows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct CostMeasurement {
    pub agent_active_ms: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    /// `true` when at least one run folded into this measurement used
    /// a model absent from the pricing table, meaning `estimated_usd`
    /// (when present) understates the true total. `false` when either
    /// every priced-eligible run had a known price, or no USD
    /// estimate was requested at all.
    pub estimated_usd_partial: bool,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Runs whose tokens were recorded and summed to a non-zero total.
    pub runs_measured: u32,
    pub runs_total: u32,
    /// Runs with NULL token columns — cost unknown, excluded from
    /// every sum in this measurement. Never conflated with a
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
}

/// One labelled slice of a [`WindowCostReport`] breakdown (by
/// execution kind, model, reasoning mode, or effort level).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostBucket {
    /// Grouping key as recorded on the row, e.g. `"chore_implementation"`
    /// (kind), a model slug, `"standard"` (reasoning), `"medium"`
    /// (effort level), or `"(none)"` when the underlying column was NULL.
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
/// aggregated across every execution and run it has ever had.
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
/// to know what it was.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct CostTopRow {
    pub execution_id: String,
    pub run_id: String,
    pub work_item_id: String,
    pub short_id: Option<i64>,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub input_tokens: i64,
    pub kind: String,
    pub name: String,
    pub output_tokens: i64,
    pub status: String,
    pub total_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_epoch_s: Option<i64>,
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
    pub generated_at_epoch_s: i64,
    pub limit: u32,
    pub pricing_gaps: Vec<String>,
    pub since_epoch_s: i64,
    /// `true` when more measured runs matched the window than `limit`
    /// allowed — `rows` is the top slice, not the full set.
    pub truncated: bool,
    pub until_epoch_s: i64,
}
