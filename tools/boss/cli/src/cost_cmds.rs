//! `boss cost …` handlers: read-only reporting over recorded per-run
//! token spend. See `boss cost --help` and `Commands::Cost`'s doc
//! comment for the token-vs-USD contract.

use crate::*;

pub(crate) async fn run_cost_command(command: CostCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        CostCommand::Sol => {
            let response = client
                .send_request(&FrontendRequest::GetSolComparisonReport)
                .await
                .map_err(CliError::internal)?;
            let report = match response {
                FrontendEvent::SolComparisonReport { report } => report,
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    return Err(CliError::application(message));
                }
                other => return Err(unexpected_event("cost sol", &other)),
            };
            print_entity(ctx, &serde_json::json!({"report": report}), || {
                for arm in &report.arms {
                    println!(
                        "{}: {} PRs, {} merged; first CI passes {}/{}; findings/PR {}; revisions/merged PR {}; tokens/merged PR {}; {} unmeasured runs",
                        arm.arm,
                        arm.prs,
                        arm.merged_prs,
                        arm.first_attempt_ci_passes,
                        arm.ci_observed_prs,
                        format_optional_average(arm.findings_per_pr),
                        format_optional_average(arm.revisions_per_merged_pr),
                        format_optional_average(arm.tokens_per_merged_pr),
                        arm.merged_pr_cost.runs_unmeasured
                    );
                }
                for note in &report.notes {
                    println!("{note}");
                }
            })
        }
        CostCommand::Task(args) => {
            let work_item = match parse_work_item_selector(&args.id) {
                WorkItemSelector::ShortId(n) => resolve_short_id_item(&mut client, ctx, args.product, n).await?,
                WorkItemSelector::ProductShortId { product_slug, n } => {
                    resolve_short_id_item(&mut client, ctx, Some(product_slug), n).await?
                }
                WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => {
                    get_work_item(&mut client, &id).await?
                }
            };
            let (task, _label) = expect_leaf_work_item(work_item)?;
            let response = client
                .send_request(&FrontendRequest::GetWorkItemCostReport {
                    work_item_id: task.id.clone(),
                    estimate_usd: args.usd,
                })
                .await
                .map_err(CliError::internal)?;
            let report = match response {
                FrontendEvent::WorkItemCostReport { report } => report,
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    return Err(CliError::application(message));
                }
                other => return Err(unexpected_event("cost task", &other)),
            };
            let tz = resolve_display_tz(args.utc);
            print_entity(ctx, &serde_json::json!({ "report": report }), || {
                print_task_cost_report(&report, &tz)
            })
        }
        CostCommand::Window(args) => {
            let (since_epoch_s, until_epoch_s) = resolve_window(&args.since, args.until.as_deref())?;
            let response = client
                .send_request(&FrontendRequest::GetCostWindowReport {
                    since_epoch_s,
                    until_epoch_s,
                    estimate_usd: args.usd,
                })
                .await
                .map_err(CliError::internal)?;
            let report = match response {
                FrontendEvent::CostWindowReport { report } => report,
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    return Err(CliError::application(message));
                }
                other => return Err(unexpected_event("cost window", &other)),
            };
            let tz = resolve_display_tz(args.utc);
            print_entity(ctx, &serde_json::json!({ "report": report }), || {
                print_window_cost_report(&report, &tz)
            })
        }
        CostCommand::Rows(args) => {
            let (since_epoch_s, until_epoch_s) = resolve_window(&args.since, args.until.as_deref())?;
            let response = client
                .send_request(&FrontendRequest::GetTokenUsageRows(
                    boss_protocol::TokenUsageRowsRequest {
                        since_epoch_s,
                        until_epoch_s,
                        filters: args.filters.clone(),
                        group_by: args.group_by.clone(),
                        roll_up_revisions: !args.no_roll_up,
                        estimate_usd: args.usd,
                    },
                ))
                .await
                .map_err(CliError::internal)?;
            let report = match response {
                FrontendEvent::TokenUsageRows { report } => report,
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    return Err(CliError::application(message));
                }
                other => return Err(unexpected_event("cost rows", &other)),
            };
            let tz = resolve_display_tz(args.utc);
            print_entity(ctx, &serde_json::json!({ "report": report }), || {
                print_usage_rows_report(&report, args.depth, args.usd, &tz)
            })
        }
        CostCommand::Top(args) => {
            let (since_epoch_s, until_epoch_s) = resolve_window(&args.since, args.until.as_deref())?;
            let response = client
                .send_request(&FrontendRequest::GetTopCostConsumers {
                    since_epoch_s,
                    until_epoch_s,
                    limit: args.limit,
                    estimate_usd: args.usd,
                })
                .await
                .map_err(CliError::internal)?;
            let report = match response {
                FrontendEvent::TopCostConsumers { report } => report,
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    return Err(CliError::application(message));
                }
                other => return Err(unexpected_event("cost top", &other)),
            };
            let tz = resolve_display_tz(args.utc);
            print_entity(ctx, &serde_json::json!({ "report": report }), || {
                print_top_cost_report(&report, &tz)
            })
        }
    }
}

// ── Time-window parsing ──────────────────────────────────────────────

/// Parse a `--since`/`--until` bound: an RFC3339 timestamp, or a
/// relative duration ago (`24h`, `7d`, `2w`) resolved against `now`.
pub(crate) fn parse_epoch_bound(input: &str, now: i64) -> Result<i64, CliError> {
    let trimmed = input.trim();
    if let Some(epoch) = boss_engine_utils::iso8601::parse_iso8601_lenient(trimmed) {
        return Ok(epoch);
    }
    let secs = boss_engine_utils::duration::parse_compact_duration_secs(trimmed).map_err(|_| {
        CliError::usage(format!(
            "could not parse {input:?} as a time bound: expected RFC3339 (e.g. 2026-07-01T00:00:00Z) \
             or a relative duration like 24h / 7d / 2w"
        ))
    })?;
    Ok(now.saturating_sub(i64::try_from(secs).unwrap_or(i64::MAX)))
}

fn resolve_window(since: &str, until: Option<&str>) -> Result<(i64, i64), CliError> {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let since_epoch_s = parse_epoch_bound(since, now)?;
    let until_epoch_s = match until {
        Some(u) => parse_epoch_bound(u, now)?,
        None => now,
    };
    if until_epoch_s <= since_epoch_s {
        return Err(CliError::usage("--until must be after --since"));
    }
    Ok((since_epoch_s, until_epoch_s))
}

// ── Rendering ─────────────────────────────────────────────────────────

fn format_tokens(n: i64) -> String {
    let negative = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let mut result: String = grouped.chars().rev().collect();
    if negative {
        result.insert(0, '-');
    }
    result
}

fn print_measurement_summary(m: &CostMeasurement) {
    if m.runs_measured == 0 && m.runs_unmeasured > 0 {
        println!(
            "  Total tokens: unknown — none of the {} run(s) here have recorded token data",
            m.runs_total
        );
    } else {
        println!("  Total tokens: {}", format_tokens(m.total_tokens));
        println!(
            "    input={} output={} cache_write={} cache_read={}",
            format_optional_tokens(m.input_tokens),
            format_optional_tokens(m.output_tokens),
            format_optional_tokens(m.cache_creation_tokens),
            format_optional_tokens(m.cache_read_tokens),
        );
    }
    if m.tokens_partial {
        println!("  Token totals are a lower bound; unknown categories are not counted.");
    }
    for reason in &m.usage_unavailable {
        println!("  usage_unavailable: {reason}");
    }
    println!(
        "  Runs: {} measured, {} unmeasured (no token data recorded), {} recorded zero",
        m.runs_measured, m.runs_unmeasured, m.runs_zero,
    );
    if let Some(usd) = m.estimated_usd {
        let partial = if m.estimated_usd_partial {
            ", PARTIAL — see pricing gaps"
        } else {
            ""
        };
        println!("  Estimated cost: ${usd:.2}{partial} (ESTIMATE, not billing truth)");
    }
}

fn print_pricing_gaps(gaps: &[String]) {
    if gaps.is_empty() {
        return;
    }
    println!();
    println!(
        "  USD estimate excludes these models (unverified rates or incomplete usage): {}",
        gaps.join(", ")
    );
}

fn print_task_cost_report(report: &TaskCostReport, tz: &DisplayTz) {
    let label = boss_protocol::short_id_label(report.short_id).unwrap_or_else(|| report.work_item_id.clone());
    println!("Cost for {label}: {}", report.name);
    println!(
        "Generated {} ({})",
        format_epoch(report.generated_at_epoch_s, tz),
        tz.label
    );
    println!();
    print_measurement_summary(&report.overall);
    print_pricing_gaps(&report.pricing_gaps);

    if report.by_execution.is_empty() {
        println!();
        println!("  No executions recorded for this work item yet.");
        return;
    }
    println!();
    let mut table = new_dynamic_table(vec![
        "EXECUTION",
        "KIND",
        "STATUS",
        "MODEL(S)",
        "TOKENS",
        "MEASURED",
        "UNMEASURED",
    ]);
    for row in &report.by_execution {
        let models = if row.models.is_empty() {
            "—".to_owned()
        } else {
            row.models.join(", ")
        };
        table.add_row(vec![
            row.execution_id.as_str(),
            row.kind.as_str(),
            row.status.as_str(),
            models.as_str(),
            &format_measurement_tokens(&row.measurement),
            &row.measurement.runs_measured.to_string(),
            &row.measurement.runs_unmeasured.to_string(),
        ]);
    }
    print_table(table);
}

fn print_cost_buckets(title: &str, buckets: &[CostBucket], caveat: Option<&str>) {
    println!();
    println!("{title}:");
    if let Some(caveat) = caveat {
        println!("  NOTE: {caveat}");
    }
    if buckets.is_empty() {
        println!("  (no runs)");
        return;
    }
    let mut table = new_dynamic_table(vec!["LABEL", "TOKENS", "MEASURED", "UNMEASURED", "ZERO"]);
    for bucket in buckets {
        table.add_row(vec![
            bucket.label.as_str(),
            &format_measurement_tokens(&bucket.measurement),
            &bucket.measurement.runs_measured.to_string(),
            &bucket.measurement.runs_unmeasured.to_string(),
            &bucket.measurement.runs_zero.to_string(),
        ]);
    }
    print_table(table);
}

fn print_window_cost_report(report: &WindowCostReport, tz: &DisplayTz) {
    println!(
        "Cost from {} to {} ({})",
        format_epoch(report.since_epoch_s, tz),
        format_epoch(report.until_epoch_s, tz),
        tz.label,
    );
    if report.spans_capture_boundary {
        println!();
        println!(
            "  WARNING: this window starts before per-run token capture began ({}). Runs before that \
             instant have no recorded tokens and are excluded from every total below — these figures \
             are a FLOOR on the window's real spend, not a complete total.",
            format_epoch(report.capture_start_epoch_s, tz),
        );
    } else if report.overall.runs_unmeasured > 0 {
        println!();
        println!(
            "  WARNING: {} run(s) in this window have no recorded tokens and are excluded from every \
             total below — these figures are a FLOOR on the window's real spend, not a complete total.",
            report.overall.runs_unmeasured,
        );
    }
    println!();
    print_measurement_summary(&report.overall);
    print_pricing_gaps(&report.pricing_gaps);

    print_cost_buckets("BY DRIVER", &report.by_driver, None);
    print_cost_buckets("BY EXECUTION KIND", &report.by_kind, None);
    print_cost_buckets("BY MODEL", &report.by_model, None);
    print_cost_buckets(
        "BY REASONING MODE",
        &report.by_reasoning,
        Some(
            "these labels reflect each work item's current setting, not necessarily the setting in force when a run executed.",
        ),
    );
    print_cost_buckets(
        "BY EFFORT LEVEL",
        &report.by_effort_level,
        Some(
            "these labels reflect each work item's current setting, not necessarily the setting in force when a run executed.",
        ),
    );
}

fn print_top_cost_report(report: &TopCostReport, tz: &DisplayTz) {
    println!(
        "Top {} cost consumers from {} to {} ({})",
        report.limit,
        format_epoch(report.since_epoch_s, tz),
        format_epoch(report.until_epoch_s, tz),
        tz.label,
    );
    if report.excluded_unmeasured_count > 0 {
        println!(
            "  {} run(s) in this window have no recorded tokens and are excluded from ranking (not scored as zero).",
            report.excluded_unmeasured_count,
        );
    }
    print_pricing_gaps(&report.pricing_gaps);

    if report.rows.is_empty() {
        println!();
        println!("  No measured runs in this window.");
        return;
    }
    println!();
    let include_usd = report.rows.iter().any(|row| row.estimated_usd.is_some());
    let mut headers = vec!["WORK ITEM", "KIND", "MODEL", "TOKENS", "STARTED", "STATUS"];
    if include_usd {
        headers.push("USD (EST)");
    }
    let mut table = new_dynamic_table(headers);
    for row in &report.rows {
        let label = boss_protocol::short_id_label(row.short_id).unwrap_or_else(|| row.work_item_id.clone());
        let identity = format!("{label}: {}", row.name);
        let started = row
            .started_at_epoch_s
            .map(|e| format_epoch(e, tz))
            .unwrap_or_else(|| "—".to_owned());
        let mut cells = vec![
            identity,
            row.kind.clone(),
            row.model.clone().unwrap_or_else(|| "—".to_owned()),
            format_token_total(row.total_tokens, row.tokens_partial),
            started,
            row.status.clone(),
        ];
        let usd = row
            .estimated_usd
            .map(|value| format!("${value:.2}"))
            .unwrap_or_else(|| "—".to_owned());
        if include_usd {
            cells.push(usd);
        }
        table.add_row(cells);
    }
    print_table(table);
    if include_usd {
        println!("  USD figures: (ESTIMATE, not billing truth)");
    }
    if report.truncated {
        println!();
        println!(
            "  (truncated to the top {} — more measured runs exist in this window)",
            report.limit
        );
    }
}

// ── `boss cost rows` ──────────────────────────────────────────────────

fn print_usage_rows_report(report: &boss_protocol::TokenUsageRowsReport, depth: UsageDepth, usd: bool, tz: &DisplayTz) {
    println!(
        "Token usage from {} to {} ({})",
        format_epoch(report.since_epoch_s, tz),
        format_epoch(report.until_epoch_s, tz),
        tz.label,
    );
    if report.spans_capture_boundary {
        println!();
        println!(
            "  WARNING: this window starts before per-run token capture began ({}). Runs before that \
             instant have no recorded tokens and are excluded from every total below — these figures \
             are a FLOOR on the window's real spend, not a complete total.",
            format_epoch(report.capture_start_epoch_s, tz),
        );
    } else if report.overall.runs_unmeasured > 0 {
        println!();
        println!(
            "  WARNING: {} run(s) in this window have no recorded tokens and are excluded from every \
             total below — these figures are a FLOOR on the window's real spend, not a complete total.",
            report.overall.runs_unmeasured,
        );
    }
    if report.truncated {
        println!();
        println!(
            "  WARNING: more than {} runs matched; only the most recent {} are shown. Narrow --since or add --filter to see the rest.",
            report.run_cap, report.run_cap,
        );
    }
    println!();
    print_measurement_summary(&report.overall);
    print_usage_coverage(&report.coverage);
    print_pricing_gaps(&report.pricing_gaps);

    if report.group_by.is_empty() {
        println!();
        print_usage_tree(&report.tasks, &report.taskless, depth, usd);
    } else {
        let levels: Vec<String> = report.group_by.iter().map(|dimension| dimension.to_string()).collect();
        println!();
        println!("Grouped by {}.", levels.join(" › "));
        print_usage_groups(&report.groups, depth, usd, 0);
    }
    if usd {
        println!();
        println!("  USD figures: (ESTIMATE, not billing truth)");
    }
}

fn print_usage_coverage(coverage: &boss_protocol::UsageCoverage) {
    println!(
        "  Coverage: {} complete, {} partial (some token classes unreported), {} recorded zero, {} unmeasured (unknown, not counted as zero)",
        coverage.runs_complete, coverage.runs_partial, coverage.runs_zero, coverage.runs_unmeasured,
    );
    if let Some(priceable) = coverage.runs_priceable {
        println!("  USD estimate covers {priceable} of {} run(s)", coverage.runs_total);
    }
    if !coverage.by_caveat.is_empty() {
        let counted: Vec<String> = coverage
            .by_caveat
            .iter()
            .map(|caveat| format!("{} ×{}", caveat.code, caveat.runs))
            .collect();
        println!("  Caveats: {}", counted.join(", "));
    }
    for note in &coverage.notes {
        println!("  NOTE ({}): {}", note.code, note.detail);
    }
}

fn print_usage_groups(groups: &[boss_protocol::UsageGroup], depth: UsageDepth, usd: bool, level: usize) {
    for group in groups {
        let Some(key) = group.key.last() else { continue };
        println!();
        println!(
            "{}{}: {} — {}, {}/{} runs measured",
            "  ".repeat(level),
            key.dimension,
            key.label,
            format_measurement_tokens(&group.measurement),
            group.measurement.runs_measured,
            group.measurement.runs_total,
        );
        if group.subgroups.is_empty() {
            print_usage_tree(&group.tasks, &group.taskless, depth, usd);
        } else {
            print_usage_groups(&group.subgroups, depth, usd, level + 1);
        }
    }
}

fn print_usage_tree(
    tasks: &[boss_protocol::TaskUsageNode],
    taskless: &[boss_protocol::ExecutionUsageNode],
    depth: UsageDepth,
    usd: bool,
) {
    let rows = usage_tree_rows(tasks, taskless, depth, usd);
    if rows.is_empty() {
        println!("  (no runs)");
        return;
    }
    let mut headers = vec!["NAME", "KIND", "DRIVER", "MODEL", "STATUS", "TOKENS", "RUNS", "CAVEATS"];
    if usd {
        headers.push("USD (EST)");
    }
    let mut table = new_dynamic_table(headers);
    for row in rows {
        table.add_row(row);
    }
    print_table(table);
}

const NOT_RECORDED: &str = "not recorded";

/// A priced run that costs under half a cent must not print as `$0.00`, which
/// reads as "free"; only an unpriceable run prints a dash.
fn format_usd_estimate(usd: f64) -> String {
    if usd > 0.0 && usd < 0.005 {
        "<$0.01".to_owned()
    } else {
        format!("${usd:.2}")
    }
}

/// One printable line per node, down to `depth`. A task shows its own
/// (direct) tokens and, on a chain root with revisions, the chain's inclusive
/// total beside it; list-level totals elsewhere count direct only.
fn usage_tree_rows(
    tasks: &[boss_protocol::TaskUsageNode],
    taskless: &[boss_protocol::ExecutionUsageNode],
    depth: UsageDepth,
    usd: bool,
) -> Vec<Vec<String>> {
    fn usd_cell(value: Option<f64>, partial: bool) -> String {
        match value {
            Some(usd) if partial => format!("{} (partial)", format_usd_estimate(usd)),
            Some(usd) => format_usd_estimate(usd),
            None => "—".to_owned(),
        }
    }

    fn push_task(
        rows: &mut Vec<Vec<String>>,
        task: &boss_protocol::TaskUsageNode,
        nested: bool,
        depth: UsageDepth,
        usd: bool,
    ) {
        let label = boss_protocol::short_id_label(task.short_id).unwrap_or_else(|| task.task_id.clone());
        let name = format!("{}{label}: {}", if nested { "  ↳ " } else { "" }, task.name);
        // A chain root that owns no run in this slice has nothing of its own
        // to count: a dash, not a zero.
        let mut tokens = if task.direct.runs_total == 0 {
            "—".to_owned()
        } else {
            format_measurement_tokens(&task.direct)
        };
        if let Some(inclusive) = task.inclusive.as_ref().filter(|_| !task.revisions.is_empty()) {
            tokens = format!("{tokens} ({} incl. revisions)", format_measurement_tokens(inclusive));
        }
        let mut runs = format!("{}/{}", task.direct.runs_measured, task.direct.runs_total);
        if task.runs_outside_window > 0 {
            runs.push_str(&format!(" (+{} outside window)", task.runs_outside_window));
        }
        let mut row = vec![
            name,
            task.kind.clone(),
            String::new(),
            String::new(),
            task.status.clone(),
            tokens,
            runs,
            String::new(),
        ];
        if usd {
            row.push(usd_cell(task.direct.estimated_usd, task.direct.estimated_usd_partial));
        }
        rows.push(row);
        if depth != UsageDepth::Task {
            for execution in &task.executions {
                push_execution(rows, execution, "    ", depth, usd);
            }
        }
        for revision in &task.revisions {
            push_task(rows, revision, true, depth, usd);
        }
    }

    fn push_execution(
        rows: &mut Vec<Vec<String>>,
        execution: &boss_protocol::ExecutionUsageNode,
        indent: &str,
        depth: UsageDepth,
        usd: bool,
    ) {
        let mut row = vec![
            format!("{indent}{} ({})", execution.kind, execution.spend_class.as_str()),
            String::new(),
            execution.driver.clone().unwrap_or_else(|| NOT_RECORDED.to_owned()),
            execution.model_launched.clone().unwrap_or_else(|| "—".to_owned()),
            execution.status.clone(),
            format_measurement_tokens(&execution.measurement),
            format!(
                "{}/{}",
                execution.measurement.runs_measured, execution.measurement.runs_total
            ),
            String::new(),
        ];
        if usd {
            row.push(usd_cell(
                execution.measurement.estimated_usd,
                execution.measurement.estimated_usd_partial,
            ));
        }
        rows.push(row);
        if depth == UsageDepth::Run {
            for run in &execution.runs {
                let tokens = match run.total_tokens {
                    Some(total) => format_token_total(total, run.coverage == boss_protocol::RunCoverage::Partial),
                    None => "unknown".to_owned(),
                };
                let caveats: Vec<&str> = run.caveats.iter().map(|caveat| caveat.code.as_str()).collect();
                let mut row = vec![
                    format!("{indent}  run {}", run.run_id),
                    String::new(),
                    String::new(),
                    run.model_observed.clone().unwrap_or_else(|| "—".to_owned()),
                    run.status.clone(),
                    tokens,
                    run.coverage.as_str().to_owned(),
                    caveats.join(","),
                ];
                if usd {
                    row.push(match (run.estimated_usd, run.unpriceable_cause.as_deref()) {
                        (Some(value), _) => format_usd_estimate(value),
                        (None, Some(cause)) => format!("— ({cause})"),
                        (None, None) => "—".to_owned(),
                    });
                }
                rows.push(row);
            }
        }
    }

    let mut rows = Vec::new();
    for task in tasks {
        push_task(&mut rows, task, false, depth, usd);
    }
    for execution in taskless {
        let mut row = vec![
            format!("(no task) {}", execution.work_item_id),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ];
        if usd {
            row.push(String::new());
        }
        rows.push(row);
        push_execution(&mut rows, execution, "  ", depth, usd);
    }
    rows
}

fn format_optional_tokens(value: Option<i64>) -> String {
    value.map(format_tokens).unwrap_or_else(|| "unknown".into())
}

fn format_token_total(value: i64, partial: bool) -> String {
    let total = format_tokens(value);
    if partial { format!("{total} (partial)") } else { total }
}

fn format_measurement_tokens(value: &CostMeasurement) -> String {
    if value.runs_measured == 0 && value.runs_unmeasured > 0 {
        "unknown".into()
    } else {
        format_token_total(value.total_tokens, value.tokens_partial)
    }
}

fn format_optional_average(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_epoch_bound_accepts_relative_durations_case_insensitively() {
        let now = 2_000_000_000;
        assert_eq!(parse_epoch_bound("7d", now).unwrap(), now - 7 * 86_400);
        assert_eq!(parse_epoch_bound("2W", now).unwrap(), now - 2 * 604_800);
        assert_eq!(parse_epoch_bound("24H", now).unwrap(), now - 24 * 3_600);
    }

    #[test]
    fn parse_epoch_bound_accepts_rfc3339() {
        assert_eq!(parse_epoch_bound("2026-07-01T00:00:00Z", 0).unwrap(), 1_782_864_000);
    }

    #[test]
    fn parse_epoch_bound_rejects_unparseable_input() {
        let err = parse_epoch_bound("yesterday-ish", 0).unwrap_err();
        assert!(err.to_string().contains("could not parse"));
    }

    #[test]
    fn resolve_window_rejects_empty_or_reversed_ranges() {
        let err = resolve_window("0h", Some("0h")).unwrap_err();
        assert!(err.to_string().contains("--until must be after --since"));
    }

    #[test]
    fn parse_usage_filter_accepts_dimension_and_values() {
        let filter = parse_usage_filter("driver=claude, codex").unwrap();
        assert_eq!(filter.dimension, boss_protocol::UsageDimension::Driver);
        assert_eq!(filter.values, ["claude", "codex"]);
        assert!(parse_usage_filter("driver").unwrap_err().contains("DIMENSION=VALUE"));
        assert!(
            parse_usage_filter("driver=")
                .unwrap_err()
                .contains("at least one value")
        );
        assert!(
            parse_usage_filter("flavour=x")
                .unwrap_err()
                .contains("unknown usage dimension")
        );
    }

    fn measurement(runs_total: u32, runs_measured: u32, total_tokens: i64) -> CostMeasurement {
        CostMeasurement::builder()
            .agent_active_ms(0)
            .estimated_usd_partial(false)
            .runs_measured(runs_measured)
            .runs_total(runs_total)
            .runs_unmeasured(runs_total - runs_measured)
            .runs_zero(0)
            .total_tokens(total_tokens)
            .maybe_input_tokens((runs_measured > 0).then_some(total_tokens))
            .build()
    }

    fn usage_run(
        id: &str,
        coverage: boss_protocol::RunCoverage,
        total: Option<i64>,
        caveat: &str,
    ) -> boss_protocol::RunUsageRow {
        boss_protocol::RunUsageRow::builder()
            .run_id(id)
            .coverage(coverage)
            .created_at_epoch_s(1)
            .host_id("local")
            .input_convention(boss_protocol::InputConvention::Net)
            .status("completed")
            .caveats(vec![boss_protocol::UsageCaveat {
                code: caveat.to_owned(),
                detail: None,
            }])
            .maybe_total_tokens(total)
            .build()
    }

    fn usage_execution(
        runs: Vec<boss_protocol::RunUsageRow>,
        driver: Option<&str>,
    ) -> boss_protocol::ExecutionUsageNode {
        boss_protocol::ExecutionUsageNode::builder()
            .execution_id("exec_1")
            .work_item_id("task_1")
            .created_at_epoch_s(1)
            .driver_source(boss_protocol::DriverSource::LaunchTuple)
            .kind("chore_implementation")
            .measurement(measurement(runs.len() as u32, 1, 40))
            .spend_class(boss_protocol::SpendClass::Implementation)
            .status("completed")
            .runs(runs)
            .maybe_driver(driver.map(str::to_owned))
            .build()
    }

    fn usage_task(
        id: &str,
        short_id: i64,
        revisions: Vec<boss_protocol::TaskUsageNode>,
    ) -> boss_protocol::TaskUsageNode {
        let chain = !revisions.is_empty();
        boss_protocol::TaskUsageNode::builder()
            .task_id(id)
            .chain_root_id(id)
            .direct(measurement(2, 1, 40))
            .is_chain_root(true)
            .kind("chore")
            .name("Fix cursor flicker")
            .product_id("prod_1")
            .runs_outside_window(3)
            .status("done")
            .executions(vec![usage_execution(
                vec![
                    usage_run(
                        "run_known",
                        boss_protocol::RunCoverage::Partial,
                        Some(40),
                        "tokens_partial",
                    ),
                    usage_run(
                        "run_unknown",
                        boss_protocol::RunCoverage::Unmeasured,
                        None,
                        "pre_capture",
                    ),
                ],
                None,
            )])
            .revisions(revisions)
            .maybe_inclusive(chain.then(|| measurement(3, 2, 70)))
            .short_id(short_id)
            .build()
    }

    #[test]
    fn usage_rows_show_unknown_never_zero_and_the_chain_total_beside_direct() {
        let revision = usage_task("task_rev", 8, Vec::new());
        let root = usage_task("task_root", 7, vec![revision]);
        let rows = usage_tree_rows(&[root], &[], UsageDepth::Run, false);
        let cells: Vec<String> = rows.iter().map(|row| row.join(" | ")).collect();

        // Task row: direct tokens, the chain's inclusive total, and the runs outside the window.
        assert!(
            cells[0].contains("40") && cells[0].contains("(70 incl. revisions)"),
            "{}",
            cells[0]
        );
        assert!(cells[0].contains("1/2 (+3 outside window)"), "{}", cells[0]);
        // The execution has no recorded driver: it says so rather than guessing.
        assert!(cells[1].contains("not recorded"), "{}", cells[1]);
        // A partial run's total is a floor; an unmeasured run is unknown and never 0.
        assert!(
            cells[2].contains("40 (partial)") && cells[2].contains("tokens_partial"),
            "{}",
            cells[2]
        );
        assert!(
            cells[3].contains("unknown") && cells[3].contains("pre_capture"),
            "{}",
            cells[3]
        );
        assert!(!cells[3].contains("| 0 |"), "{}", cells[3]);
        // The revision nests under its root.
        assert!(cells[4].contains("↳") && cells[4].contains("T8"), "{}", cells[4]);
    }

    #[test]
    fn a_chain_root_with_no_runs_of_its_own_prints_a_dash_not_zero() {
        let mut root = usage_task("task_root", 7, vec![usage_task("task_rev", 8, Vec::new())]);
        root.direct = measurement(0, 0, 0);
        root.executions.clear();
        let rows = usage_tree_rows(&[root], &[], UsageDepth::Task, false);
        assert!(
            rows[0][5].starts_with("—") && rows[0][5].contains("incl. revisions"),
            "{:?}",
            rows[0]
        );
    }

    #[test]
    fn usage_rows_stop_at_the_requested_depth_and_label_taskless_executions() {
        let taskless = usage_execution(
            vec![usage_run(
                "run_guide",
                boss_protocol::RunCoverage::Measured,
                Some(5),
                "gross_input",
            )],
            Some("codex"),
        );
        let at_task = usage_tree_rows(&[usage_task("task_a", 1, Vec::new())], &[], UsageDepth::Task, false);
        assert_eq!(at_task.len(), 1);
        let at_execution = usage_tree_rows(
            &[usage_task("task_a", 1, Vec::new())],
            std::slice::from_ref(&taskless),
            UsageDepth::Execution,
            false,
        );
        assert_eq!(
            at_execution.len(),
            4,
            "task, its execution, the no-task header, that execution"
        );
        assert!(
            at_execution[2][0].starts_with("(no task) task_1"),
            "{:?}",
            at_execution[2]
        );
        assert!(!at_execution.iter().any(|row| row[0].contains("run ")));
    }

    #[test]
    fn a_tiny_priced_run_never_prints_as_zero_dollars() {
        assert_eq!(format_usd_estimate(0.0018), "<$0.01");
        assert_eq!(format_usd_estimate(0.0), "$0.00", "only a priced zero prints as zero");
        assert_eq!(format_usd_estimate(15.54366), "$15.54");
    }

    #[test]
    fn display_helpers_format_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(1_000_000), "1,000,000");
        assert_eq!(format_tokens(-1234), "-1,234");
        assert_eq!(format_optional_tokens(None), "unknown");
        assert_eq!(format_optional_tokens(Some(0)), "0");
        assert_eq!(format_token_total(12, true), "12 (partial)");
    }
}
