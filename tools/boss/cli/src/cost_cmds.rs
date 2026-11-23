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
            let task_id = resolve_leaf_work_item_id(&mut client, ctx, &args.id, args.product).await?;
            let response = client
                .send_request(&FrontendRequest::GetWorkItemCostReport {
                    work_item_id: task_id,
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

async fn resolve_leaf_work_item_id(
    client: &mut WorkerClient,
    ctx: &RunContext,
    raw_selector: &str,
    product: Option<String>,
) -> Result<String, CliError> {
    let work_item = match parse_work_item_selector(raw_selector) {
        WorkItemSelector::ShortId(n) => resolve_short_id_item(client, ctx, product, n).await?,
        WorkItemSelector::ProductShortId { product_slug, n } => {
            resolve_short_id_item(client, ctx, Some(product_slug), n).await?
        }
        WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => get_work_item(client, &id).await?,
    };
    let (task, _label) = expect_leaf_work_item(work_item)?;
    Ok(task.id.clone())
}

/// How `boss runs list --work-item` treats a selector: friendly short ids
/// still resolve through the engine, while canonical owner ids (task,
/// comment, automation, comparison, product, project) pass through as
/// `work_executions.work_item_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunsListOwnerSelector {
    Friendly,
    Canonical(String),
}

fn classify_runs_list_owner(raw: &str) -> RunsListOwnerSelector {
    match parse_work_item_selector(raw) {
        WorkItemSelector::ShortId(_) | WorkItemSelector::ProductShortId { .. } => RunsListOwnerSelector::Friendly,
        WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => RunsListOwnerSelector::Canonical(id),
    }
}

async fn resolve_runs_list_owner_id(
    client: &mut WorkerClient,
    ctx: &RunContext,
    raw_selector: &str,
    product: Option<String>,
) -> Result<String, CliError> {
    match classify_runs_list_owner(raw_selector) {
        RunsListOwnerSelector::Friendly => resolve_leaf_work_item_id(client, ctx, raw_selector, product).await,
        RunsListOwnerSelector::Canonical(id) => Ok(id),
    }
}

pub(crate) async fn run_runs_command(command: RunsCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        RunsCommand::List(args) => {
            let work_item_id = match args.work_item.as_deref() {
                Some(raw) => Some(resolve_runs_list_owner_id(&mut client, ctx, raw, args.product).await?),
                None => None,
            };
            let (since_epoch_s, until_epoch_s) =
                resolve_optional_window(args.since.as_deref(), args.until.as_deref(), work_item_id.is_some())?;
            let response = client
                .send_request(&FrontendRequest::GetRunHistory {
                    since_epoch_s,
                    until_epoch_s,
                    driver: args.driver,
                    kind: args.kind,
                    status: args.status,
                    work_item_id,
                    limit: args.limit,
                })
                .await
                .map_err(CliError::internal)?;
            let report = match response {
                FrontendEvent::RunHistory { report } => report,
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    return Err(CliError::application(message));
                }
                other => return Err(unexpected_event("runs list", &other)),
            };
            let tz = resolve_display_tz(args.utc);
            print_entity(ctx, &serde_json::json!({ "report": report }), || {
                print_runs_list_report(&report, &tz)
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

pub(crate) fn resolve_optional_window(
    since: Option<&str>,
    until: Option<&str>,
    has_work_item: bool,
) -> Result<(Option<i64>, Option<i64>), CliError> {
    match (since, until) {
        (Some(s), u) => {
            let (since_s, until_s) = resolve_window(s, u)?;
            Ok((Some(since_s), Some(until_s)))
        }
        (None, u) if !has_work_item => {
            let (since_s, until_s) = resolve_window("24h", u)?;
            Ok((Some(since_s), Some(until_s)))
        }
        (None, Some(u)) => {
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            Ok((None, Some(parse_epoch_bound(u, now)?)))
        }
        (None, None) => Ok((None, None)),
    }
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
    let mut headers = vec!["WORK ITEM", "KIND", "DRIVER", "MODEL", "TOKENS", "STARTED", "STATUS"];
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
            row.driver.clone().unwrap_or_else(|| "—".to_owned()),
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

fn print_runs_list_report(report: &RunsListReport, tz: &DisplayTz) {
    match (report.since_epoch_s, report.until_epoch_s) {
        (Some(since), Some(until)) => println!(
            "Runs from {} to {} ({})",
            format_epoch(since, tz),
            format_epoch(until, tz),
            tz.label,
        ),
        _ => println!(
            "Runs (generated {} {})",
            format_epoch(report.generated_at_epoch_s, tz),
            tz.label,
        ),
    }
    if report.rows.is_empty() {
        println!();
        println!("  No matching runs.");
        return;
    }
    println!();
    let mut table = new_dynamic_table(vec![
        "WORK ITEM",
        "KIND",
        "DRIVER",
        "MODEL",
        "STATUS",
        "STARTED",
        "FINISHED",
        "TOKENS",
    ]);
    for row in &report.rows {
        let label = boss_protocol::short_id_label(row.short_id).unwrap_or_else(|| row.work_item_id.clone());
        let identity = format!("{label}: {}", row.name);
        let started = row
            .started_at_epoch_s
            .map(|e| format_epoch(e, tz))
            .unwrap_or_else(|| "—".to_owned());
        let finished = row
            .finished_at_epoch_s
            .map(|e| format_epoch(e, tz))
            .unwrap_or_else(|| "—".to_owned());
        let tokens = if !row.tokens_measured {
            "unmeasured".to_owned()
        } else {
            format_token_total(row.total_tokens.unwrap_or(0), row.tokens_partial)
        };
        table.add_row(vec![
            identity,
            row.kind.clone(),
            row.driver.clone().unwrap_or_else(|| "—".to_owned()),
            row.model.clone().unwrap_or_else(|| "—".to_owned()),
            row.status.clone(),
            started,
            finished,
            tokens,
        ]);
    }
    print_table(table);
    if report.truncated
        && let Some(limit) = report.limit
    {
        println!();
        println!("  (truncated to {limit} — more matching runs exist)");
    }
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
    fn display_helpers_format_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(1_000_000), "1,000,000");
        assert_eq!(format_tokens(-1234), "-1,234");
        assert_eq!(format_optional_tokens(None), "unknown");
        assert_eq!(format_optional_tokens(Some(0)), "0");
        assert_eq!(format_token_total(12, true), "12 (partial)");
    }

    #[test]
    fn classify_runs_list_owner_passes_canonical_non_task_ids() {
        assert_eq!(classify_runs_list_owner("42"), RunsListOwnerSelector::Friendly);
        assert_eq!(classify_runs_list_owner("t12"), RunsListOwnerSelector::Friendly);
        assert_eq!(classify_runs_list_owner("acme/7"), RunsListOwnerSelector::Friendly);
        assert_eq!(
            classify_runs_list_owner("task_abc"),
            RunsListOwnerSelector::Canonical("task_abc".into())
        );
        assert_eq!(
            classify_runs_list_owner("comment_owner_1"),
            RunsListOwnerSelector::Canonical("comment_owner_1".into())
        );
        assert_eq!(
            classify_runs_list_owner("auto_owner_1"),
            RunsListOwnerSelector::Canonical("auto_owner_1".into())
        );
        assert_eq!(
            classify_runs_list_owner("cmp_owner_1"),
            RunsListOwnerSelector::Canonical("cmp_owner_1".into())
        );
    }

    #[test]
    fn resolve_optional_window_defaults_to_24h_when_unscoped() {
        let (since, until) = resolve_optional_window(None, None, false).unwrap();
        assert_eq!(until.unwrap() - since.unwrap(), 86_400);
    }

    #[test]
    fn resolve_optional_window_keeps_explicit_since() {
        let (since, until) = resolve_optional_window(Some("7d"), None, false).unwrap();
        assert_eq!(until.unwrap() - since.unwrap(), 7 * 86_400);
    }

    #[test]
    fn resolve_optional_window_with_work_item_and_no_since_is_unbounded() {
        let (since, until) = resolve_optional_window(None, None, true).unwrap();
        assert_eq!((since, until), (None, None));
    }

    #[test]
    fn resolve_optional_window_until_only_with_work_item() {
        let (since, until) = resolve_optional_window(None, Some("2026-07-01T00:00:00Z"), true).unwrap();
        assert_eq!(since, None);
        assert_eq!(until, Some(1_782_864_000));
    }

    #[test]
    fn resolve_optional_window_until_only_without_work_item_still_starts_24h_ago() {
        let (since, until) = resolve_optional_window(None, Some("1h"), false).unwrap();
        assert_eq!(until.unwrap() - since.unwrap(), 23 * 3_600);
    }

    #[test]
    fn runs_list_cli_rejects_limit_zero() {
        let err = Cli::try_parse_from(["boss", "runs", "list", "--limit", "0"]).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("must be at least 1"),
            "expected --limit 0 to be rejected, got: {text}"
        );
    }

    #[test]
    fn runs_list_cli_parses_canonical_non_task_work_item() {
        let cli = Cli::try_parse_from([
            "boss",
            "runs",
            "list",
            "--work-item",
            "comment_owner_1",
            "--kind",
            "answer_agent",
        ])
        .expect("canonical owner id is accepted");
        let Commands::Runs {
            command: RunsCommand::List(args),
        } = cli.command
        else {
            panic!("expected RunsCommand::List");
        };
        assert_eq!(args.work_item.as_deref(), Some("comment_owner_1"));
        assert_eq!(args.kind.as_deref(), Some("answer_agent"));
    }

    #[test]
    fn runs_list_cli_parses_filters_and_help_cross_references() {
        use clap::CommandFactory;
        let cli = Cli::try_parse_from([
            "boss",
            "runs",
            "list",
            "--since",
            "14d",
            "--until",
            "1d",
            "--driver",
            "agy",
            "--kind",
            "task_implementation",
            "--status",
            "failed",
            "--work-item",
            "42",
            "--limit",
            "100",
            "--json",
        ])
        .expect("boss runs list parses");
        assert!(cli.global.json);
        let Commands::Runs {
            command: RunsCommand::List(args),
        } = cli.command
        else {
            panic!("expected RunsCommand::List");
        };
        assert_eq!(args.since.as_deref(), Some("14d"));
        assert_eq!(args.until.as_deref(), Some("1d"));
        assert_eq!(args.driver.as_deref(), Some("agy"));
        assert_eq!(args.kind.as_deref(), Some("task_implementation"));
        assert_eq!(args.status.as_deref(), Some("failed"));
        assert_eq!(args.work_item.as_deref(), Some("42"));
        assert_eq!(args.limit, Some(100));

        let mut root_cmd = Cli::command();
        let root_help = root_cmd.render_help().to_string();
        assert!(
            root_help.contains("runs"),
            "boss --help should mention runs: {root_help}"
        );

        let cost_cmd = root_cmd.find_subcommand_mut("cost").expect("cost subcommand");
        let top_cmd = cost_cmd.find_subcommand_mut("top").expect("cost top subcommand");
        let top_help = top_cmd.render_help().to_string();
        assert!(
            top_help.contains("boss runs list"),
            "boss cost top --help should cross-reference boss runs list: {top_help}"
        );
    }
}
