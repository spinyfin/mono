//! `bossctl dispatch stats` — aggregate dispatch wait times by defer
//! reason, plus the current top blocked items and a consolidated
//! per-pool queue summary. Read-only over
//! `dispatch-events/current.jsonl` via `dispatch_reader::compute_wait_stats`
//! — no engine RPC, no change to dispatch behavior.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use boss_engine::dispatch_reader::{self, DispatchWaitReport, PoolQueueSummary};
use serde::Serialize;

use super::{now_epoch_ms, resolve_state_root};

/// Window over which the "dispatch rate" line in the consolidated
/// summary counts completed dispatches.
const DISPATCH_RATE_WINDOW_MS: u128 = 15 * 60 * 1_000;

/// Combined `--json` shape: the existing wait-stats report plus the
/// consolidated per-pool queue summary and recent dispatch rate. Kept as
/// one struct (rather than two separate `println!`s) so `--json`
/// consumers get everything in a single parseable object.
#[derive(Debug, Serialize)]
struct DispatchStatsOutput {
    #[serde(flatten)]
    report: DispatchWaitReport,
    queue_summary: Vec<PoolQueueSummary>,
    dispatched_last_15m: usize,
}

/// Parse a `--since` value into an absolute epoch-ms cutoff. Accepts a
/// bare non-negative integer with a `s`/`m`/`h`/`d` suffix (e.g.
/// `30m`, `6h`, `2d`), measured back from `now_ms`.
fn parse_since(value: &str, now_ms: u128) -> Result<u128> {
    let value = value.trim();
    let (digits, unit_ms) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1_000u128),
        Some('m') => (&value[..value.len() - 1], 60_000u128),
        Some('h') => (&value[..value.len() - 1], 3_600_000u128),
        Some('d') => (&value[..value.len() - 1], 86_400_000u128),
        _ => bail!("invalid --since `{value}`: expected a number followed by s/m/h/d, e.g. `30m`"),
    };
    let count: u128 = digits
        .parse()
        .with_context(|| format!("invalid --since `{value}`: expected a number followed by s/m/h/d"))?;
    Ok(now_ms.saturating_sub(count.saturating_mul(unit_ms)))
}

pub(crate) fn dispatch_stats(json: bool, state_root: Option<PathBuf>, since: Option<&str>, top: usize) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let now = now_epoch_ms();
    let since_ms = since.map(|s| parse_since(s, now)).transpose()?;
    let events = dispatch_reader::read_current(&root)?;
    let report = dispatch_reader::compute_wait_stats(&events, now, since_ms);
    let queue_summary = dispatch_reader::summarize_queue_by_pool(&report.blocked_now);
    let dispatched_last_15m = dispatch_reader::dispatches_in_window(&events, now, DISPATCH_RATE_WINDOW_MS);

    if json {
        let output = DispatchStatsOutput {
            report,
            queue_summary,
            dispatched_last_15m,
        };
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    println!("queue summary (per pool):");
    if queue_summary.is_empty() {
        println!("  nothing queued");
    } else {
        println!("  {:<12} {:>7} {:>16}", "pool", "queued", "oldest_waiting");
        for pool in &queue_summary {
            println!(
                "  {:<12} {:>7} {:>16}",
                pool.pool,
                pool.queued,
                format_ms(pool.oldest_wait_ms),
            );
        }
    }
    println!(
        "dispatch rate (last 15m): {dispatched_last_15m} dispatched (~{:.2}/min)",
        dispatched_last_15m as f64 / 15.0,
    );
    println!();

    if report.by_reason.is_empty() {
        println!("no resolved dispatch waits recorded");
    } else {
        println!("dispatch wait by reason:");
        println!(
            "  {:<24} {:>6} {:>10} {:>10} {:>10}",
            "reason", "count", "p50", "p95", "max"
        );
        for bucket in &report.by_reason {
            println!(
                "  {:<24} {:>6} {:>10} {:>10} {:>10}",
                bucket.reason,
                bucket.count,
                format_ms(bucket.p50_ms),
                format_ms(bucket.p95_ms),
                format_ms(bucket.max_ms),
            );
        }
    }

    println!();
    if report.blocked_now.is_empty() {
        println!("no currently-blocked executions");
    } else {
        println!("top blocked now:");
        for entry in report.blocked_now.iter().take(top) {
            let work_item = entry.work_item_id.as_deref().unwrap_or("-");
            println!(
                "  {}  work_item={}  pool={}  reason={}  waiting={}",
                entry.execution_id,
                work_item,
                entry.pool,
                entry.reason,
                format_ms(entry.wait_so_far_ms),
            );
        }
    }
    Ok(())
}

fn format_ms(ms: u128) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        format!("{:.1}m", ms as f64 / 60_000.0)
    } else {
        format!("{:.1}h", ms as f64 / 3_600_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u128 = 1_700_000_000_000;

    #[test]
    fn parse_since_handles_each_unit_suffix() {
        assert_eq!(parse_since("45s", NOW).unwrap(), NOW - 45 * 1_000);
        assert_eq!(parse_since("30m", NOW).unwrap(), NOW - 30 * 60_000);
        assert_eq!(parse_since("6h", NOW).unwrap(), NOW - 6 * 3_600_000);
        assert_eq!(parse_since("2d", NOW).unwrap(), NOW - 2 * 86_400_000);
    }

    #[test]
    fn parse_since_tolerates_surrounding_whitespace() {
        assert_eq!(parse_since("  30m\n", NOW).unwrap(), NOW - 30 * 60_000);
    }

    #[test]
    fn parse_since_accepts_zero() {
        assert_eq!(parse_since("0h", NOW).unwrap(), NOW);
    }

    #[test]
    fn parse_since_rejects_bare_number_without_suffix() {
        assert!(parse_since("30", NOW).is_err());
    }

    #[test]
    fn parse_since_rejects_unknown_suffix() {
        assert!(parse_since("30x", NOW).is_err());
    }

    #[test]
    fn parse_since_rejects_missing_or_non_numeric_magnitude() {
        assert!(parse_since("m", NOW).is_err());
        assert!(parse_since("abcm", NOW).is_err());
        assert!(parse_since("-5m", NOW).is_err());
        assert!(parse_since("", NOW).is_err());
    }

    #[test]
    fn parse_since_saturates_at_zero_when_older_than_now() {
        assert_eq!(parse_since("2d", 1_000).unwrap(), 0);
    }

    #[test]
    fn parse_since_saturates_instead_of_overflowing_on_large_magnitude() {
        let huge = format!("{}d", u128::MAX);
        assert_eq!(parse_since(&huge, NOW).unwrap(), 0);
    }

    #[test]
    fn format_ms_renders_sub_second_as_whole_millis() {
        assert_eq!(format_ms(0), "0ms");
        assert_eq!(format_ms(1), "1ms");
        assert_eq!(format_ms(999), "999ms");
    }

    #[test]
    fn format_ms_switches_to_seconds_at_one_second() {
        assert_eq!(format_ms(1_000), "1.0s");
        assert_eq!(format_ms(1_500), "1.5s");
        assert_eq!(format_ms(59_999), "60.0s");
    }

    #[test]
    fn format_ms_switches_to_minutes_at_one_minute() {
        assert_eq!(format_ms(60_000), "1.0m");
        assert_eq!(format_ms(90_000), "1.5m");
        assert_eq!(format_ms(3_599_999), "60.0m");
    }

    #[test]
    fn format_ms_switches_to_hours_at_one_hour() {
        assert_eq!(format_ms(3_600_000), "1.0h");
        assert_eq!(format_ms(5_400_000), "1.5h");
    }
}
