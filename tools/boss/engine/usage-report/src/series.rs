//! Bucketed token usage over time with normalizer ratios and honest range
//! coverage: the pure builder behind `boss cost series` and the Token Usage
//! window's Over-time tab.
//!
//! ## Folding into the metrics catalog (`GetMetricSeries`)
//!
//! This request exists so the Token Usage window does not wait on the
//! performance-metrics design's generic `GetMetricSeries` RPC (see
//! `tools/boss/docs/designs/token-usage-window.md` §4 and
//! `tools/boss/docs/designs/performance-metrics-timeseries-capture-and-interactive-swiftui-charts.md`).
//! Once `GetMetricSeries` and its metrics catalog land, this builder folds into
//! the catalog without rewriting its arithmetic or tests:
//!
//! * [`UsageCell::measurement`] maps directly to the catalog's `Tokens` and
//!   `Usd` value kinds (`tokens`, `cost_usd`).
//! * `executions_completed`, `tasks_completed`, `distinct_prs` map to `Count`
//!   catalog series over the same projection, and each field of [`UsageRatios`]
//!   maps to a `Ratio` catalog series.
//! * A thin catalog handler re-wraps [`build_usage_series_report`]'s output
//!   into `MetricSeriesReport`, with a one-line catalog registration per series.
//! * `GetTokenUsageSeries` should be deleted only once the app's Over-time tab
//!   consumes `GetMetricSeries`, so there is never a window with two series
//!   contracts serving the same view.
//!
//! ## Honesty rules enforced here
//!
//! * **Absent bucket for no facts.** A bucket with no run, no completed
//!   execution, no finished PR execution and no completed chain-root task is
//!   omitted from the reply; clients never fabricate zeros.
//! * **Per-class NULL preservation.** Bucket sums use the same [`RunFold`] as
//!   `boss cost` and `boss cost rows`: a bucket whose measured runs lack a
//!   token class (e.g. Grok or Antigravity lacking `cache_creation`) reports
//!   that class as `None` with `tokens_partial = true`, never zero.
//! * **Zero denominator or unmeasured numerator → `None`.** Every ratio in
//!   [`UsageRatios`] returns `None` when its denominator is zero or its
//!   numerator has no measured runs.
//! * **Active-hour restriction.** `tokens_per_active_hour` restricts **both**
//!   numerator and denominator to measured runs that have `agent_active_ms`
//!   recorded (Grok and some Antigravity runs report tokens without active
//!   time), and emits an `active_time_missing` coverage note counting how many
//!   measured runs were excluded.
//! * **Attributed inclusive task cost.** `tasks_completed_cost` attributes the
//!   full inclusive chain cost (root + revisions, whenever spent, including
//!   out-of-window runs) to the bucket of `tasks.completed_at`.
//! * **Coarsen, never truncate.** Past [`DEFAULT_CELL_CAP`] cells, the builder
//!   coarsens the bucket width along `hour → day → week → month` and emits a
//!   `bucket_coarsened` note instead of silently dropping buckets. If the
//!   coarsest width still exceeds the cap (or `month` was requested and is
//!   already over), every fact is kept and a `cell_cap_exceeded` note says so.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use boss_protocol::{
    COORDINATOR_USAGE_KIND, CostMeasurement, DriverSource, RunCoverage, SpendClass, TokenUsageSeriesReport,
    UsageBucket, UsageBucketWidth, UsageCaveatCount, UsageCell, UsageDimension, UsageFilter, UsageRatios,
    UsageSeriesCoverage, UsageSeriesNote,
};

use crate::classify::{
    ClassifyContext, DimensionSource, RunFacts, UsdEstimator, classify, dimension_value, dimension_value_of,
    for_each_caveat_code, has_recorded_dimension, known_total, latest_run, price_if_requested, spend_class,
};
use crate::fold::RunFold;
use crate::record::{UsageExecutionRecord, UsageSeriesRecords, UsageTaskRecord};
use crate::rows::{compile_filters, note, passes_filters, pricing_gaps};

/// Maximum `(bucket, group)` cells one series reply carries before the engine
/// coarsens the bucket width along `hour → day → week → month`.
pub const DEFAULT_CELL_CAP: usize = 5_000;

/// Group key emitted when `group_by` is `None`.
pub const ALL_GROUP: &str = "(all)";

const DAY_SECS: i64 = 86_400;
const HOUR_MAX_SPAN_SECS: i64 = 3 * DAY_SECS;
const DAY_MAX_SPAN_SECS: i64 = 200 * DAY_SECS;
const WEEK_MAX_SPAN_SECS: i64 = 4 * 365 * DAY_SECS;
/// Unix epoch 0 was Thursday 1970-01-01; offset by 4 days so week buckets
/// start on Monday 00:00:00 UTC.
const WEEK_EPOCH_OFFSET_S: i64 = 4 * DAY_SECS;
const MS_PER_HOUR: f64 = 3_600_000.0;

/// What a series request asks for, plus the engine constants the report
/// depends on (so this crate owns none of them).
#[derive(Debug, Clone, bon::Builder)]
pub struct UsageSeriesQuery {
    pub since_epoch_s: i64,
    pub until_epoch_s: i64,
    pub generated_at_epoch_s: i64,
    /// Before this instant no run has token data (`TOKEN_CAPTURE_START_EPOCH_S`).
    pub capture_start_epoch_s: i64,
    /// When `tasks.completed_at` went live (`COMPLETED_AT_MIGRATION_EPOCH_S`);
    /// terminal tasks before this were backfilled with `created_at`.
    pub completed_at_start_epoch_s: i64,
    /// The age past which never-started failed, abandoned, orphaned and
    /// cancelled executions become eligible for pruning
    /// (`DEFAULT_RETENTION_MAX_AGE_SECS`); executions with a worker run are
    /// kept.
    pub retention_max_age_secs: i64,
    pub bucket: Option<UsageBucketWidth>,
    pub group_by: Option<UsageDimension>,
    #[builder(default)]
    pub filters: Vec<UsageFilter>,
    #[builder(default = DEFAULT_CELL_CAP)]
    pub cell_cap: usize,
}

impl UsageSeriesQuery {
    /// Rejects a request the builder cannot meaningfully satisfy.
    pub fn validate(&self) -> Result<(), String> {
        if self.until_epoch_s <= self.since_epoch_s {
            return Err("until_epoch_s must be after since_epoch_s".to_owned());
        }
        if self.cell_cap == 0 {
            return Err("cell_cap must be at least 1".to_owned());
        }
        Ok(())
    }
}

/// Engine-chosen bucket width when the client does not specify one:
/// hour for ranges up to 3 days, day up to 200 days, week up to 4 years,
/// month beyond.
pub fn choose_bucket_width(since_epoch_s: i64, until_epoch_s: i64) -> UsageBucketWidth {
    let span = until_epoch_s.saturating_sub(since_epoch_s);
    if span <= HOUR_MAX_SPAN_SECS {
        UsageBucketWidth::Hour
    } else if span <= DAY_MAX_SPAN_SECS {
        UsageBucketWidth::Day
    } else if span <= WEEK_MAX_SPAN_SECS {
        UsageBucketWidth::Week
    } else {
        UsageBucketWidth::Month
    }
}

/// Start timestamp (epoch seconds) of the bucket containing `epoch_s`.
/// Hour and day are UTC-midnight aligned; week is Monday 00:00:00 UTC;
/// month is the UTC calendar-month start.
pub fn bucket_start_epoch_s(epoch_s: i64, width: UsageBucketWidth) -> i64 {
    let secs = width.bucket_secs();
    match width {
        UsageBucketWidth::Week => WEEK_EPOCH_OFFSET_S + (epoch_s - WEEK_EPOCH_OFFSET_S).div_euclid(secs) * secs,
        UsageBucketWidth::Hour | UsageBucketWidth::Day => epoch_s.div_euclid(secs) * secs,
        UsageBucketWidth::Month => month_start_epoch_s(epoch_s),
    }
}

/// UTC calendar-month start containing `epoch_s`.
fn month_start_epoch_s(epoch_s: i64) -> i64 {
    let days = epoch_s.div_euclid(DAY_SECS);
    let (year, month, _) = civil_from_epoch_days(days);
    epoch_days_from_civil(year, month, 1) * DAY_SECS
}

/// Days since Unix epoch for a UTC civil date (Howard Hinnant's algorithm).
fn epoch_days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let mut year = i64::from(year);
    let month = i64::from(month);
    let day = i64::from(day);
    if month <= 2 {
        year -= 1;
    }
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_epoch_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year as i32, month as u32, day as u32)
}

/// Builds the bucketed usage series report. `pricer` is `Some` exactly when
/// the caller requested USD estimates.
pub fn build_usage_series_report(
    query: &UsageSeriesQuery,
    records: &UsageSeriesRecords,
    pricer: Option<&dyn UsdEstimator>,
) -> TokenUsageSeriesReport {
    let tasks: HashMap<&str, &UsageTaskRecord> =
        records.tasks.iter().map(|task| (task.task_id.as_str(), task)).collect();
    let compiled_filters = compile_filters(&query.filters);
    let classify_ctx = ClassifyContext {
        capture_start_epoch_s: query.capture_start_epoch_s,
        pricer,
    };

    // 1. Classify every run once (deduped by run_id). `records.runs` carries
    //    both in-window runs and out-of-window chain runs of completed tasks.
    let mut seen_runs: HashSet<&str> = HashSet::with_capacity(records.runs.len());
    let classified_runs: Vec<RunFacts<'_>> = records
        .runs
        .iter()
        .filter(|run| seen_runs.insert(run.run_id.as_str()))
        .map(|run| {
            let mut facts = classify(run, tasks.get(run.work_item_id.as_str()).copied());
            price_if_requested(&mut facts, &classify_ctx);
            facts
        })
        .collect();

    let mut runs_by_execution: HashMap<&str, Vec<&RunFacts<'_>>> = HashMap::new();
    let mut chain_runs_by_root: HashMap<&str, Vec<&RunFacts<'_>>> = HashMap::new();
    let mut window_runs: Vec<&RunFacts<'_>> = Vec::new();
    for facts in &classified_runs {
        runs_by_execution
            .entry(facts.rec.execution_id.as_str())
            .or_default()
            .push(facts);
        if let Some(task) = facts.task {
            let root = root_of(task, &tasks);
            chain_runs_by_root.entry(root).or_default().push(facts);
        }
        if facts.rec.run_created_at_epoch_s >= query.since_epoch_s
            && facts.rec.run_created_at_epoch_s < query.until_epoch_s
            && passes_filters(facts, &compiled_filters, None)
        {
            window_runs.push(facts);
        }
    }

    // 2. Filter finished executions in [since, until) that contribute either
    //    to `executions_completed` (status == completed) or `distinct_prs`.
    let mut seen_executions: HashSet<&str> = HashSet::with_capacity(records.finished_executions.len());
    let finished_execs: Vec<ExecFact<'_>> = records
        .finished_executions
        .iter()
        .filter(|exec| {
            exec.execution_finished_at_epoch_s >= query.since_epoch_s
                && exec.execution_finished_at_epoch_s < query.until_epoch_s
                && seen_executions.insert(exec.execution_id.as_str())
        })
        .filter_map(|exec| {
            let is_completed = exec.execution_status == "completed";
            let pr_url = exec.execution_pr_url.as_deref().filter(|url| !url.is_empty());
            if !is_completed && pr_url.is_none() {
                return None;
            }
            let task = tasks.get(exec.work_item_id.as_str()).copied();
            let exec_runs = runs_by_execution
                .get(exec.execution_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let fact = ExecFact {
                exec,
                task,
                runs: exec_runs,
                is_completed,
                pr_url,
            };
            fact.passes_filters(&compiled_filters).then_some(fact)
        })
        .collect();

    // 3. Collect completed chain-root tasks in [since, until) with their
    //    attributed all-time inclusive chain runs.
    let mut seen_tasks: HashSet<&str> = HashSet::new();
    let completed_tasks: Vec<CompletedTaskFact<'_>> = records
        .tasks
        .iter()
        .filter(|task| seen_tasks.insert(task.task_id.as_str()))
        .filter_map(|task| {
            // Only non-revision chain roots are counted, matching the SQL seed
            // (`t.kind != 'revision'`). An orphan revision whose parent is
            // missing resolves `root_of` to itself and must not be counted:
            // its out-of-window chain runs were never fetched.
            if task.kind == "revision" || root_of(task, &tasks) != task.task_id.as_str() {
                return None;
            }
            let completed_at = task.completed_at_epoch_s?;
            if completed_at < query.since_epoch_s || completed_at >= query.until_epoch_s {
                return None;
            }
            let chain_runs = chain_runs_by_root
                .get(task.task_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let representative = representative_run_for_task(task.task_id.as_str(), chain_runs);
            let fact = CompletedTaskFact {
                task,
                completed_at_epoch_s: completed_at,
                chain_runs,
                representative,
            };
            fact.passes_filters(&compiled_filters).then_some(fact)
        })
        .collect();

    // 4. Bucket facts and coarsen if the cell count exceeds `query.cell_cap`.
    let initial_width = query
        .bucket
        .unwrap_or_else(|| choose_bucket_width(query.since_epoch_s, query.until_epoch_s));
    let mut width = initial_width;
    let mut grid = bucket_facts(width, query.group_by, &window_runs, &finished_execs, &completed_tasks);
    while grid.cell_count() > query.cell_cap {
        let Some(next) = width.coarser() else {
            break;
        };
        width = next;
        grid = bucket_facts(width, query.group_by, &window_runs, &finished_execs, &completed_tasks);
    }
    let cell_count = grid.cell_count();
    let over_cell_cap = cell_count > query.cell_cap;

    // 5. Render order of groups, cells, buckets.
    let (groups, group_labels) = order_groups(query.group_by, &grid);
    let group_rank: HashMap<&str, usize> = groups
        .iter()
        .enumerate()
        .map(|(index, key)| (key.as_str(), index))
        .collect();

    let buckets: Vec<UsageBucket> = grid
        .buckets
        .into_iter()
        .map(|(start_epoch_s, cells_by_group)| {
            let mut finished: Vec<(String, CostMeasurement, CellDerived)> = cells_by_group
                .into_iter()
                .map(|(group, cell)| {
                    let (measurement, derived) = cell.finish();
                    (group, measurement, derived)
                })
                .collect();
            finished.sort_by_key(|(group, _, _)| group_rank.get(group.as_str()).copied().unwrap_or(usize::MAX));

            let bucket_measured_tokens: i64 = finished
                .iter()
                .filter(|(_, measurement, _)| measurement.runs_measured > 0)
                .map(|(_, measurement, _)| measurement.total_tokens)
                .sum();

            let cells: Vec<UsageCell> = finished
                .into_iter()
                .map(|(group, measurement, derived)| {
                    let share_of_bucket_total_tokens = (measurement.runs_measured > 0 && bucket_measured_tokens > 0)
                        .then(|| measurement.total_tokens as f64 / bucket_measured_tokens as f64);
                    let worker_runs_measured = measurement
                        .runs_measured
                        .saturating_sub(derived.coordinator_slices_measured);
                    let tokens_per_run = (worker_runs_measured > 0).then(|| {
                        (measurement.total_tokens - derived.coordinator_slice_tokens) as f64
                            / worker_runs_measured as f64
                    });
                    let tokens_per_completed_execution = (measurement.runs_measured > 0
                        && derived.executions_completed > 0)
                        .then(|| measurement.total_tokens as f64 / derived.executions_completed as f64);
                    let tokens_per_active_hour = (derived.active_measured_runs > 0 && derived.active_ms > 0)
                        .then(|| derived.active_tokens as f64 / (derived.active_ms as f64 / MS_PER_HOUR));
                    let tokens_per_completed_task = (derived.tasks_completed > 0
                        && derived.tasks_completed_cost.runs_measured > 0)
                        .then(|| derived.tasks_completed_cost.total_tokens as f64 / derived.tasks_completed as f64);
                    let tokens_per_pr = (measurement.runs_measured > 0 && derived.distinct_prs > 0)
                        .then(|| measurement.total_tokens as f64 / derived.distinct_prs as f64);

                    UsageCell::builder()
                        .distinct_prs(derived.distinct_prs)
                        .executions_completed(derived.executions_completed)
                        .group(group)
                        .measurement(measurement)
                        .ratios(
                            UsageRatios::builder()
                                .maybe_tokens_per_active_hour(tokens_per_active_hour)
                                .maybe_tokens_per_completed_execution(tokens_per_completed_execution)
                                .maybe_tokens_per_completed_task(tokens_per_completed_task)
                                .maybe_tokens_per_pr(tokens_per_pr)
                                .maybe_tokens_per_run(tokens_per_run)
                                .build(),
                        )
                        .tasks_completed(derived.tasks_completed)
                        .tasks_completed_cost(derived.tasks_completed_cost)
                        .maybe_share_of_bucket_total_tokens(share_of_bucket_total_tokens)
                        .build()
                })
                .collect();

            UsageBucket { start_epoch_s, cells }
        })
        .collect();

    // 6. Coverage, gaps and notes.
    let mut all_priced_runs: Vec<&RunFacts<'_>> = window_runs.clone();
    for task in &completed_tasks {
        all_priced_runs.extend(task.chain_runs.iter().copied());
    }
    let gaps = pricing_gaps(&all_priced_runs);
    let coverage = build_coverage(
        query,
        initial_width,
        width,
        &classify_ctx,
        (over_cell_cap, cell_count),
        CoverageFacts {
            window_runs: &window_runs,
            finished_execs: &finished_execs,
            completed_tasks: &completed_tasks,
            gaps: &gaps,
        },
    );

    TokenUsageSeriesReport::builder()
        .bucket_secs(width.bucket_secs())
        .bucket_width(width)
        .capture_start_epoch_s(query.capture_start_epoch_s)
        .coverage(coverage)
        .generated_at_epoch_s(query.generated_at_epoch_s)
        .since_epoch_s(query.since_epoch_s)
        .spans_capture_boundary(query.since_epoch_s < query.capture_start_epoch_s)
        .until_epoch_s(query.until_epoch_s)
        .buckets(buckets)
        .group_labels(group_labels)
        .groups(groups)
        .pricing_gaps(gaps.into_iter().collect())
        .maybe_group_by(query.group_by)
        .build()
}

// ── Fact helpers ──────────────────────────────────────────────────────────

fn root_of<'t>(task: &'t UsageTaskRecord, tasks: &HashMap<&str, &UsageTaskRecord>) -> &'t str {
    if task.chain_root_id != task.task_id && tasks.contains_key(task.chain_root_id.as_str()) {
        task.chain_root_id.as_str()
    } else {
        task.task_id.as_str()
    }
}

/// Representative run for attributing execution/run-level dimensions (driver,
/// model, effort, etc.) to a completed chain-root task: prefer direct
/// implementation/design runs on the root (latest), else any direct root run,
/// else any run in the chain.
fn representative_run_for_task<'a>(root_task_id: &str, chain_runs: &[&'a RunFacts<'a>]) -> Option<&'a RunFacts<'a>> {
    latest_run(chain_runs.iter().copied().filter(|facts| {
        facts.rec.work_item_id == root_task_id
            && matches!(facts.spend_class, SpendClass::Implementation | SpendClass::Design)
    }))
    .or_else(|| {
        latest_run(
            chain_runs
                .iter()
                .copied()
                .filter(|facts| facts.rec.work_item_id == root_task_id),
        )
    })
    .or_else(|| latest_run(chain_runs.iter().copied()))
}

struct ExecFact<'a> {
    exec: &'a UsageExecutionRecord,
    task: Option<&'a UsageTaskRecord>,
    runs: &'a [&'a RunFacts<'a>],
    is_completed: bool,
    pr_url: Option<&'a str>,
}

impl<'a> ExecFact<'a> {
    fn latest_run(&self) -> Option<&'a RunFacts<'a>> {
        latest_run(self.runs.iter().copied())
    }

    fn coverage(&self) -> Option<RunCoverage> {
        if self.runs.is_empty() {
            return None;
        }
        if self.runs.iter().all(|f| f.coverage == RunCoverage::Unmeasured) {
            Some(RunCoverage::Unmeasured)
        } else if self.runs.iter().any(|f| f.coverage == RunCoverage::Partial) {
            Some(RunCoverage::Partial)
        } else if self.runs.iter().all(|f| f.coverage == RunCoverage::Zero) {
            Some(RunCoverage::Zero)
        } else {
            Some(RunCoverage::Measured)
        }
    }

    fn dimension_source(&self) -> DimensionSource<'a> {
        let latest = self.latest_run();
        DimensionSource {
            task: self.task,
            execution_kind: Some(self.exec.execution_kind.as_str()),
            execution_status: Some(self.exec.execution_status.as_str()),
            execution_effort: self.exec.execution_effort.as_deref(),
            execution_model: self.exec.execution_model.as_deref(),
            driver: self.exec.driver.slug(),
            driver_source: self.exec.driver.source(),
            run_model: latest.and_then(|facts| facts.rec.run_model.as_deref()),
            coverage: self.coverage(),
            host_id: latest
                .map(|facts| facts.rec.run_host_id.as_str())
                .or(self.exec.execution_host_id.as_deref()),
            spend_class: Some(spend_class(&self.exec.execution_kind)),
        }
    }

    fn dimension_value(&self, dimension: UsageDimension) -> (&str, &str) {
        dimension_value_of(&self.dimension_source(), dimension)
    }

    fn has_recorded_dimension(&self, dimension: UsageDimension) -> bool {
        has_recorded_dimension(&self.dimension_source(), dimension)
    }

    fn passes_filters(&self, filters: &[(UsageDimension, HashSet<&str>)]) -> bool {
        filters.iter().all(|(dimension, values)| {
            let (value, _) = self.dimension_value(*dimension);
            values.contains(value)
        })
    }
}

struct CompletedTaskFact<'a> {
    task: &'a UsageTaskRecord,
    completed_at_epoch_s: i64,
    chain_runs: &'a [&'a RunFacts<'a>],
    representative: Option<&'a RunFacts<'a>>,
}

impl<'a> CompletedTaskFact<'a> {
    fn dimension_source(&self) -> DimensionSource<'a> {
        match self.representative {
            Some(facts) => {
                let mut src = DimensionSource::from_run(facts);
                src.task = Some(self.task);
                src
            }
            None => DimensionSource {
                task: Some(self.task),
                execution_kind: None,
                execution_status: None,
                execution_effort: None,
                execution_model: None,
                driver: None,
                driver_source: DriverSource::Unknown,
                run_model: None,
                coverage: None,
                host_id: None,
                spend_class: None,
            },
        }
    }

    fn dimension_value(&self, dimension: UsageDimension) -> (&str, &str) {
        dimension_value_of(&self.dimension_source(), dimension)
    }

    fn has_recorded_dimension(&self, dimension: UsageDimension) -> bool {
        has_recorded_dimension(&self.dimension_source(), dimension)
    }

    fn passes_filters(&self, filters: &[(UsageDimension, HashSet<&str>)]) -> bool {
        filters.iter().all(|(dimension, values)| {
            let (value, _) = self.dimension_value(*dimension);
            values.contains(value)
        })
    }
}

// ── Bucketing ─────────────────────────────────────────────────────────────

#[derive(Default, bon::Builder)]
struct CellAcc<'a> {
    measurement: RunFold,
    active_tokens: i64,
    active_ms: i64,
    active_measured_runs: u32,
    /// Measured coordinator hour slices and their tokens. A slice is an hour
    /// of coordinator activity, not a unit of work, so it stays out of the
    /// `tokens_per_run` numerator and denominator.
    coordinator_slices_measured: u32,
    coordinator_slice_tokens: i64,
    executions_completed: u32,
    distinct_prs: BTreeSet<&'a str>,
    tasks_completed: u32,
    tasks_completed_cost: RunFold,
}

impl<'a> CellAcc<'a> {
    fn add_run(&mut self, facts: &'a RunFacts<'a>) {
        self.measurement.add(&facts.sample());
        if facts.rec.execution_kind == COORDINATOR_USAGE_KIND && facts.coverage != RunCoverage::Unmeasured {
            self.coordinator_slices_measured += 1;
            self.coordinator_slice_tokens += known_total(facts.rec, facts.coverage).unwrap_or(0);
        }
        // Restrict BOTH numerator and denominator of `tokens_per_active_hour`
        // to measured runs that actually have `agent_active_ms` recorded.
        if facts.coverage != RunCoverage::Unmeasured
            && let Some(active_ms) = facts.rec.agent_active_ms
        {
            self.active_measured_runs += 1;
            self.active_ms += active_ms;
            self.active_tokens += known_total(facts.rec, facts.coverage).unwrap_or(0);
        }
    }

    fn add_execution(&mut self, exec: &ExecFact<'a>) {
        if exec.is_completed {
            self.executions_completed += 1;
        }
        if let Some(pr_url) = exec.pr_url {
            self.distinct_prs.insert(pr_url);
        }
    }

    fn add_completed_task(&mut self, task: &CompletedTaskFact<'a>) {
        self.tasks_completed += 1;
        for facts in task.chain_runs {
            self.tasks_completed_cost.add(&facts.sample());
        }
    }

    fn finish(self) -> (CostMeasurement, CellDerived) {
        (
            self.measurement.finish(),
            CellDerived {
                active_tokens: self.active_tokens,
                active_ms: self.active_ms,
                active_measured_runs: self.active_measured_runs,
                coordinator_slices_measured: self.coordinator_slices_measured,
                coordinator_slice_tokens: self.coordinator_slice_tokens,
                executions_completed: self.executions_completed,
                distinct_prs: self.distinct_prs.len() as u32,
                tasks_completed: self.tasks_completed,
                tasks_completed_cost: self.tasks_completed_cost.finish(),
            },
        )
    }
}

#[derive(bon::Builder)]
struct CellDerived {
    active_tokens: i64,
    active_ms: i64,
    active_measured_runs: u32,
    coordinator_slices_measured: u32,
    coordinator_slice_tokens: i64,
    executions_completed: u32,
    distinct_prs: u32,
    tasks_completed: u32,
    tasks_completed_cost: CostMeasurement,
}

struct BucketGrid<'a> {
    buckets: BTreeMap<i64, BTreeMap<String, CellAcc<'a>>>,
    group_labels: BTreeMap<String, String>,
    group_measured_tokens: BTreeMap<String, i64>,
    group_completed_task_tokens: BTreeMap<String, i64>,
}

impl BucketGrid<'_> {
    fn cell_count(&self) -> usize {
        self.buckets.values().map(BTreeMap::len).sum()
    }
}

fn bucket_facts<'a>(
    width: UsageBucketWidth,
    group_by: Option<UsageDimension>,
    window_runs: &[&'a RunFacts<'a>],
    finished_execs: &[ExecFact<'a>],
    completed_tasks: &[CompletedTaskFact<'a>],
) -> BucketGrid<'a> {
    let mut grid = BucketGrid {
        buckets: BTreeMap::new(),
        group_labels: BTreeMap::new(),
        group_measured_tokens: BTreeMap::new(),
        group_completed_task_tokens: BTreeMap::new(),
    };

    for &facts in window_runs {
        let bucket = bucket_start_epoch_s(facts.rec.run_created_at_epoch_s, width);
        let (group, label) = match group_by {
            Some(dimension) => {
                let (value, label) = dimension_value(facts, dimension);
                (value.to_owned(), label.to_owned())
            }
            None => (ALL_GROUP.to_owned(), "All".to_owned()),
        };
        grid.group_labels.entry(group.clone()).or_insert(label);
        if let Some(total) = known_total(facts.rec, facts.coverage) {
            *grid.group_measured_tokens.entry(group.clone()).or_default() += total;
        }
        grid.buckets
            .entry(bucket)
            .or_default()
            .entry(group)
            .or_default()
            .add_run(facts);
    }

    for exec in finished_execs {
        let bucket = bucket_start_epoch_s(exec.exec.execution_finished_at_epoch_s, width);
        let (group, label) = match group_by {
            Some(dimension) => {
                let (value, label) = exec.dimension_value(dimension);
                (value.to_owned(), label.to_owned())
            }
            None => (ALL_GROUP.to_owned(), "All".to_owned()),
        };
        grid.group_labels.entry(group.clone()).or_insert(label);
        grid.buckets
            .entry(bucket)
            .or_default()
            .entry(group)
            .or_default()
            .add_execution(exec);
    }

    for task in completed_tasks {
        let bucket = bucket_start_epoch_s(task.completed_at_epoch_s, width);
        let (group, label) = match group_by {
            Some(dimension) => {
                let (value, label) = task.dimension_value(dimension);
                (value.to_owned(), label.to_owned())
            }
            None => (ALL_GROUP.to_owned(), "All".to_owned()),
        };
        grid.group_labels.entry(group.clone()).or_insert(label);
        let task_tokens: i64 = task
            .chain_runs
            .iter()
            .filter_map(|facts| known_total(facts.rec, facts.coverage))
            .sum();
        *grid.group_completed_task_tokens.entry(group.clone()).or_default() += task_tokens;
        grid.buckets
            .entry(bucket)
            .or_default()
            .entry(group)
            .or_default()
            .add_completed_task(task);
    }

    grid
}

fn order_groups(group_by: Option<UsageDimension>, grid: &BucketGrid<'_>) -> (Vec<String>, BTreeMap<String, String>) {
    if group_by.is_none() {
        let mut labels = BTreeMap::new();
        labels.insert(ALL_GROUP.to_owned(), "All".to_owned());
        return (vec![ALL_GROUP.to_owned()], labels);
    }
    let mut groups: Vec<String> = grid.group_labels.keys().cloned().collect();
    groups.sort_by(|a, b| {
        let a_tok = grid.group_measured_tokens.get(a).copied().unwrap_or(0);
        let b_tok = grid.group_measured_tokens.get(b).copied().unwrap_or(0);
        let a_task = grid.group_completed_task_tokens.get(a).copied().unwrap_or(0);
        let b_task = grid.group_completed_task_tokens.get(b).copied().unwrap_or(0);
        b_tok
            .cmp(&a_tok)
            .then_with(|| b_task.cmp(&a_task))
            .then_with(|| a.cmp(b))
    });
    (groups, grid.group_labels.clone())
}

// ── Coverage and boundary notes ───────────────────────────────────────────

struct CoverageFacts<'a> {
    window_runs: &'a [&'a RunFacts<'a>],
    finished_execs: &'a [ExecFact<'a>],
    completed_tasks: &'a [CompletedTaskFact<'a>],
    gaps: &'a BTreeSet<String>,
}

fn build_coverage(
    query: &UsageSeriesQuery,
    initial_width: UsageBucketWidth,
    final_width: UsageBucketWidth,
    classify_ctx: &ClassifyContext<'_>,
    (over_cell_cap, cell_count): (bool, usize),
    facts: CoverageFacts<'_>,
) -> UsageSeriesCoverage {
    let CoverageFacts {
        window_runs,
        finished_execs,
        completed_tasks,
        gaps,
    } = facts;
    let earliest_fact = window_runs
        .iter()
        .map(|f| f.rec.run_created_at_epoch_s)
        .chain(finished_execs.iter().map(|e| e.exec.execution_finished_at_epoch_s))
        .chain(completed_tasks.iter().map(|t| t.completed_at_epoch_s))
        .min();
    let data_from_epoch_s = earliest_fact.map(|earliest| earliest.max(query.capture_start_epoch_s));

    let (dimension_from_epoch_s, has_unrecorded_before_dim_start) = match query.group_by {
        None => (None, false),
        // Only the launch-tuple dimensions have a recording era. Every other
        // dimension is absent permanently for some facts (a taskless run has
        // no product, a project-less chore no project), which is not a
        // coverage boundary the app should shade.
        Some(dimension) if !dimension_has_recording_era(dimension) => (None, false),
        Some(dimension) => {
            let mut recorded_min: Option<i64> = None;
            let mut unrecorded_min: Option<i64> = None;
            let mut observe = |ts: i64, recorded: bool| {
                let slot = if recorded {
                    &mut recorded_min
                } else {
                    &mut unrecorded_min
                };
                *slot = Some(slot.map_or(ts, |prev| prev.min(ts)));
            };
            for &facts in window_runs {
                observe(
                    facts.rec.run_created_at_epoch_s,
                    has_recorded_dimension(&DimensionSource::from_run(facts), dimension),
                );
            }
            for exec in finished_execs {
                observe(
                    exec.exec.execution_finished_at_epoch_s,
                    exec.has_recorded_dimension(dimension),
                );
            }
            for task in completed_tasks {
                observe(task.completed_at_epoch_s, task.has_recorded_dimension(dimension));
            }
            let unrecorded_before = match (recorded_min, unrecorded_min) {
                (Some(rec), Some(unrec)) => unrec < rec,
                (None, Some(_)) => true,
                _ => false,
            };
            (recorded_min, unrecorded_before)
        }
    };

    let mut by_caveat: BTreeMap<&str, u32> = BTreeMap::new();
    let mut total_measured_runs: u32 = 0;
    let mut runs_excluded_from_active_hour: u32 = 0;
    for &facts in window_runs {
        for_each_caveat_code(facts, classify_ctx, |code| {
            *by_caveat.entry(code).or_default() += 1;
        });
        if facts.coverage != RunCoverage::Unmeasured {
            total_measured_runs += 1;
            if facts.rec.agent_active_ms.is_none() {
                runs_excluded_from_active_hour += 1;
            }
        }
    }

    let mut notes = Vec::new();
    if window_runs
        .iter()
        .any(|facts| facts.rec.execution_kind == COORDINATOR_USAGE_KIND)
    {
        notes.push(UsageSeriesNote {
            code: "coordinator_hour_slices".into(), epoch_s: None,
            detail: "Coordinator usage is recorded in UTC hourly/model slices whose hour starts fall within the window (whole slices at partial-hour edges), included in work volume as slices and excluded from both tokens and run counts for tokens per run; that ratio describes worker runs only.".into(),
        });
    }
    if query.since_epoch_s < query.capture_start_epoch_s {
        notes.push(UsageSeriesNote {
            code: note::CAPTURE_STARTED.to_owned(),
            epoch_s: Some(query.capture_start_epoch_s),
            detail: format!(
                "the window starts before per-run token capture began (epoch {}); earlier runs have no token data, so figures before that point are a floor",
                query.capture_start_epoch_s
            ),
        });
    }
    if query.since_epoch_s < query.completed_at_start_epoch_s {
        notes.push(UsageSeriesNote {
            code: note::DIMENSION_STARTED.to_owned(),
            epoch_s: Some(query.completed_at_start_epoch_s),
            detail: format!(
                "tasks.completed_at was added at epoch {}; terminal tasks before that migration were backfilled with created_at, so tokens_per_completed_task is only meaningful from that instant forward",
                query.completed_at_start_epoch_s
            ),
        });
    }
    if let Some(dimension) = query.group_by
        && has_unrecorded_before_dim_start
    {
        let detail = match dimension_from_epoch_s {
            Some(epoch_s) => format!(
                "grouped by {dimension}: earlier facts in this window lack a recorded {dimension} value; coverage begins at epoch {epoch_s}"
            ),
            None => format!("grouped by {dimension}: no fact in this window carries a recorded {dimension} value"),
        };
        notes.push(UsageSeriesNote {
            code: note::DIMENSION_STARTED.to_owned(),
            epoch_s: dimension_from_epoch_s,
            detail,
        });
    }
    let retention_cutoff = query.generated_at_epoch_s.saturating_sub(query.retention_max_age_secs);
    if query.since_epoch_s < retention_cutoff {
        notes.push(UsageSeriesNote {
            code: note::RETENTION_BOUNDED.to_owned(),
            epoch_s: Some(retention_cutoff),
            detail: format!(
                "the window starts more than {} days ago; never-started failed, abandoned, orphaned and cancelled executions older than that are pruned past a per-work-item floor, so older buckets can undercount those never-started rows. Executions that had a worker run are retained",
                query.retention_max_age_secs / DAY_SECS
            ),
        });
    }
    if runs_excluded_from_active_hour > 0 {
        notes.push(UsageSeriesNote {
            code: note::ACTIVE_TIME_MISSING.to_owned(),
            epoch_s: None,
            detail: format!(
                "{runs_excluded_from_active_hour} of {total_measured_runs} measured run(s) lack agent_active_ms (for example Grok) and are excluded from both numerator and denominator of tokens_per_active_hour"
            ),
        });
    }
    if !gaps.is_empty() {
        notes.push(UsageSeriesNote {
            code: note::PRICING_GAPS.to_owned(),
            epoch_s: None,
            detail: format!(
                "the USD estimate leaves out runs it cannot price (unverified rates, subscription billing, gross input or unknown tokens); models: {}",
                gaps.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
        });
    }
    if final_width != initial_width {
        let detail = if over_cell_cap {
            format!(
                "bucket width coarsened from {initial_width} to {final_width}; the coarsest width still produces {cell_count} cells, above the {} cell cap. Every fact is kept",
                query.cell_cap
            )
        } else {
            format!(
                "bucket width coarsened from {initial_width} to {final_width} to stay within the {} cell cap",
                query.cell_cap
            )
        };
        notes.push(UsageSeriesNote {
            code: note::BUCKET_COARSENED.to_owned(),
            epoch_s: None,
            detail,
        });
    }
    if over_cell_cap {
        notes.push(UsageSeriesNote {
            code: note::CELL_CAP_EXCEEDED.to_owned(),
            epoch_s: None,
            detail: format!(
                "{final_width} buckets produce {cell_count} cells, above the {} cell cap. Every fact is kept; narrow the range or group by fewer values to shrink the grid",
                query.cell_cap
            ),
        });
    }

    UsageSeriesCoverage::builder()
        .by_caveat(
            by_caveat
                .into_iter()
                .map(|(code, runs)| UsageCaveatCount {
                    code: code.to_owned(),
                    runs,
                })
                .collect(),
        )
        .notes(notes)
        .runs_excluded_from_active_hour(runs_excluded_from_active_hour)
        .maybe_data_from_epoch_s(data_from_epoch_s)
        .maybe_dimension_from_epoch_s(dimension_from_epoch_s)
        .build()
}

/// Dimensions whose value was only recorded from some point in time (the
/// launch-tuple driver, model and effort columns). All others either always
/// existed or are legitimately absent for some facts.
fn dimension_has_recording_era(dimension: UsageDimension) -> bool {
    matches!(
        dimension,
        UsageDimension::Driver | UsageDimension::Model | UsageDimension::Effort
    )
}
