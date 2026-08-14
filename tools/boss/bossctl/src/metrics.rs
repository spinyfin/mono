//! `bossctl metrics` command handlers.
//!
//! Split out of `main.rs` (which had reached the repo's 3000-line file
//! cap) as a self-contained group: everything here renders the engine's
//! counter/gauge metrics or its GitHub API attribution, reading either
//! `state.db` directly — so the verbs work while the engine is wedged,
//! same as `hosts` / `work executions` — or the live in-memory registry
//! over the socket for `metrics show --live`. Pure structural move; no
//! behavioural change.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use boss_engine::work::WorkDb;
use boss_protocol::{FrontendEvent, FrontendRequest, MetricLiveEntry};

use crate::{connect, format_age_ms, now_epoch_ms, resolve_db_path};

/// A unified metric row for rendering, covering both counters and
/// gauges loaded from `state.db`.
///
/// On the builder per the repo's >5-field convention; `stale` defaults to
/// `false` because a row read back from `state.db` was written by the
/// engine's own flush and carries no staleness of its own (only the live
/// registry, which has its own type, can report one).
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct MetricRow {
    name: String,
    description: String,
    kind: &'static str,
    value: i64,
    timestamp_ms: i64,
    #[builder(default)]
    stale: bool,
}

fn load_metric_rows(db_path: PathBuf, prefix: Option<&str>) -> Result<Vec<MetricRow>> {
    let db = WorkDb::open(db_path).context("opening state.db")?;
    let (counters, gauges) = db.metrics_load_all().context("reading metrics from state.db")?;

    let mut rows: Vec<MetricRow> = counters
        .into_iter()
        .map(|c| {
            MetricRow::builder()
                .name(c.name)
                .description(c.description)
                .kind("counter")
                .value(c.value as i64)
                .timestamp_ms(c.updated_at_ms)
                .build()
        })
        .chain(gauges.into_iter().map(|g| {
            MetricRow::builder()
                .name(g.name)
                .description(g.description)
                .kind("gauge")
                .value(g.value)
                .timestamp_ms(g.observed_at_ms)
                .build()
        }))
        .collect();

    if let Some(p) = prefix {
        rows.retain(|r| r.name.starts_with(p));
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

fn print_metric_row_short(row: &MetricRow, now_ms: u128, name_width: usize) {
    let age = format_age_ms(row.timestamp_ms, now_ms);
    let stale_tag = if row.stale { " [stale]" } else { "" };
    println!(
        "{:<width$}  {:>12}  {:>10}  {}{}",
        row.name,
        row.value,
        age,
        row.kind,
        stale_tag,
        width = name_width,
    );
}

pub(crate) fn metrics_list(json: bool, state_root: Option<PathBuf>, prefix: Option<&str>) -> Result<()> {
    let db_path = resolve_db_path(state_root)?;
    let rows = load_metric_rows(db_path, prefix)?;
    let now = now_epoch_ms();

    if json {
        let entries: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "description": r.description,
                    "kind": r.kind,
                    "value": r.value,
                    "timestamp_ms": r.timestamp_ms,
                    "stale": r.stale,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "metrics": entries }));
    } else if rows.is_empty() {
        println!("no metrics in state.db (engine may not have flushed yet)");
    } else {
        let name_width = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
        for row in &rows {
            print_metric_row_short(row, now, name_width);
        }
    }
    Ok(())
}

pub(crate) fn metrics_show(json: bool, state_root: Option<PathBuf>, name: &str) -> Result<()> {
    let db_path = resolve_db_path(state_root)?;
    let rows = load_metric_rows(db_path, None)?;
    let now = now_epoch_ms();

    let row = rows.iter().find(|r| r.name == name);
    match row {
        None => {
            if json {
                println!("{}", serde_json::json!({ "entry": null, "name": name }));
            } else {
                println!("metric not found: {name}");
                println!("  (engine may not have flushed yet; try --live to read in-memory value)");
            }
        }
        Some(r) => {
            let age = format_age_ms(r.timestamp_ms, now);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "entry": {
                            "name": r.name,
                            "description": r.description,
                            "kind": r.kind,
                            "value": r.value,
                            "timestamp_ms": r.timestamp_ms,
                            "stale": r.stale,
                        }
                    })
                );
            } else {
                let stale_tag = if r.stale {
                    "  [stale: not registered by current engine]"
                } else {
                    ""
                };
                println!("{}{}", r.name, stale_tag);
                println!("  description:   {}", r.description);
                println!("  kind:          {}", r.kind);
                println!("  value:         {}", r.value);
                println!("  last_updated:  {age}");
            }
        }
    }
    Ok(())
}

pub(crate) async fn metrics_show_live(socket_path: &Option<String>, json: bool, name: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::MetricsShowLive { name: name.clone() })
        .await
        .context("sending MetricsShowLive")?;
    match response {
        FrontendEvent::MetricsShowLiveResult { entry } => {
            print_metric_live_entry(json, &name, entry.as_ref());
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected metrics show --live: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

fn print_metric_live_entry(json: bool, name: &str, entry: Option<&MetricLiveEntry>) {
    let now = now_epoch_ms();
    match entry {
        None => {
            if json {
                println!("{}", serde_json::json!({ "entry": null, "name": name }));
            } else {
                println!("metric not found: {name}");
                println!("  (not registered in the current engine binary)");
            }
        }
        Some(e) => {
            let age = format_age_ms(e.timestamp_ms, now);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "entry": {
                            "name": e.name,
                            "description": e.description,
                            "kind": e.kind,
                            "value": e.value,
                            "timestamp_ms": e.timestamp_ms,
                            "stale": e.stale,
                            "source": "live",
                        }
                    })
                );
            } else {
                let stale_tag = if e.stale {
                    "  [stale: not registered by current engine]"
                } else {
                    ""
                };
                println!("{}{}", e.name, stale_tag);
                println!("  description:   {}", e.description);
                println!("  kind:          {}  (live — read from in-memory atomic)", e.kind);
                println!("  value:         {}", e.value);
                println!("  last_updated:  {age}");
            }
        }
    }
}

/// GitHub's hourly quota for a personal token, on each of the two
/// independently-metered buckets. Rates are printed against this so the
/// table answers "over or under budget" without the reader doing the
/// division.
pub(crate) const GITHUB_HOURLY_BUDGET: f64 = 5000.0;

/// Convert a spend and an observed span into a points-per-hour rate.
///
/// Divides by the span the data *actually* covers, not by the requested
/// look-back: an engine that has been running the instrumented build for
/// 20 minutes, queried with `--hours 24`, would otherwise report a rate
/// ~70x under the truth. A span shorter than a minute is not a usable
/// denominator (one burst would extrapolate to an absurd rate), so it
/// reports `None` rather than a confident wrong number.
pub(crate) fn points_per_hour(points: i64, span_ms: i64) -> Option<f64> {
    if span_ms < 60_000 {
        return None;
    }
    Some(points as f64 * 3_600_000.0 / span_ms as f64)
}

/// Per-subsystem GitHub API attribution over a time window.
pub(crate) fn metrics_github(json: bool, state_root: Option<PathBuf>, hours: u32) -> Result<()> {
    let db_path = resolve_db_path(state_root)?;
    let db = WorkDb::open(db_path).context("opening state.db")?;
    let since_ms = now_epoch_ms() as i64 - (hours as i64) * 3_600_000;

    let buckets = db
        .github_api_usage_by_caller(since_ms)
        .context("reading github_api_calls from state.db")?;
    let window = db
        .github_api_usage_window(since_ms)
        .context("reading github_api_calls window from state.db")?;
    // A single sample spans zero time; treat the observed span as at
    // least the gap between first and last row.
    let span_ms = window.map(|(min, max)| max - min).unwrap_or(0);

    if json {
        let entries: Vec<serde_json::Value> = buckets
            .iter()
            .map(|b| {
                serde_json::json!({
                    "caller": b.caller,
                    "api": b.api,
                    "calls": b.calls,
                    "points": b.points,
                    "calls_without_reading": b.calls_without_reading,
                    "errors": b.errors,
                    "rate_limited": b.rate_limited,
                    "points_per_hour": points_per_hour(b.points, span_ms),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "since_epoch_ms": since_ms,
                "observed_span_ms": span_ms,
                "hourly_budget": GITHUB_HOURLY_BUDGET,
                "usage": entries,
            })
        );
        return Ok(());
    }

    if buckets.is_empty() {
        println!("no github_api_calls rows in the last {hours}h (engine may not have made a call yet)");
        return Ok(());
    }

    let span_hours = span_ms as f64 / 3_600_000.0;
    println!("GitHub API usage — {span_hours:.2}h observed (requested {hours}h look-back)");
    println!();
    let name_width = buckets.iter().map(|b| b.caller.len()).max().unwrap_or(10).max(10);
    println!(
        "{:<width$}  {:>8}  {:>8}  {:>9}  {:>11}  {:>7}  {:>7}",
        "SUBSYSTEM",
        "API",
        "CALLS",
        "UNITS",
        "UNITS/HR",
        "ERRORS",
        "429s",
        width = name_width,
    );
    for b in &buckets {
        let rate = match points_per_hour(b.points, span_ms) {
            Some(rate) => format!("{rate:.0}"),
            None => "—".to_owned(),
        };
        println!(
            "{:<width$}  {:>8}  {:>8}  {:>9}  {:>11}  {:>7}  {:>7}",
            b.caller,
            b.api,
            b.calls,
            b.points,
            rate,
            b.errors,
            b.rate_limited,
            width = name_width,
        );
    }

    // Per-bucket totals: GraphQL and REST have separate 5000/hour
    // budgets, so a combined total would compare against a limit that
    // does not exist.
    println!();
    for api in ["graphql", "rest", "cli"] {
        let points: i64 = buckets.iter().filter(|b| b.api == api).map(|b| b.points).sum();
        let calls: i64 = buckets.iter().filter(|b| b.api == api).map(|b| b.calls).sum();
        if calls == 0 {
            continue;
        }
        match points_per_hour(points, span_ms) {
            Some(rate) => {
                let pct = rate / GITHUB_HOURLY_BUDGET * 100.0;
                let verdict = if rate > GITHUB_HOURLY_BUDGET {
                    "OVER BUDGET"
                } else {
                    "within budget"
                };
                println!("{api:>8}: {calls} calls, {points} units → {rate:.0}/hr ({pct:.0}% of 5000/hr) — {verdict}");
            }
            None => println!("{api:>8}: {calls} calls, {points} units (window too short for a rate)"),
        }
    }

    let unmeasured: i64 = buckets.iter().map(|b| b.calls_without_reading).sum();
    if unmeasured > 0 {
        println!();
        println!(
            "note: {unmeasured} call(s) carried no rateLimit reading — their spend is counted \
             as calls but contributes 0 units, so the rates above are a lower bound.",
        );
    }
    Ok(())
}

pub(crate) async fn metrics_reset(socket_path: &Option<String>, json: bool, name: Option<String>) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::MetricsReset { name: name.clone() })
        .await
        .context("sending MetricsReset")?;
    match response {
        FrontendEvent::MetricsResetDone {
            name: returned_name,
            counters_reset,
            gauges_reset,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "reset",
                        "name": returned_name,
                        "counters_reset": counters_reset,
                        "gauges_reset": gauges_reset,
                    })
                );
            } else {
                match &name {
                    Some(n) => {
                        if counters_reset == 0 && gauges_reset == 0 {
                            println!("metric not found: {n}");
                        } else {
                            println!("reset {n} ({} counter(s), {} gauge(s))", counters_reset, gauges_reset);
                        }
                    }
                    None => {
                        println!(
                            "reset all metrics ({} counter(s), {} gauge(s))",
                            counters_reset, gauges_reset
                        );
                    }
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected metrics reset: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}
