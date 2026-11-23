//! Unit tests pinning every invariant of [`crate::series`].

use boss_protocol::{TokenUsageSeriesReport, UsageBucketWidth, UsageDimension, UsageFilter, UsageRatios};

use super::*;

const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;
const COMPLETED_AT_START: i64 = 1_782_805_232;
const CAPTURE_START: i64 = 1_785_144_180;
/// Aligned to UTC midnight (`1_790_035_200 % 86_400 == 0`, 2026-09-22T00:00:00Z).
const DAY0: i64 = 1_790_035_200;
const RETENTION: i64 = 14 * DAY;

fn series_query(since: i64, until: i64) -> UsageSeriesQuery {
    UsageSeriesQuery::builder()
        .since_epoch_s(since)
        .until_epoch_s(until)
        .generated_at_epoch_s(until)
        .capture_start_epoch_s(CAPTURE_START)
        .completed_at_start_epoch_s(COMPLETED_AT_START)
        .retention_max_age_secs(RETENTION)
        .build()
}

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
        .driver(RecordedDriver::LaunchTuple("claude".into()))
        .build()
}

fn with_tokens(
    mut rec: UsageRunRecord,
    input: Option<i64>,
    output: Option<i64>,
    cache_creation: Option<i64>,
    cache_read: Option<i64>,
    active_ms: Option<i64>,
) -> UsageRunRecord {
    rec.input_tokens = input;
    rec.output_tokens = output;
    rec.cache_creation_tokens = cache_creation;
    rec.cache_read_tokens = cache_read;
    rec.agent_active_ms = active_ms;
    rec
}

fn exec(id: &str, item: &str, status: &str, finished_at: i64) -> UsageExecutionRecord {
    UsageExecutionRecord::builder()
        .execution_id(id)
        .work_item_id(item)
        .execution_kind("task_implementation")
        .execution_status(status)
        .execution_created_at_epoch_s(finished_at - 60)
        .execution_finished_at_epoch_s(finished_at)
        .driver("claude")
        .build()
}

fn task(id: &str, root: &str, completed_at: Option<i64>) -> UsageTaskRecord {
    UsageTaskRecord::builder()
        .task_id(id)
        .name(format!("task {id}"))
        .kind(if id == root { "task" } else { "revision" })
        .status(if completed_at.is_some() { "done" } else { "in_progress" })
        .product_id("prod_a")
        .product_name("Product A")
        .chain_root_id(root)
        .run_count_all_time(1)
        .maybe_completed_at_epoch_s(completed_at)
        .build()
}

fn approx(actual: Option<f64>, expected: f64) {
    let actual = actual.unwrap_or_else(|| panic!("expected Some({expected}), got None"));
    assert!((actual - expected).abs() < 1e-6, "expected {expected}, got {actual}");
}

// ── Width selection, alignment, absent buckets ───────────────────────────

#[test]
fn engine_chooses_bucket_width_from_window_span() {
    assert_eq!(choose_bucket_width(0, HOUR), UsageBucketWidth::Hour);
    assert_eq!(choose_bucket_width(0, 3 * DAY), UsageBucketWidth::Hour);
    assert_eq!(choose_bucket_width(0, 3 * DAY + 1), UsageBucketWidth::Day);
    assert_eq!(choose_bucket_width(0, 200 * DAY), UsageBucketWidth::Day);
    assert_eq!(choose_bucket_width(0, 200 * DAY + 1), UsageBucketWidth::Week);
    assert_eq!(choose_bucket_width(0, 4 * 365 * DAY), UsageBucketWidth::Week);
    assert_eq!(choose_bucket_width(0, 4 * 365 * DAY + 1), UsageBucketWidth::Month);

    // Week buckets align to Monday 00:00:00 UTC (epoch 4 * 86_400 = 1970-01-05).
    let week_start = bucket_start_epoch_s(DAY0 + 123, UsageBucketWidth::Week);
    assert_eq!((week_start - 4 * DAY).rem_euclid(7 * DAY), 0);

    // Month buckets align to UTC calendar-month starts, not 30-day epoch blocks.
    // DAY0 is 2026-09-22T00:00:00Z; its month starts 21 days earlier.
    assert_eq!(bucket_start_epoch_s(DAY0, UsageBucketWidth::Month), DAY0 - 21 * DAY);
    assert_eq!(
        bucket_start_epoch_s(DAY0 + 9 * DAY, UsageBucketWidth::Month),
        DAY0 + 9 * DAY,
        "2026-10-01 is itself a month start"
    );
    assert_eq!(bucket_start_epoch_s(0, UsageBucketWidth::Month), 0);
    // 2024-02-29 12:00 UTC (leap day) belongs to February, not a 30-day block.
    let leap_noon: i64 = 1_709_208_000; // 2024-02-29T12:00:00Z
    let feb_2024: i64 = 1_706_745_600; // 2024-02-01T00:00:00Z
    assert_eq!(bucket_start_epoch_s(leap_noon, UsageBucketWidth::Month), feb_2024);
}

#[test]
fn empty_buckets_are_absent_never_fabricated_as_zero() {
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        ..series_query(DAY0, DAY0 + 10 * DAY)
    };
    let empty = build_usage_series_report(&q, &UsageSeriesRecords::default(), None);
    assert!(empty.buckets.is_empty());
    assert_eq!(empty.groups, vec![ALL_GROUP]);
    assert_eq!(empty.coverage.data_from_epoch_s, None);

    // Facts on day 1 and day 7 only -> days 0, 2..6, 8, 9 are absent.
    let r1 = with_tokens(
        run("r1", "e1", "t1", DAY0 + DAY + 60),
        Some(100),
        Some(50),
        Some(0),
        Some(0),
        Some(60_000),
    );
    let r2 = with_tokens(
        run("r2", "e2", "t2", DAY0 + 7 * DAY + 60),
        Some(200),
        Some(100),
        Some(0),
        Some(0),
        Some(60_000),
    );
    let recs = UsageSeriesRecords {
        runs: vec![r1, r2],
        finished_executions: Vec::new(),
        tasks: vec![task("t1", "t1", None), task("t2", "t2", None)],
    };
    let report = build_usage_series_report(&q, &recs, None);
    let starts: Vec<i64> = report.buckets.iter().map(|b| b.start_epoch_s).collect();
    assert_eq!(starts, vec![DAY0 + DAY, DAY0 + 7 * DAY]);
}

// ── Per-class NULL handling & zero denominators ──────────────────────────

#[test]
fn bucket_sums_preserve_per_class_nulls_and_mark_partial() {
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        ..series_query(DAY0, DAY0 + 2 * DAY)
    };
    let claude = with_tokens(
        run("r_claude", "e1", "t1", DAY0 + 100),
        Some(1_000),
        Some(500),
        Some(200),
        Some(300),
        Some(1_800_000),
    );
    let mut grok = with_tokens(
        run("r_grok", "e2", "t2", DAY0 + 200),
        Some(400),
        Some(100),
        None,
        Some(500),
        None,
    );
    grok.driver = RecordedDriver::LaunchTuple("grok".into());

    let recs = UsageSeriesRecords {
        runs: vec![claude, grok],
        finished_executions: Vec::new(),
        tasks: vec![task("t1", "t1", None), task("t2", "t2", None)],
    };
    let report = build_usage_series_report(&q, &recs, None);
    assert_eq!(report.buckets.len(), 1);
    let cell = &report.buckets[0].cells[0];
    let m = &cell.measurement;
    assert_eq!(m.runs_total, 2);
    assert_eq!(m.runs_measured, 2);
    assert_eq!(m.input_tokens, Some(1_400));
    assert_eq!(m.output_tokens, Some(600));
    assert_eq!(
        m.cache_creation_tokens, None,
        "a bucket with a measured run lacking cache_creation must keep it unknown, never zero"
    );
    assert_eq!(m.cache_read_tokens, Some(800));
    assert!(m.tokens_partial);
    assert_eq!(m.total_tokens, 3_000);
}

#[test]
fn ratios_are_none_on_zero_denominators_and_unmeasured_numerators() {
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        ..series_query(DAY0, DAY0 + 4 * DAY)
    };

    // Day 0: unmeasured run only + completed execution + PR -> all ratios None.
    let r_unmeasured = run("r0", "e0", "t0", DAY0 + 10);
    let mut e0 = exec("e0", "t0", "completed", DAY0 + 20);
    e0.execution_pr_url = Some("https://github.com/org/repo/pull/1".to_owned());

    // Day 1: measured run (1000 tokens, active_ms = 0 -> zero active-hour denominator),
    // no completed executions, no completed tasks, no PRs.
    let r_measured_no_denoms = with_tokens(
        run("r1", "e1", "t1", DAY0 + DAY + 10),
        Some(600),
        Some(400),
        Some(0),
        Some(0),
        Some(0),
    );

    // Day 2: measured-zero run (all 0s), 1 completed execution, 1 PR ->
    // tokens_per_run / per_completed_execution / per_pr are Some(0.0),
    // tokens_per_active_hour and share_of_bucket_total_tokens are None.
    let r_zero = with_tokens(
        run("r2", "e2", "t2", DAY0 + 2 * DAY + 10),
        Some(0),
        Some(0),
        Some(0),
        Some(0),
        Some(0),
    );
    let mut e2 = exec("e2", "t2", "completed", DAY0 + 2 * DAY + 20);
    e2.execution_pr_url = Some("https://github.com/org/repo/pull/2".to_owned());

    let recs = UsageSeriesRecords {
        runs: vec![r_unmeasured, r_measured_no_denoms, r_zero],
        finished_executions: vec![e0, e2],
        tasks: vec![task("t0", "t0", None), task("t1", "t1", None), task("t2", "t2", None)],
    };
    let report = build_usage_series_report(&q, &recs, None);
    assert_eq!(report.buckets.len(), 3);

    // Day 0: unmeasured numerator => every ratio None.
    let c0 = &report.buckets[0].cells[0];
    assert_eq!(c0.measurement.runs_measured, 0);
    assert_eq!(c0.measurement.runs_unmeasured, 1);
    assert_eq!(c0.executions_completed, 1);
    assert_eq!(c0.distinct_prs, 1);
    assert_eq!(c0.share_of_bucket_total_tokens, None);
    assert_eq!(c0.ratios, UsageRatios::default());

    // Day 1: only tokens_per_run is defined; all zero denominators yield None.
    let c1 = &report.buckets[1].cells[0];
    approx(c1.ratios.tokens_per_run, 1_000.0);
    assert_eq!(c1.ratios.tokens_per_completed_execution, None);
    assert_eq!(c1.ratios.tokens_per_active_hour, None);
    assert_eq!(c1.ratios.tokens_per_completed_task, None);
    assert_eq!(c1.ratios.tokens_per_pr, None);
    approx(c1.share_of_bucket_total_tokens, 1.0);

    // Day 2: recorded zero yields Some(0.0) where denominator > 0.
    let c2 = &report.buckets[2].cells[0];
    approx(c2.ratios.tokens_per_run, 0.0);
    approx(c2.ratios.tokens_per_completed_execution, 0.0);
    approx(c2.ratios.tokens_per_pr, 0.0);
    assert_eq!(c2.ratios.tokens_per_active_hour, None);
    assert_eq!(c2.ratios.tokens_per_completed_task, None);
    assert_eq!(c2.share_of_bucket_total_tokens, None);
}

// ── Active-hour restriction for missing agent_active_ms (Grok / agy) ─────

#[test]
fn tokens_per_active_hour_excludes_missing_active_time_from_both_numerator_and_denominator() {
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        ..series_query(DAY0, DAY0 + 3 * DAY)
    };

    // Day 0: Claude (10,000 tokens in 0.5h = 20,000 tok/hr) + Grok (50,000 tokens, active_ms = None).
    // If Grok's tokens leaked into the numerator we'd get 120,000 tok/hr — wrong!
    let claude = with_tokens(
        run("r_claude", "e1", "t1", DAY0 + 10),
        Some(4_000),
        Some(1_000),
        Some(2_000),
        Some(3_000),
        Some(1_800_000), // 0.5 hour
    );
    let mut grok = with_tokens(
        run("r_grok", "e2", "t2", DAY0 + 20),
        Some(20_000),
        Some(5_000),
        None,
        Some(25_000),
        None,
    );
    grok.driver = RecordedDriver::LaunchTuple("grok".into());

    // Day 1: ONLY a Grok run (active_ms = None) -> tokens_per_active_hour must be None.
    let mut grok_only = with_tokens(
        run("r_grok2", "e3", "t3", DAY0 + DAY + 10),
        Some(8_000),
        Some(2_000),
        None,
        Some(10_000),
        None,
    );
    grok_only.driver = RecordedDriver::LaunchTuple("grok".into());

    let recs = UsageSeriesRecords {
        runs: vec![claude, grok, grok_only],
        finished_executions: Vec::new(),
        tasks: vec![task("t1", "t1", None), task("t2", "t2", None), task("t3", "t3", None)],
    };
    let report = build_usage_series_report(&q, &recs, None);
    assert_eq!(report.buckets.len(), 2);

    // Day 0: 10,000 / 0.5h = 20,000.0 (Grok excluded from both numerator & denominator).
    let d0 = &report.buckets[0].cells[0];
    assert_eq!(d0.measurement.total_tokens, 60_000);
    approx(d0.ratios.tokens_per_run, 30_000.0);
    approx(d0.ratios.tokens_per_active_hour, 20_000.0);

    // Day 1: only Grok => tokens_per_active_hour is None.
    let d1 = &report.buckets[1].cells[0];
    assert_eq!(d1.ratios.tokens_per_active_hour, None);

    // Coverage records 2 excluded measured runs and emits active_time_missing note.
    assert_eq!(report.coverage.runs_excluded_from_active_hour, 2);
    let active_note = report
        .coverage
        .notes
        .iter()
        .find(|n| n.code == note::ACTIVE_TIME_MISSING)
        .expect("missing active time must emit a coverage note");
    assert!(
        active_note.detail.contains("2 of 3"),
        "expected '2 of 3' in detail, got: {}",
        active_note.detail
    );
}

// ── Attributed inclusive task cost (including out-of-window runs) ────────

#[test]
fn attributed_task_cost_includes_out_of_window_root_and_revision_runs_without_double_counting_revisions() {
    let since = DAY0;
    let until = DAY0 + 5 * DAY;
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        ..series_query(since, until)
    };

    // Root task completed on day 3. Its own run happened 40 days BEFORE the window!
    // Its revision also completed on day 3, with a run on day 1 (inside the window).
    let root = task("root", "root", Some(DAY0 + 3 * DAY + 100));
    let rev = task("rev", "root", Some(DAY0 + 3 * DAY + 100));

    let r_old_root = with_tokens(
        run("r_old_root", "e_root", "root", DAY0 - 40 * DAY),
        Some(50_000),
        Some(10_000),
        Some(0),
        Some(0),
        Some(600_000),
    );
    let r_in_window_rev = with_tokens(
        run("r_rev", "e_rev", "rev", DAY0 + DAY + 50),
        Some(15_000),
        Some(5_000),
        Some(0),
        Some(0),
        Some(300_000),
    );

    let recs = UsageSeriesRecords {
        runs: vec![r_old_root, r_in_window_rev],
        finished_executions: Vec::new(),
        tasks: vec![root, rev],
    };
    let report = build_usage_series_report(&q, &recs, None);

    // Day 1 has the revision's in-window run; Day 3 has the completed task (and NO runs of its own).
    let starts: Vec<i64> = report.buckets.iter().map(|b| b.start_epoch_s).collect();
    assert_eq!(starts, vec![DAY0 + DAY, DAY0 + 3 * DAY]);

    let day1 = &report.buckets[0].cells[0];
    assert_eq!(day1.measurement.total_tokens, 20_000);
    assert_eq!(day1.tasks_completed, 0);
    assert_eq!(day1.ratios.tokens_per_completed_task, None);

    let day3 = &report.buckets[1].cells[0];
    assert_eq!(day3.measurement.runs_total, 0);
    assert_eq!(
        day3.tasks_completed, 1,
        "only the chain root counts as a completed task; revisions do not double-count"
    );
    assert_eq!(day3.tasks_completed_cost.runs_total, 2);
    assert_eq!(
        day3.tasks_completed_cost.total_tokens, 80_000,
        "inclusive task cost must include both the out-of-window root run (60k) and the revision run (20k)"
    );
    approx(day3.ratios.tokens_per_completed_task, 80_000.0);
    assert_eq!(day3.ratios.tokens_per_run, None);
    assert_eq!(day3.ratios.tokens_per_completed_execution, None);
    assert_eq!(day3.ratios.tokens_per_active_hour, None);
    assert_eq!(day3.ratios.tokens_per_pr, None);
}

// ── Executions completed, distinct PRs, grouping, share & filters ────────

#[test]
fn distinct_prs_deduplicates_urls_and_grouping_computes_bucket_shares() {
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        group_by: Some(UsageDimension::Driver),
        ..series_query(DAY0, DAY0 + 2 * DAY)
    };

    let r_claude = with_tokens(
        run("r1", "e1", "t1", DAY0 + 10),
        Some(3_000),
        Some(0),
        Some(0),
        Some(0),
        Some(3_600_000),
    );
    let mut r_codex = with_tokens(
        run("r2", "e2", "t2", DAY0 + 20),
        Some(1_000),
        Some(0),
        Some(0),
        Some(0),
        Some(1_800_000),
    );
    r_codex.driver = RecordedDriver::LaunchTuple("codex".into());
    let mut r_unmeasured_grok = run("r3", "e3", "t3", DAY0 + 30);
    r_unmeasured_grok.driver = RecordedDriver::LaunchTuple("grok".into());

    // Two Claude executions finish on Day 0 with the SAME PR URL -> distinct_prs = 1, executions_completed = 2.
    let mut e1 = exec("e1", "t1", "completed", DAY0 + 40);
    e1.execution_pr_url = Some("https://github.com/org/repo/pull/42".to_owned());
    let mut e1_rev = exec("e1_rev", "t1", "completed", DAY0 + 50);
    e1_rev.execution_pr_url = Some("https://github.com/org/repo/pull/42".to_owned());
    // A failed execution with a distinct PR URL still counts in distinct_prs, not executions_completed.
    let mut e_failed_pr = exec("e_failed", "t1", "failed", DAY0 + 60);
    e_failed_pr.execution_pr_url = Some("https://github.com/org/repo/pull/43".to_owned());

    let recs = UsageSeriesRecords {
        runs: vec![r_claude, r_codex, r_unmeasured_grok],
        finished_executions: vec![e1, e1_rev, e_failed_pr],
        tasks: vec![task("t1", "t1", None), task("t2", "t2", None), task("t3", "t3", None)],
    };
    let report = build_usage_series_report(&q, &recs, None);
    assert_eq!(report.groups, vec!["claude", "codex", "grok"]);
    assert_eq!(report.buckets.len(), 1);
    let cells = &report.buckets[0].cells;
    assert_eq!(cells.len(), 3);

    assert_eq!(cells[0].group, "claude");
    approx(cells[0].share_of_bucket_total_tokens, 0.75);
    assert_eq!(cells[0].executions_completed, 2);
    assert_eq!(cells[0].distinct_prs, 2);
    approx(cells[0].ratios.tokens_per_completed_execution, 1_500.0);
    approx(cells[0].ratios.tokens_per_pr, 1_500.0);

    assert_eq!(cells[1].group, "codex");
    approx(cells[1].share_of_bucket_total_tokens, 0.25);

    assert_eq!(cells[2].group, "grok");
    assert_eq!(
        cells[2].share_of_bucket_total_tokens, None,
        "an unmeasured group must not get share 0.0"
    );

    // Coverage caveat counts expose gross_input for Codex so mix shifts show comparability.
    let gross_count = report
        .coverage
        .by_caveat
        .iter()
        .find(|c| c.code == caveat::GROSS_INPUT)
        .map(|c| c.runs);
    assert_eq!(gross_count, Some(1));

    // Filtering restricts runs, executions and tasks consistently.
    let filtered_q = UsageSeriesQuery {
        filters: vec![UsageFilter {
            dimension: UsageDimension::Driver,
            values: vec!["claude".to_owned()],
        }],
        ..q
    };
    let filtered = build_usage_series_report(&filtered_q, &recs, None);
    assert_eq!(filtered.groups, vec!["claude"]);
    approx(filtered.buckets[0].cells[0].share_of_bucket_total_tokens, 1.0);
}

// ── Coverage boundaries & notes ──────────────────────────────────────────

#[test]
fn coverage_emits_notes_for_every_boundary_and_pricing_gap() {
    // Window starts before BOTH completed_at migration and capture_start, and
    // more than 14 days before generated_at.
    let since = COMPLETED_AT_START - 2 * DAY;
    let until = DAY0 + 2 * DAY;
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        group_by: Some(UsageDimension::Driver),
        ..series_query(since, until)
    };

    // Pre-capture run with NO recorded driver (NULL launch tuple).
    let mut pre = run("r_pre", "e_pre", "t_pre", COMPLETED_AT_START - DAY);
    pre.driver = RecordedDriver::Unknown;

    // Post-capture Grok run (recorded driver, subscription-unpriced -> pricing gap, no active_ms).
    let mut grok = with_tokens(
        run("r_grok", "e_grok", "t_grok", DAY0 + 100),
        Some(500),
        Some(100),
        None,
        Some(200),
        None,
    );
    grok.driver = RecordedDriver::LaunchTuple("grok".into());
    grok.run_model = Some("grok-4.5-build".to_owned());

    let recs = UsageSeriesRecords {
        runs: vec![pre, grok],
        finished_executions: Vec::new(),
        tasks: vec![task("t_pre", "t_pre", None), task("t_grok", "t_grok", None)],
    };
    let pricer = |_model: &str, _tokens: PricedTokens| Some(1.0);
    let report = build_usage_series_report(&q, &recs, Some(&pricer));

    assert!(report.spans_capture_boundary);
    assert_eq!(
        report.coverage.data_from_epoch_s,
        Some(CAPTURE_START),
        "data_from_epoch_s clamps upward to capture_start_epoch_s"
    );
    assert_eq!(
        report.coverage.dimension_from_epoch_s,
        Some(DAY0 + 100),
        "dimension_from_epoch_s is the earliest fact carrying a recorded launch-tuple driver"
    );

    let note_codes: Vec<&str> = report.coverage.notes.iter().map(|n| n.code.as_str()).collect();
    assert!(note_codes.contains(&note::CAPTURE_STARTED), "{note_codes:?}");
    assert!(note_codes.contains(&note::RETENTION_BOUNDED), "{note_codes:?}");
    let retention = report
        .coverage
        .notes
        .iter()
        .find(|n| n.code == note::RETENTION_BOUNDED)
        .unwrap();
    assert!(
        retention.detail.contains("never-started")
            && retention.detail.contains("per-work-item floor")
            && retention.detail.contains("worker run"),
        "{}",
        retention.detail
    );
    assert!(note_codes.contains(&note::ACTIVE_TIME_MISSING), "{note_codes:?}");
    assert!(note_codes.contains(&note::PRICING_GAPS), "{note_codes:?}");

    // Two `dimension_started` notes: one for the `completed_at` migration boundary
    // and one for the `driver` grouping boundary.
    let dim_notes: Vec<_> = report
        .coverage
        .notes
        .iter()
        .filter(|n| n.code == note::DIMENSION_STARTED)
        .collect();
    assert_eq!(
        dim_notes.len(),
        2,
        "expected completed_at + driver notes: {dim_notes:?}"
    );
    assert!(
        dim_notes.iter().any(|n| n.epoch_s == Some(COMPLETED_AT_START)),
        "completed_at migration boundary must be stamped: {dim_notes:?}"
    );
    assert!(
        dim_notes.iter().any(|n| n.epoch_s == Some(DAY0 + 100)),
        "grouped dimension boundary must be stamped: {dim_notes:?}"
    );
    assert_eq!(report.pricing_gaps, vec!["grok-4.5-build"]);
}

#[test]
fn recent_window_with_complete_dimension_emits_no_spurious_boundary_notes() {
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        group_by: Some(UsageDimension::Driver),
        ..series_query(DAY0, DAY0 + 5 * DAY)
    };
    let r = with_tokens(
        run("r1", "e1", "t1", DAY0 + DAY),
        Some(100),
        Some(50),
        Some(10),
        Some(20),
        Some(60_000),
    );
    let recs = UsageSeriesRecords {
        runs: vec![r],
        finished_executions: Vec::new(),
        tasks: vec![task("t1", "t1", None)],
    };
    let report = build_usage_series_report(&q, &recs, None);
    assert!(!report.spans_capture_boundary);
    assert_eq!(report.coverage.data_from_epoch_s, Some(DAY0 + DAY));
    assert_eq!(report.coverage.dimension_from_epoch_s, Some(DAY0 + DAY));
    assert!(
        report.coverage.notes.is_empty(),
        "unexpected boundary notes on a clean recent window: {:?}",
        report.coverage.notes
    );
}

// ── Bucket coarsening at the cell cap ────────────────────────────────────

#[test]
fn exceeding_cell_cap_coarsens_bucket_width_and_preserves_every_token() {
    // 10 hours across 3 drivers = 30 cells at Hour. With cell_cap = 10, Hour
    // (30 cells) coarsens to Day (3 cells <= 10) without dropping any runs.
    let mut runs = Vec::new();
    let drivers = ["claude", "codex", "grok"];
    for hour in 0..10 {
        for (d_idx, driver) in drivers.iter().enumerate() {
            let id = format!("r_{hour}_{d_idx}");
            let mut r = with_tokens(
                run(&id, &id, "t1", DAY0 + hour * HOUR + d_idx as i64),
                Some(100),
                Some(0),
                Some(0),
                Some(0),
                Some(60_000),
            );
            r.driver = RecordedDriver::LaunchTuple((*driver).to_owned());
            runs.push(r);
        }
    }
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Hour),
        group_by: Some(UsageDimension::Driver),
        cell_cap: 10,
        ..series_query(DAY0, DAY0 + DAY)
    };
    let recs = UsageSeriesRecords {
        runs,
        finished_executions: Vec::new(),
        tasks: vec![task("t1", "t1", None)],
    };
    let report = build_usage_series_report(&q, &recs, None);
    assert_eq!(report.bucket_secs, UsageBucketWidth::Day.bucket_secs());
    assert_eq!(report.buckets.len(), 1);
    assert_eq!(report.buckets[0].cells.len(), 3);
    let total_tokens: i64 = report.buckets[0].cells.iter().map(|c| c.measurement.total_tokens).sum();
    assert_eq!(total_tokens, 30 * 100, "coarsening must preserve every run's tokens");
    let coarsen_note = report
        .coverage
        .notes
        .iter()
        .find(|n| n.code == note::BUCKET_COARSENED)
        .expect("coarsening must emit a bucket_coarsened note");
    assert!(
        coarsen_note.detail.contains("hour") && coarsen_note.detail.contains("day"),
        "{}",
        coarsen_note.detail
    );

    // Multi-step coarsening: 40 days across 1 group = 40 cells at Day;
    // cap = 5 coarsens Day (40) -> Week (7) -> Month (2 <= 5).
    let multi_runs: Vec<_> = (0..40)
        .map(|day| {
            let id = format!("d_{day}");
            with_tokens(
                run(&id, &id, "t1", DAY0 + day * DAY + 10),
                Some(50),
                Some(0),
                Some(0),
                Some(0),
                Some(60_000),
            )
        })
        .collect();
    let multi_q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        cell_cap: 5,
        ..series_query(DAY0, DAY0 + 45 * DAY)
    };
    let multi = build_usage_series_report(
        &multi_q,
        &UsageSeriesRecords {
            runs: multi_runs,
            finished_executions: Vec::new(),
            tasks: vec![task("t1", "t1", None)],
        },
        None,
    );
    assert_eq!(multi.bucket_secs, UsageBucketWidth::Month.bucket_secs());
    let multi_tokens: i64 = multi
        .buckets
        .iter()
        .flat_map(|b| b.cells.iter())
        .map(|c| c.measurement.total_tokens)
        .sum();
    assert_eq!(multi_tokens, 40 * 50);
    assert!(
        multi
            .coverage
            .notes
            .iter()
            .any(|n| n.code == note::BUCKET_COARSENED && n.detail.contains("month"))
    );
}

#[test]
fn explicit_month_request_over_cap_keeps_every_fact_and_notes_the_overage() {
    // 6 groups × 2 UTC calendar months (Sep 22 and Oct 1 of 2026) = 12 cells.
    // cell_cap 10 cannot coarsen past Month, so the grid is returned with a note.
    let mut runs = Vec::new();
    for (g, driver) in ["d0", "d1", "d2", "d3", "d4", "d5"].iter().enumerate() {
        for (offset, day) in [0_i64, 9].iter().enumerate() {
            let mut r = with_tokens(
                run(
                    &format!("r{g}_{offset}"),
                    &format!("e{g}_{offset}"),
                    "t1",
                    DAY0 + day * DAY + g as i64,
                ),
                Some(10),
                Some(0),
                Some(0),
                Some(0),
                Some(1_000),
            );
            r.driver = RecordedDriver::LaunchTuple((*driver).to_owned());
            runs.push(r);
        }
    }
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Month),
        group_by: Some(UsageDimension::Driver),
        cell_cap: 10,
        ..series_query(DAY0, DAY0 + 40 * DAY)
    };
    let report = build_usage_series_report(
        &q,
        &UsageSeriesRecords {
            runs,
            finished_executions: Vec::new(),
            tasks: vec![task("t1", "t1", None)],
        },
        None,
    );
    assert_eq!(report.bucket_secs, UsageBucketWidth::Month.bucket_secs());
    let cells: usize = report.buckets.iter().map(|b| b.cells.len()).sum();
    assert_eq!(cells, 12);
    let tokens: i64 = report
        .buckets
        .iter()
        .flat_map(|b| b.cells.iter())
        .map(|c| c.measurement.total_tokens)
        .sum();
    assert_eq!(tokens, 12 * 10);
    assert!(
        report
            .coverage
            .notes
            .iter()
            .any(|n| n.code == note::CELL_CAP_EXCEEDED && n.detail.contains("12") && n.detail.contains("10")),
        "{:?}",
        report.coverage.notes
    );
    assert!(
        report.coverage.notes.iter().all(|n| n.code != note::BUCKET_COARSENED),
        "an explicit month request must not claim coarsening stayed within the cap: {:?}",
        report.coverage.notes
    );
}

#[test]
fn coarsening_into_month_that_still_exceeds_cap_notes_the_overage() {
    // 40 day-buckets × 6 groups coarsens Day → Week → Month (2 calendar months
    // × 6 groups = 12 cells) against cell_cap 10.
    let mut runs = Vec::new();
    for day in 0..40 {
        for (g, driver) in ["d0", "d1", "d2", "d3", "d4", "d5"].iter().enumerate() {
            let mut r = with_tokens(
                run(
                    &format!("d{day}_{g}"),
                    &format!("e{day}_{g}"),
                    "t1",
                    DAY0 + day * DAY + g as i64,
                ),
                Some(5),
                Some(0),
                Some(0),
                Some(0),
                Some(1_000),
            );
            r.driver = RecordedDriver::LaunchTuple((*driver).to_owned());
            runs.push(r);
        }
    }
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        group_by: Some(UsageDimension::Driver),
        cell_cap: 10,
        ..series_query(DAY0, DAY0 + 45 * DAY)
    };
    let report = build_usage_series_report(
        &q,
        &UsageSeriesRecords {
            runs,
            finished_executions: Vec::new(),
            tasks: vec![task("t1", "t1", None)],
        },
        None,
    );
    assert_eq!(report.bucket_secs, UsageBucketWidth::Month.bucket_secs());
    let tokens: i64 = report
        .buckets
        .iter()
        .flat_map(|b| b.cells.iter())
        .map(|c| c.measurement.total_tokens)
        .sum();
    assert_eq!(tokens, 40 * 6 * 5);
    let coarsen = report
        .coverage
        .notes
        .iter()
        .find(|n| n.code == note::BUCKET_COARSENED)
        .expect("coarsening note");
    assert!(
        coarsen.detail.contains("month") && coarsen.detail.contains("above the 10 cell cap"),
        "{}",
        coarsen.detail
    );
    assert!(
        report.coverage.notes.iter().any(|n| n.code == note::CELL_CAP_EXCEEDED),
        "{:?}",
        report.coverage.notes
    );
}

// ── Validation & JSON round-trip ─────────────────────────────────────────

#[test]
fn query_validation_and_report_json_round_trip() {
    let reversed = series_query(DAY0, DAY0);
    assert!(reversed.validate().is_err());

    let zero_cap = UsageSeriesQuery {
        cell_cap: 0,
        ..series_query(DAY0, DAY0 + DAY)
    };
    assert!(zero_cap.validate().is_err());

    let q = series_query(DAY0, DAY0 + DAY);
    q.validate().unwrap();
    let r = with_tokens(
        run("r1", "e1", "t1", DAY0 + 100),
        Some(10),
        Some(20),
        Some(30),
        Some(40),
        Some(1_000),
    );
    let report = build_usage_series_report(
        &q,
        &UsageSeriesRecords {
            runs: vec![r],
            finished_executions: vec![exec("e1", "t1", "completed", DAY0 + 200)],
            tasks: vec![task("t1", "t1", Some(DAY0 + 300))],
        },
        None,
    );
    let json = serde_json::to_value(&report).unwrap();
    let decoded: TokenUsageSeriesReport = serde_json::from_value(json).unwrap();
    assert_eq!(decoded, report);
}

#[test]
fn in_window_execution_with_pre_window_run_groups_and_filters_by_observed_model() {
    let since = DAY0 + 2 * DAY;
    let until = DAY0 + 3 * DAY;
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        group_by: Some(UsageDimension::Model),
        filters: vec![
            UsageFilter {
                dimension: UsageDimension::Model,
                values: vec!["claude-sonnet-4".to_owned()],
            },
            UsageFilter {
                dimension: UsageDimension::Coverage,
                values: vec!["measured".to_owned()],
            },
        ],
        ..series_query(since, until)
    };
    let mut r = with_tokens(
        run("r_old", "e1", "t1", DAY0 + 100),
        Some(400),
        Some(100),
        Some(0),
        Some(0),
        Some(60_000),
    );
    r.run_model = Some("claude-sonnet-4".to_owned());
    let mut e = exec("e1", "t1", "completed", since + 200);
    e.execution_model = Some("opus".to_owned());
    let recs = UsageSeriesRecords {
        runs: vec![r],
        finished_executions: vec![e],
        tasks: vec![task("t1", "t1", None)],
    };
    let report = build_usage_series_report(&q, &recs, None);
    assert_eq!(report.groups, vec!["claude-sonnet-4"]);
    assert_eq!(report.buckets.len(), 1);
    assert_eq!(report.buckets[0].start_epoch_s, since);
    let cell = &report.buckets[0].cells[0];
    assert_eq!(cell.executions_completed, 1);
    assert_eq!(
        cell.measurement.runs_total, 0,
        "the pre-window run must not enter bucket measurement"
    );
    assert!(
        report.coverage.notes.iter().all(|n| n.code != note::DIMENSION_STARTED),
        "observed model/coverage from the pre-window run must not look unrecorded: {:?}",
        report.coverage.notes
    );
}

#[test]
fn orphan_revision_completed_in_window_is_not_counted_as_a_chain_root() {
    let q = UsageSeriesQuery {
        bucket: Some(UsageBucketWidth::Day),
        ..series_query(DAY0, DAY0 + 2 * DAY)
    };
    let mut orphan = task("orphan", "orphan", Some(DAY0 + 100));
    orphan.kind = "revision".to_owned();
    let r = with_tokens(
        run("r_orphan", "e_orphan", "orphan", DAY0 + 50),
        Some(80),
        Some(20),
        Some(0),
        Some(0),
        Some(1_000),
    );
    let report = build_usage_series_report(
        &q,
        &UsageSeriesRecords {
            runs: vec![r],
            finished_executions: Vec::new(),
            tasks: vec![orphan],
        },
        None,
    );
    let cell = &report.buckets[0].cells[0];
    assert_eq!(cell.measurement.total_tokens, 100);
    assert_eq!(
        cell.tasks_completed, 0,
        "an orphan revision must not count as a completed chain-root task"
    );
    assert_eq!(cell.tasks_completed_cost.runs_total, 0);
}
