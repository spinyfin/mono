//! The rows builder: projected records in, the wire tree out.
//!
//! A pure function of [`UsageWindowRecords`] and a [`UsageRowsQuery`], so every
//! honesty rule is a unit test over plain data. The steps, in order:
//!
//! 1. keep runs created in `[since, until)` and classify each one;
//! 2. count each dimension's observed values (for filter chips);
//! 3. apply the filters, then the run cap (most recent runs win);
//! 4. fold the surviving runs into `overall`, coverage and notes;
//! 5. emit the task tree, once, or once per group when grouping.
//!
//! Every sum is made by one [`RunFold`], the same accumulator `boss cost`
//! uses, so the NULL-versus-zero contract is implemented once.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use boss_protocol::{
    CostBucket, CostMeasurement, ExecutionUsageNode, ModelUsage, RunCoverage, RunUsageRow, SpendClass, TaskUsageNode,
    TokenUsageRowsReport, UsageCaveatCount, UsageCoverage, UsageDimension, UsageDimensionValue, UsageDimensionValues,
    UsageFilter, UsageGroup, UsageGroupKey, UsageNote,
};

use crate::classify::{ClassifyContext, RunFacts, UsdEstimator, classify, dimension_value, finalize_kept};
use crate::classify::{known_total, model_label, token_classes};
use crate::fold::{RunFold, UsdOutcome};
use crate::record::{UsageRunRecord, UsageTaskRecord, UsageWindowRecords};

/// The most runs one reply carries. Past it the most recent runs are kept
/// and the reply says so; nothing is dropped silently.
///
/// Sized so a full reply stays around 12 MB of compact JSON (about 2.5 KB per
/// run); a larger cap costs seconds of engine CPU and a reply an app cannot
/// decode comfortably. Latency is not addressed by raising this cap; see the
/// design's Latency budget section.
pub const DEFAULT_RUN_CAP: u32 = 5_000;

/// Grouping depth the reply shape supports: the outer level and one subgroup level.
pub const MAX_GROUP_LEVELS: usize = 2;

/// Note codes on [`UsageCoverage::notes`] and [`boss_protocol::UsageSeriesCoverage::notes`].
pub mod note {
    /// The window starts before per-run token capture began.
    pub const CAPTURE_STARTED: &str = "capture_started";
    /// A grouped dimension or derived metric (`tasks.completed_at`) only has
    /// meaningful history starting at `epoch_s`.
    pub const DIMENSION_STARTED: &str = "dimension_started";
    /// The window starts before the never-started-execution retention bound.
    pub const RETENTION_BOUNDED: &str = "retention_bounded";
    /// A USD estimate left some models out.
    pub const PRICING_GAPS: &str = "pricing_gaps";
    /// The engine coarsened the requested or default bucket width to stay
    /// within the reply cell cap.
    pub const BUCKET_COARSENED: &str = "bucket_coarsened";
    /// The coarsest bucket width still produces more cells than the cap;
    /// every fact is kept and this note says the grid is over cap.
    pub const CELL_CAP_EXCEEDED: &str = "cell_cap_exceeded";
    /// One or more measured runs lacked `agent_active_ms` (e.g. Grok) and were
    /// excluded from both numerator and denominator of `tokens_per_active_hour`.
    pub const ACTIVE_TIME_MISSING: &str = "active_time_missing";
}

/// What a rows request asks for, plus the engine constants the report
/// depends on (so this crate owns none of them).
#[derive(Debug, Clone, bon::Builder)]
pub struct UsageRowsQuery {
    pub since_epoch_s: i64,
    pub until_epoch_s: i64,
    pub generated_at_epoch_s: i64,
    /// Before this instant no run has token data (`TOKEN_CAPTURE_START_EPOCH_S`).
    pub capture_start_epoch_s: i64,
    /// The age past which never-started failed, abandoned, orphaned and
    /// cancelled executions become eligible for pruning
    /// (`DEFAULT_RETENTION_MAX_AGE_SECS`); executions with a worker run are
    /// kept.
    pub retention_max_age_secs: i64,
    #[builder(default)]
    pub filters: Vec<UsageFilter>,
    #[builder(default)]
    pub group_by: Vec<UsageDimension>,
    #[builder(default = true)]
    pub roll_up_revisions: bool,
    #[builder(default = DEFAULT_RUN_CAP)]
    pub run_cap: u32,
}

impl UsageRowsQuery {
    /// Rejects a request the reply shape cannot express.
    pub fn validate(&self) -> Result<(), String> {
        if self.group_by.len() > MAX_GROUP_LEVELS {
            return Err(format!(
                "group_by takes at most {MAX_GROUP_LEVELS} levels, got {}",
                self.group_by.len()
            ));
        }
        for (index, dimension) in self.group_by.iter().enumerate() {
            if self.group_by[..index].contains(dimension) {
                return Err(format!("group_by names {dimension} twice"));
            }
        }
        Ok(())
    }
}

/// Builds the rows report. `pricer` is `Some` exactly when the caller asked
/// for a USD estimate.
pub fn build_usage_rows_report(
    query: &UsageRowsQuery,
    records: &UsageWindowRecords,
    pricer: Option<&dyn UsdEstimator>,
) -> TokenUsageRowsReport {
    let want_usd = pricer.is_some();
    let tasks: HashMap<&str, &UsageTaskRecord> =
        records.tasks.iter().map(|task| (task.task_id.as_str(), task)).collect();
    let classify_ctx = ClassifyContext {
        capture_start_epoch_s: query.capture_start_epoch_s,
        pricer,
    };

    // 1. The window is half-open on `work_runs.created_at`.
    let mut in_window: Vec<RunFacts<'_>> = records
        .runs
        .iter()
        .filter(|run| {
            run.run_created_at_epoch_s >= query.since_epoch_s && run.run_created_at_epoch_s < query.until_epoch_s
        })
        .map(|run| classify(run, tasks.get(run.work_item_id.as_str()).copied()))
        .collect();

    let mut in_window_per_task: HashMap<&str, u32> = HashMap::new();
    for facts in &in_window {
        *in_window_per_task.entry(facts.rec.work_item_id.as_str()).or_default() += 1;
    }

    // 2. Filter chips count what each value would add, so a dimension's own
    //    filter is left out of its own counts.
    let compiled_filters = compile_filters(&query.filters);
    let dimension_values = dimension_values(&in_window, &compiled_filters);

    // 3. Filters, then the cap, then pricing and caveats on the surviving runs.
    let mut kept_mut: Vec<&mut RunFacts<'_>> = in_window
        .iter_mut()
        .filter(|facts| passes_filters(facts, &compiled_filters, None))
        .collect();
    let truncated = kept_mut.len() > query.run_cap as usize;
    if truncated {
        kept_mut.sort_by(|a, b| {
            b.rec
                .run_created_at_epoch_s
                .cmp(&a.rec.run_created_at_epoch_s)
                .then_with(|| b.rec.run_id.cmp(&a.rec.run_id))
        });
        kept_mut.truncate(query.run_cap as usize);
    }
    for facts in &mut kept_mut {
        finalize_kept(facts, &classify_ctx);
    }
    let kept: Vec<&RunFacts<'_>> = kept_mut.into_iter().map(|facts| &*facts).collect();

    // 4. Totals, coverage, notes.
    let overall = measure(&kept);
    let gaps = pricing_gaps(&kept);
    let coverage = coverage(query, &kept, &gaps, want_usd);

    // 5. The tree.
    let ctx = TreeCtx {
        tasks: &tasks,
        in_window_per_task: &in_window_per_task,
        roll_up: query.roll_up_revisions,
    };
    let (task_nodes, taskless, groups) = if query.group_by.is_empty() {
        let (task_nodes, taskless) = build_tree(&ctx, &kept);
        (task_nodes, taskless, Vec::new())
    } else {
        (Vec::new(), Vec::new(), build_groups(&ctx, &kept, &query.group_by, &[]))
    };

    TokenUsageRowsReport::builder()
        .capture_start_epoch_s(query.capture_start_epoch_s)
        .coverage(coverage)
        .generated_at_epoch_s(query.generated_at_epoch_s)
        .overall(overall)
        .run_cap(query.run_cap)
        .since_epoch_s(query.since_epoch_s)
        .spans_capture_boundary(query.since_epoch_s < query.capture_start_epoch_s)
        .truncated(truncated)
        .until_epoch_s(query.until_epoch_s)
        .dimension_values(dimension_values)
        .group_by(query.group_by.clone())
        .groups(groups)
        .pricing_gaps(gaps.into_iter().collect())
        .taskless(taskless)
        .tasks(task_nodes)
        .build()
}

// ── Accumulation ──────────────────────────────────────────────────────────

fn measure(runs: &[&RunFacts<'_>]) -> CostMeasurement {
    let mut fold = RunFold::default();
    for facts in runs {
        fold.add(&facts.sample());
    }
    fold.finish()
}

fn spend_class_buckets(runs: &[&RunFacts<'_>]) -> Vec<CostBucket> {
    let mut by_class: BTreeMap<SpendClass, RunFold> = BTreeMap::new();
    for facts in runs {
        by_class.entry(facts.spend_class).or_default().add(&facts.sample());
    }
    let mut buckets: Vec<CostBucket> = by_class
        .into_iter()
        .map(|(class, acc)| CostBucket {
            label: class.as_str().to_owned(),
            measurement: acc.finish(),
        })
        .collect();
    buckets.sort_by(|a, b| {
        b.measurement
            .total_tokens
            .cmp(&a.measurement.total_tokens)
            .then_with(|| a.label.cmp(&b.label))
    });
    buckets
}

// ── Filters, dimension values, coverage ───────────────────────────────────

/// A filter with no values restricts nothing, so a client can send one chip
/// row per dimension without special-casing an empty selection. Several
/// filters on the same dimension union their values (OR within one
/// dimension), matching the wire contract and a client that sends one chip
/// row per value. Distinct dimensions still AND.
pub(crate) fn compile_filters(filters: &[UsageFilter]) -> Vec<(UsageDimension, HashSet<&str>)> {
    let mut by_dimension: BTreeMap<UsageDimension, HashSet<&str>> = BTreeMap::new();
    for filter in filters {
        if filter.values.is_empty() {
            continue;
        }
        by_dimension
            .entry(filter.dimension)
            .or_default()
            .extend(filter.values.iter().map(String::as_str));
    }
    by_dimension.into_iter().collect()
}

pub(crate) fn passes_filters(
    facts: &RunFacts<'_>,
    filters: &[(UsageDimension, HashSet<&str>)],
    skip: Option<UsageDimension>,
) -> bool {
    filters.iter().all(|(dimension, values)| {
        if Some(*dimension) == skip {
            return true;
        }
        let (value, _) = dimension_value(facts, *dimension);
        values.contains(value)
    })
}

fn dimension_values(
    in_window: &[RunFacts<'_>],
    filters: &[(UsageDimension, HashSet<&str>)],
) -> Vec<UsageDimensionValues> {
    UsageDimension::ALL
        .into_iter()
        .map(|dimension| {
            let mut counts: HashMap<&str, (&str, u32)> = HashMap::new();
            for facts in in_window
                .iter()
                .filter(|facts| passes_filters(facts, filters, Some(dimension)))
            {
                let (value, label) = dimension_value(facts, dimension);
                counts.entry(value).or_insert((label, 0)).1 += 1;
            }
            let mut values: Vec<UsageDimensionValue> = counts
                .into_iter()
                .map(|(value, (label, runs))| UsageDimensionValue {
                    value: value.to_owned(),
                    label: label.to_owned(),
                    runs,
                })
                .collect();
            values.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.value.cmp(&b.value)));
            UsageDimensionValues { dimension, values }
        })
        .collect()
}

/// Distinct models a requested USD estimate left out, among measured runs.
/// Subscription-billed drivers (`subscription_unpriced`) are excluded by driver
/// rather than a pricing gap, so they are omitted.
pub(crate) fn pricing_gaps(runs: &[&RunFacts<'_>]) -> BTreeSet<String> {
    runs.iter()
        .filter(|facts| {
            facts.coverage != RunCoverage::Unmeasured
                && matches!(
                    facts.usd,
                    UsdOutcome::Unpriceable(c) if c != crate::classify::cause::SUBSCRIPTION_UNPRICED
                )
        })
        .map(|facts| model_label(facts.rec))
        .collect()
}

fn coverage(query: &UsageRowsQuery, runs: &[&RunFacts<'_>], gaps: &BTreeSet<String>, want_usd: bool) -> UsageCoverage {
    let count = |state: RunCoverage| runs.iter().filter(|facts| facts.coverage == state).count() as u32;
    let mut by_caveat: BTreeMap<&str, u32> = BTreeMap::new();
    for facts in runs {
        for caveat in &facts.caveats {
            *by_caveat.entry(caveat.code.as_str()).or_default() += 1;
        }
    }
    let mut notes = Vec::new();
    if query.since_epoch_s < query.capture_start_epoch_s {
        notes.push(UsageNote {
            code: note::CAPTURE_STARTED.to_owned(),
            detail: format!(
                "the window starts before per-run token capture began (epoch {}); earlier runs have no token data, so every figure is a floor",
                query.capture_start_epoch_s
            ),
        });
    }
    if query.since_epoch_s < query.generated_at_epoch_s.saturating_sub(query.retention_max_age_secs) {
        notes.push(UsageNote {
            code: note::RETENTION_BOUNDED.to_owned(),
            detail: format!(
                "the window starts more than {} days ago; never-started failed, abandoned, orphaned and cancelled executions older than that are pruned past a per-work-item floor, so older ranges can undercount those never-started rows. Executions that had a worker run are retained",
                query.retention_max_age_secs / 86_400
            ),
        });
    }
    if !gaps.is_empty() {
        notes.push(UsageNote {
            code: note::PRICING_GAPS.to_owned(),
            detail: format!(
                "the USD estimate leaves out runs it cannot price (unverified rates, gross input or unknown tokens); models: {}",
                gaps.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
        });
    }
    UsageCoverage::builder()
        .runs_complete(count(RunCoverage::Measured))
        .runs_partial(count(RunCoverage::Partial))
        .runs_total(runs.len() as u32)
        .runs_unmeasured(count(RunCoverage::Unmeasured))
        .runs_zero(count(RunCoverage::Zero))
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
        .maybe_runs_priceable(want_usd.then(|| {
            runs.iter()
                .filter(|facts| matches!(facts.usd, UsdOutcome::Estimate(_)))
                .count() as u32
        }))
        .build()
}

// ── The tree ──────────────────────────────────────────────────────────────

struct TreeCtx<'a> {
    tasks: &'a HashMap<&'a str, &'a UsageTaskRecord>,
    in_window_per_task: &'a HashMap<&'a str, u32>,
    roll_up: bool,
}

impl TreeCtx<'_> {
    /// The chain root a task nests under. A task whose resolved root has no
    /// record (it should not happen) is its own root rather than a dangling
    /// reference.
    fn root_of<'t>(&self, task: &'t UsageTaskRecord) -> &'t str {
        if task.chain_root_id != task.task_id && self.tasks.contains_key(task.chain_root_id.as_str()) {
            task.chain_root_id.as_str()
        } else {
            task.task_id.as_str()
        }
    }
}

fn build_groups(
    ctx: &TreeCtx<'_>,
    runs: &[&RunFacts<'_>],
    dims: &[UsageDimension],
    path: &[UsageGroupKey],
) -> Vec<UsageGroup> {
    let dimension = dims[0];
    let mut parts: HashMap<&str, (&str, Vec<&RunFacts<'_>>)> = HashMap::new();
    for facts in runs {
        let (value, label) = dimension_value(facts, dimension);
        parts.entry(value).or_insert_with(|| (label, Vec::new())).1.push(facts);
    }
    let mut groups: Vec<UsageGroup> = parts
        .into_iter()
        .map(|(value, (label, members))| {
            let mut key = path.to_vec();
            key.push(UsageGroupKey {
                dimension,
                value: value.to_owned(),
                label: label.to_owned(),
            });
            let measurement = measure(&members);
            if dims.len() > 1 {
                UsageGroup::builder()
                    .key(key.clone())
                    .measurement(measurement)
                    .subgroups(build_groups(ctx, &members, &dims[1..], &key))
                    .build()
            } else {
                let (tasks, taskless) = build_tree(ctx, &members);
                UsageGroup::builder()
                    .key(key)
                    .measurement(measurement)
                    .tasks(tasks)
                    .taskless(taskless)
                    .build()
            }
        })
        .collect();
    // Highest spend first; ties break on the value so output is deterministic.
    groups.sort_by(|a, b| {
        b.measurement
            .total_tokens
            .cmp(&a.measurement.total_tokens)
            .then_with(|| a.key.last().map(|k| &k.value).cmp(&b.key.last().map(|k| &k.value)))
    });
    groups
}

/// The task tree and the taskless executions for one set of runs.
fn build_tree(ctx: &TreeCtx<'_>, runs: &[&RunFacts<'_>]) -> (Vec<TaskUsageNode>, Vec<ExecutionUsageNode>) {
    let (task_runs, taskless_runs): (Vec<&RunFacts<'_>>, Vec<&RunFacts<'_>>) =
        runs.iter().copied().partition(|facts| facts.task.is_some());
    let taskless = execution_nodes(&taskless_runs);

    let mut by_task: BTreeMap<&str, Vec<&RunFacts<'_>>> = BTreeMap::new();
    for facts in task_runs {
        by_task.entry(facts.rec.work_item_id.as_str()).or_default().push(facts);
    }
    // Chain root → the tasks of that chain that have runs here.
    let mut chains: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (task_id, members) in &by_task {
        let task = members[0].task.expect("task runs carry their task");
        chains.entry(ctx.root_of(task)).or_default().push(task_id);
    }

    let mut top: Vec<TaskUsageNode> = Vec::new();
    for (root_id, member_ids) in chains {
        let chain_runs: Vec<&RunFacts<'_>> = member_ids.iter().flat_map(|id| by_task[id].iter().copied()).collect();
        let inclusive = measure(&chain_runs);
        if ctx.roll_up {
            let root_record = ctx.tasks[root_id];
            let own: &[&RunFacts<'_>] = by_task.get(root_id).map(Vec::as_slice).unwrap_or(&[]);
            let mut node = task_node(ctx, root_record, own);
            node.inclusive = Some(inclusive);
            let mut revisions: Vec<(i64, TaskUsageNode)> = member_ids
                .iter()
                .filter(|id| **id != root_id)
                .map(|id| {
                    let members = &by_task[id];
                    let first = members.iter().map(|f| f.rec.run_created_at_epoch_s).min().unwrap_or(0);
                    (
                        first,
                        task_node(ctx, members[0].task.expect("task runs carry their task"), members),
                    )
                })
                .collect();
            revisions.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.task_id.cmp(&b.1.task_id)));
            node.revisions = revisions.into_iter().map(|(_, node)| node).collect();
            top.push(node);
        } else {
            for id in member_ids {
                let members = &by_task[id];
                let mut node = task_node(ctx, members[0].task.expect("task runs carry their task"), members);
                if id == root_id {
                    node.inclusive = Some(inclusive.clone());
                }
                top.push(node);
            }
        }
    }
    top.sort_by(|a, b| {
        let key = |node: &TaskUsageNode| node.inclusive.as_ref().unwrap_or(&node.direct).total_tokens;
        key(b).cmp(&key(a)).then_with(|| a.task_id.cmp(&b.task_id))
    });
    (top, taskless)
}

fn task_node(ctx: &TreeCtx<'_>, task: &UsageTaskRecord, runs: &[&RunFacts<'_>]) -> TaskUsageNode {
    let in_window = ctx.in_window_per_task.get(task.task_id.as_str()).copied().unwrap_or(0);
    TaskUsageNode::builder()
        .task_id(&task.task_id)
        .chain_root_id(ctx.root_of(task))
        .direct(measure(runs))
        .is_chain_root(ctx.root_of(task) == task.task_id)
        .kind(&task.kind)
        .name(&task.name)
        .product_id(&task.product_id)
        .runs_outside_window(task.run_count_all_time.saturating_sub(in_window))
        .status(&task.status)
        .by_spend_class(spend_class_buckets(runs))
        .executions(execution_nodes(runs))
        .maybe_completed_at_epoch_s(task.completed_at_epoch_s)
        .maybe_effort_current(task.effort_level.clone())
        .maybe_pr_url(task.pr_url.clone())
        .maybe_project_id(task.project_id.clone())
        .maybe_reasoning_current(task.reasoning.clone())
        .maybe_short_id(task.short_id)
        .build()
}

fn execution_nodes(runs: &[&RunFacts<'_>]) -> Vec<ExecutionUsageNode> {
    let mut by_execution: BTreeMap<&str, Vec<&RunFacts<'_>>> = BTreeMap::new();
    for facts in runs {
        by_execution
            .entry(facts.rec.execution_id.as_str())
            .or_default()
            .push(facts);
    }
    let mut nodes: Vec<ExecutionUsageNode> = by_execution
        .into_values()
        .map(|mut members| {
            members.sort_by(|a, b| {
                a.rec
                    .run_created_at_epoch_s
                    .cmp(&b.rec.run_created_at_epoch_s)
                    .then_with(|| a.rec.run_id.cmp(&b.rec.run_id))
            });
            let first = members[0];
            let rec = first.rec;
            ExecutionUsageNode::builder()
                .execution_id(&rec.execution_id)
                .work_item_id(&rec.work_item_id)
                .created_at_epoch_s(rec.execution_created_at_epoch_s)
                .driver_source(rec.driver.source())
                .kind(&rec.execution_kind)
                .measurement(measure(&members))
                .spend_class(first.spend_class)
                .status(&rec.execution_status)
                .runs(members.iter().map(|facts| run_row(facts)).collect())
                .maybe_driver(first.driver.map(str::to_owned))
                .maybe_effort_launched(rec.execution_effort.clone())
                .maybe_finished_at_epoch_s(rec.execution_finished_at_epoch_s)
                .maybe_host_id(rec.execution_host_id.clone())
                .maybe_model_launched(rec.execution_model.clone())
                .maybe_pr_url(rec.execution_pr_url.clone())
                .maybe_started_at_epoch_s(rec.execution_started_at_epoch_s)
                .build()
        })
        .collect();
    nodes.sort_by(|a, b| {
        a.created_at_epoch_s
            .cmp(&b.created_at_epoch_s)
            .then_with(|| a.execution_id.cmp(&b.execution_id))
    });
    nodes
}

fn run_row(facts: &RunFacts<'_>) -> RunUsageRow {
    let rec: &UsageRunRecord = facts.rec;
    let tokens = token_classes(rec, facts.coverage);
    let models = rec
        .run_model
        .iter()
        .map(|model| ModelUsage {
            model: model.clone(),
            tokens: tokens.clone(),
        })
        .collect();
    let (estimated_usd, unpriceable_cause) = match facts.usd {
        UsdOutcome::NotRequested => (None, None),
        UsdOutcome::Estimate(usd) => (Some(usd), None),
        UsdOutcome::Unpriceable(cause) => (None, Some(cause.to_owned())),
    };
    RunUsageRow::builder()
        .run_id(&rec.run_id)
        .coverage(facts.coverage)
        .created_at_epoch_s(rec.run_created_at_epoch_s)
        .host_id(&rec.run_host_id)
        .input_convention(facts.convention)
        .status(&rec.run_status)
        .caveats(facts.caveats.clone())
        .models(models)
        .maybe_tokens(tokens)
        .maybe_agent_active_ms(rec.agent_active_ms)
        .maybe_finished_at_epoch_s(rec.run_finished_at_epoch_s)
        .maybe_model_observed(rec.run_model.clone())
        .maybe_rounds(rec.rounds)
        .maybe_started_at_epoch_s(rec.run_started_at_epoch_s)
        .maybe_total_tokens(known_total(rec, facts.coverage))
        .maybe_usage_unavailable(rec.usage_unavailable.clone())
        .maybe_estimated_usd(estimated_usd)
        .maybe_unpriceable_cause(unpriceable_cause)
        .build()
}
