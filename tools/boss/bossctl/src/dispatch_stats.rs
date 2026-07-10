//! `bossctl dispatch stats` — aggregate dispatch wait times by defer
//! reason, plus the current top blocked items. Read-only over
//! `dispatch-events/current.jsonl` via `dispatch_reader::compute_wait_stats`
//! — no engine RPC, no change to dispatch behavior.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use boss_engine::dispatch_reader;

use super::{now_epoch_ms, resolve_state_root};

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

    if json {
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }

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
                "  {}  work_item={}  reason={}  waiting={}",
                entry.execution_id,
                work_item,
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
