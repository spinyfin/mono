//! `boss cost …` handlers: read-only reporting over recorded per-run
//! token spend. See `boss cost --help` and `Commands::Cost`'s doc
//! comment for the token-vs-USD contract.

use crate::*;

#[derive(Debug, Subcommand)]
pub(crate) enum CostCommand {
    /// Compare Sol-eligible PRs by Sol/Astra arm, including revisions and retries.
    Sol,
    /// Token cost for one work item, aggregated across every execution
    /// and run it has ever had. The natural companion to `boss task
    /// show` / `boss chore show`.
    Task(CostTaskArgs),
    /// Token cost recorded in a time window, broken down by execution
    /// kind, model, reasoning mode, and effort level — the groupings
    /// that answer "where did the budget go."
    Window(CostWindowArgs),
    /// The highest-token-cost runs recorded in a time window, ranked
    /// descending.
    Top(CostTopArgs),
    /// Run-grain token usage in a time window as a task → execution → run
    /// tree with the driver and model each execution and run used. Every
    /// figure is computed by the engine; runs with no recorded tokens are
    /// shown as unknown, never as zero. A task's revisions roll up under its
    /// chain root: the root shows its own tokens and the chain's inclusive
    /// total, and list totals count each run once.
    Rows(CostRowsArgs),
    /// Bucketed token usage over time with engine-computed normalizer ratios
    /// (tokens per run, per completed execution, per active hour, per
    /// completed task, per distinct PR) and range coverage notes.
    Series(CostSeriesArgs),
}

/// How deep `boss cost rows` prints the tree in text mode (`--json` is always
/// the whole reply).
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum UsageDepth {
    Task,
    Execution,
    Run,
}

/// Parse `DIMENSION=VALUE[,VALUE…]` for `--filter`.
pub(crate) fn parse_usage_filter(input: &str) -> Result<boss_protocol::UsageFilter, String> {
    let (dimension, values) = input
        .split_once('=')
        .ok_or_else(|| format!("expected DIMENSION=VALUE[,VALUE…], got {input:?}"))?;
    let dimension: boss_protocol::UsageDimension = dimension.trim().parse()?;
    // An entire arm set is one dimension value, even when it contains commas.
    if dimension == boss_protocol::UsageDimension::ExperimentArm && values.trim_start().starts_with('{') {
        let arms: std::collections::BTreeMap<String, String> =
            serde_json::from_str(values).map_err(|error| format!("invalid experiment arm set: {error}"))?;
        return Ok(boss_protocol::UsageFilter {
            dimension,
            values: vec![serde_json::to_string(&arms).map_err(|e| e.to_string())?],
        });
    }
    let values: Vec<String> = values
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect();
    if values.is_empty() {
        return Err(format!("--filter {dimension} needs at least one value"));
    }
    Ok(boss_protocol::UsageFilter { dimension, values })
}

#[derive(Debug, Clone, Args)]
pub(crate) struct CostTaskArgs {
    /// Task/chore id. Accepts: primary id (`task_…`), friendly short id
    /// (case-insensitive `t`-prefixed form, `42`, or `#42`), or
    /// cross-product form (`boss/42` or `boss/#42`). Globally unique
    /// short ids resolve without `--product`.
    #[arg(value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) id: String,

    /// Resolve a friendly short id (`42` or `#42`) against this product
    /// (slug or id). Optional when the short id is globally unique.
    /// Ignored when the selector already embeds a product slug or is a
    /// primary id.
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Include an estimated USD figure alongside the token totals,
    /// priced from `boss-engine`'s human-maintained pricing table
    /// (`tools/boss/engine/core/src/cost_pricing.rs`). Always labelled
    /// as an estimate in the output — never billing truth.
    #[arg(long)]
    pub(crate) usd: bool,

    /// Print report timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct CostWindowArgs {
    /// Start of the window: an RFC3339 timestamp (e.g.
    /// `2026-07-01T00:00:00Z`) or a relative duration ago (`24h`, `7d`,
    /// `2w`).
    #[arg(long)]
    pub(crate) since: String,

    /// End of the window, same accepted formats as `--since`. Defaults
    /// to now.
    #[arg(long)]
    pub(crate) until: Option<String>,

    /// Include an estimated USD figure alongside the token totals. See
    /// `boss cost task --usd`'s help for the pricing-table caveat.
    #[arg(long)]
    pub(crate) usd: bool,

    /// Print report timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct CostTopArgs {
    /// Start of the window: an RFC3339 timestamp (e.g.
    /// `2026-07-01T00:00:00Z`) or a relative duration ago (`24h`, `7d`,
    /// `2w`).
    #[arg(long)]
    pub(crate) since: String,

    /// End of the window, same accepted formats as `--since`. Defaults
    /// to now.
    #[arg(long)]
    pub(crate) until: Option<String>,

    /// How many of the highest-token-cost runs to return.
    #[arg(long, default_value_t = 20)]
    pub(crate) limit: u32,

    /// Include an estimated USD figure alongside the token totals. See
    /// `boss cost task --usd`'s help for the pricing-table caveat.
    #[arg(long)]
    pub(crate) usd: bool,

    /// Print report timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct CostRowsArgs {
    /// Select the experiment for experiment_arm grouping and filters. Without
    /// this option the dimension is the whole recorded arm set. Selected values:
    /// treatment, control, stopped, not_enrolled, not_recorded.
    #[arg(long, value_name = "KEY")]
    pub(crate) experiment: Option<String>,

    /// Start of the window: an RFC3339 timestamp (e.g.
    /// `2026-07-01T00:00:00Z`) or a relative duration ago (`24h`, `7d`,
    /// `2w`).
    #[arg(long)]
    pub(crate) since: String,

    /// End of the window, same accepted formats as `--since`. Defaults
    /// to now.
    #[arg(long)]
    pub(crate) until: Option<String>,

    /// Group runs by up to two dimensions, outermost first (e.g.
    /// `driver,model`). Dimensions: product, project, task_kind,
    /// execution_kind, spend_class, driver, model, effort, reasoning,
    /// task_status, execution_status, coverage, host, experiment_arm, pipeline_stage. Reasoning is the task's
    /// current setting; the launch tuple does not record it.
    #[arg(long, value_delimiter = ',', value_name = "DIMENSION")]
    pub(crate) group_by: Vec<boss_protocol::UsageDimension>,

    /// Keep runs whose value for DIMENSION is one of the listed values, e.g.
    /// `--filter driver=claude,codex`. Repeat to filter on several
    /// dimensions: filters AND across dimensions and OR within one.
    /// Repeating the same dimension unions its values, so
    /// `--filter driver=claude --filter driver=codex` equals
    /// `--filter driver=claude,codex`.
    #[arg(long = "filter", value_name = "DIMENSION=VALUE[,VALUE…]", value_parser = parse_usage_filter)]
    pub(crate) filters: Vec<boss_protocol::UsageFilter>,

    /// List revisions as top-level rows instead of nesting them under
    /// their chain root.
    #[arg(long)]
    pub(crate) no_roll_up: bool,

    /// How deep the text output goes. `--json` always carries every level.
    #[arg(long, value_enum, default_value_t = UsageDepth::Execution)]
    pub(crate) depth: UsageDepth,

    /// Include an estimated USD figure alongside the token totals. See
    /// `boss cost task --usd`'s help for the pricing-table caveat; Grok and
    /// Antigravity are subscription-billed and never priced.
    #[arg(long)]
    pub(crate) usd: bool,

    /// Print report timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct CostSeriesArgs {
    /// Select the experiment for experiment_arm grouping and filters. Without
    /// this option the dimension is the whole recorded arm set. Selected values:
    /// treatment, control, stopped, not_enrolled, not_recorded.
    #[arg(long, value_name = "KEY")]
    pub(crate) experiment: Option<String>,

    /// Start of the window: an RFC3339 timestamp (e.g.
    /// `2026-07-01T00:00:00Z`) or a relative duration ago (`24h`, `7d`,
    /// `90d`).
    #[arg(long)]
    pub(crate) since: String,

    /// End of the window, same accepted formats as `--since`. Defaults
    /// to now.
    #[arg(long)]
    pub(crate) until: Option<String>,

    /// Bucket width: `hour`, `day`, `week`, or `month` (UTC calendar months).
    /// Omitted lets the engine choose from the window span and coarsen at the
    /// 5,000-cell cap. All bucket starts are UTC-aligned (week = Monday).
    #[arg(long, value_name = "WIDTH")]
    pub(crate) bucket: Option<boss_protocol::UsageBucketWidth>,

    /// Group series cells by one dimension (e.g. `driver`, `model`,
    /// `experiment_arm`, `pipeline_stage`).
    #[arg(long, value_name = "DIMENSION")]
    pub(crate) group_by: Option<boss_protocol::UsageDimension>,

    /// Keep facts whose value for DIMENSION is one of the listed values, e.g.
    /// `--filter driver=claude,codex`. Repeat to filter on several dimensions
    /// (AND across dimensions, OR within one).
    #[arg(long = "filter", value_name = "DIMENSION=VALUE[,VALUE…]", value_parser = parse_usage_filter)]
    pub(crate) filters: Vec<boss_protocol::UsageFilter>,

    /// Include an estimated USD figure alongside the token totals. Always
    /// labelled as an estimate; Grok and Antigravity are never priced.
    #[arg(long)]
    pub(crate) usd: bool,

    /// Print report timestamps in UTC instead of this host's local time.
    #[arg(long)]
    pub(crate) utc: bool,
}

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
                        experiment: args.experiment.clone(),
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
        CostCommand::Series(args) => {
            let (since_epoch_s, until_epoch_s) = resolve_window(&args.since, args.until.as_deref())?;
            let response = client
                .send_request(&FrontendRequest::GetTokenUsageSeries(
                    boss_protocol::TokenUsageSeriesRequest {
                        since_epoch_s,
                        until_epoch_s,
                        bucket: args.bucket,
                        experiment: args.experiment.clone(),
                        group_by: args.group_by,
                        filters: args.filters.clone(),
                        estimate_usd: args.usd,
                    },
                ))
                .await
                .map_err(CliError::internal)?;
            let report = match response {
                FrontendEvent::TokenUsageSeries { report } => report,
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    return Err(CliError::application(message));
                }
                other => return Err(unexpected_event("cost series", &other)),
            };
            let tz = resolve_display_tz(args.utc);
            print_entity(ctx, &serde_json::json!({ "report": report }), || {
                print_usage_series_report(&report, args.usd, &tz)
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

fn format_measurement_usd_summary(m: &CostMeasurement) -> Option<String> {
    let usd = m.estimated_usd?;
    let mut notes = Vec::new();
    if m.estimated_usd_partial {
        notes.push("PARTIAL — see pricing gaps".to_owned());
    }
    if m.unsplit_cache_write_runs > 0 {
        notes.push(format!(
            "unsplit_cache_write ×{} — priced at single 5m cache-write rate",
            m.unsplit_cache_write_runs
        ));
    }
    if notes.is_empty() {
        Some(format!("${usd:.2} (ESTIMATE, not billing truth)"))
    } else {
        Some(format!("${usd:.2}, {} (ESTIMATE, not billing truth)", notes.join("; ")))
    }
}

fn format_top_row_usd(row: &boss_protocol::CostTopRow) -> String {
    match row.estimated_usd {
        Some(value) if row.unsplit_cache_write => format!("${value:.2} (unsplit_cache_write)"),
        Some(value) => format!("${value:.2}"),
        None => "—".to_owned(),
    }
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
    if let Some(summary) = format_measurement_usd_summary(m) {
        println!("  Estimated cost: {summary}");
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

fn format_unmeasured_coordinators(report: &TopCostReport) -> String {
    report
        .unmeasured_coordinator
        .iter()
        .map(|row| {
            format!(
                "  {} ({}): usage UNKNOWN, not ranked and not counted as zero{}\n",
                row.label,
                row.status,
                row.reason
                    .as_deref()
                    .map(|reason| format!(" — {reason}"))
                    .unwrap_or_default()
            )
        })
        .collect()
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
    print!("{}", format_unmeasured_coordinators(report));
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
        if include_usd {
            cells.push(format_top_row_usd(row));
        }
        table.add_row(cells);
    }
    print_table(table);
    if include_usd {
        let unsplit_count = report.rows.iter().filter(|row| row.unsplit_cache_write).count();
        if unsplit_count > 0 {
            println!(
                "  USD figures: (ESTIMATE, not billing truth; {unsplit_count} row(s) labelled unsplit_cache_write used the single 5m cache-write rate)"
            );
        } else {
            println!("  USD figures: (ESTIMATE, not billing truth)");
        }
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
    println!("  Calls: {}", format_call_stats(&report.overall_calls));
    print_usage_coverage(&report.coverage);
    print_observed_experiments(&report.observed_experiments);
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

/// Lists each recorded experiment arm with its run and distinct-family counts.
/// Counts cover the whole window before filters and the row cap.
fn print_observed_experiments(observed: &[boss_protocol::UsageObservedExperiment]) {
    if observed.is_empty() {
        return;
    }
    println!("  Observed experiments (whole window, before filters and the row cap):");
    for experiment in observed {
        for arm in &experiment.arms {
            println!(
                "    {} / {}: {} run(s), {} famil{}",
                experiment.experiment,
                arm.arm,
                arm.runs,
                arm.families,
                if arm.families == 1 { "y" } else { "ies" },
            );
        }
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
        println!("{}  calls: {}", "  ".repeat(level), format_call_stats(&group.calls));
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
    let mut headers = vec![
        "NAME", "KIND", "DRIVER", "MODEL", "STATUS", "TOKENS", "RUNS", "ROUNDS", "PEAK CTX", "COMPACT", "CAVEATS",
    ];
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
        let [rounds, peak, compacted] = node_call_cells(&task.calls);
        let mut row = vec![
            name,
            task.kind.clone(),
            String::new(),
            String::new(),
            task.status.clone(),
            tokens,
            runs,
            rounds,
            peak,
            compacted,
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
        let [rounds, peak, compacted] = node_call_cells(&execution.calls);
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
            rounds,
            peak,
            compacted,
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
                let [rounds, peak, compacted] = run_call_cells(run);
                let mut row = vec![
                    format!("{indent}  run {}", run.run_id),
                    String::new(),
                    String::new(),
                    run.model_observed.clone().unwrap_or_else(|| "—".to_owned()),
                    run.status.clone(),
                    tokens,
                    run.coverage.as_str().to_owned(),
                    rounds,
                    peak,
                    compacted,
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
        let heading = execution
            .display_label
            .clone()
            .unwrap_or_else(|| format!("(no task) {}", execution.work_item_id));
        let mut row = vec![heading];
        row.resize(11, String::new());
        if usd {
            row.push(String::new());
        }
        rows.push(row);
        push_execution(&mut rows, execution, "  ", depth, usd);
    }
    rows
}

const NO_FIGURE: &str = "—";

/// The three call columns of a run: its own figures, each a dash when the
/// driver's transcript did not expose it (never `0`).
fn run_call_cells(run: &boss_protocol::RunUsageRow) -> [String; 3] {
    let cell = |value: Option<i64>, format: fn(i64) -> String| value.map_or_else(|| NO_FIGURE.to_owned(), format);
    [
        cell(run.rounds, |rounds| rounds.to_string()),
        cell(run.peak_context_tokens, format_tokens),
        cell(run.compactions, |compactions| compactions.to_string()),
    ]
}

/// The three call columns of a task or execution row: the average rounds per
/// run, the largest peak context, and how many runs compacted out of the runs
/// that record compactions. A dash means no run recorded the figure.
fn node_call_cells(calls: &boss_protocol::CallStats) -> [String; 3] {
    [
        calls
            .rounds_per_run
            .map_or_else(|| NO_FIGURE.to_owned(), |average| format!("{average:.1}/run")),
        calls
            .max_peak_context
            .map_or_else(|| NO_FIGURE.to_owned(), |peak| format!("max {}", format_tokens(peak))),
        if calls.runs_with_compactions_recorded == 0 {
            NO_FIGURE.to_owned()
        } else {
            format!("{}/{} runs", calls.runs_compacted, calls.runs_with_compactions_recorded)
        },
    ]
}

/// One line of call statistics for a total or a group. Every part names how
/// many runs it is over, and a part no run recorded says `unknown` instead of
/// printing a zero.
fn format_call_stats(calls: &boss_protocol::CallStats) -> String {
    let runs = |count: u32| format!("{count} run{}", if count == 1 { "" } else { "s" });
    let rounds = if calls.runs_with_rounds == 0 {
        "rounds unknown".to_owned()
    } else {
        let mut text = format!(
            "{} rounds over {}",
            format_tokens(calls.rounds),
            runs(calls.runs_with_rounds)
        );
        if let Some(average) = calls.rounds_per_run {
            text.push_str(&format!(" ({average:.1}/run)"));
        }
        if let Some(per_round) = calls.tokens_per_round {
            text.push_str(&format!(", {} tokens/round", format_tokens(per_round.round() as i64)));
        }
        text
    };
    let first = match calls.median_first_call_context {
        Some(median) => format!(
            "first-call context median {} ({})",
            format_tokens(median.round() as i64),
            runs(calls.runs_with_first_call_context)
        ),
        None => "first-call context unknown".to_owned(),
    };
    let peak = match (calls.median_peak_context, calls.max_peak_context) {
        (Some(median), Some(max)) => format!(
            "peak context median {}, max {} ({})",
            format_tokens(median.round() as i64),
            format_tokens(max),
            runs(calls.runs_with_peak_context)
        ),
        _ => "peak context unknown".to_owned(),
    };
    let compactions = if calls.runs_with_compactions_recorded == 0 {
        "compactions unknown".to_owned()
    } else {
        format!(
            "{} of {} compacted",
            calls.runs_compacted,
            runs(calls.runs_with_compactions_recorded)
        )
    };
    format!("{rounds}; {first}; {peak}; {compactions}")
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

// ── `boss cost series` ────────────────────────────────────────────────

fn format_bucket_width(width: boss_protocol::UsageBucketWidth) -> &'static str {
    match width {
        boss_protocol::UsageBucketWidth::Hour => "hour (3,600s)",
        boss_protocol::UsageBucketWidth::Day => "day (86,400s)",
        boss_protocol::UsageBucketWidth::Week => "week (604,800s)",
        boss_protocol::UsageBucketWidth::Month => "month (UTC calendar)",
    }
}

fn format_optional_ratio(value: Option<f64>) -> String {
    match value {
        Some(v) => format_tokens(v.round() as i64),
        None => "—".to_owned(),
    }
}

fn format_optional_share(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{:.1}%", v * 100.0),
        None => "—".to_owned(),
    }
}

fn series_table_rows(report: &boss_protocol::TokenUsageSeriesReport, usd: bool, tz: &DisplayTz) -> Vec<Vec<String>> {
    let grouped = report.group_by.is_some();
    let mut rows = Vec::new();
    for bucket in &report.buckets {
        let stamp = format_epoch(bucket.start_epoch_s, tz);
        for cell in &bucket.cells {
            let tokens = if cell.measurement.runs_total == 0 {
                "—".to_owned()
            } else {
                format_measurement_tokens(&cell.measurement)
            };
            let mut row = vec![stamp.clone()];
            if grouped {
                let label = report
                    .group_labels
                    .get(&cell.group)
                    .cloned()
                    .unwrap_or_else(|| cell.group.clone());
                row.push(label);
            }
            row.extend([
                tokens,
                format!("{}/{}", cell.measurement.runs_measured, cell.measurement.runs_total),
                cell.executions_completed.to_string(),
                cell.tasks_completed.to_string(),
                cell.distinct_prs.to_string(),
                format_optional_ratio(cell.ratios.tokens_per_run),
                format_optional_ratio(cell.ratios.tokens_per_completed_execution),
                format_optional_ratio(cell.ratios.tokens_per_active_hour),
                format_optional_ratio(cell.ratios.tokens_per_completed_task),
                format_optional_ratio(cell.ratios.tokens_per_pr),
            ]);
            if grouped {
                row.push(format_optional_share(cell.share_of_bucket_total_tokens));
            }
            if usd {
                let cell_usd = match cell.measurement.estimated_usd {
                    Some(v) if cell.measurement.estimated_usd_partial => {
                        format!("{} (partial)", format_usd_estimate(v))
                    }
                    Some(v) => format_usd_estimate(v),
                    None => "—".to_owned(),
                };
                row.push(cell_usd);
            }
            rows.push(row);
        }
    }
    rows
}

fn print_usage_series_report(report: &boss_protocol::TokenUsageSeriesReport, usd: bool, tz: &DisplayTz) {
    println!(
        "Token usage series from {} to {} ({}), bucket {}",
        format_epoch(report.since_epoch_s, tz),
        format_epoch(report.until_epoch_s, tz),
        tz.label,
        format_bucket_width(report.bucket_width),
    );
    if tz.offset_s != 0 {
        println!("  Bucket starts are UTC-aligned (week = Monday, month = calendar month).");
    }
    if let Some(dim) = report.group_by {
        println!("  Grouped by {dim}.");
    }
    if report.spans_capture_boundary {
        println!();
        println!(
            "  WARNING: this window starts before per-run token capture began ({}). Runs before that \
             instant have no recorded tokens — buckets before that point are a FLOOR, not a complete total.",
            format_epoch(report.capture_start_epoch_s, tz),
        );
    }
    println!();
    if let Some(data_from) = report.coverage.data_from_epoch_s {
        println!("  Data from: {}", format_epoch(data_from, tz));
    } else {
        println!("  Data from: none (no facts in window)");
    }
    if let Some(dim_from) = report.coverage.dimension_from_epoch_s {
        println!("  Dimension from: {}", format_epoch(dim_from, tz));
    }
    if !report.coverage.by_caveat.is_empty() {
        let counted: Vec<String> = report
            .coverage
            .by_caveat
            .iter()
            .map(|caveat| format!("{} ×{}", caveat.code, caveat.runs))
            .collect();
        println!("  Caveats: {}", counted.join(", "));
    }
    for note in &report.coverage.notes {
        if let Some(epoch_s) = note.epoch_s {
            println!(
                "  NOTE ({} @ {}): {}",
                note.code,
                format_epoch(epoch_s, tz),
                note.detail
            );
        } else {
            println!("  NOTE ({}): {}", note.code, note.detail);
        }
    }
    print_observed_experiments(&report.observed_experiments);
    print_pricing_gaps(&report.pricing_gaps);

    let rows = series_table_rows(report, usd, tz);
    println!();
    if rows.is_empty() {
        println!("  (no buckets with facts in this window)");
        return;
    }
    let grouped = report.group_by.is_some();
    let mut headers = vec!["BUCKET"];
    if grouped {
        headers.push("GROUP");
    }
    headers.extend([
        "TOKENS", "RUNS", "EXECS", "TASKS", "PRS", "TOK/RUN", "TOK/EXEC", "TOK/HR", "TOK/TASK", "TOK/PR",
    ]);
    if grouped {
        headers.push("SHARE");
    }
    if usd {
        headers.push("USD (EST)");
    }
    let mut table = new_dynamic_table(headers);
    for row in rows {
        table.add_row(row);
    }
    print_table(table);
    if usd {
        println!();
        println!("  USD figures: (ESTIMATE, not billing truth)");
    }
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
    fn experiment_and_pipeline_dimensions_parse_in_cli_requests() {
        use clap::Parser;
        #[derive(Parser)]
        struct Rows {
            #[command(flatten)]
            args: CostRowsArgs,
        }
        let rows = Rows::try_parse_from([
            "rows",
            "--since",
            "14d",
            "--experiment",
            "batched_search",
            "--group-by",
            "experiment_arm,pipeline_stage",
            "--filter",
            "pipeline_stage=ci_fix_revision",
            "--filter",
            "experiment_arm=treatment,control",
        ])
        .unwrap();
        assert_eq!(rows.args.experiment.as_deref(), Some("batched_search"));
        assert_eq!(
            rows.args.group_by,
            vec![
                boss_protocol::UsageDimension::ExperimentArm,
                boss_protocol::UsageDimension::PipelineStage
            ]
        );
        assert_eq!(rows.args.filters[1].values, ["treatment", "control"]);
        #[derive(Parser)]
        struct Series {
            #[command(flatten)]
            args: CostSeriesArgs,
        }
        let series = Series::try_parse_from([
            "series",
            "--since",
            "14d",
            "--experiment",
            "batched_search",
            "--group-by",
            "experiment_arm",
        ])
        .unwrap();
        assert_eq!(series.args.experiment.as_deref(), Some("batched_search"));
        assert_eq!(series.args.group_by, Some(boss_protocol::UsageDimension::ExperimentArm));
        let set = parse_usage_filter(r#"experiment_arm={"zeta":"control","alpha":"treatment"}"#).unwrap();
        assert_eq!(set.values, [r#"{"alpha":"treatment","zeta":"control"}"#]);
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

        let mut coord_exec = usage_execution(
            vec![usage_run(
                "coord_run_1",
                boss_protocol::RunCoverage::Measured,
                Some(50),
                "estimate_only",
            )],
            Some("claude"),
        );
        coord_exec.execution_id = "coord_sess_abcd1234-5678-90ab-cdef-1234567890ab".into();
        coord_exec.work_item_id = "coordinator:abcd1234-5678-90ab-cdef-1234567890ab".into();
        coord_exec.kind = boss_protocol::COORDINATOR_USAGE_KIND.into();
        coord_exec.spend_class = boss_protocol::SpendClass::Overhead;
        coord_exec.display_label = Some(boss_protocol::coordinator_session_label(&coord_exec.execution_id));
        let coord_rows = usage_tree_rows(&[], &[coord_exec], UsageDepth::Execution, false);
        assert_eq!(coord_rows[0][0], "Coordinator session abcd1234");
    }

    #[test]
    fn call_columns_show_each_run_figure_and_a_dash_for_one_the_driver_did_not_expose() {
        let exposed = boss_protocol::RunUsageRow::builder()
            .run_id("run_exposed")
            .coverage(boss_protocol::RunCoverage::Measured)
            .created_at_epoch_s(1)
            .host_id("local")
            .input_convention(boss_protocol::InputConvention::Net)
            .status("completed")
            .rounds(41)
            .peak_context_tokens(173_801)
            .compactions(2)
            .build();
        let hidden = boss_protocol::RunUsageRow::builder()
            .run_id("run_hidden")
            .coverage(boss_protocol::RunCoverage::Partial)
            .created_at_epoch_s(1)
            .host_id("local")
            .input_convention(boss_protocol::InputConvention::Net)
            .status("completed")
            .rounds(3)
            .build();
        let rows = usage_tree_rows(
            &[],
            &[usage_execution(vec![exposed, hidden], Some("claude"))],
            UsageDepth::Run,
            false,
        );
        // [0] is the no-task header, [1] the execution, [2] and [3] the runs.
        assert_eq!(rows[2][7..10], ["41", "173,801", "2"]);
        assert_eq!(
            rows[3][7..10],
            ["3", "—", "—"],
            "a peak or compaction count that was not recorded is a dash, never 0"
        );
    }

    #[test]
    fn node_call_columns_average_rounds_and_count_compacted_runs_over_those_that_record_them() {
        let calls = boss_protocol::CallStats::builder()
            .runs_with_rounds(4)
            .rounds(52)
            .rounds_per_run(13.0)
            .max_peak_context(220_000)
            .runs_with_compactions_recorded(3)
            .runs_compacted(1)
            .build();
        assert_eq!(node_call_cells(&calls), ["13.0/run", "max 220,000", "1/3 runs"]);
        assert_eq!(
            node_call_cells(&boss_protocol::CallStats::default()),
            ["—", "—", "—"],
            "no run recorded anything: dashes, not zeros"
        );
    }

    #[test]
    fn call_stats_line_names_the_runs_each_figure_is_over_and_says_unknown_for_the_rest() {
        let calls = boss_protocol::CallStats::builder()
            .runs_with_rounds(4)
            .rounds(52)
            .rounds_per_run(13.0)
            .tokens_per_round(47_213.4)
            .runs_with_first_call_context(2)
            .median_first_call_context(24_500.0)
            .runs_with_peak_context(3)
            .median_peak_context(180_000.0)
            .max_peak_context(220_000)
            .runs_with_compactions_recorded(3)
            .runs_compacted(1)
            .build();
        assert_eq!(
            format_call_stats(&calls),
            "52 rounds over 4 runs (13.0/run), 47,213 tokens/round; \
             first-call context median 24,500 (2 runs); \
             peak context median 180,000, max 220,000 (3 runs); 1 of 3 runs compacted"
        );
        assert_eq!(
            format_call_stats(&boss_protocol::CallStats::default()),
            "rounds unknown; first-call context unknown; peak context unknown; compactions unknown"
        );
    }

    #[test]
    fn a_tiny_priced_run_never_prints_as_zero_dollars() {
        assert_eq!(format_usd_estimate(0.0018), "<$0.01");
        assert_eq!(format_usd_estimate(0.0), "$0.00", "only a priced zero prints as zero");
        assert_eq!(format_usd_estimate(15.54366), "$15.54");
    }

    #[test]
    fn usd_summaries_label_unsplit_cache_writes_for_task_window_and_top() {
        let mut clean = measurement(1, 1, 1_000_000);
        clean.estimated_usd = Some(13.60);
        assert_eq!(
            format_measurement_usd_summary(&clean).as_deref(),
            Some("$13.60 (ESTIMATE, not billing truth)")
        );

        let mut unsplit = measurement(2, 2, 2_000_000);
        unsplit.estimated_usd = Some(23.60);
        unsplit.unsplit_cache_write_runs = 1;
        assert_eq!(
            format_measurement_usd_summary(&unsplit).as_deref(),
            Some("$23.60, unsplit_cache_write ×1 — priced at single 5m cache-write rate (ESTIMATE, not billing truth)")
        );

        let mut partial_and_unsplit = unsplit.clone();
        partial_and_unsplit.estimated_usd_partial = true;
        assert_eq!(
            format_measurement_usd_summary(&partial_and_unsplit).as_deref(),
            Some(
                "$23.60, PARTIAL — see pricing gaps; unsplit_cache_write ×1 — priced at single 5m cache-write rate (ESTIMATE, not billing truth)"
            )
        );

        let top_row = |estimated_usd: Option<f64>, unsplit_cache_write: bool| {
            boss_protocol::CostTopRow::builder()
                .execution_id("e1")
                .run_id("r1")
                .work_item_id("t1")
                .kind("chore_implementation")
                .name("Test")
                .status("completed")
                .total_tokens(1_000_000)
                .maybe_estimated_usd(estimated_usd)
                .unsplit_cache_write(unsplit_cache_write)
                .build()
        };
        assert_eq!(format_top_row_usd(&top_row(Some(13.60), false)), "$13.60");
        assert_eq!(
            format_top_row_usd(&top_row(Some(10.00), true)),
            "$10.00 (unsplit_cache_write)"
        );
        assert_eq!(format_top_row_usd(&top_row(None, false)), "—");
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
    fn series_table_renders_unknown_and_missing_ratios_as_dashes_never_zero() {
        let mut partial_m = measurement(2, 2, 3_000);
        partial_m.tokens_partial = true;
        let unmeasured_m = measurement(1, 0, 0);
        let report = boss_protocol::TokenUsageSeriesReport::builder()
            .bucket_secs(86_400)
            .bucket_width(boss_protocol::UsageBucketWidth::Day)
            .capture_start_epoch_s(1_785_144_180)
            .coverage(boss_protocol::UsageSeriesCoverage::builder().build())
            .generated_at_epoch_s(1_790_121_600)
            .since_epoch_s(1_790_035_200)
            .spans_capture_boundary(false)
            .until_epoch_s(1_790_121_600)
            .group_by(boss_protocol::UsageDimension::Driver)
            .groups(vec!["claude".into(), "grok".into()])
            .buckets(vec![boss_protocol::UsageBucket {
                start_epoch_s: 1_790_035_200,
                cells: vec![
                    boss_protocol::UsageCell::builder()
                        .distinct_prs(1)
                        .executions_completed(2)
                        .group("claude")
                        .measurement(partial_m)
                        .ratios(
                            boss_protocol::UsageRatios::builder()
                                .tokens_per_run(1_500.0)
                                .tokens_per_completed_execution(1_500.0)
                                .tokens_per_pr(3_000.0)
                                .build(),
                        )
                        .tasks_completed(0)
                        .tasks_completed_cost(measurement(0, 0, 0))
                        .share_of_bucket_total_tokens(1.0)
                        .build(),
                    boss_protocol::UsageCell::builder()
                        .distinct_prs(0)
                        .executions_completed(0)
                        .group("grok")
                        .measurement(unmeasured_m)
                        .ratios(boss_protocol::UsageRatios::default())
                        .tasks_completed(0)
                        .tasks_completed_cost(measurement(0, 0, 0))
                        .build(),
                ],
            }])
            .build();
        let tz = resolve_display_tz(true);
        let rows = series_table_rows(&report, false, &tz);
        assert_eq!(rows.len(), 2);
        let claude = rows[0].join(" | ");
        assert!(claude.contains("3,000 (partial)"), "{claude}");
        assert!(claude.contains("100.0%"), "{claude}");
        assert!(claude.contains("—"), "missing ratios print as dash: {claude}");
        let grok = rows[1].join(" | ");
        assert!(grok.contains("unknown"), "unmeasured bucket prints unknown: {grok}");
        assert!(
            !grok.contains("| 0.0%"),
            "unmeasured group never gets 0.0% share: {grok}"
        );
    }
}

#[cfg(test)]
mod coordinator_tests {
    use super::*;

    #[test]
    fn unreadable_only_session_is_unknown_in_text_and_json() {
        let label = boss_protocol::coordinator_session_label("short-session");
        let report: TopCostReport = serde_json::from_value(serde_json::json!({
            "since_epoch_s":0,"until_epoch_s":3600,"limit":10,"rows":[],
            "excluded_unmeasured_count":1,"generated_at_epoch_s":3600,
            "pricing_gaps":[], "truncated":false, "unmeasured_coordinator":[{
                "execution_id":"coordinator:short-session", "label":label,
                "status":"active", "reason":"Transcript unreadable"
            }]
        }))
        .unwrap();
        let text = format_unmeasured_coordinators(&report);
        assert!(text.contains("Coordinator session short-session"));
        assert!(text.contains("usage UNKNOWN, not ranked and not counted as zero"));
        assert!(text.contains("Transcript unreadable"));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["unmeasured_coordinator"][0]["label"], label);
        assert_eq!(json["unmeasured_coordinator"][0]["reason"], "Transcript unreadable");
        assert_eq!(json["rows"], serde_json::json!([]));
        assert!(json["unmeasured_coordinator"][0].get("total_tokens").is_none());
    }
}
