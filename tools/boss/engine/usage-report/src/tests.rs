//! Each test pins one rule the usage builders enforce, over plain records.

use boss_protocol::{
    DriverSource, InputConvention, RunCoverage, SpendClass, TaskUsageNode, TokenUsageRowsReport, UsageDimension,
    UsageFilter,
};

use super::*;

const CAPTURE_START: i64 = 1_785_144_180;
const T0: i64 = 1_790_000_000;
const DAY: i64 = 86_400;
const RETENTION: i64 = 14 * DAY;

fn query(since: i64, until: i64) -> UsageRowsQuery {
    UsageRowsQuery::builder()
        .since_epoch_s(since)
        .until_epoch_s(until)
        .generated_at_epoch_s(until)
        .capture_start_epoch_s(CAPTURE_START)
        .retention_max_age_secs(RETENTION)
        .build()
}

fn window() -> UsageRowsQuery {
    query(T0 - 100, T0 + 100_000)
}

/// A completed Claude run with no recorded tokens, owned by `item`.
fn run(id: &str, execution: &str, item: &str, created: i64) -> UsageRunRecord {
    UsageRunRecord::builder()
        .run_id(id)
        .run_status("completed")
        .run_created_at_epoch_s(created)
        .run_host_id("local")
        .execution_id(execution)
        .work_item_id(item)
        .execution_kind("task_implementation")
        .execution_status("completed")
        .execution_created_at_epoch_s(created)
        .driver("claude")
        .build()
}

fn tokens(mut record: UsageRunRecord, input: i64, output: i64, cache_creation: i64, cache_read: i64) -> UsageRunRecord {
    record.input_tokens = Some(input);
    record.output_tokens = Some(output);
    record.cache_creation_tokens = Some(cache_creation);
    record.cache_read_tokens = Some(cache_read);
    record
}

fn with_driver(mut record: UsageRunRecord, driver: Option<&str>) -> UsageRunRecord {
    record.driver = driver.map(str::to_owned);
    record
}

fn with_model(mut record: UsageRunRecord, model: &str) -> UsageRunRecord {
    record.run_model = Some(model.to_owned());
    record
}

fn task(id: &str, root: &str, runs_all_time: u32) -> UsageTaskRecord {
    UsageTaskRecord::builder()
        .task_id(id)
        .name(format!("task {id}"))
        .kind(if id == root { "task" } else { "revision" })
        .status("done")
        .product_id("prod_a")
        .product_name("Product A")
        .chain_root_id(root)
        .run_count_all_time(runs_all_time)
        .build()
}

fn records(runs: Vec<UsageRunRecord>, tasks: Vec<UsageTaskRecord>) -> UsageWindowRecords {
    UsageWindowRecords { runs, tasks }
}

fn run_codes(report: &TokenUsageRowsReport) -> Vec<Vec<String>> {
    fn collect(tasks: &[TaskUsageNode], out: &mut Vec<Vec<String>>) {
        for task in tasks {
            for execution in &task.executions {
                for run in &execution.runs {
                    out.push(run.caveats.iter().map(|c| c.code.clone()).collect());
                }
            }
            collect(&task.revisions, out);
        }
    }
    let mut out = Vec::new();
    collect(&report.tasks, &mut out);
    out
}

fn only_run(report: &TokenUsageRowsReport) -> &boss_protocol::RunUsageRow {
    &report.tasks[0].executions[0].runs[0]
}

fn build(query: &UsageRowsQuery, records: &UsageWindowRecords) -> TokenUsageRowsReport {
    build_usage_rows_report(query, records, None)
}

fn flat_pricer(_model: &str, _tokens: PricedTokens) -> Option<f64> {
    Some(2.0)
}

fn priced_only_claude(model: &str, _tokens: PricedTokens) -> Option<f64> {
    model.starts_with("claude").then_some(1.5)
}

// ── NULL is never zero ────────────────────────────────────────────────────

#[test]
fn an_unmeasured_run_is_excluded_from_every_sum_and_never_read_as_zero() {
    let report = build(
        &window(),
        &records(
            vec![
                tokens(run("r1", "e1", "t1", T0), 100, 50, 10, 5),
                run("r2", "e1", "t1", T0 + 1),
            ],
            vec![task("t1", "t1", 2)],
        ),
    );
    let overall = &report.overall;
    assert_eq!(
        (overall.runs_total, overall.runs_measured, overall.runs_unmeasured),
        (2, 1, 1)
    );
    assert_eq!(overall.runs_zero, 0);
    assert_eq!(overall.total_tokens, 165);
    assert_eq!(overall.input_tokens, Some(100));
    assert!(!overall.tokens_partial);

    let runs = &report.tasks[0].executions[0].runs;
    let unmeasured = runs.iter().find(|r| r.run_id == "r2").unwrap();
    assert_eq!(unmeasured.coverage, RunCoverage::Unmeasured);
    assert_eq!(
        unmeasured.tokens, None,
        "an unmeasured run carries no token classes at all"
    );
    assert_eq!(unmeasured.total_tokens, None, "and no total: unknown, never zero");
    assert_eq!(report.coverage.runs_unmeasured, 1);
    assert_eq!(report.coverage.runs_complete, 1);
}

#[test]
fn a_window_of_only_unmeasured_runs_reports_unknown_classes_not_zero() {
    let report = build(
        &window(),
        &records(vec![run("r1", "e1", "t1", T0)], vec![task("t1", "t1", 1)]),
    );
    let overall = &report.overall;
    assert_eq!(overall.runs_measured, 0);
    assert_eq!(overall.input_tokens, None);
    assert_eq!(overall.output_tokens, None);
    assert_eq!(overall.cache_creation_tokens, None);
    assert_eq!(overall.cache_read_tokens, None);
}

#[test]
fn a_missing_class_stays_unknown_and_makes_the_run_partial() {
    // Grok and Antigravity never report cache creation: the other classes are
    // present, that one is NULL.
    let mut partial = run("r1", "e1", "t1", T0);
    partial.driver = Some("grok".into());
    partial.input_tokens = Some(2_486);
    partial.output_tokens = Some(115);
    partial.cache_read_tokens = Some(24_576);
    let report = build(&window(), &records(vec![partial], vec![task("t1", "t1", 1)]));

    let row = only_run(&report);
    assert_eq!(row.coverage, RunCoverage::Partial);
    assert_eq!(
        row.total_tokens,
        Some(2_486 + 115 + 24_576),
        "the known classes only, a floor"
    );
    let classes = row.tokens.as_ref().expect("a partial run is measured");
    assert_eq!(classes.input, Some(2_486));
    assert_eq!(classes.cache_creation, None, "the unreported class is NULL, not 0");
    assert_eq!(classes.cache_read, Some(24_576));

    let overall = &report.overall;
    assert_eq!(overall.cache_creation_tokens, None);
    assert_eq!(overall.input_tokens, Some(2_486));
    assert!(overall.tokens_partial, "known classes are a floor");
    assert_eq!(overall.total_tokens, 2_486 + 115 + 24_576);
    assert_eq!((report.coverage.runs_partial, report.coverage.runs_complete), (1, 0));
}

#[test]
fn per_class_nulls_never_collapse_into_zero_in_a_mixed_set() {
    let mut output_only = run("r2", "e1", "t1", T0 + 1);
    output_only.output_tokens = Some(7);
    let report = build(
        &window(),
        &records(
            vec![tokens(run("r1", "e1", "t1", T0), 10, 20, 30, 40), output_only],
            vec![task("t1", "t1", 2)],
        ),
    );
    let overall = &report.overall;
    // Output is known on both runs; every other class is unknown on one.
    assert_eq!(overall.output_tokens, Some(27));
    assert_eq!(overall.input_tokens, None);
    assert_eq!(overall.cache_creation_tokens, None);
    assert_eq!(overall.cache_read_tokens, None);
    assert!(overall.tokens_partial);
    assert_eq!(overall.runs_measured, 2);
}

#[test]
fn only_a_fully_recorded_zero_counts_as_zero() {
    let mut input_zero_only = run("r2", "e1", "t1", T0 + 1);
    input_zero_only.input_tokens = Some(0);
    let report = build(
        &window(),
        &records(
            vec![tokens(run("r1", "e1", "t1", T0), 0, 0, 0, 0), input_zero_only],
            vec![task("t1", "t1", 2)],
        ),
    );
    let runs = &report.tasks[0].executions[0].runs;
    assert_eq!(runs[0].coverage, RunCoverage::Zero);
    assert_eq!(runs[0].tokens.as_ref().unwrap().input, Some(0));
    assert_eq!(
        runs[1].coverage,
        RunCoverage::Partial,
        "one recorded zero among NULLs is not a zero run"
    );
    assert_eq!(report.overall.runs_zero, 1);
}

// ── Totals ────────────────────────────────────────────────────────────────

fn mixed_records() -> UsageWindowRecords {
    let mut codex = tokens(run("r3", "e3", "t2", T0 + 3), 500, 60, 0, 400);
    codex.driver = Some("codex".into());
    let mut review = tokens(run("r5", "e5", "comment_1", T0 + 5), 9, 9, 0, 0);
    review.execution_kind = "answer_agent".into();
    records(
        vec![
            with_model(tokens(run("r1", "e1", "t1", T0), 100, 10, 5, 50), "claude-opus"),
            with_model(tokens(run("r2", "e2", "rev1", T0 + 1), 40, 4, 2, 20), "claude-opus"),
            codex,
            run("r4", "e4", "rev2", T0 + 4),
            review,
        ],
        vec![
            task("t1", "t1", 1),
            task("rev1", "t1", 1),
            task("rev2", "t1", 1),
            task("t2", "t2", 1),
        ],
    )
}

fn sum_direct(tasks: &[TaskUsageNode]) -> (i64, u32) {
    tasks.iter().fold((0, 0), |(tokens, runs), node| {
        let (child_tokens, child_runs) = sum_direct(&node.revisions);
        (
            tokens + node.direct.total_tokens + child_tokens,
            runs + node.direct.runs_total + child_runs,
        )
    })
}

#[test]
fn overall_is_the_sum_of_direct_over_tasks_and_taskless_never_inclusive() {
    let report = build(&window(), &mixed_records());
    let taskless_tokens: i64 = report.taskless.iter().map(|e| e.measurement.total_tokens).sum();
    let taskless_runs: u32 = report.taskless.iter().map(|e| e.measurement.runs_total).sum();
    let (task_tokens, task_runs) = sum_direct(&report.tasks);

    assert_eq!(report.overall.total_tokens, task_tokens + taskless_tokens);
    assert_eq!(report.overall.runs_total, task_runs + taskless_runs);
    assert_eq!(report.overall.runs_total, 5);

    assert_eq!(
        report.overall.total_tokens,
        1_191 + 18,
        "direct of 4 task runs plus the taskless run"
    );
}

#[test]
fn group_sums_equal_overall_at_each_level() {
    let mut query = window();
    query.group_by = vec![UsageDimension::Driver, UsageDimension::ExecutionKind];
    let report = build(&query, &mixed_records());

    assert!(
        report.tasks.is_empty() && report.taskless.is_empty(),
        "a grouped reply carries trees only in groups"
    );
    let outer_tokens: i64 = report.groups.iter().map(|g| g.measurement.total_tokens).sum();
    let outer_runs: u32 = report.groups.iter().map(|g| g.measurement.runs_total).sum();
    assert_eq!(outer_tokens, report.overall.total_tokens);
    assert_eq!(outer_runs, report.overall.runs_total);

    for group in &report.groups {
        let inner_tokens: i64 = group.subgroups.iter().map(|g| g.measurement.total_tokens).sum();
        let inner_runs: u32 = group.subgroups.iter().map(|g| g.measurement.runs_total).sum();
        assert_eq!(inner_tokens, group.measurement.total_tokens, "{:?}", group.key);
        assert_eq!(inner_runs, group.measurement.runs_total, "{:?}", group.key);
        assert!(group.tasks.is_empty(), "a non-leaf group holds subgroups, not a tree");
        for sub in &group.subgroups {
            assert_eq!(sub.key.len(), 2, "a subgroup's key is the full path");
            let (tree_tokens, tree_runs) = sum_direct(&sub.tasks);
            let taskless: i64 = sub.taskless.iter().map(|e| e.measurement.total_tokens).sum();
            let taskless_runs: u32 = sub.taskless.iter().map(|e| e.measurement.runs_total).sum();
            assert_eq!(tree_tokens + taskless, sub.measurement.total_tokens);
            assert_eq!(tree_runs + taskless_runs, sub.measurement.runs_total);
        }
    }
}

#[test]
fn a_task_with_two_drivers_appears_under_both_groups_with_only_matching_executions() {
    let mut codex = tokens(run("r2", "e2", "t1", T0 + 1), 5, 5, 0, 0);
    codex.driver = Some("codex".into());
    let mut query = window();
    query.group_by = vec![UsageDimension::Driver];
    let report = build(
        &query,
        &records(
            vec![tokens(run("r1", "e1", "t1", T0), 1, 1, 0, 0), codex],
            vec![task("t1", "t1", 2)],
        ),
    );
    assert_eq!(report.groups.len(), 2);
    for group in &report.groups {
        assert_eq!(group.tasks.len(), 1);
        assert_eq!(
            group.tasks[0].executions.len(),
            1,
            "only this group's execution sits beneath"
        );
        assert_eq!(group.tasks[0].direct.runs_total, 1);
    }
}

// ── Chains ────────────────────────────────────────────────────────────────

#[test]
fn inclusive_exists_only_on_chain_roots() {
    let report = build(&window(), &mixed_records());
    let root = report.tasks.iter().find(|t| t.task_id == "t1").unwrap();
    assert!(root.is_chain_root);
    let inclusive = root.inclusive.as_ref().expect("a chain root carries inclusive");
    assert_eq!(root.revisions.len(), 2);
    let revisions_direct: i64 = root.revisions.iter().map(|r| r.direct.total_tokens).sum();
    assert_eq!(inclusive.total_tokens, root.direct.total_tokens + revisions_direct);
    assert_eq!(inclusive.runs_total, 3);
    for revision in &root.revisions {
        assert!(!revision.is_chain_root);
        assert_eq!(revision.chain_root_id, "t1");
        assert_eq!(revision.inclusive, None, "a revision never carries inclusive");
        assert!(revision.revisions.is_empty());
    }
    let lone = report.tasks.iter().find(|t| t.task_id == "t2").unwrap();
    assert!(lone.is_chain_root, "a task with no revisions is its own chain root");
    assert_eq!(lone.inclusive.as_ref().unwrap().total_tokens, lone.direct.total_tokens);
}

#[test]
fn a_root_with_no_runs_of_its_own_is_still_emitted_to_hold_its_revisions() {
    let report = build(
        &window(),
        &records(
            vec![tokens(run("r1", "e1", "rev1", T0), 30, 3, 0, 0)],
            vec![task("t1", "t1", 4), task("rev1", "t1", 1)],
        ),
    );
    assert_eq!(
        report.tasks.len(),
        1,
        "the revision nests; it is not a second top-level row"
    );
    let root = &report.tasks[0];
    assert_eq!(root.task_id, "t1");
    assert!(root.executions.is_empty());
    assert_eq!(root.direct.runs_total, 0);
    assert_eq!(root.direct.total_tokens, 0);
    assert_eq!(root.direct.input_tokens, None, "no measured run: unknown, not zero");
    assert_eq!(
        root.runs_outside_window, 4,
        "the root's own runs all fall outside the window"
    );
    assert_eq!(root.revisions.len(), 1);
    assert_eq!(root.inclusive.as_ref().unwrap().total_tokens, 33);
    assert_eq!(
        report.overall.runs_total, 1,
        "an emitted empty root adds nothing to the list total"
    );
}

#[test]
fn revisions_are_top_level_siblings_when_not_rolled_up() {
    let mut query = window();
    query.roll_up_revisions = false;
    let report = build(&query, &mixed_records());
    assert!(report.tasks.iter().all(|t| t.revisions.is_empty()));
    let ids: Vec<&str> = report.tasks.iter().map(|t| t.task_id.as_str()).collect();
    assert!(ids.contains(&"rev1") && ids.contains(&"rev2") && ids.contains(&"t1"));
    for node in &report.tasks {
        assert_eq!(node.inclusive.is_some(), node.is_chain_root, "{}", node.task_id);
        assert_eq!(node.chain_root_id == node.task_id, node.is_chain_root);
    }
    let (tokens, runs) = sum_direct(&report.tasks);
    let taskless: i64 = report.taskless.iter().map(|e| e.measurement.total_tokens).sum();
    assert_eq!(tokens + taskless, report.overall.total_tokens);
    assert_eq!(runs, 4);
    // Summing inclusive down a list that also lists the revisions double counts them.
    let naive: i64 = report
        .tasks
        .iter()
        .map(|t| t.inclusive.as_ref().unwrap_or(&t.direct).total_tokens)
        .sum();
    assert!(naive > tokens, "{naive} should exceed the direct sum {tokens}");
}

#[test]
fn a_revision_whose_root_has_no_record_is_its_own_root() {
    let report = build(
        &window(),
        &records(
            vec![tokens(run("r1", "e1", "rev1", T0), 1, 1, 0, 0)],
            vec![task("rev1", "missing_root", 1)],
        ),
    );
    assert_eq!(report.tasks.len(), 1);
    assert!(report.tasks[0].is_chain_root);
    assert_eq!(report.tasks[0].chain_root_id, "rev1");
}

#[test]
fn runs_outside_window_counts_a_tasks_other_runs() {
    let report = build(
        &window(),
        &records(
            vec![tokens(run("r1", "e1", "t1", T0), 1, 1, 0, 0)],
            vec![task("t1", "t1", 6)],
        ),
    );
    assert_eq!(report.tasks[0].runs_outside_window, 5);
}

// ── Taskless executions ───────────────────────────────────────────────────

#[test]
fn taskless_runs_are_listed_and_counted_in_overall() {
    let report = build(&window(), &mixed_records());
    assert_eq!(report.taskless.len(), 1);
    let node = &report.taskless[0];
    assert_eq!(node.kind, "answer_agent");
    assert_eq!(node.work_item_id, "comment_1");
    assert_eq!(node.spend_class, SpendClass::Overhead);
    assert_eq!(report.overall.runs_total, 5, "the taskless run is in the window total");
    assert_eq!(report.coverage.runs_total, 5);
}

// ── Window, cap, notes ────────────────────────────────────────────────────

#[test]
fn the_window_is_half_open_on_run_created_at() {
    let query = query(T0, T0 + 10);
    let report = build(
        &query,
        &records(
            vec![
                run("before", "e0", "t1", T0 - 1),
                run("at_since", "e1", "t1", T0),
                run("last", "e2", "t1", T0 + 9),
                run("at_until", "e3", "t1", T0 + 10),
            ],
            vec![task("t1", "t1", 4)],
        ),
    );
    let ids: Vec<&str> = report.tasks[0]
        .executions
        .iter()
        .flat_map(|e| e.runs.iter().map(|r| r.run_id.as_str()))
        .collect();
    assert_eq!(ids, ["at_since", "last"]);
    assert_eq!(report.tasks[0].runs_outside_window, 2);
}

#[test]
fn truncation_keeps_the_most_recent_runs_and_says_so() {
    let mut query = window();
    query.run_cap = 3;
    let runs: Vec<_> = (0..5)
        .map(|i| tokens(run(&format!("r{i}"), &format!("e{i}"), "t1", T0 + i), 1, 0, 0, 0))
        .collect();
    let report = build(&query, &records(runs, vec![task("t1", "t1", 5)]));
    assert!(report.truncated);
    assert_eq!(report.run_cap, 3);
    assert_eq!(
        report.overall.runs_total, 3,
        "the totals describe the reply, not the window"
    );
    let kept: Vec<&str> = report.tasks[0]
        .executions
        .iter()
        .map(|e| e.runs[0].run_id.as_str())
        .collect();
    assert_eq!(kept, ["r2", "r3", "r4"]);

    query.run_cap = 5;
    let exact = build(
        &query,
        &records(
            (0..5)
                .map(|i| run(&format!("r{i}"), &format!("e{i}"), "t1", T0 + i))
                .collect(),
            vec![task("t1", "t1", 5)],
        ),
    );
    assert!(!exact.truncated, "exactly at the cap is not truncation");
}

#[test]
fn the_cap_applies_after_filters_so_a_narrow_filter_is_not_truncated_by_other_runs() {
    let mut codex_runs: Vec<_> = (0..4)
        .map(|i| with_driver(run(&format!("c{i}"), &format!("ce{i}"), "t1", T0 + i), Some("codex")))
        .collect();
    codex_runs.push(run("claude_run", "e_claude", "t1", T0 + 9));
    let mut query = window();
    query.run_cap = 2;
    query.filters = vec![UsageFilter {
        dimension: UsageDimension::Driver,
        values: vec!["claude".into()],
    }];
    let report = build(&query, &records(codex_runs, vec![task("t1", "t1", 5)]));
    assert!(!report.truncated);
    assert_eq!(report.overall.runs_total, 1);
}

#[test]
fn report_notes_name_the_capture_boundary_the_retention_bound_and_pricing_gaps() {
    let now = CAPTURE_START + 30 * DAY;
    let old = build_usage_rows_report(
        &UsageRowsQuery::builder()
            .since_epoch_s(CAPTURE_START - DAY)
            .until_epoch_s(now)
            .generated_at_epoch_s(now)
            .capture_start_epoch_s(CAPTURE_START)
            .retention_max_age_secs(RETENTION)
            .build(),
        &records(
            vec![with_model(
                tokens(run("r1", "e1", "t1", CAPTURE_START + 1), 1, 1, 0, 0),
                "<synthetic>",
            )],
            vec![task("t1", "t1", 1)],
        ),
        Some(&priced_only_claude),
    );
    assert!(old.spans_capture_boundary);
    let codes: Vec<&str> = old.coverage.notes.iter().map(|n| n.code.as_str()).collect();
    assert_eq!(codes, ["capture_started", "retention_bounded", "pricing_gaps"]);
    assert_eq!(old.pricing_gaps, ["<synthetic>"]);

    let recent = build(&query(now - DAY, now), &records(vec![], vec![]));
    assert!(!recent.spans_capture_boundary);
    assert!(recent.coverage.notes.is_empty());
}

#[test]
fn a_query_asking_for_three_levels_or_a_repeated_dimension_is_rejected() {
    let mut query = window();
    query.group_by = vec![UsageDimension::Driver, UsageDimension::Model, UsageDimension::Host];
    assert!(query.validate().unwrap_err().contains("at most 2"));
    query.group_by = vec![UsageDimension::Driver, UsageDimension::Driver];
    assert!(query.validate().unwrap_err().contains("twice"));
    query.group_by = vec![UsageDimension::Driver, UsageDimension::Model];
    assert!(query.validate().is_ok());
}

// ── Filters and dimension values ──────────────────────────────────────────

fn two_driver_records() -> UsageWindowRecords {
    records(
        vec![
            with_model(tokens(run("r1", "e1", "t1", T0), 1, 1, 0, 0), "claude-opus"),
            with_model(tokens(run("r2", "e2", "t1", T0 + 1), 1, 1, 0, 0), "claude-sonnet"),
            with_driver(
                with_model(tokens(run("r3", "e3", "t1", T0 + 2), 1, 1, 0, 0), "gpt"),
                Some("codex"),
            ),
        ],
        vec![task("t1", "t1", 3)],
    )
}

fn filter(dimension: UsageDimension, values: &[&str]) -> UsageFilter {
    UsageFilter {
        dimension,
        values: values.iter().map(|v| (*v).to_owned()).collect(),
    }
}

#[test]
fn filters_and_across_dimensions_and_or_within_one() {
    let mut query = window();
    query.filters = vec![
        filter(UsageDimension::Driver, &["claude"]),
        filter(UsageDimension::Model, &["claude-opus", "claude-sonnet"]),
    ];
    assert_eq!(build(&query, &two_driver_records()).overall.runs_total, 2);

    query.filters = vec![
        filter(UsageDimension::Driver, &["claude"]),
        filter(UsageDimension::Model, &["gpt"]),
    ];
    assert_eq!(build(&query, &two_driver_records()).overall.runs_total, 0);

    query.filters = vec![filter(UsageDimension::Driver, &[])];
    assert_eq!(
        build(&query, &two_driver_records()).overall.runs_total,
        3,
        "a filter with no values restricts nothing"
    );
}

#[test]
fn dimension_values_count_what_each_value_would_add_ignoring_its_own_filter() {
    let mut query = window();
    query.filters = vec![filter(UsageDimension::Driver, &["claude"])];
    let report = build(&query, &two_driver_records());
    let values = |dimension| {
        report
            .dimension_values
            .iter()
            .find(|d| d.dimension == dimension)
            .unwrap()
            .values
            .iter()
            .map(|v| (v.value.as_str(), v.runs))
            .collect::<Vec<_>>()
    };
    // The driver chips still show codex so it can be added to the selection.
    assert_eq!(values(UsageDimension::Driver), [("claude", 2), ("codex", 1)]);
    // Other dimensions count only runs that pass the driver filter.
    assert_eq!(
        values(UsageDimension::Model),
        [("claude-opus", 1), ("claude-sonnet", 1)]
    );
    assert_eq!(report.dimension_values.len(), UsageDimension::ALL.len());
}

#[test]
fn dimension_values_carry_labels_and_name_an_absent_driver_not_recorded() {
    let report = build(
        &window(),
        &records(
            vec![with_driver(tokens(run("r1", "e1", "t1", T0), 1, 1, 0, 0), None)],
            vec![task("t1", "t1", 1)],
        ),
    );
    let find = |dimension| {
        report
            .dimension_values
            .iter()
            .find(|d| d.dimension == dimension)
            .unwrap()
            .values[0]
            .clone()
    };
    let driver = find(UsageDimension::Driver);
    assert_eq!(
        (driver.value.as_str(), driver.label.as_str()),
        ("(none)", "not recorded")
    );
    let product = find(UsageDimension::Product);
    assert_eq!(
        (product.value.as_str(), product.label.as_str()),
        ("prod_a", "Product A")
    );
}

// ── Classification ────────────────────────────────────────────────────────

#[test]
fn input_convention_is_net_for_claude_grok_and_antigravity_and_gross_for_codex() {
    assert_eq!(input_convention(Some("claude")), InputConvention::Net);
    assert_eq!(
        input_convention(Some("grok")),
        InputConvention::Net,
        "Grok input is fresh input (inputTokens minus cached reads)"
    );
    assert_eq!(input_convention(Some("agy")), InputConvention::Net);
    assert_eq!(input_convention(Some("codex")), InputConvention::Gross);
    assert_eq!(input_convention(Some("someday")), InputConvention::Unknown);
    assert_eq!(input_convention(None), InputConvention::Unknown);
}

#[test]
fn spend_class_follows_the_execution_kind_and_an_unknown_kind_is_unclassified() {
    for kind in [
        "task_implementation",
        "chore_implementation",
        "revision_implementation",
        "investigation_implementation",
        "ci_remediation",
        "conflict_resolution",
    ] {
        assert_eq!(spend_class(kind), SpendClass::Implementation, "{kind}");
    }
    for kind in ["project_design", "product_design"] {
        assert_eq!(spend_class(kind), SpendClass::Design, "{kind}");
    }
    for kind in [
        "pr_review",
        "pr_review_guide",
        "automation_triage",
        "answer_agent",
        "progression",
    ] {
        assert_eq!(spend_class(kind), SpendClass::Overhead, "{kind}");
    }
    assert_eq!(spend_class("a_kind_from_the_future"), SpendClass::Unclassified);
}

#[test]
fn a_tasks_spend_class_split_covers_its_direct_runs() {
    let mut design = tokens(run("r2", "e2", "t1", T0 + 1), 7, 0, 0, 0);
    design.execution_kind = "project_design".into();
    let report = build(
        &window(),
        &records(
            vec![tokens(run("r1", "e1", "t1", T0), 3, 0, 0, 0), design],
            vec![task("t1", "t1", 2)],
        ),
    );
    let split: Vec<(&str, i64)> = report.tasks[0]
        .by_spend_class
        .iter()
        .map(|b| (b.label.as_str(), b.measurement.total_tokens))
        .collect();
    assert_eq!(split, [("design", 7), ("implementation", 3)]);
}

#[test]
fn the_driver_is_the_launch_tuple_or_unknown_never_assumed() {
    let report = build(
        &window(),
        &records(
            vec![
                tokens(run("r1", "e1", "t1", T0), 1, 1, 0, 0),
                with_driver(tokens(run("r2", "e2", "t1", T0 + 1), 1, 1, 0, 0), None),
            ],
            vec![task("t1", "t1", 2)],
        ),
    );
    let executions = &report.tasks[0].executions;
    assert_eq!(
        (executions[0].driver.as_deref(), executions[0].driver_source),
        (Some("claude"), DriverSource::LaunchTuple)
    );
    assert_eq!(
        (executions[1].driver.as_deref(), executions[1].driver_source),
        (None, DriverSource::Unknown)
    );
}

// ── Caveats ───────────────────────────────────────────────────────────────

fn caveat_codes_of(record: UsageRunRecord) -> Vec<String> {
    let report = build(&window(), &records(vec![record], vec![task("t1", "t1", 1)]));
    run_codes(&report).remove(0)
}

fn codes(codes: &[&str]) -> Vec<String> {
    codes.iter().map(|code| (*code).to_owned()).collect()
}

#[test]
fn caveat_codes_follow_each_case() {
    // A healthy Claude run still carries the standing subagent caveat.
    assert_eq!(
        caveat_codes_of(with_model(tokens(run("r", "e", "t1", T0), 1, 1, 1, 1), "claude-opus")),
        codes(&[caveat::SUBAGENTS_EXCLUDED])
    );
    // Created before capture existed, with no tokens and the stored reason.
    let mut pre = run("r", "e", "t1", CAPTURE_START - 10);
    pre.usage_unavailable = Some("No provider token usage observed".into());
    let mut pre_query = window();
    pre_query.since_epoch_s = CAPTURE_START - 100;
    let pre_report = build(&pre_query, &records(vec![pre], vec![task("t1", "t1", 1)]));
    assert_eq!(
        run_codes(&pre_report)[0],
        codes(&[caveat::PRE_CAPTURE, caveat::USAGE_UNAVAILABLE])
    );
    assert_eq!(
        only_run(&pre_report).caveats[1].detail.as_deref(),
        Some("No provider token usage observed")
    );
    // No driver recorded.
    assert_eq!(
        caveat_codes_of(with_driver(run("r", "e", "t1", T0), None)),
        codes(&[caveat::DRIVER_NOT_RECORDED])
    );
    // Codex input is gross.
    assert_eq!(
        caveat_codes_of(with_driver(tokens(run("r", "e", "t1", T0), 9, 1, 0, 4), Some("codex"))),
        codes(&[caveat::GROSS_INPUT])
    );
    // The literal synthetic marker, with the launched alias as detail.
    let mut synthetic = with_model(tokens(run("r", "e", "t1", T0), 1, 1, 1, 1), "<synthetic>");
    synthetic.execution_model = Some("opus".into());
    let report = build(&window(), &records(vec![synthetic], vec![task("t1", "t1", 1)]));
    assert_eq!(
        run_codes(&report)[0],
        codes(&[caveat::SUBAGENTS_EXCLUDED, caveat::SYNTHETIC_MODEL])
    );
    assert_eq!(only_run(&report).caveats[1].detail.as_deref(), Some("opus"));
    assert_eq!(
        only_run(&report).model_observed.as_deref(),
        Some("<synthetic>"),
        "never replaced"
    );
    // No observed model: the launched alias stands in, marked.
    let mut aliased = tokens(run("r", "e", "t1", T0), 1, 1, 1, 1);
    aliased.execution_model = Some("opus".into());
    assert_eq!(
        caveat_codes_of(aliased),
        codes(&[caveat::SUBAGENTS_EXCLUDED, caveat::MODEL_FROM_LAUNCH_TUPLE])
    );
}

#[test]
fn grok_and_antigravity_runs_carry_no_driver_no_capture_caveat() {
    // Both drivers now record tokens: a measured Grok run is partial, not unmeasured.
    for driver in ["grok", "agy"] {
        let mut record = with_driver(run("r", "e", "t1", T0), Some(driver));
        record.input_tokens = Some(10);
        record.output_tokens = Some(2);
        record.cache_read_tokens = Some(5);
        let got = caveat_codes_of(record);
        assert_eq!(got, codes(&[caveat::TOKENS_PARTIAL]), "{driver}");
        assert!(!got.iter().any(|c| c.contains("no_capture")));
    }
}

#[test]
fn a_partial_run_names_the_classes_it_does_not_report() {
    let mut record = with_driver(run("r", "e", "t1", T0), Some("agy"));
    record.input_tokens = Some(10);
    record.output_tokens = Some(2);
    record.cache_read_tokens = Some(5);
    let report = build(&window(), &records(vec![record], vec![task("t1", "t1", 1)]));
    assert_eq!(
        only_run(&report).caveats[0].detail.as_deref(),
        Some("not reported: cache_creation")
    );
}

#[test]
fn a_reported_usage_unavailable_reason_becomes_a_caveat_and_stays_on_the_run() {
    let mut record = run("r", "e", "t1", T0);
    record.usage_unavailable =
        Some("One or more provider responses omit valid input, output, or cache-read usage".into());
    let report = build(&window(), &records(vec![record], vec![task("t1", "t1", 1)]));
    let row = only_run(&report);
    assert_eq!(row.caveats.len(), 1);
    assert_eq!(row.caveats[0].code, caveat::USAGE_UNAVAILABLE);
    assert!(
        row.usage_unavailable
            .as_deref()
            .unwrap()
            .starts_with("One or more provider responses")
    );
    assert_eq!(report.overall.usage_unavailable.len(), 1);
    let counted: Vec<(&str, u32)> = report
        .coverage
        .by_caveat
        .iter()
        .map(|c| (c.code.as_str(), c.runs))
        .collect();
    assert_eq!(
        counted,
        [(caveat::USAGE_UNAVAILABLE, 1)],
        "subagents_excluded is not attached to an unmeasured run"
    );
}

#[test]
fn coverage_counts_each_caveat_once_per_run() {
    let report = build(&window(), &mixed_records());
    let count = |code: &str| {
        report
            .coverage
            .by_caveat
            .iter()
            .find(|c| c.code == code)
            .map(|c| c.runs)
            .unwrap_or(0)
    };
    assert_eq!(count(caveat::GROSS_INPUT), 1);
    assert_eq!(count(caveat::SUBAGENTS_EXCLUDED), 3);
    assert_eq!(count(caveat::DRIVER_NOT_RECORDED), 0);
    let total = report.coverage;
    assert_eq!(
        total.runs_complete + total.runs_partial + total.runs_zero + total.runs_unmeasured,
        total.runs_total,
        "the four states are exclusive and exhaustive"
    );
}

// ── USD ───────────────────────────────────────────────────────────────────

#[test]
fn usd_is_excluded_by_driver_for_subscription_drivers_even_when_the_model_name_is_priced() {
    // The pricer would price anything: the exclusion must not rely on the model name.
    let mut agy = with_model(tokens(run("a", "ea", "t1", T0), 1, 1, 1, 1), "gemini-4-neon");
    agy.driver = Some("agy".into());
    let mut grok = with_model(tokens(run("g", "eg", "t1", T0 + 1), 1, 1, 1, 1), "grok-4.5-build");
    grok.driver = Some("grok".into());
    let report = build_usage_rows_report(
        &window(),
        &records(vec![agy, grok], vec![task("t1", "t1", 2)]),
        Some(&flat_pricer),
    );
    for execution in &report.tasks[0].executions {
        let row = &execution.runs[0];
        assert_eq!(row.estimated_usd, None);
        assert_eq!(row.unpriceable_cause.as_deref(), Some(cause::SUBSCRIPTION_UNPRICED));
    }
    assert_eq!(
        report.overall.estimated_usd, None,
        "nothing priced is None, never $0.00"
    );
    assert!(report.overall.estimated_usd_partial);
    assert_eq!(report.coverage.runs_priceable, Some(0));
}

#[test]
fn usd_causes_follow_the_first_rule_that_applies() {
    let mut codex = with_model(tokens(run("codex", "e1", "t1", T0), 9, 1, 0, 4), "gpt");
    codex.driver = Some("codex".into());
    let no_driver = with_driver(
        with_model(tokens(run("nodriver", "e2", "t1", T0 + 1), 1, 1, 1, 1), "claude-x"),
        None,
    );
    let mut incomplete = with_model(run("incomplete", "e3", "t1", T0 + 2), "claude-x");
    incomplete.input_tokens = Some(1);
    let unknown = with_model(tokens(run("unknown", "e4", "t1", T0 + 3), 1, 1, 1, 1), "mystery");
    let unmodelled = tokens(run("unmodelled", "e5", "t1", T0 + 4), 1, 1, 1, 1);
    let unmeasured = run("unmeasured", "e6", "t1", T0 + 5);
    let priced = with_model(
        tokens(run("priced", "e7", "t1", T0 + 6), 1_000, 100, 0, 0),
        "claude-opus",
    );
    let report = build_usage_rows_report(
        &window(),
        &records(
            vec![codex, no_driver, incomplete, unknown, unmodelled, unmeasured, priced],
            vec![task("t1", "t1", 7)],
        ),
        Some(&priced_only_claude),
    );
    let cause_of = |id: &str| {
        report.tasks[0]
            .executions
            .iter()
            .flat_map(|e| &e.runs)
            .find(|r| r.run_id == id)
            .map(|r| (r.estimated_usd, r.unpriceable_cause.clone()))
            .unwrap()
    };
    assert_eq!(cause_of("codex").1.as_deref(), Some(cause::CAPTURE_UNNORMALISED));
    assert_eq!(cause_of("nodriver").1.as_deref(), Some(cause::DRIVER_UNKNOWN));
    assert_eq!(cause_of("incomplete").1.as_deref(), Some(cause::TOKENS_INCOMPLETE));
    assert_eq!(cause_of("unknown").1.as_deref(), Some(cause::UNKNOWN_MODEL));
    assert_eq!(cause_of("unmodelled").1.as_deref(), Some(cause::UNKNOWN_MODEL));
    assert_eq!(cause_of("unmeasured").1.as_deref(), Some(cause::UNMEASURED));
    assert_eq!(cause_of("priced"), (Some(1.5), None));

    assert_eq!(
        report.overall.estimated_usd,
        Some(1.5),
        "only the priced run contributes"
    );
    assert!(report.overall.estimated_usd_partial);
    assert_eq!(report.coverage.runs_priceable, Some(1));
    // The unmeasured run is not a pricing gap: it has no tokens to price.
    assert_eq!(
        report.pricing_gaps,
        ["(no model recorded)", "claude-x", "gpt", "mystery"],
        "measured, unpriceable runs name their model; an unmeasured run does not"
    );
}

#[test]
fn every_usd_figure_carries_the_estimate_caveat_and_no_usd_is_made_unasked() {
    let record = with_model(tokens(run("r", "e", "t1", T0), 1, 1, 1, 1), "claude-opus");
    let recs = records(vec![record], vec![task("t1", "t1", 1)]);
    let priced = build_usage_rows_report(&window(), &recs, Some(&priced_only_claude));
    assert!(
        only_run(&priced)
            .caveats
            .iter()
            .any(|c| c.code == caveat::ESTIMATE_ONLY)
    );
    let unasked = build(&window(), &recs);
    assert_eq!(only_run(&unasked).estimated_usd, None);
    assert_eq!(only_run(&unasked).unpriceable_cause, None);
    assert_eq!(unasked.overall.estimated_usd, None);
    assert!(!unasked.overall.estimated_usd_partial);
    assert_eq!(unasked.coverage.runs_priceable, None);
}

// ── Wire shape ────────────────────────────────────────────────────────────

#[test]
fn the_report_round_trips_through_json_with_unmeasured_tokens_as_null() {
    let report = build(&window(), &mixed_records());
    let json = serde_json::to_value(&report).unwrap();
    let decoded: TokenUsageRowsReport = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(decoded, report);
    let unmeasured_run = json["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["revisions"].as_array().unwrap().iter())
        .flat_map(|t| t["executions"].as_array().unwrap().iter())
        .flat_map(|e| e["runs"].as_array().unwrap().iter())
        .find(|r| r["coverage"] == "unmeasured")
        .expect("the fixture has an unmeasured revision run");
    assert!(unmeasured_run["tokens"].is_null());
}
