//! `boss cost …` handlers: read-only reporting over recorded per-run
//! token spend. See `boss cost --help` and `Commands::Cost`'s doc
//! comment for the token-vs-USD contract.

use crate::*;

pub(crate) async fn run_cost_command(command: CostCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
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
fn parse_epoch_bound(input: &str, now: i64) -> Result<i64, CliError> {
    let trimmed = input.trim();
    if let Some(epoch) = boss_engine_utils::iso8601::parse_iso8601_lenient(trimmed) {
        return Ok(epoch);
    }
    let (digits, unit_secs) = if let Some(rest) = trimmed.strip_suffix(['h', 'H']) {
        (rest, 3_600_i64)
    } else if let Some(rest) = trimmed.strip_suffix(['d', 'D']) {
        (rest, 86_400_i64)
    } else if let Some(rest) = trimmed.strip_suffix(['w', 'W']) {
        (rest, 604_800_i64)
    } else {
        return Err(CliError::usage(format!(
            "could not parse {input:?} as a time bound: expected RFC3339 (e.g. 2026-07-01T00:00:00Z) \
             or a relative duration like 24h / 7d / 2w"
        )));
    };
    let count: i64 = digits
        .trim()
        .parse()
        .map_err(|_| CliError::usage(format!("could not parse relative duration {input:?}")))?;
    Ok(now - count.saturating_mul(unit_secs))
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

// ── Local-time display ───────────────────────────────────────────────
//
// The workspace `chrono` dependency ships without the `clock` feature, so
// there is no portable in-process "what's my local timezone" lookup
// without a new external dependency. Shelling out to `date +%z` (POSIX,
// identical on BSD and GNU `date`) gets this host's *current* UTC offset
// with zero new dependencies. Applying that one fixed offset to every
// timestamp in a report is an approximation — a timestamp on the other
// side of a DST transition from "now" can be off by an hour — so the
// report header always states the offset in use and callers who need
// exactness can pass `--utc`.

struct DisplayTz {
    offset_s: i32,
    label: String,
}

fn detect_local_utc_offset_seconds() -> Option<i32> {
    let output = Command::new("date").arg("+%z").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.len() != 5 {
        return None;
    }
    let bytes = text.as_bytes();
    let sign: i32 = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hh: i32 = std::str::from_utf8(&bytes[1..3]).ok()?.parse().ok()?;
    let mm: i32 = std::str::from_utf8(&bytes[3..5]).ok()?.parse().ok()?;
    Some(sign * (hh * 3_600 + mm * 60))
}

fn offset_suffix(offset_s: i32) -> String {
    let sign = if offset_s < 0 { '-' } else { '+' };
    let abs = offset_s.unsigned_abs();
    format!("{sign}{:02}:{:02}", abs / 3_600, (abs % 3_600) / 60)
}

fn resolve_display_tz(force_utc: bool) -> DisplayTz {
    if force_utc {
        return DisplayTz {
            offset_s: 0,
            label: "UTC".to_owned(),
        };
    }
    match detect_local_utc_offset_seconds() {
        Some(offset_s) => DisplayTz {
            offset_s,
            label: format!(
                "this host's local time, UTC{} — its current offset, applied uniformly; a timestamp \
                 near a DST change may be off by up to 1 hour. Pass --utc for exact UTC.",
                offset_suffix(offset_s)
            ),
        },
        None => DisplayTz {
            offset_s: 0,
            label: "UTC (could not detect this host's local offset)".to_owned(),
        },
    }
}

fn format_epoch(epoch_s: i64, tz: &DisplayTz) -> String {
    let shifted = epoch_s.saturating_add(i64::from(tz.offset_s));
    let utc_form = boss_engine_utils::iso8601::format_epoch_iso8601(shifted);
    format!("{}{}", utc_form.trim_end_matches('Z'), offset_suffix(tz.offset_s))
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
            format_tokens(m.input_tokens),
            format_tokens(m.output_tokens),
            format_tokens(m.cache_creation_tokens),
            format_tokens(m.cache_read_tokens),
        );
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
        "  USD estimate excludes these models (no pricing-table entry): {}",
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
            &format_tokens(row.measurement.total_tokens),
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
            &format_tokens(bucket.measurement.total_tokens),
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
            format_tokens(row.total_tokens),
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
    fn display_helpers_format_offsets_epochs_and_tokens() {
        assert_eq!(offset_suffix(-19_800), "-05:30");
        assert_eq!(offset_suffix(0), "+00:00");
        let tz = DisplayTz {
            offset_s: -3_600,
            label: "test".to_owned(),
        };
        assert_eq!(format_epoch(0, &tz), "1969-12-31T23:00:00-01:00");
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(1_000_000), "1,000,000");
        assert_eq!(format_tokens(-1234), "-1,234");
    }
}
