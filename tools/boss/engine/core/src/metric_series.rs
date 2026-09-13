//! Pure aggregation behind `GetMetricSeries` / `GetMetricCatalog`.
//!
//! The `work` DB layer projects windowed rows into [`ExecutionFact`] /
//! [`TaskFact`]; every bucket, percentile, group-by, coverage note, auto
//! bucket width, and the 5,000-cell cap is a pure function of that slice
//! so it can be unit-tested without a database. Mirrors [`crate::cost_report`].

use std::collections::BTreeMap;

use boss_protocol::{
    CoverageNote, CoverageNoteKind, MetricBucket, MetricCatalog, MetricCell, MetricDimensionInfo, MetricDimensionValue,
    MetricFilter, MetricFilterPreset, MetricSeriesInfo, MetricSeriesReport, MetricValue, MetricValueKind,
    SeriesCoverage,
};

/// Maximum `groups × potential-buckets` a reply may occupy. Exceeding it
/// coarsens the bucket rather than truncating cells.
pub const CELL_CAP: usize = 5_000;

/// How far back `GetMetricCatalog` scans to compute observed dimension
/// values and coverage: bounded rather than unbounded so the one query
/// with no client-supplied window doesn't grow linearly with all of
/// history. Two years comfortably covers any zoom range the app offers.
pub const CATALOG_LOOKBACK_SECS: i64 = 2 * 365 * BucketWidth::DAY_SECS;

/// Group key when the query does not name a `group_by` dimension.
pub const UNGROUPED_KEY: &str = "all";

/// Group / filter key for a NULL dimension value. Matches `boss cost`'s
/// `(none)` label so a missing driver is never silently dropped.
pub const NONE_KEY: &str = "(none)";

pub const SERIES_REVIEW_DURATION: &str = "review_duration";
pub const SERIES_EXECUTION_DURATION: &str = "execution_duration";
pub const SERIES_TASK_LEAD_TIME: &str = "task_lead_time";
pub const SERIES_EXECUTION_OUTCOMES: &str = "execution_outcomes";
pub const SERIES_PRS_GENERATED: &str = "prs_generated";

pub const PRESET_FAILED_OR_REAPED: &str = "failed_or_reaped";

pub const DIM_DRIVER: &str = "driver";
pub const DIM_EFFORT_LEVEL: &str = "effort_level";
pub const DIM_KIND: &str = "kind";
pub const DIM_MODEL: &str = "model";
pub const DIM_PRODUCT: &str = "product";
pub const DIM_REASONING: &str = "reasoning";
pub const DIM_REPO: &str = "repo";
pub const DIM_STATUS: &str = "status";

pub const REVIEW_DURATION_KIND: &str = "pr_review";
pub const COMPLETED_STATUS: &str = "completed";

/// Implementation and design execution kinds for `execution_duration`.
pub const EXECUTION_DURATION_KINDS: &[&str] = &[
    "chore_implementation",
    "investigation_implementation",
    "product_design",
    "project_design",
    "revision_implementation",
    "task_implementation",
];

/// Terminal `work_executions.status` values. `finished_at` is set only
/// when the status is one of these.
pub const TERMINAL_STATUSES: &[&str] = &["abandoned", "cancelled", "completed", "failed", "orphaned"];

/// Catalog preset "failed or reaped": reaped is not a status, so the
/// chip is this status set rather than an invented semantic.
pub const FAILED_OR_REAPED_STATUSES: &[&str] = &["abandoned", "failed", "orphaned"];

const DIMS_REVIEW_DURATION: &[&str] = &[DIM_DRIVER, DIM_EFFORT_LEVEL, DIM_MODEL, DIM_PRODUCT, DIM_REPO];
const DIMS_EXECUTION_DURATION: &[&str] = &[DIM_DRIVER, DIM_EFFORT_LEVEL, DIM_KIND, DIM_MODEL, DIM_PRODUCT, DIM_REPO];
const DIMS_TASK_LEAD_TIME: &[&str] = &[DIM_EFFORT_LEVEL, DIM_KIND, DIM_PRODUCT, DIM_REASONING, DIM_REPO];
const DIMS_EXECUTION_OUTCOMES: &[&str] = &[
    DIM_DRIVER,
    DIM_EFFORT_LEVEL,
    DIM_KIND,
    DIM_MODEL,
    DIM_PRODUCT,
    DIM_REPO,
    DIM_STATUS,
];
const DIMS_PRS_GENERATED: &[&str] = &[DIM_DRIVER, DIM_KIND, DIM_MODEL, DIM_PRODUCT, DIM_REPO];

/// Fixed-width bucket. Calendar months would make `bucket_secs` vary;
/// the wire carries a single `bucket_secs`, so widths are epoch-aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketWidth {
    Hour,
    Day,
    Week,
    Month,
}

impl BucketWidth {
    pub const HOUR_SECS: i64 = 3_600;
    pub const DAY_SECS: i64 = 86_400;
    pub const WEEK_SECS: i64 = 604_800;
    /// 30-day month so `bucket_secs` is a single integer.
    pub const MONTH_SECS: i64 = 2_592_000;

    pub fn secs(self) -> i64 {
        match self {
            Self::Hour => Self::HOUR_SECS,
            Self::Day => Self::DAY_SECS,
            Self::Week => Self::WEEK_SECS,
            Self::Month => Self::MONTH_SECS,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    pub fn parse(s: &str) -> Result<Self, SeriesError> {
        match s {
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            other => Err(SeriesError::UnknownBucket(other.to_owned())),
        }
    }

    pub fn coarser(self) -> Option<Self> {
        match self {
            Self::Hour => Some(Self::Day),
            Self::Day => Some(Self::Week),
            Self::Week => Some(Self::Month),
            Self::Month => None,
        }
    }
}

/// Pick a bucket when the client does not name one: hour up to three
/// days, day up to 200 days, week up to four years, month beyond.
pub fn pick_bucket_width(range_secs: i64) -> BucketWidth {
    if range_secs <= 3 * BucketWidth::DAY_SECS {
        BucketWidth::Hour
    } else if range_secs <= 200 * BucketWidth::DAY_SECS {
        BucketWidth::Day
    } else if range_secs <= 4 * 365 * BucketWidth::DAY_SECS {
        BucketWidth::Week
    } else {
        BucketWidth::Month
    }
}

pub fn bucket_start(epoch_s: i64, width: BucketWidth) -> i64 {
    let w = width.secs();
    epoch_s.div_euclid(w) * w
}

/// One `work_executions` row projected for the execution-sourced series.
#[derive(Debug, Clone, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct ExecutionFact {
    pub finished_at_epoch_s: i64,
    pub kind: String,
    pub status: String,
    pub driver: Option<String>,
    pub duration_ms: Option<i64>,
    pub effort_level: Option<String>,
    pub model: Option<String>,
    pub pr_url: Option<String>,
    pub product: Option<String>,
    pub repo: Option<String>,
}

/// One `tasks` row projected for `task_lead_time`.
#[derive(Debug, Clone, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct TaskFact {
    pub completed_at_epoch_s: i64,
    pub duration_ms: i64,
    pub kind: String,
    pub effort_level: Option<String>,
    pub product: Option<String>,
    pub reasoning: Option<String>,
    pub repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeriesError {
    CellCapExceeded { groups: usize, cap: usize },
    InvalidWindow { since_epoch_s: i64, until_epoch_s: i64 },
    UnknownBucket(String),
    UnknownFilterDimension { series: String, dimension: String },
    UnknownGroupBy { series: String, dimension: String },
    UnknownSeries(String),
}

impl std::fmt::Display for SeriesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CellCapExceeded { groups, cap } => {
                write!(
                    f,
                    "{groups} groups exceed the {cap}-cell cap even at the coarsest supported bucket width; narrow the window, add a filter, or drop the group_by"
                )
            }
            Self::InvalidWindow {
                since_epoch_s,
                until_epoch_s,
            } => write!(
                f,
                "until_epoch_s ({until_epoch_s}) must be greater than since_epoch_s ({since_epoch_s})"
            ),
            Self::UnknownBucket(bucket) => {
                write!(f, "unknown bucket {bucket:?}; expected one of: hour, day, week, month")
            }
            Self::UnknownFilterDimension { series, dimension } => {
                write!(f, "series {series} does not support filter dimension {dimension}")
            }
            Self::UnknownGroupBy { series, dimension } => {
                write!(f, "series {series} does not support group_by dimension {dimension}")
            }
            Self::UnknownSeries(series) => {
                let ids: Vec<&str> = series_specs().iter().map(|s| s.id).collect();
                write!(
                    f,
                    "unknown metric series {series:?}; expected one of: {}",
                    ids.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for SeriesError {}

/// Where a series reads its facts, plus the kind/status predicates the
/// DB projection (and catalog coverage) apply.
#[derive(Debug, Clone, Copy)]
pub enum SeriesSource {
    Executions {
        kinds: Option<&'static [&'static str]>,
        require_duration: bool,
        require_pr_url: bool,
        statuses: Option<&'static [&'static str]>,
        unique_by_pr_url: bool,
    },
    Tasks,
}

#[derive(Debug, Clone, Copy, bon::Builder)]
#[builder(on(String, into))]
pub struct SeriesSpec {
    pub id: &'static str,
    pub default_group_by: Option<&'static str>,
    pub dimensions: &'static [&'static str],
    pub source: SeriesSource,
    pub title: &'static str,
    pub value_kind: MetricValueKind,
}

struct PresetSpec {
    id: &'static str,
    title: &'static str,
    dimension: &'static str,
    values: &'static [&'static str],
}

const REVIEW_DURATION_KINDS: &[&str] = &[REVIEW_DURATION_KIND];
const COMPLETED_STATUSES: &[&str] = &[COMPLETED_STATUS];

const SERIES_SPECS: &[SeriesSpec] = &[
    SeriesSpec {
        id: SERIES_REVIEW_DURATION,
        default_group_by: Some(DIM_DRIVER),
        dimensions: DIMS_REVIEW_DURATION,
        source: SeriesSource::Executions {
            kinds: Some(REVIEW_DURATION_KINDS),
            require_duration: true,
            require_pr_url: false,
            statuses: Some(COMPLETED_STATUSES),
            unique_by_pr_url: false,
        },
        title: "Review duration",
        value_kind: MetricValueKind::Duration,
    },
    SeriesSpec {
        id: SERIES_EXECUTION_DURATION,
        default_group_by: Some(DIM_KIND),
        dimensions: DIMS_EXECUTION_DURATION,
        source: SeriesSource::Executions {
            kinds: Some(EXECUTION_DURATION_KINDS),
            require_duration: true,
            require_pr_url: false,
            statuses: Some(COMPLETED_STATUSES),
            unique_by_pr_url: false,
        },
        title: "Execution duration",
        value_kind: MetricValueKind::Duration,
    },
    SeriesSpec {
        id: SERIES_TASK_LEAD_TIME,
        default_group_by: Some(DIM_KIND),
        dimensions: DIMS_TASK_LEAD_TIME,
        source: SeriesSource::Tasks,
        title: "Task lead time",
        value_kind: MetricValueKind::Duration,
    },
    SeriesSpec {
        id: SERIES_EXECUTION_OUTCOMES,
        default_group_by: Some(DIM_STATUS),
        dimensions: DIMS_EXECUTION_OUTCOMES,
        source: SeriesSource::Executions {
            kinds: None,
            require_duration: false,
            require_pr_url: false,
            statuses: Some(TERMINAL_STATUSES),
            unique_by_pr_url: false,
        },
        title: "Execution outcomes",
        value_kind: MetricValueKind::Count,
    },
    SeriesSpec {
        id: SERIES_PRS_GENERATED,
        default_group_by: Some(DIM_KIND),
        dimensions: DIMS_PRS_GENERATED,
        source: SeriesSource::Executions {
            kinds: None,
            require_duration: false,
            require_pr_url: true,
            statuses: Some(TERMINAL_STATUSES),
            unique_by_pr_url: true,
        },
        title: "PRs generated",
        value_kind: MetricValueKind::Count,
    },
];

const FAILED_OR_REAPED_PRESET: &[PresetSpec] = &[PresetSpec {
    id: PRESET_FAILED_OR_REAPED,
    title: "failed or reaped",
    dimension: DIM_STATUS,
    values: FAILED_OR_REAPED_STATUSES,
}];

pub fn series_specs() -> &'static [SeriesSpec] {
    SERIES_SPECS
}

pub fn series_spec(id: &str) -> Option<&'static SeriesSpec> {
    series_specs().iter().find(|s| s.id == id)
}

fn presets_for(id: &str) -> &'static [PresetSpec] {
    if id == SERIES_EXECUTION_OUTCOMES {
        FAILED_OR_REAPED_PRESET
    } else {
        &[]
    }
}

/// Parsed `GetMetricSeries` arguments after wire validation.
#[derive(Debug, Clone, bon::Builder)]
pub struct SeriesQuery<'a> {
    pub series: &'a str,
    pub since_epoch_s: i64,
    pub until_epoch_s: i64,
    pub bucket: Option<BucketWidth>,
    pub group_by: Option<&'a str>,
    pub filters: &'a [MetricFilter],
}

pub fn parse_bucket(raw: Option<&str>) -> Result<Option<BucketWidth>, SeriesError> {
    match raw {
        None => Ok(None),
        Some(s) => BucketWidth::parse(s).map(Some),
    }
}

pub fn validate_query<'a>(query: &SeriesQuery<'a>) -> Result<&'static SeriesSpec, SeriesError> {
    if query.until_epoch_s <= query.since_epoch_s {
        return Err(SeriesError::InvalidWindow {
            since_epoch_s: query.since_epoch_s,
            until_epoch_s: query.until_epoch_s,
        });
    }
    let spec = series_spec(query.series).ok_or_else(|| SeriesError::UnknownSeries(query.series.to_owned()))?;
    if let Some(dim) = query.group_by
        && !spec.dimensions.contains(&dim)
    {
        return Err(SeriesError::UnknownGroupBy {
            series: spec.id.to_owned(),
            dimension: dim.to_owned(),
        });
    }
    for filter in query.filters {
        if !spec.dimensions.contains(&filter.dimension.as_str()) {
            return Err(SeriesError::UnknownFilterDimension {
                series: spec.id.to_owned(),
                dimension: filter.dimension.clone(),
            });
        }
    }
    Ok(spec)
}

fn dim_value(value: Option<&str>) -> &str {
    match value {
        Some(v) if !v.is_empty() => v,
        _ => NONE_KEY,
    }
}

fn execution_dim<'a>(fact: &'a ExecutionFact, dim: &str) -> Option<&'a str> {
    match dim {
        DIM_DRIVER => fact.driver.as_deref(),
        DIM_EFFORT_LEVEL => fact.effort_level.as_deref(),
        DIM_KIND => Some(fact.kind.as_str()),
        DIM_MODEL => fact.model.as_deref(),
        DIM_PRODUCT => fact.product.as_deref(),
        DIM_REPO => fact.repo.as_deref(),
        DIM_STATUS => Some(fact.status.as_str()),
        _ => None,
    }
}

fn task_dim<'a>(fact: &'a TaskFact, dim: &str) -> Option<&'a str> {
    match dim {
        DIM_EFFORT_LEVEL => fact.effort_level.as_deref(),
        DIM_KIND => Some(fact.kind.as_str()),
        DIM_PRODUCT => fact.product.as_deref(),
        DIM_REASONING => fact.reasoning.as_deref(),
        DIM_REPO => fact.repo.as_deref(),
        _ => None,
    }
}

fn matches_filters<F>(filters: &[MetricFilter], mut get: F) -> bool
where
    F: FnMut(&str) -> Option<String>,
{
    filters.iter().all(|filter| {
        let value = get(&filter.dimension).unwrap_or_else(|| NONE_KEY.to_owned());
        filter.values.iter().any(|v| v == &value)
    })
}

fn group_key<F>(group_by: Option<&str>, mut get: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    match group_by {
        None => UNGROUPED_KEY.to_owned(),
        Some(dim) => get(dim).unwrap_or_else(|| NONE_KEY.to_owned()),
    }
}

fn execution_matches_source(fact: &ExecutionFact, source: SeriesSource) -> bool {
    let SeriesSource::Executions {
        kinds,
        require_duration,
        require_pr_url,
        statuses,
        ..
    } = source
    else {
        return false;
    };
    if let Some(kinds) = kinds
        && !kinds.contains(&fact.kind.as_str())
    {
        return false;
    }
    if let Some(statuses) = statuses
        && !statuses.contains(&fact.status.as_str())
    {
        return false;
    }
    if require_pr_url && fact.pr_url.as_ref().is_none_or(|u| u.is_empty()) {
        return false;
    }
    if require_duration && fact.duration_ms.is_none() {
        return false;
    }
    true
}

/// Keep the earliest terminal execution per `pr_url` so a PR revised five
/// times counts once, in the bucket where it first appeared.
pub fn first_pr_facts(facts: &[ExecutionFact]) -> Vec<ExecutionFact> {
    let mut by_url: BTreeMap<&str, &ExecutionFact> = BTreeMap::new();
    for fact in facts {
        let Some(url) = fact.pr_url.as_deref().filter(|u| !u.is_empty()) else {
            continue;
        };
        match by_url.get(url) {
            None => {
                by_url.insert(url, fact);
            }
            Some(existing) if fact.finished_at_epoch_s < existing.finished_at_epoch_s => {
                by_url.insert(url, fact);
            }
            Some(_) => {}
        }
    }
    by_url.into_values().cloned().collect()
}

#[derive(Clone)]
struct Point {
    at_epoch_s: i64,
    group: String,
    duration_ms: Option<i64>,
    dim_present: bool,
}

fn percentile_nearest(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn duration_value(samples: &mut [i64]) -> MetricValue {
    samples.sort_unstable();
    let n = samples.len() as i64;
    let sum: i64 = samples.iter().copied().sum();
    MetricValue::Duration {
        max_ms: samples.last().copied().unwrap_or(0),
        mean_ms: if n == 0 { 0 } else { sum / n },
        p50_ms: percentile_nearest(samples, 0.50),
        p90_ms: percentile_nearest(samples, 0.90),
    }
}

/// Number of occupied buckets at `width` across `[since_epoch_s,
/// until_epoch_s)`, counted from aligned bucket boundaries rather than
/// `ceil(range / width)`: a window whose `since` falls mid-bucket still
/// spans the bucket containing `until - 1`, which the raw range length
/// alone underestimates whenever `since` isn't already bucket-aligned.
fn potential_cells(groups: usize, since_epoch_s: i64, until_epoch_s: i64, width: BucketWidth) -> usize {
    let last_included = (until_epoch_s - 1).max(since_epoch_s);
    let start_bucket = bucket_start(since_epoch_s, width);
    let end_bucket = bucket_start(last_included, width);
    let buckets = (end_bucket - start_bucket) / width.secs() + 1;
    groups.saturating_mul(buckets.max(1) as usize)
}

fn choose_width(
    requested: Option<BucketWidth>,
    since_epoch_s: i64,
    until_epoch_s: i64,
    groups: usize,
) -> Result<(BucketWidth, bool), SeriesError> {
    let range_secs = until_epoch_s.saturating_sub(since_epoch_s);
    let mut width = requested.unwrap_or_else(|| pick_bucket_width(range_secs));
    let original = width;
    while potential_cells(groups, since_epoch_s, until_epoch_s, width) > CELL_CAP {
        match width.coarser() {
            Some(next) => width = next,
            None => {
                return Err(SeriesError::CellCapExceeded { groups, cap: CELL_CAP });
            }
        }
    }
    Ok((width, width != original))
}

struct BuiltBuckets {
    groups: Vec<String>,
    buckets: Vec<MetricBucket>,
    coverage: SeriesCoverage,
    bucket_secs: i64,
}

fn build_from_points(
    points: &[Point],
    value_kind: MetricValueKind,
    since_epoch_s: i64,
    until_epoch_s: i64,
    requested_bucket: Option<BucketWidth>,
    grouped: bool,
) -> Result<BuiltBuckets, SeriesError> {
    // Coverage (`data_from`/`dimension_from`) is computed over every point
    // the series predicates and query filters admit, with no window lower
    // bound: `points` may include facts from before `since_epoch_s` (the
    // caller is expected to have projected history back to the true start,
    // not just this query's window) so the reported capture boundary is a
    // fact about the series, not an artifact of where the operator zoomed.
    // Buckets and groups, in contrast, are built only from points inside
    // the requested window.
    let mut data_from: Option<i64> = None;
    let mut dimension_from: Option<i64> = None;
    for point in points {
        data_from = Some(data_from.map_or(point.at_epoch_s, |m| m.min(point.at_epoch_s)));
        if point.dim_present {
            dimension_from = Some(dimension_from.map_or(point.at_epoch_s, |m| m.min(point.at_epoch_s)));
        }
    }

    let windowed: Vec<&Point> = points
        .iter()
        .filter(|p| p.at_epoch_s >= since_epoch_s && p.at_epoch_s < until_epoch_s)
        .collect();

    let mut group_totals: BTreeMap<String, u32> = BTreeMap::new();
    for point in &windowed {
        *group_totals.entry(point.group.clone()).or_insert(0) += 1;
    }
    let mut groups: Vec<(String, u32)> = group_totals.into_iter().collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let groups: Vec<String> = groups.into_iter().map(|(g, _)| g).collect();

    let (width, coarsened) = choose_width(requested_bucket, since_epoch_s, until_epoch_s, groups.len())?;

    let mut cells: BTreeMap<i64, BTreeMap<String, CellAcc>> = BTreeMap::new();
    for point in &windowed {
        let start = bucket_start(point.at_epoch_s, width);
        let acc = cells.entry(start).or_default().entry(point.group.clone()).or_default();
        acc.n += 1;
        if let Some(ms) = point.duration_ms {
            acc.durations.push(ms);
        }
    }

    let buckets: Vec<MetricBucket> = cells
        .into_iter()
        .map(|(start_epoch_s, by_group)| {
            let cells: Vec<MetricCell> = groups
                .iter()
                .filter_map(|group| {
                    let mut acc = by_group.get(group)?.clone();
                    let value = match value_kind {
                        MetricValueKind::Duration => duration_value(&mut acc.durations),
                        MetricValueKind::Count => MetricValue::Count { n: acc.n },
                        MetricValueKind::Points => MetricValue::Points { points: acc.n as i64 },
                        MetricValueKind::Tokens => MetricValue::Tokens {
                            cache_read: 0,
                            cache_write: 0,
                            input: 0,
                            output: 0,
                        },
                        MetricValueKind::Usd => MetricValue::Usd {
                            estimated: None,
                            partial: false,
                            unpriceable_runs: 0,
                        },
                    };
                    Some(MetricCell {
                        group: group.clone(),
                        n: acc.n,
                        value,
                    })
                })
                .collect();
            debug_assert!(!cells.is_empty());
            MetricBucket { start_epoch_s, cells }
        })
        .collect();

    let mut notes = Vec::new();
    if coarsened {
        notes.push(
            CoverageNote::builder()
                .detail(format!(
                    "bucket coarsened to {} ({}s) to stay under {CELL_CAP} cells",
                    width.as_str(),
                    width.secs()
                ))
                .kind(CoverageNoteKind::BucketCoarsened)
                .build(),
        );
    }
    if grouped && dimension_from.is_some() && dimension_from != data_from {
        notes.push(
            CoverageNote::builder()
                .detail("group-by dimension is unpopulated before this instant".to_owned())
                .kind(CoverageNoteKind::DimensionStarted)
                .maybe_epoch_s(dimension_from)
                .build(),
        );
    }

    Ok(BuiltBuckets {
        groups,
        buckets,
        coverage: SeriesCoverage::builder()
            .notes(notes)
            .maybe_data_from_epoch_s(data_from)
            .maybe_dimension_from_epoch_s(if grouped { dimension_from } else { None })
            .build(),
        bucket_secs: width.secs(),
    })
}

#[derive(Clone, Default)]
struct CellAcc {
    n: u32,
    durations: Vec<i64>,
}

fn finish_report(
    spec: &SeriesSpec,
    query: &SeriesQuery<'_>,
    built: BuiltBuckets,
    generated_at_epoch_s: i64,
) -> MetricSeriesReport {
    MetricSeriesReport::builder()
        .series(spec.id)
        .bucket_secs(built.bucket_secs)
        .buckets(built.buckets)
        .coverage(built.coverage)
        .generated_at_epoch_s(generated_at_epoch_s)
        .groups(built.groups)
        .since_epoch_s(query.since_epoch_s)
        .until_epoch_s(query.until_epoch_s)
        .value_kind(spec.value_kind)
        .build()
}

/// Build a report from facts projected for the requested window.
pub fn build_execution_series_report(
    query: &SeriesQuery<'_>,
    facts: &[ExecutionFact],
    generated_at_epoch_s: i64,
) -> Result<MetricSeriesReport, SeriesError> {
    let spec = validate_query(query)?;
    let mut facts: Vec<ExecutionFact> = facts
        .iter()
        .filter(|f| execution_matches_source(f, spec.source))
        .cloned()
        .collect();
    let grouped = query.group_by.is_some();
    facts.retain(|fact| {
        matches_filters(query.filters, |dim| {
            execution_dim(fact, dim).map(|v| dim_value(Some(v)).to_owned())
        })
    });
    if let SeriesSource::Executions {
        unique_by_pr_url: true, ..
    } = spec.source
    {
        facts = first_pr_facts(&facts);
    }
    let points: Vec<Point> = facts
        .iter()
        .map(|fact| {
            let dim_present = query
                .group_by
                .map(|dim| execution_dim(fact, dim).is_some_and(|v| !v.is_empty()))
                .unwrap_or(true);
            Point {
                at_epoch_s: fact.finished_at_epoch_s,
                group: group_key(query.group_by, |dim| {
                    execution_dim(fact, dim).map(|v| dim_value(Some(v)).to_owned())
                }),
                duration_ms: fact.duration_ms,
                dim_present,
            }
        })
        .collect();
    let built = build_from_points(
        &points,
        spec.value_kind,
        query.since_epoch_s,
        query.until_epoch_s,
        query.bucket,
        grouped,
    )?;
    Ok(finish_report(spec, query, built, generated_at_epoch_s))
}

/// Build a task report from facts projected for the requested window.
pub fn build_task_series_report(
    query: &SeriesQuery<'_>,
    facts: &[TaskFact],
    generated_at_epoch_s: i64,
) -> Result<MetricSeriesReport, SeriesError> {
    let spec = validate_query(query)?;
    let grouped = query.group_by.is_some();
    let points: Vec<Point> = facts
        .iter()
        .filter(|fact| {
            matches_filters(query.filters, |dim| {
                task_dim(fact, dim).map(|v| dim_value(Some(v)).to_owned())
            })
        })
        .map(|fact| {
            let dim_present = query
                .group_by
                .map(|dim| task_dim(fact, dim).is_some_and(|v| !v.is_empty()))
                .unwrap_or(true);
            Point {
                at_epoch_s: fact.completed_at_epoch_s,
                group: group_key(query.group_by, |dim| {
                    task_dim(fact, dim).map(|v| dim_value(Some(v)).to_owned())
                }),
                duration_ms: Some(fact.duration_ms),
                dim_present,
            }
        })
        .collect();
    let built = build_from_points(
        &points,
        spec.value_kind,
        query.since_epoch_s,
        query.until_epoch_s,
        query.bucket,
        grouped,
    )?;
    Ok(finish_report(spec, query, built, generated_at_epoch_s))
}

/// `lookback_since_epoch_s`: `0` means the catalog scan was unbounded; any
/// other value means the scan started there, so `data_from` may understate
/// the series' true start and a [`CoverageNoteKind::RetentionBounded`] note
/// is attached to say so.
fn coverage_for_spec(
    spec: &SeriesSpec,
    execution_facts: &[ExecutionFact],
    task_facts: &[TaskFact],
    lookback_since_epoch_s: i64,
) -> SeriesCoverage {
    let data_from = match spec.source {
        SeriesSource::Executions { .. } => execution_facts
            .iter()
            .filter(|f| execution_matches_source(f, spec.source))
            .map(|f| f.finished_at_epoch_s)
            .min(),
        SeriesSource::Tasks => task_facts.iter().map(|f| f.completed_at_epoch_s).min(),
    };
    let mut notes = Vec::new();
    if lookback_since_epoch_s > 0 {
        notes.push(
            CoverageNote::builder()
                .detail("catalog scan is bounded; earlier facts may exist but are not reflected here".to_owned())
                .kind(CoverageNoteKind::RetentionBounded)
                .epoch_s(lookback_since_epoch_s)
                .build(),
        );
    }
    SeriesCoverage::builder()
        .notes(notes)
        .maybe_data_from_epoch_s(data_from)
        .build()
}

fn push_dim_value(
    into: &mut BTreeMap<&'static str, BTreeMap<String, (u32, i64)>>,
    dim: &'static str,
    value: Option<&str>,
    at: i64,
) {
    let key = dim_value(value).to_owned();
    let entry = into.entry(dim).or_default().entry(key).or_insert((0, at));
    entry.0 += 1;
    if at < entry.1 {
        entry.1 = at;
    }
}

const ALL_DIMENSIONS: &[(&str, &str)] = &[
    (DIM_DRIVER, "Driver"),
    (DIM_EFFORT_LEVEL, "Effort"),
    (DIM_KIND, "Kind"),
    (DIM_MODEL, "Model"),
    (DIM_PRODUCT, "Product"),
    (DIM_REASONING, "Reasoning"),
    (DIM_REPO, "Repo"),
    (DIM_STATUS, "Status"),
];

/// Build the catalog from the current fact set: static series descriptors
/// plus observed dimension values and per-series coverage.
///
/// `lookback_since_epoch_s` is `0` when `execution_facts`/`task_facts` span
/// unbounded history, or the epoch the caller's scan was bounded to
/// otherwise; each series' coverage carries a `RetentionBounded` note in
/// the latter case so the catalog never silently understates history.
pub fn build_catalog(
    execution_facts: &[ExecutionFact],
    task_facts: &[TaskFact],
    generated_at_epoch_s: i64,
    lookback_since_epoch_s: i64,
) -> MetricCatalog {
    let mut observed: BTreeMap<&'static str, BTreeMap<String, (u32, i64)>> = BTreeMap::new();
    for fact in execution_facts {
        push_dim_value(
            &mut observed,
            DIM_DRIVER,
            fact.driver.as_deref(),
            fact.finished_at_epoch_s,
        );
        push_dim_value(
            &mut observed,
            DIM_EFFORT_LEVEL,
            fact.effort_level.as_deref(),
            fact.finished_at_epoch_s,
        );
        push_dim_value(&mut observed, DIM_KIND, Some(&fact.kind), fact.finished_at_epoch_s);
        push_dim_value(
            &mut observed,
            DIM_MODEL,
            fact.model.as_deref(),
            fact.finished_at_epoch_s,
        );
        push_dim_value(
            &mut observed,
            DIM_PRODUCT,
            fact.product.as_deref(),
            fact.finished_at_epoch_s,
        );
        push_dim_value(&mut observed, DIM_REPO, fact.repo.as_deref(), fact.finished_at_epoch_s);
        push_dim_value(&mut observed, DIM_STATUS, Some(&fact.status), fact.finished_at_epoch_s);
    }
    for fact in task_facts {
        push_dim_value(
            &mut observed,
            DIM_EFFORT_LEVEL,
            fact.effort_level.as_deref(),
            fact.completed_at_epoch_s,
        );
        push_dim_value(&mut observed, DIM_KIND, Some(&fact.kind), fact.completed_at_epoch_s);
        push_dim_value(
            &mut observed,
            DIM_PRODUCT,
            fact.product.as_deref(),
            fact.completed_at_epoch_s,
        );
        push_dim_value(
            &mut observed,
            DIM_REASONING,
            fact.reasoning.as_deref(),
            fact.completed_at_epoch_s,
        );
        push_dim_value(&mut observed, DIM_REPO, fact.repo.as_deref(), fact.completed_at_epoch_s);
    }

    let dimensions: Vec<MetricDimensionInfo> = ALL_DIMENSIONS
        .iter()
        .map(|(id, title)| {
            let mut values: Vec<MetricDimensionValue> = observed
                .get(id)
                .map(|m| {
                    m.iter()
                        .map(|(value, (count, first_seen))| {
                            MetricDimensionValue::builder()
                                .value(value.clone())
                                .count(*count)
                                .first_seen_epoch_s(*first_seen)
                                .build()
                        })
                        .collect()
                })
                .unwrap_or_default();
            values.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
            MetricDimensionInfo::builder()
                .id(*id)
                .title(*title)
                .values(values)
                .build()
        })
        .collect();

    let series: Vec<MetricSeriesInfo> = series_specs()
        .iter()
        .map(|spec| {
            let presets: Vec<MetricFilterPreset> = presets_for(spec.id)
                .iter()
                .map(|preset| {
                    MetricFilterPreset::builder()
                        .id(preset.id)
                        .filters(vec![MetricFilter {
                            dimension: preset.dimension.to_owned(),
                            values: preset.values.iter().map(|v| (*v).to_owned()).collect(),
                        }])
                        .title(preset.title)
                        .build()
                })
                .collect();
            MetricSeriesInfo::builder()
                .id(spec.id)
                .coverage(coverage_for_spec(
                    spec,
                    execution_facts,
                    task_facts,
                    lookback_since_epoch_s,
                ))
                .dimensions(spec.dimensions.iter().map(|d| (*d).to_owned()).collect())
                .presets(presets)
                .title(spec.title)
                .value_kind(spec.value_kind)
                .maybe_default_group_by(spec.default_group_by.map(|s| s.to_owned()))
                .build()
        })
        .collect();

    MetricCatalog::builder()
        .dimensions(dimensions)
        .generated_at_epoch_s(generated_at_epoch_s)
        .series(series)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_fact(finished: i64, kind: &str, status: &str) -> ExecutionFact {
        ExecutionFact::builder()
            .finished_at_epoch_s(finished)
            .kind(kind)
            .status(status)
            .duration_ms(1_000)
            .build()
    }

    fn query<'a>(
        series: &'a str,
        since: i64,
        until: i64,
        bucket: Option<BucketWidth>,
        group_by: Option<&'a str>,
        filters: &'a [MetricFilter],
    ) -> SeriesQuery<'a> {
        SeriesQuery {
            series,
            since_epoch_s: since,
            until_epoch_s: until,
            bucket,
            group_by,
            filters,
        }
    }

    #[test]
    fn pick_bucket_width_follows_the_documented_thresholds() {
        assert_eq!(pick_bucket_width(2 * BucketWidth::DAY_SECS), BucketWidth::Hour);
        assert_eq!(pick_bucket_width(3 * BucketWidth::DAY_SECS), BucketWidth::Hour);
        assert_eq!(pick_bucket_width(4 * BucketWidth::DAY_SECS), BucketWidth::Day);
        assert_eq!(pick_bucket_width(200 * BucketWidth::DAY_SECS), BucketWidth::Day);
        assert_eq!(pick_bucket_width(201 * BucketWidth::DAY_SECS), BucketWidth::Week);
        assert_eq!(pick_bucket_width(4 * 365 * BucketWidth::DAY_SECS), BucketWidth::Week);
        assert_eq!(
            pick_bucket_width(4 * 365 * BucketWidth::DAY_SECS + 1),
            BucketWidth::Month
        );
    }

    #[test]
    fn percentile_nearest_rank_on_a_known_set() {
        let samples = [1_000_i64, 2_000, 3_000, 4_000, 5_000];
        assert_eq!(percentile_nearest(&samples, 0.50), 3_000);
        assert_eq!(percentile_nearest(&samples, 0.90), 5_000);
        assert_eq!(percentile_nearest(&[7_000], 0.50), 7_000);
        assert_eq!(percentile_nearest(&[], 0.50), 0);
    }

    #[test]
    fn empty_facts_yield_no_buckets_and_no_data_from() {
        let q = query(SERIES_EXECUTION_OUTCOMES, 0, 86_400, Some(BucketWidth::Day), None, &[]);
        let report = build_execution_series_report(&q, &[], 10).unwrap();
        assert!(report.buckets.is_empty());
        assert!(report.groups.is_empty());
        assert!(report.coverage.data_from_epoch_s.is_none());
    }

    #[test]
    fn ungrouped_uses_the_all_key() {
        let fact = exec_fact(100, "chore_implementation", "completed");
        let q = query(SERIES_EXECUTION_OUTCOMES, 0, 1_000, Some(BucketWidth::Hour), None, &[]);
        let report = build_execution_series_report(&q, &[fact], 10).unwrap();
        assert_eq!(report.groups, vec![UNGROUPED_KEY]);
        assert_eq!(report.buckets.len(), 1);
        assert_eq!(report.buckets[0].cells[0].n, 1);
        match &report.buckets[0].cells[0].value {
            MetricValue::Count { n } => assert_eq!(*n, 1),
            other => panic!("expected count, got {other:?}"),
        }
    }

    #[test]
    fn null_group_by_dimension_lands_in_none_not_dropped() {
        let mut fact = exec_fact(100, "chore_implementation", "completed");
        fact.driver = None;
        let q = query(
            SERIES_EXECUTION_OUTCOMES,
            0,
            1_000,
            Some(BucketWidth::Hour),
            Some(DIM_DRIVER),
            &[],
        );
        let report = build_execution_series_report(&q, &[fact], 10).unwrap();
        assert_eq!(report.groups, vec![NONE_KEY]);
    }

    #[test]
    fn a_zero_duration_bucket_is_present_a_gap_is_absent() {
        // Two completed reviews: one at t=10 (duration 0), none in the next hour.
        let mut fact = exec_fact(10, "pr_review", "completed");
        fact.duration_ms = Some(0);
        fact.driver = Some("claude".into());
        let q = query(
            SERIES_REVIEW_DURATION,
            0,
            BucketWidth::HOUR_SECS * 3,
            Some(BucketWidth::Hour),
            None,
            &[],
        );
        let report = build_execution_series_report(&q, &[fact], 10).unwrap();
        assert_eq!(report.buckets.len(), 1);
        assert_eq!(report.buckets[0].start_epoch_s, 0);
        assert_eq!(report.buckets[0].cells[0].n, 1);
        match &report.buckets[0].cells[0].value {
            MetricValue::Duration {
                p50_ms,
                p90_ms,
                mean_ms,
                max_ms,
            } => {
                assert_eq!(*p50_ms, 0);
                assert_eq!(*p90_ms, 0);
                assert_eq!(*mean_ms, 0);
                assert_eq!(*max_ms, 0);
            }
            other => panic!("expected duration, got {other:?}"),
        }
    }

    #[test]
    fn duration_percentiles_fold_every_sample_in_the_cell() {
        let facts: Vec<ExecutionFact> = [1_000, 2_000, 3_000, 4_000, 5_000]
            .into_iter()
            .map(|ms| {
                let mut f = exec_fact(10, "pr_review", "completed");
                f.duration_ms = Some(ms);
                f
            })
            .collect();
        let q = query(SERIES_REVIEW_DURATION, 0, 100, Some(BucketWidth::Hour), None, &[]);
        let report = build_execution_series_report(&q, &facts, 10).unwrap();
        match &report.buckets[0].cells[0].value {
            MetricValue::Duration {
                p50_ms,
                p90_ms,
                mean_ms,
                max_ms,
            } => {
                assert_eq!(*p50_ms, 3_000);
                assert_eq!(*p90_ms, 5_000);
                assert_eq!(*mean_ms, 3_000);
                assert_eq!(*max_ms, 5_000);
            }
            other => panic!("expected duration, got {other:?}"),
        }
        assert_eq!(report.buckets[0].cells[0].n, 5);
    }

    #[test]
    fn filters_and_across_dimensions_or_within() {
        let mut a = exec_fact(10, "chore_implementation", "failed");
        a.driver = Some("claude".into());
        let mut b = exec_fact(11, "chore_implementation", "orphaned");
        b.driver = Some("codex".into());
        let mut c = exec_fact(12, "chore_implementation", "completed");
        c.driver = Some("claude".into());
        let filters = vec![
            MetricFilter {
                dimension: DIM_STATUS.to_owned(),
                values: vec!["failed".into(), "orphaned".into()],
            },
            MetricFilter {
                dimension: DIM_DRIVER.to_owned(),
                values: vec!["claude".into()],
            },
        ];
        let q = query(
            SERIES_EXECUTION_OUTCOMES,
            0,
            100,
            Some(BucketWidth::Hour),
            None,
            &filters,
        );
        let report = build_execution_series_report(&q, &[a, b, c], 10).unwrap();
        assert_eq!(report.buckets[0].cells[0].n, 1);
    }

    #[test]
    fn prs_generated_counts_distinct_urls_at_first_finished_at() {
        let mut first = exec_fact(10, "chore_implementation", "completed");
        first.pr_url = Some("https://github.com/o/r/pull/1".into());
        first.kind = "chore_implementation".into();
        let mut later = exec_fact(10 + BucketWidth::HOUR_SECS * 2, "revision_implementation", "completed");
        later.pr_url = Some("https://github.com/o/r/pull/1".into());
        let mut other = exec_fact(11, "task_implementation", "completed");
        other.pr_url = Some("https://github.com/o/r/pull/2".into());
        let q = query(
            SERIES_PRS_GENERATED,
            0,
            BucketWidth::HOUR_SECS * 4,
            Some(BucketWidth::Hour),
            Some(DIM_KIND),
            &[],
        );
        let report = build_execution_series_report(&q, &[later.clone(), first.clone(), other.clone()], 10).unwrap();
        // Two URLs, attributed to the earliest execution of each.
        let total_n: u32 = report.buckets.iter().flat_map(|b| b.cells.iter()).map(|c| c.n).sum();
        assert_eq!(total_n, 2);
        // The shared URL lands in the first bucket under chore_implementation,
        // not in the later revision bucket.
        assert_eq!(report.buckets[0].start_epoch_s, 0);
        let first_groups: Vec<&str> = report.buckets[0].cells.iter().map(|c| c.group.as_str()).collect();
        assert!(first_groups.contains(&"chore_implementation"));
        assert!(first_groups.contains(&"task_implementation"));
        assert!(
            !report
                .buckets
                .iter()
                .any(|b| b.cells.iter().any(|c| c.group == "revision_implementation"))
        );
    }

    #[test]
    fn cell_cap_coarsens_hour_to_day_and_notes_it() {
        // 2 groups × 3 days of hour buckets = 2 * 72 = 144, under cap.
        // Force the cap with many groups: 200 groups × 72 hours = 14,400.
        let facts: Vec<ExecutionFact> = (0..200)
            .map(|i| {
                let mut f = exec_fact(10, "chore_implementation", "completed");
                f.driver = Some(format!("d{i}"));
                f
            })
            .collect();
        let q = query(
            SERIES_EXECUTION_OUTCOMES,
            0,
            3 * BucketWidth::DAY_SECS,
            Some(BucketWidth::Hour),
            Some(DIM_DRIVER),
            &[],
        );
        let report = build_execution_series_report(&q, &facts, 10).unwrap();
        assert_eq!(report.bucket_secs, BucketWidth::DAY_SECS);
        assert!(
            report
                .coverage
                .notes
                .iter()
                .any(|n| n.kind == CoverageNoteKind::BucketCoarsened),
            "expected bucket_coarsened note, got {:?}",
            report.coverage.notes
        );
    }

    #[test]
    fn cell_cap_unaligned_window_still_counts_the_extra_partial_bucket() {
        // 50 groups over a 100-hour window that starts 1s past an hour
        // boundary: the true bucket count is 101 (the window spans 101
        // distinct aligned hour buckets), not ceil(100h / 1h) = 100, so
        // 50 * 101 = 5,050 must coarsen even though the naive estimate
        // (50 * 100 = 5,000) would accept it.
        let facts: Vec<ExecutionFact> = (0..50)
            .map(|i| {
                let mut f = exec_fact(1, "chore_implementation", "completed");
                f.driver = Some(format!("d{i}"));
                f
            })
            .collect();
        let q = query(
            SERIES_EXECUTION_OUTCOMES,
            1,
            1 + 100 * BucketWidth::HOUR_SECS,
            Some(BucketWidth::Hour),
            Some(DIM_DRIVER),
            &[],
        );
        let report = build_execution_series_report(&q, &facts, 10).unwrap();
        assert_eq!(report.bucket_secs, BucketWidth::DAY_SECS);
        assert!(
            report
                .coverage
                .notes
                .iter()
                .any(|n| n.kind == CoverageNoteKind::BucketCoarsened)
        );
    }

    #[test]
    fn cell_cap_exceeded_even_at_month_width_is_an_error() {
        // 6,000 groups can never fit under 5,000 cells at any bucket width
        // (a single Month bucket alone is already 6,000 cells), so this
        // must return an error rather than silently overshoot the cap.
        let facts: Vec<ExecutionFact> = (0..6_000)
            .map(|i| {
                let mut f = exec_fact(10, "chore_implementation", "completed");
                f.driver = Some(format!("d{i}"));
                f
            })
            .collect();
        let q = query(
            SERIES_EXECUTION_OUTCOMES,
            0,
            BucketWidth::DAY_SECS,
            None,
            Some(DIM_DRIVER),
            &[],
        );
        assert!(matches!(
            build_execution_series_report(&q, &facts, 10),
            Err(SeriesError::CellCapExceeded { .. })
        ));
    }

    #[test]
    fn prs_generated_windowed_query_still_dedupes_against_pre_window_history() {
        // Same URL first appears at t=10 (before the query window), then
        // again at t=10+2h (inside it). The caller must have projected
        // history back before `since`, so the pre-window fact wins the
        // dedup and the revision inside the window contributes nothing.
        let mut original = exec_fact(10, "chore_implementation", "completed");
        original.pr_url = Some("https://github.com/o/r/pull/1".into());
        let mut revision = exec_fact(10 + BucketWidth::HOUR_SECS * 2, "revision_implementation", "completed");
        revision.pr_url = Some("https://github.com/o/r/pull/1".into());

        let q = query(
            SERIES_PRS_GENERATED,
            BucketWidth::HOUR_SECS,
            BucketWidth::HOUR_SECS * 4,
            Some(BucketWidth::Hour),
            None,
            &[],
        );
        let report = build_execution_series_report(&q, &[original, revision], 10).unwrap();
        assert!(report.buckets.is_empty(), "expected no cells, got {:?}", report.buckets);
    }

    #[test]
    fn coverage_data_from_reflects_true_history_not_the_query_window() {
        // The earliest fact predates the window entirely; a client zooming
        // into a later slice must still see the true capture start, not
        // "no coverage" just because nothing in-window happens to be first.
        let early = exec_fact(5, "chore_implementation", "completed");
        let q = query(
            SERIES_EXECUTION_OUTCOMES,
            1_000,
            2_000,
            Some(BucketWidth::Hour),
            None,
            &[],
        );
        let report = build_execution_series_report(&q, &[early], 10).unwrap();
        assert!(report.buckets.is_empty());
        assert_eq!(report.coverage.data_from_epoch_s, Some(5));
    }

    #[test]
    fn coverage_data_from_is_stable_as_the_window_slides_past_it() {
        let fact = exec_fact(5, "chore_implementation", "completed");
        let early_window = query(SERIES_EXECUTION_OUTCOMES, 0, 100, Some(BucketWidth::Hour), None, &[]);
        let late_window = query(
            SERIES_EXECUTION_OUTCOMES,
            1_000,
            2_000,
            Some(BucketWidth::Hour),
            None,
            &[],
        );
        let early_report = build_execution_series_report(&early_window, std::slice::from_ref(&fact), 10).unwrap();
        let late_report = build_execution_series_report(&late_window, &[fact], 10).unwrap();
        assert_eq!(early_report.coverage.data_from_epoch_s, Some(5));
        assert_eq!(late_report.coverage.data_from_epoch_s, Some(5));
    }

    #[test]
    fn unknown_series_and_group_by_are_errors() {
        let q = query("not_a_series", 0, 10, None, None, &[]);
        assert!(matches!(
            build_execution_series_report(&q, &[], 0),
            Err(SeriesError::UnknownSeries(_))
        ));
        let q = query(SERIES_REVIEW_DURATION, 0, 10, None, Some(DIM_STATUS), &[]);
        assert!(matches!(
            build_execution_series_report(&q, &[], 0),
            Err(SeriesError::UnknownGroupBy { .. })
        ));
    }

    #[test]
    fn reversed_window_is_an_error() {
        let q = query(SERIES_EXECUTION_OUTCOMES, 10, 10, None, None, &[]);
        assert!(matches!(
            build_execution_series_report(&q, &[], 0),
            Err(SeriesError::InvalidWindow { .. })
        ));
    }

    #[test]
    fn catalog_includes_the_five_v1_series_and_failed_or_reaped_preset() {
        let mut fact = exec_fact(50, "chore_implementation", "failed");
        fact.driver = Some("claude".into());
        let catalog = build_catalog(&[fact], &[], 99, 0);
        let ids: Vec<&str> = catalog.series.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                SERIES_REVIEW_DURATION,
                SERIES_EXECUTION_DURATION,
                SERIES_TASK_LEAD_TIME,
                SERIES_EXECUTION_OUTCOMES,
                SERIES_PRS_GENERATED,
            ]
        );
        let outcomes = catalog
            .series
            .iter()
            .find(|s| s.id == SERIES_EXECUTION_OUTCOMES)
            .unwrap();
        assert_eq!(outcomes.presets.len(), 1);
        assert_eq!(outcomes.presets[0].id, PRESET_FAILED_OR_REAPED);
        assert_eq!(outcomes.presets[0].filters[0].values, FAILED_OR_REAPED_STATUSES);
        let driver = catalog.dimensions.iter().find(|d| d.id == DIM_DRIVER).unwrap();
        assert_eq!(driver.values[0].value, "claude");
        assert_eq!(driver.values[0].count, 1);
        assert_eq!(driver.values[0].first_seen_epoch_s, 50);
    }

    #[test]
    fn task_lead_time_uses_completed_at_and_duration() {
        let fact = TaskFact::builder()
            .completed_at_epoch_s(500)
            .duration_ms(12_000)
            .kind("chore")
            .build();
        let q = query(SERIES_TASK_LEAD_TIME, 0, 1_000, Some(BucketWidth::Hour), None, &[]);
        let report = build_task_series_report(&q, &[fact], 10).unwrap();
        assert_eq!(report.value_kind, MetricValueKind::Duration);
        assert_eq!(report.buckets[0].cells[0].n, 1);
        match &report.buckets[0].cells[0].value {
            MetricValue::Duration { p50_ms, .. } => assert_eq!(*p50_ms, 12_000),
            other => panic!("expected duration, got {other:?}"),
        }
    }

    #[test]
    fn dimension_started_note_when_group_by_is_newer_than_the_series() {
        let mut early = exec_fact(10, "chore_implementation", "completed");
        early.driver = None;
        let mut later = exec_fact(80, "chore_implementation", "completed");
        later.driver = Some("claude".into());
        let q = query(
            SERIES_EXECUTION_OUTCOMES,
            0,
            200,
            Some(BucketWidth::Hour),
            Some(DIM_DRIVER),
            &[],
        );
        let report = build_execution_series_report(&q, &[early, later], 10).unwrap();
        assert_eq!(report.coverage.data_from_epoch_s, Some(10));
        assert_eq!(report.coverage.dimension_from_epoch_s, Some(80));
        assert!(
            report
                .coverage
                .notes
                .iter()
                .any(|n| n.kind == CoverageNoteKind::DimensionStarted && n.epoch_s == Some(80))
        );
    }

    #[test]
    fn review_duration_ignores_non_completed_reviews() {
        let mut failed = exec_fact(10, "pr_review", "failed");
        failed.duration_ms = Some(5_000);
        let q = query(SERIES_REVIEW_DURATION, 0, 100, Some(BucketWidth::Hour), None, &[]);
        let report = build_execution_series_report(&q, &[failed], 10).unwrap();
        assert!(report.buckets.is_empty());
    }
}
