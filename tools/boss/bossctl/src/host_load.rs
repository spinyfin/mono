//! `bossctl metrics host` — the current host-load sample and the retained
//! series over a time window.
//!
//! Reads `state.db` directly (same resolution as `metrics list`), so it
//! works with the engine wedged. Storage is UTC epoch milliseconds; text
//! output shows local time, and `--json` keeps the epoch milliseconds so
//! it is unambiguous to a consumer in any timezone.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use boss_engine::host_load::{HostSample, format_local_ms};

/// Rows printed in text mode before the series is thinned. `--json`
/// always returns every sample.
const MAX_TEXT_ROWS: usize = 120;

/// Parse `90s`, `30m`, `6h`, `2d` (a bare number is minutes) into
/// milliseconds.
pub(crate) fn parse_window_ms(text: &str) -> Result<i64> {
    let text = text.trim();
    let (digits, unit_ms) = match text.chars().last() {
        Some('s') => (&text[..text.len() - 1], 1_000),
        Some('m') => (&text[..text.len() - 1], 60_000),
        Some('h') => (&text[..text.len() - 1], 3_600_000),
        Some('d') => (&text[..text.len() - 1], 86_400_000),
        Some(c) if c.is_ascii_digit() => (text, 60_000),
        _ => bail!("invalid window {text:?}: use a number plus s/m/h/d, e.g. 30m, 6h, 2d"),
    };
    let n: i64 = digits
        .parse()
        .with_context(|| format!("invalid window {text:?}: use a number plus s/m/h/d, e.g. 30m, 6h, 2d"))?;
    if n <= 0 {
        bail!("invalid window {text:?}: must be positive");
    }
    n.checked_mul(unit_ms)
        .with_context(|| format!("window {text:?} is too large"))
}

/// The machine-readable shape. Pure so its field names are pinned by a
/// test.
pub(crate) fn build_json(
    window: &str,
    since_ms: i64,
    until_ms: i64,
    current: Option<&HostSample>,
    samples: &[HostSample],
) -> serde_json::Value {
    serde_json::json!({
        "window": {
            "requested": window,
            "since_ms": since_ms,
            "until_ms": until_ms,
        },
        "current": current,
        "current_age_ms": current.map(|s| (until_ms - s.ts_ms).max(0)),
        "samples": samples,
    })
}

pub(crate) fn host_load(json: bool, state_root: Option<PathBuf>, window: &str) -> Result<()> {
    let window_ms = parse_window_ms(window)?;
    let db = crate::open_state_db(state_root)?;
    let until_ms = crate::now_epoch_ms() as i64;
    let since_ms = until_ms - window_ms;

    let current = db.host_load_latest().context("reading host_load_samples")?;
    let samples = db
        .host_load_samples_since(since_ms)
        .context("reading host_load_samples")?;

    if json {
        println!("{}", build_json(window, since_ms, until_ms, current.as_ref(), &samples));
        return Ok(());
    }

    let Some(current) = current else {
        println!("no host-load samples in state.db (the engine records one per minute once running)");
        return Ok(());
    };
    print_current(&current, until_ms);
    println!();
    print_series(window, &samples);
    Ok(())
}

fn opt<T>(value: Option<T>, render: impl FnOnce(T) -> String) -> String {
    value.map_or_else(|| "-".to_owned(), render)
}

fn gib(bytes: i64) -> String {
    format!("{:.1}G", bytes as f64 / (1u64 << 30) as f64)
}

fn print_current(s: &HostSample, now_ms: i64) {
    let age_s = ((now_ms - s.ts_ms) / 1000).max(0);
    println!("Host load — latest sample {} ({age_s}s ago)", format_local_ms(s.ts_ms));
    println!(
        "  load average   1m {}   5m {}   15m {}   ({} logical CPUs)",
        opt(s.load_avg_1m, |v| format!("{v:.2}")),
        opt(s.load_avg_5m, |v| format!("{v:.2}")),
        opt(s.load_avg_15m, |v| format!("{v:.2}")),
        opt(s.logical_cpus, |v| v.to_string()),
    );
    println!(
        "  cpu            busy {}   user {}   system {}   idle {}",
        opt(s.cpu_busy_pct, |v| format!("{v:.1}%")),
        opt(s.cpu_user_pct, |v| format!("{v:.1}%")),
        opt(s.cpu_system_pct, |v| format!("{v:.1}%")),
        opt(s.cpu_idle_pct, |v| format!("{v:.1}%")),
    );
    println!(
        "  memory         used {} of {}   compressor {}   free {}   available {}   pressure level {}",
        opt(s.mem_used_bytes, gib),
        opt(s.mem_total_bytes, gib),
        opt(s.mem_compressor_bytes, gib),
        opt(s.mem_free_bytes, gib),
        opt(s.mem_available_bytes, gib),
        opt(s.mem_pressure_level_pct, |v| format!("{v}%")),
    );
    println!(
        "  swap           used {} of {}",
        opt(s.swap_used_bytes, gib),
        opt(s.swap_total_bytes, gib),
    );
}

fn print_series(window: &str, samples: &[HostSample]) {
    if samples.is_empty() {
        println!("no samples in the last {window}");
        return;
    }
    let step = samples.len().div_ceil(MAX_TEXT_ROWS);
    println!(
        "Series, last {window} — {} sample(s), local time{}",
        samples.len(),
        if step > 1 {
            format!(" (showing every {step}th; use --json for all)")
        } else {
            String::new()
        },
    );
    println!(
        "{:<25} {:>7} {:>7} {:>7}  {:>6} {:>6} {:>6} {:>6}  {:>7} {:>7} {:>7} {:>5}  {:>7}",
        "TIME", "LOAD1", "LOAD5", "LOAD15", "BUSY%", "USER%", "SYS%", "IDLE%", "USED", "COMPR", "AVAIL", "LVL%", "SWAP",
    );
    for s in samples.iter().step_by(step) {
        println!(
            "{:<25} {:>7} {:>7} {:>7}  {:>6} {:>6} {:>6} {:>6}  {:>7} {:>7} {:>7} {:>5}  {:>7}",
            format_local_ms(s.ts_ms),
            opt(s.load_avg_1m, |v| format!("{v:.2}")),
            opt(s.load_avg_5m, |v| format!("{v:.2}")),
            opt(s.load_avg_15m, |v| format!("{v:.2}")),
            opt(s.cpu_busy_pct, |v| format!("{v:.1}")),
            opt(s.cpu_user_pct, |v| format!("{v:.1}")),
            opt(s.cpu_system_pct, |v| format!("{v:.1}")),
            opt(s.cpu_idle_pct, |v| format!("{v:.1}")),
            opt(s.mem_used_bytes, gib),
            opt(s.mem_compressor_bytes, gib),
            opt(s.mem_available_bytes, gib),
            opt(s.mem_pressure_level_pct, |v| v.to_string()),
            opt(s.swap_used_bytes, gib),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_parse_with_units_and_default_to_minutes() {
        assert_eq!(parse_window_ms("90s").unwrap(), 90_000);
        assert_eq!(parse_window_ms("30m").unwrap(), 1_800_000);
        assert_eq!(parse_window_ms("6h").unwrap(), 21_600_000);
        assert_eq!(parse_window_ms("2d").unwrap(), 172_800_000);
        assert_eq!(parse_window_ms("45").unwrap(), 2_700_000);
        for bad in ["", "h", "abc", "-5m", "0h", "1.5h", "5w"] {
            assert!(parse_window_ms(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn json_shape_has_window_current_age_and_every_sample_field() {
        let sample = HostSample {
            ts_ms: 90_000,
            logical_cpus: Some(12),
            load_avg_1m: Some(155.5),
            load_avg_5m: Some(170.0),
            load_avg_15m: Some(185.0),
            cpu_busy_pct: Some(100.0),
            cpu_user_pct: Some(80.0),
            cpu_system_pct: Some(20.0),
            cpu_idle_pct: Some(0.0),
            mem_total_bytes: Some(64),
            mem_used_bytes: Some(61),
            mem_compressor_bytes: Some(23),
            mem_free_bytes: Some(1),
            mem_available_bytes: Some(3),
            mem_pressure_level_pct: Some(4),
            swap_total_bytes: Some(12),
            swap_used_bytes: Some(10),
        };
        let first = HostSample {
            ts_ms: 30_000,
            ..HostSample::default()
        };
        let v = build_json("1h", 0, 100_000, Some(&sample), &[first, sample.clone()]);

        assert_eq!(
            v["window"],
            serde_json::json!({ "requested": "1h", "since_ms": 0, "until_ms": 100_000 })
        );
        assert_eq!(v["current_age_ms"], 10_000);
        assert_eq!(v["current"]["ts_ms"], 90_000);
        assert_eq!(v["samples"].as_array().unwrap().len(), 2);

        let mut keys: Vec<_> = v["current"].as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "cpu_busy_pct",
                "cpu_idle_pct",
                "cpu_system_pct",
                "cpu_user_pct",
                "load_avg_15m",
                "load_avg_1m",
                "load_avg_5m",
                "logical_cpus",
                "mem_available_bytes",
                "mem_compressor_bytes",
                "mem_free_bytes",
                "mem_pressure_level_pct",
                "mem_total_bytes",
                "mem_used_bytes",
                "swap_total_bytes",
                "swap_used_bytes",
                "ts_ms",
            ]
        );
        // Unmeasured fields serialize as null, not absent or zero.
        assert!(v["samples"][0]["cpu_busy_pct"].is_null());
    }

    #[test]
    fn json_with_no_samples_has_null_current() {
        let v = build_json("1h", 0, 100, None, &[]);
        assert!(v["current"].is_null());
        assert!(v["current_age_ms"].is_null());
        assert_eq!(v["samples"], serde_json::json!([]));
    }
}
