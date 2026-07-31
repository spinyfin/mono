//! Pure aggregation logic behind `boss cost` — turns a flat list of
//! per-`work_runs`-row cost records into the three report shapes the CLI
//! renders (`TaskCostReport`, `WindowCostReport`, `TopCostReport`).
//!
//! Mirrors [`crate::audit_effort`]'s split: the `work` DB layer projects
//! rows into [`CostRunRecord`], and every report is a pure function of
//! that slice — easy to unit test without a database, and the caller
//! (`app/cost.rs`) stays a thin fetch-then-build shim.
//!
//! See `boss_protocol::CostMeasurement`'s doc comment for the NULL-vs-zero
//! contract every builder here upholds: a run with no recorded tokens
//! (`input_tokens: None`) is *unmeasured*, never folded into a sum as
//! zero.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use boss_protocol::{
    CostBucket, CostMeasurement, CostTopRow, ExecutionCostRow, TaskCostReport, TopCostReport, WindowCostReport,
};

use crate::cost_pricing;

/// 2026-07-27T09:23:00Z (2026-07-27 04:23 America/Chicago, CDT) — the
/// instant per-run token capture went live. Every `work_runs` row created
/// before this has NULL token columns by construction, not by data loss;
/// [`build_window_cost_report`] echoes this back so a window spanning it
/// can be flagged rather than silently under-reported.
pub const TOKEN_CAPTURE_START_EPOCH_S: i64 = 1_785_144_180;

/// One `work_runs` row, projected with just enough context to group and
/// rank it. The `work` DB layer's production mapper builds these with a
/// struct literal (every column named explicitly, per this repo's DB
/// mapper convention); tests may use the builder.
#[derive(Debug, Clone, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct CostRunRecord {
    pub run_id: String,
    pub execution_id: String,
    pub work_item_id: String,
    pub work_item_name: String,
    pub execution_kind: String,
    pub execution_status: String,
    pub run_status: String,
    pub created_at_epoch_s: i64,
    pub work_item_short_id: Option<i64>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub effort_level: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub agent_active_ms: Option<i64>,
    pub started_at_epoch_s: Option<i64>,
}

const NO_VALUE_LABEL: &str = "(none)";
const NO_MODEL_GAP_LABEL: &str = "(no model recorded)";

/// Run-count breakdown within one [`Accumulator`]: measured/unmeasured/zero
/// per the NULL-vs-zero contract, plus the total folded in.
#[derive(Default)]
struct RunCounts {
    total: u32,
    measured: u32,
    unmeasured: u32,
    zero: u32,
}

/// Summed token counts within one [`Accumulator`], across only its
/// measured runs.
#[derive(Default)]
struct TokenTotals {
    input: i64,
    output: i64,
    cache_creation: i64,
    cache_read: i64,
}

impl TokenTotals {
    fn sum(&self) -> i64 {
        self.input + self.output + self.cache_creation + self.cache_read
    }
}

/// Running total for one grouping bucket. Folds [`CostRunRecord`]s one at
/// a time so every report can build every bucket in a single pass.
#[derive(Default)]
struct Accumulator {
    counts: RunCounts,
    tokens: TokenTotals,
    agent_active_ms: i64,
    priced_usd: f64,
    partial: bool,
}

impl Accumulator {
    /// Folds one record in. `gaps` collects the distinct unpriced model
    /// strings seen across the whole report (not just this bucket) so the
    /// caller can surface them once at the report level.
    fn add(&mut self, record: &CostRunRecord, want_usd: bool, gaps: &mut BTreeSet<String>) {
        self.counts.total += 1;
        let Some(input_tokens) = record.input_tokens else {
            self.counts.unmeasured += 1;
            return;
        };
        self.counts.measured += 1;
        let output_tokens = record.output_tokens.unwrap_or(0);
        let cache_creation_tokens = record.cache_creation_tokens.unwrap_or(0);
        let cache_read_tokens = record.cache_read_tokens.unwrap_or(0);
        if input_tokens == 0 && output_tokens == 0 && cache_creation_tokens == 0 && cache_read_tokens == 0 {
            self.counts.zero += 1;
        }
        self.tokens.input += input_tokens;
        self.tokens.output += output_tokens;
        self.tokens.cache_creation += cache_creation_tokens;
        self.tokens.cache_read += cache_read_tokens;
        self.agent_active_ms += record.agent_active_ms.unwrap_or(0);
        if want_usd {
            match record.model.as_deref().and_then(cost_pricing::price_for_model) {
                Some(price) => {
                    self.priced_usd += cost_pricing::estimate_usd(
                        price,
                        input_tokens,
                        output_tokens,
                        cache_creation_tokens,
                        cache_read_tokens,
                    );
                }
                None => {
                    self.partial = true;
                    gaps.insert(record.model.as_deref().unwrap_or(NO_MODEL_GAP_LABEL).to_owned());
                }
            }
        }
    }

    fn finish(&self, want_usd: bool) -> CostMeasurement {
        CostMeasurement::builder()
            .agent_active_ms(self.agent_active_ms)
            .cache_creation_tokens(self.tokens.cache_creation)
            .cache_read_tokens(self.tokens.cache_read)
            .estimated_usd_partial(want_usd && self.partial)
            .input_tokens(self.tokens.input)
            .output_tokens(self.tokens.output)
            .runs_measured(self.counts.measured)
            .runs_total(self.counts.total)
            .runs_unmeasured(self.counts.unmeasured)
            .runs_zero(self.counts.zero)
            .total_tokens(self.tokens.sum())
            .maybe_estimated_usd(want_usd.then_some(self.priced_usd))
            .build()
    }
}

fn label_or_none(value: Option<&str>) -> String {
    value.map(str::to_owned).unwrap_or_else(|| NO_VALUE_LABEL.to_owned())
}

fn finish_buckets(map: BTreeMap<String, Accumulator>, want_usd: bool) -> Vec<CostBucket> {
    let mut buckets: Vec<CostBucket> = map
        .into_iter()
        .map(|(label, acc)| CostBucket {
            label,
            measurement: acc.finish(want_usd),
        })
        .collect();
    // Highest-spend bucket first — the decision-relevant ordering for a
    // "where did the budget go" report. Ties break alphabetically for
    // deterministic output.
    buckets.sort_by(|a, b| {
        b.measurement
            .total_tokens
            .cmp(&a.measurement.total_tokens)
            .then_with(|| a.label.cmp(&b.label))
    });
    buckets
}

/// Build the `boss cost task <id>` report: overall totals plus a
/// per-execution breakdown, in the order executions were first seen in
/// `records` (oldest first, matching how executions are normally listed).
pub fn build_task_cost_report(
    work_item_id: &str,
    short_id: Option<i64>,
    name: &str,
    records: &[CostRunRecord],
    want_usd: bool,
    generated_at_epoch_s: i64,
) -> TaskCostReport {
    struct ExecutionAcc {
        kind: String,
        status: String,
        models: BTreeSet<String>,
        acc: Accumulator,
    }

    let mut overall = Accumulator::default();
    let mut gaps: BTreeSet<String> = BTreeSet::new();
    let mut exec_order: Vec<String> = Vec::new();
    let mut exec_map: HashMap<String, ExecutionAcc> = HashMap::new();

    for record in records {
        overall.add(record, want_usd, &mut gaps);
        let entry = exec_map.entry(record.execution_id.clone()).or_insert_with(|| {
            exec_order.push(record.execution_id.clone());
            ExecutionAcc {
                kind: record.execution_kind.clone(),
                status: record.execution_status.clone(),
                models: BTreeSet::new(),
                acc: Accumulator::default(),
            }
        });
        entry.acc.add(record, want_usd, &mut gaps);
        if let Some(model) = &record.model {
            entry.models.insert(model.clone());
        }
    }

    let by_execution = exec_order
        .into_iter()
        .map(|execution_id| {
            let entry = exec_map.remove(&execution_id).expect("execution present after fold");
            ExecutionCostRow {
                execution_id,
                kind: entry.kind,
                models: entry.models.into_iter().collect(),
                status: entry.status,
                measurement: entry.acc.finish(want_usd),
            }
        })
        .collect();

    TaskCostReport::builder()
        .work_item_id(work_item_id)
        .maybe_short_id(short_id)
        .by_execution(by_execution)
        .generated_at_epoch_s(generated_at_epoch_s)
        .name(name)
        .overall(overall.finish(want_usd))
        .pricing_gaps(gaps.into_iter().collect())
        .build()
}

/// Build the `boss cost window` report: totals over `[since_epoch_s,
/// until_epoch_s)`, broken down by execution kind, model, reasoning mode,
/// and effort level. `records` must already be filtered to the window —
/// this function does not look at timestamps beyond echoing the bounds
/// back and comparing `since_epoch_s` to [`TOKEN_CAPTURE_START_EPOCH_S`].
pub fn build_window_cost_report(
    since_epoch_s: i64,
    until_epoch_s: i64,
    records: &[CostRunRecord],
    want_usd: bool,
    generated_at_epoch_s: i64,
) -> WindowCostReport {
    let mut overall = Accumulator::default();
    let mut gaps: BTreeSet<String> = BTreeSet::new();
    let mut by_kind: BTreeMap<String, Accumulator> = BTreeMap::new();
    let mut by_model: BTreeMap<String, Accumulator> = BTreeMap::new();
    let mut by_reasoning: BTreeMap<String, Accumulator> = BTreeMap::new();
    let mut by_effort_level: BTreeMap<String, Accumulator> = BTreeMap::new();

    for record in records {
        overall.add(record, want_usd, &mut gaps);
        by_kind
            .entry(record.execution_kind.clone())
            .or_default()
            .add(record, want_usd, &mut gaps);
        by_model
            .entry(label_or_none(record.model.as_deref()))
            .or_default()
            .add(record, want_usd, &mut gaps);
        by_reasoning
            .entry(label_or_none(record.reasoning.as_deref()))
            .or_default()
            .add(record, want_usd, &mut gaps);
        by_effort_level
            .entry(label_or_none(record.effort_level.as_deref()))
            .or_default()
            .add(record, want_usd, &mut gaps);
    }

    WindowCostReport::builder()
        .by_effort_level(finish_buckets(by_effort_level, want_usd))
        .by_kind(finish_buckets(by_kind, want_usd))
        .by_model(finish_buckets(by_model, want_usd))
        .by_reasoning(finish_buckets(by_reasoning, want_usd))
        .capture_start_epoch_s(TOKEN_CAPTURE_START_EPOCH_S)
        .generated_at_epoch_s(generated_at_epoch_s)
        .overall(overall.finish(want_usd))
        .pricing_gaps(gaps.into_iter().collect())
        .since_epoch_s(since_epoch_s)
        .spans_capture_boundary(since_epoch_s < TOKEN_CAPTURE_START_EPOCH_S)
        .until_epoch_s(until_epoch_s)
        .build()
}

/// Build the `boss cost top` report: the `limit` highest-`total_tokens`
/// runs in `records`, descending. A record with no recorded tokens
/// cannot be ranked and is excluded from `rows` — counted in
/// `excluded_unmeasured_count` instead of being sorted in as a zero.
pub fn build_top_cost_report(
    since_epoch_s: i64,
    until_epoch_s: i64,
    limit: u32,
    records: &[CostRunRecord],
    want_usd: bool,
    generated_at_epoch_s: i64,
) -> TopCostReport {
    let mut gaps: BTreeSet<String> = BTreeSet::new();
    let mut excluded_unmeasured_count = 0u32;
    let mut rows: Vec<CostTopRow> = Vec::new();

    for record in records {
        let Some(input_tokens) = record.input_tokens else {
            excluded_unmeasured_count += 1;
            continue;
        };
        let output_tokens = record.output_tokens.unwrap_or(0);
        let cache_creation_tokens = record.cache_creation_tokens.unwrap_or(0);
        let cache_read_tokens = record.cache_read_tokens.unwrap_or(0);
        let total_tokens = input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens;

        let estimated_usd = want_usd
            .then(|| record.model.as_deref().and_then(cost_pricing::price_for_model))
            .flatten()
            .map(|price| {
                cost_pricing::estimate_usd(
                    price,
                    input_tokens,
                    output_tokens,
                    cache_creation_tokens,
                    cache_read_tokens,
                )
            });
        if want_usd && estimated_usd.is_none() {
            gaps.insert(record.model.as_deref().unwrap_or(NO_MODEL_GAP_LABEL).to_owned());
        }

        rows.push(
            CostTopRow::builder()
                .execution_id(record.execution_id.clone())
                .run_id(record.run_id.clone())
                .work_item_id(record.work_item_id.clone())
                .maybe_short_id(record.work_item_short_id)
                .cache_creation_tokens(cache_creation_tokens)
                .cache_read_tokens(cache_read_tokens)
                .input_tokens(input_tokens)
                .kind(record.execution_kind.clone())
                .name(record.work_item_name.clone())
                .output_tokens(output_tokens)
                .status(record.run_status.clone())
                .total_tokens(total_tokens)
                .maybe_effort_level(record.effort_level.clone())
                .maybe_estimated_usd(estimated_usd)
                .maybe_model(record.model.clone())
                .maybe_reasoning(record.reasoning.clone())
                .maybe_started_at_epoch_s(record.started_at_epoch_s)
                .build(),
        );
    }

    rows.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
    let truncated = rows.len() > limit as usize;
    rows.truncate(limit as usize);

    TopCostReport::builder()
        .rows(rows)
        .excluded_unmeasured_count(excluded_unmeasured_count)
        .generated_at_epoch_s(generated_at_epoch_s)
        .limit(limit)
        .pricing_gaps(gaps.into_iter().collect())
        .since_epoch_s(since_epoch_s)
        .truncated(truncated)
        .until_epoch_s(until_epoch_s)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(run_id: &str, execution_id: &str) -> CostRunRecord {
        CostRunRecord::builder()
            .run_id(run_id)
            .execution_id(execution_id)
            .work_item_id("task_1")
            .work_item_name("Fix cursor flicker")
            .execution_kind("chore_implementation")
            .execution_status("completed")
            .run_status("completed")
            .created_at_epoch_s(1_800_000_000)
            .build()
    }

    fn measured(mut r: CostRunRecord, input: i64, output: i64, cache_creation: i64, cache_read: i64) -> CostRunRecord {
        r.input_tokens = Some(input);
        r.output_tokens = Some(output);
        r.cache_creation_tokens = Some(cache_creation);
        r.cache_read_tokens = Some(cache_read);
        r
    }

    #[test]
    fn unmeasured_runs_are_excluded_from_sums_not_zeroed() {
        let records = vec![
            measured(record("run_1", "exec_1"), 100, 50, 0, 0),
            record("run_2", "exec_1"), // NULL tokens — unmeasured
        ];
        let report = build_task_cost_report("task_1", Some(42), "Fix cursor flicker", &records, false, 1_800_000_100);
        assert_eq!(report.overall.runs_total, 2);
        assert_eq!(report.overall.runs_measured, 1);
        assert_eq!(report.overall.runs_unmeasured, 1);
        assert_eq!(report.overall.runs_zero, 0);
        // Sum comes only from the measured run — the unmeasured run's
        // absence must not silently read as an extra zero.
        assert_eq!(report.overall.total_tokens, 150);
    }

    #[test]
    fn a_recorded_zero_is_distinct_from_unmeasured() {
        let records = vec![measured(record("run_1", "exec_1"), 0, 0, 0, 0)];
        let report = build_task_cost_report("task_1", None, "t", &records, false, 0);
        assert_eq!(report.overall.runs_measured, 1);
        assert_eq!(report.overall.runs_unmeasured, 0);
        assert_eq!(report.overall.runs_zero, 1);
        assert_eq!(report.overall.total_tokens, 0);
    }

    #[test]
    fn task_report_groups_runs_by_execution_in_first_seen_order() {
        let records = vec![
            measured(record("run_1", "exec_a"), 10, 0, 0, 0),
            measured(record("run_2", "exec_b"), 20, 0, 0, 0),
            measured(record("run_3", "exec_a"), 5, 0, 0, 0),
        ];
        let report = build_task_cost_report("task_1", None, "t", &records, false, 0);
        assert_eq!(report.by_execution.len(), 2);
        assert_eq!(report.by_execution[0].execution_id, "exec_a");
        assert_eq!(report.by_execution[0].measurement.total_tokens, 15);
        assert_eq!(report.by_execution[1].execution_id, "exec_b");
        assert_eq!(report.by_execution[1].measurement.total_tokens, 20);
    }

    #[test]
    fn window_report_flags_spanning_the_capture_boundary() {
        let records: Vec<CostRunRecord> = vec![];
        let before = build_window_cost_report(
            TOKEN_CAPTURE_START_EPOCH_S - 1,
            TOKEN_CAPTURE_START_EPOCH_S + 100,
            &records,
            false,
            0,
        );
        assert!(before.spans_capture_boundary);
        let after = build_window_cost_report(
            TOKEN_CAPTURE_START_EPOCH_S + 1,
            TOKEN_CAPTURE_START_EPOCH_S + 100,
            &records,
            false,
            0,
        );
        assert!(!after.spans_capture_boundary);
    }

    #[test]
    fn window_report_buckets_synthetic_model_on_its_own_without_dropping_it() {
        let mut r = measured(record("run_1", "exec_1"), 10, 5, 0, 0);
        r.model = Some("<synthetic>".to_owned());
        let report = build_window_cost_report(0, i64::MAX, &[r], false, 0);
        assert_eq!(report.by_model.len(), 1);
        assert_eq!(report.by_model[0].label, "<synthetic>");
        assert_eq!(report.by_model[0].measurement.total_tokens, 15);
    }

    #[test]
    fn window_report_buckets_sort_by_total_tokens_descending() {
        let mut small = measured(record("run_1", "exec_1"), 10, 0, 0, 0);
        small.model = Some("haiku".to_owned());
        let mut big = measured(record("run_2", "exec_2"), 1000, 0, 0, 0);
        big.model = Some("opus".to_owned());
        let report = build_window_cost_report(0, i64::MAX, &[small, big], false, 0);
        assert_eq!(report.by_model[0].label, "opus");
        assert_eq!(report.by_model[1].label, "haiku");
    }

    #[test]
    fn top_report_excludes_unmeasured_runs_from_ranking() {
        let records = vec![
            measured(record("run_1", "exec_1"), 10, 0, 0, 0),
            record("run_2", "exec_1"),
        ];
        let report = build_top_cost_report(0, i64::MAX, 10, &records, false, 0);
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.excluded_unmeasured_count, 1);
    }

    #[test]
    fn top_report_ranks_descending_and_truncates_with_flag() {
        let records = vec![
            measured(record("run_1", "exec_1"), 10, 0, 0, 0),
            measured(record("run_2", "exec_1"), 1000, 0, 0, 0),
            measured(record("run_3", "exec_1"), 500, 0, 0, 0),
        ];
        let report = build_top_cost_report(0, i64::MAX, 2, &records, false, 0);
        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows[0].run_id, "run_2");
        assert_eq!(report.rows[1].run_id, "run_3");
        assert!(report.truncated);
    }

    #[test]
    fn usd_estimate_is_partial_when_a_model_is_unpriced() {
        let mut priced = measured(record("run_1", "exec_1"), 1_000_000, 0, 0, 0);
        priced.model = Some("claude-sonnet-5".to_owned());
        let mut unpriced = measured(record("run_2", "exec_1"), 1_000_000, 0, 0, 0);
        unpriced.model = Some("gpt-5-codex".to_owned());
        let report = build_task_cost_report("task_1", None, "t", &[priced, unpriced], true, 0);
        assert!(report.overall.estimated_usd_partial);
        assert!(report.overall.estimated_usd.is_some());
        // Only the priced run's cost is counted — the unpriced run's
        // tokens are excluded from the dollar figure, not assumed free.
        assert!((report.overall.estimated_usd.unwrap() - 3.0).abs() < 1e-9);
        assert_eq!(report.pricing_gaps, vec!["gpt-5-codex".to_owned()]);
    }

    #[test]
    fn usd_estimate_absent_when_not_requested() {
        let records = vec![measured(record("run_1", "exec_1"), 10, 0, 0, 0)];
        let report = build_task_cost_report("task_1", None, "t", &records, false, 0);
        assert!(report.overall.estimated_usd.is_none());
        assert!(!report.overall.estimated_usd_partial);
        assert!(report.pricing_gaps.is_empty());
    }
}
