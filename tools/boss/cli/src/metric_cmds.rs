//! `boss metrics catalog|series` handlers: read-only performance series
//! over the primary tables. See `boss metrics --help`. `--json` renders
//! the wire shape verbatim.

use std::collections::BTreeMap;

use crate::*;
use boss_protocol::{MetricCatalog, MetricFilter, MetricSeriesReport, MetricValue};

pub(crate) async fn run_metric_command(command: MetricCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        MetricCommand::Catalog => {
            let response = client
                .send_request(&FrontendRequest::GetMetricCatalog)
                .await
                .map_err(CliError::internal)?;
            let catalog = match response {
                FrontendEvent::MetricCatalogResult { catalog } => catalog,
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    return Err(CliError::application(message));
                }
                other => return Err(unexpected_event("metrics catalog", &other)),
            };
            print_entity(ctx, &serde_json::json!({ "catalog": catalog }), || {
                print_catalog(&catalog)
            })
        }
        MetricCommand::Series(args) => {
            let (since_epoch_s, until_epoch_s) = resolve_window(&args.since, args.until.as_deref())?;
            let filters = parse_filters(&args.filters)?;
            let response = client
                .send_request(&FrontendRequest::GetMetricSeries {
                    series: args.series.clone(),
                    since_epoch_s,
                    until_epoch_s,
                    bucket: args.bucket.clone(),
                    filters,
                    group_by: args.group_by.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            let report = match response {
                FrontendEvent::MetricSeriesResult { report } => report,
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    return Err(CliError::application(message));
                }
                other => return Err(unexpected_event("metrics series", &other)),
            };
            let tz = resolve_display_tz(args.utc);
            print_entity(ctx, &serde_json::json!({ "report": report }), || {
                print_series_report(&report, &tz)
            })
        }
    }
}

fn parse_filters(raw: &[String]) -> Result<Vec<MetricFilter>, CliError> {
    let mut by_dim: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for item in raw {
        let Some((dim, values)) = item.split_once('=') else {
            return Err(CliError::usage(format!(
                "could not parse filter {item:?}; expected dim=value[,value...]"
            )));
        };
        let dim = dim.trim();
        if dim.is_empty() {
            return Err(CliError::usage(format!(
                "could not parse filter {item:?}; dimension name is empty"
            )));
        }
        for value in values.split(',') {
            let value = value.trim();
            if !value.is_empty() {
                by_dim.entry(dim.to_owned()).or_default().push(value.to_owned());
            }
        }
    }
    Ok(by_dim
        .into_iter()
        .map(|(dimension, values)| MetricFilter { dimension, values })
        .collect())
}

fn print_catalog(catalog: &MetricCatalog) {
    println!("Metric catalog (generated {})", catalog.generated_at_epoch_s);
    println!();
    let mut table = new_dynamic_table(vec!["ID", "TITLE", "KIND", "GROUP-BY", "DIMENSIONS", "PRESETS"]);
    for series in &catalog.series {
        let presets = series
            .presets
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
            .join(",");
        table.add_row(vec![
            series.id.as_str(),
            series.title.as_str(),
            series.value_kind.as_str(),
            series.default_group_by.as_deref().unwrap_or("—"),
            &series.dimensions.join(","),
            if presets.is_empty() { "—" } else { presets.as_str() },
        ]);
    }
    print_table(table);
    println!();
    println!("Dimensions (observed values):");
    for dim in &catalog.dimensions {
        if dim.values.is_empty() {
            println!("  {}: (none observed)", dim.id);
            continue;
        }
        let shown: Vec<String> = dim
            .values
            .iter()
            .take(12)
            .map(|v| format!("{} ({})", v.value, v.count))
            .collect();
        let extra = dim.values.len().saturating_sub(shown.len());
        let suffix = if extra > 0 {
            format!(" … +{extra} more")
        } else {
            String::new()
        };
        println!("  {}: {}{suffix}", dim.id, shown.join(", "));
    }
}

fn print_series_report(report: &MetricSeriesReport, tz: &DisplayTz) {
    println!(
        "{} ({}) from {} to {}  bucket={}s  groups={}",
        report.series,
        report.value_kind.as_str(),
        format_epoch(report.since_epoch_s, tz),
        format_epoch(report.until_epoch_s, tz),
        report.bucket_secs,
        report.groups.len(),
    );
    if let Some(from) = report.coverage.data_from_epoch_s {
        println!("  data from {}", format_epoch(from, tz));
    }
    if let Some(from) = report.coverage.dimension_from_epoch_s {
        println!("  group-by dimension from {}", format_epoch(from, tz));
    }
    for note in &report.coverage.notes {
        let instant = note
            .epoch_s
            .map(|e| format!(" @ {}", format_epoch(e, tz)))
            .unwrap_or_default();
        println!("  note [{}]{instant}: {}", note.kind.as_str(), note.detail);
    }
    if report.buckets.is_empty() {
        println!();
        println!("  No facts in this window.");
        return;
    }
    println!();
    let mut table = new_dynamic_table(vec!["BUCKET", "GROUP", "N", "VALUE"]);
    for bucket in &report.buckets {
        for cell in &bucket.cells {
            table.add_row(vec![
                format_epoch(bucket.start_epoch_s, tz),
                cell.group.clone(),
                cell.n.to_string(),
                format_metric_value(&cell.value),
            ]);
        }
    }
    print_table(table);
}

fn format_metric_value(value: &MetricValue) -> String {
    match value {
        MetricValue::Count { n } => n.to_string(),
        MetricValue::Duration {
            p50_ms,
            p90_ms,
            mean_ms,
            max_ms,
        } => format!("p50={p50_ms}ms p90={p90_ms}ms mean={mean_ms}ms max={max_ms}ms"),
        MetricValue::Points { points } => format!("{points} pts"),
        MetricValue::Tokens {
            input,
            output,
            cache_write,
            cache_read,
        } => format!("in={input} out={output} cw={cache_write} cr={cache_read}"),
        MetricValue::Usd {
            estimated,
            unpriceable_runs,
            partial,
        } => match estimated {
            Some(usd) => format!(
                "${usd:.2}{} unpriceable={unpriceable_runs}",
                if *partial { " (partial)" } else { "" }
            ),
            None => format!("unpriced unpriceable={unpriceable_runs}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_filters_splits_dim_and_or_values_and_ands_across_flags() {
        let filters = parse_filters(&[
            "status=failed,orphaned".into(),
            "driver=claude".into(),
            "status=abandoned".into(),
        ])
        .unwrap();
        assert_eq!(filters.len(), 2);
        let status = filters.iter().find(|f| f.dimension == "status").unwrap();
        assert_eq!(status.values, vec!["failed", "orphaned", "abandoned"]);
        let driver = filters.iter().find(|f| f.dimension == "driver").unwrap();
        assert_eq!(driver.values, vec!["claude"]);
    }

    #[test]
    fn parse_filters_rejects_missing_equals() {
        let err = parse_filters(&["status:failed".into()]).unwrap_err();
        assert!(err.to_string().contains("expected dim=value"));
    }

    #[test]
    fn format_metric_value_renders_each_kind() {
        assert_eq!(format_metric_value(&MetricValue::Count { n: 4 }), "4");
        assert_eq!(
            format_metric_value(&MetricValue::Duration {
                max_ms: 9,
                mean_ms: 4,
                p50_ms: 3,
                p90_ms: 8,
            }),
            "p50=3ms p90=8ms mean=4ms max=9ms"
        );
    }
}
