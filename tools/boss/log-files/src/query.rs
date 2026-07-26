//! Structured log query: time windows, field filters, and capped tails
//! with explicit truncation reporting.
//!
//! Scans a chronological list of files (live + rotated / day-dated) as one
//! logical stream. Matching is field-aware for JSONL records so a filter on
//! `target` does not false-positive on the same string appearing in a message
//! body. Non-JSON lines still support raw `--grep` only.
//!
//! Results are always the **last** `limit` matches (tail semantics). When more
//! matches exist than the limit, [`QueryResult::truncated`] is set so the
//! caller can surface that fact — silent truncation is the bug this module
//! exists to prevent.

use std::collections::VecDeque;
use std::io::BufRead;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Filters applied to each log line while scanning.
///
/// Builder-equipped: more than five fields, and several are optional so
/// call sites stay additive (see repo builder-pattern convention).
#[derive(Debug, Clone, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct LogFilter {
    /// Raw case-sensitive substring match against the whole line.
    pub grep: Option<String>,
    /// Match the top-level JSON `target` field exactly, or as a module
    /// prefix (`boss_engine::app` matches `boss_engine::app::server`).
    pub target: Option<String>,
    /// Match the top-level JSON `level` field case-insensitively
    /// (`info` matches `INFO`).
    pub level: Option<String>,
    /// Match structured fields: top-level keys first, then inside a
    /// nested `fields` object (tracing-json layout). Each entry is
    /// `(key, expected_value)` with string equality after JSON
    /// stringification of non-string values. All entries are ANDed.
    #[builder(default)]
    pub fields: Vec<(String, String)>,
    /// Match records whose `execution_id` **or** `run_id` equals this
    /// value (top-level or under `fields`). Spawn diagnostics use
    /// `run_id`; dispatch / trace use `execution_id`.
    pub execution_or_run_id: Option<String>,
    /// Inclusive lower bound on the record timestamp, epoch milliseconds.
    pub since_ms: Option<u128>,
    /// Inclusive upper bound on the record timestamp, epoch milliseconds.
    pub until_ms: Option<u128>,
}

impl LogFilter {
    /// True when no structured / time / grep constraints are set.
    pub fn is_empty(&self) -> bool {
        self.grep.is_none()
            && self.target.is_none()
            && self.level.is_none()
            && self.fields.is_empty()
            && self.execution_or_run_id.is_none()
            && self.since_ms.is_none()
            && self.until_ms.is_none()
    }
}

/// Outcome of a capped query over one or more log files.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// Matching lines in chronological order, at most `limit` long.
    pub lines: Vec<String>,
    /// Total number of lines that matched filters (before the tail cap).
    pub matched_total: usize,
    /// True when `matched_total > lines.len()` — the result is a partial view.
    pub truncated: bool,
    /// Files that were opened during the scan (missing files are skipped
    /// silently and do not appear here).
    pub paths_scanned: Vec<PathBuf>,
}

/// Scan `paths` (oldest → newest) applying `filter`, returning the last
/// `limit` matching lines. `limit == 0` means unlimited (return every match).
///
/// Missing files are ignored. Partial / non-UTF8 lines are dropped the same
/// way as [`crate::reader::read_file_lines`].
pub fn query_log_files(paths: &[PathBuf], filter: &LogFilter, limit: usize) -> Result<QueryResult> {
    let mut ring: VecDeque<String> = VecDeque::new();
    let mut matched_total: usize = 0;
    let mut paths_scanned: Vec<PathBuf> = Vec::new();

    for path in paths {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        paths_scanned.push(path.clone());
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().map_while(std::io::Result::ok) {
            if line.is_empty() {
                continue;
            }
            if !line_matches(&line, filter) {
                continue;
            }
            matched_total += 1;
            if limit == 0 {
                ring.push_back(line);
            } else {
                if ring.len() == limit {
                    ring.pop_front();
                }
                ring.push_back(line);
            }
        }
    }

    let lines: Vec<String> = ring.into_iter().collect();
    let truncated = limit > 0 && matched_total > lines.len();
    Ok(QueryResult {
        lines,
        matched_total,
        truncated,
        paths_scanned,
    })
}

/// Does `line` pass every active constraint in `filter`?
pub fn line_matches(line: &str, filter: &LogFilter) -> bool {
    if filter.is_empty() {
        return true;
    }

    // Cheap raw grep first when it is the only constraint.
    if filter.grep.is_some()
        && filter.target.is_none()
        && filter.level.is_none()
        && filter.fields.is_empty()
        && filter.execution_or_run_id.is_none()
        && filter.since_ms.is_none()
        && filter.until_ms.is_none()
    {
        return filter.grep.as_deref().is_none_or(|g| line.contains(g));
    }

    let parsed: Option<Value> = serde_json::from_str(line).ok();

    if filter.since_ms.is_some() || filter.until_ms.is_some() {
        let Some(ts_ms) = parsed.as_ref().and_then(record_timestamp_ms) else {
            // Undated lines cannot be placed in a time window.
            return false;
        };
        if filter.since_ms.is_some_and(|s| ts_ms < s) {
            return false;
        }
        if filter.until_ms.is_some_and(|u| ts_ms > u) {
            return false;
        }
    }

    if let Some(want_level) = filter.level.as_deref() {
        let Some(v) = parsed.as_ref() else {
            return false;
        };
        let got = v.get("level").and_then(|x| x.as_str()).unwrap_or("");
        if !got.eq_ignore_ascii_case(want_level) {
            return false;
        }
    }

    if let Some(want_target) = filter.target.as_deref() {
        let Some(v) = parsed.as_ref() else {
            return false;
        };
        let got = v.get("target").and_then(|x| x.as_str()).unwrap_or("");
        if !target_matches(got, want_target) {
            return false;
        }
    }

    if !filter.fields.is_empty() {
        let Some(v) = parsed.as_ref() else {
            return false;
        };
        for (key, want) in &filter.fields {
            if !field_value_matches(v, key, want) {
                return false;
            }
        }
    }

    if let Some(id) = filter.execution_or_run_id.as_deref() {
        let Some(v) = parsed.as_ref() else {
            return false;
        };
        let hit = field_value_matches(v, "execution_id", id) || field_value_matches(v, "run_id", id);
        if !hit {
            return false;
        }
    }

    if let Some(g) = filter.grep.as_deref()
        && !line.contains(g)
    {
        return false;
    }

    true
}

/// Hierarchical / exact match on a tracing `target` field.
fn target_matches(got: &str, want: &str) -> bool {
    if got == want {
        return true;
    }
    // Module-prefix: `boss_engine::app` matches `boss_engine::app::server`.
    got.starts_with(want) && got.as_bytes().get(want.len()) == Some(&b':')
}

/// Look up `key` at the top level, then under `fields`, and compare to `want`.
fn field_value_matches(v: &Value, key: &str, want: &str) -> bool {
    if let Some(val) = v.get(key)
        && json_value_eq(val, want)
    {
        return true;
    }
    if let Some(fields) = v.get("fields")
        && let Some(val) = fields.get(key)
        && json_value_eq(val, want)
    {
        return true;
    }
    false
}

fn json_value_eq(val: &Value, want: &str) -> bool {
    match val {
        Value::String(s) => s == want,
        Value::Number(n) => number_eq(n, want),
        Value::Bool(b) => match want {
            "true" => *b,
            "false" => !*b,
            _ => false,
        },
        Value::Null => want == "null",
        // Objects / arrays are not useful field-filter targets.
        Value::Array(_) | Value::Object(_) => false,
    }
}

fn number_eq(n: &serde_json::Number, want: &str) -> bool {
    if let Ok(w) = want.parse::<u64>() {
        return n.as_u64() == Some(w);
    }
    if let Ok(w) = want.parse::<i64>() {
        return n.as_i64() == Some(w);
    }
    if let Ok(w) = want.parse::<f64>() {
        return n.as_f64().is_some_and(|g| (g - w).abs() < f64::EPSILON);
    }
    false
}

/// Extract a record timestamp as epoch milliseconds from common Boss log
/// shapes: `ts_epoch_ms`, `ts_epoch_s`, `timestamp` (RFC3339), `ts` (RFC3339).
pub fn record_timestamp_ms(v: &Value) -> Option<u128> {
    if let Some(n) = v.get("ts_epoch_ms") {
        return json_number_u128(n);
    }
    if let Some(n) = v.get("ts_epoch_s") {
        return json_number_u128(n).map(|s| s.saturating_mul(1000));
    }
    if let Some(s) = v.get("timestamp").and_then(|x| x.as_str()) {
        return parse_rfc3339_to_epoch_ms(s);
    }
    if let Some(s) = v.get("ts").and_then(|x| x.as_str()) {
        return parse_rfc3339_to_epoch_ms(s);
    }
    None
}

fn json_number_u128(v: &Value) -> Option<u128> {
    match v {
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Some(u as u128)
            } else if let Some(i) = n.as_i64() {
                if i >= 0 { Some(i as u128) } else { None }
            } else {
                // JSON numbers that don't fit u64 (very large) — try string form.
                n.to_string().parse().ok()
            }
        }
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Parse a `--since` / `--until` value into epoch milliseconds.
///
/// Accepts:
/// - relative offsets: `30s`, `15m`, `6h`, `2d` (measured back/forward from `now_ms`)
/// - absolute RFC3339: `2026-07-26T06:20:00Z`, `2026-07-26T06:20:00+00:00`
/// - absolute date `YYYY-MM-DD`:
///   - for `--since` (`date_end_of_day = false`): UTC start of that day (`T00:00:00Z`)
///   - for `--until` (`date_end_of_day = true`): inclusive end of that UTC day
///     (exclusive next midnight − 1 ms), so the whole calendar day is included
/// - bare integer epoch: seconds if `< 10^12`, else milliseconds
///
/// For relative forms, pass the intended direction via `relative_is_past`
/// (true = subtract from `now_ms`).
pub fn parse_time_spec(value: &str, now_ms: u128, relative_is_past: bool, date_end_of_day: bool) -> Result<u128> {
    let value = value.trim();
    if value.is_empty() {
        bail!("empty time value");
    }

    // Relative: number + unit suffix.
    if let Some(last) = value.chars().last()
        && matches!(last, 's' | 'm' | 'h' | 'd')
        && value.len() >= 2
        && value[..value.len() - 1].chars().all(|c| c.is_ascii_digit())
    {
        let digits = &value[..value.len() - 1];
        let count: u128 = digits
            .parse()
            .with_context(|| format!("invalid relative time `{value}`"))?;
        let unit_ms: u128 = match last {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            _ => unreachable!(),
        };
        let delta = count.saturating_mul(unit_ms);
        return Ok(if relative_is_past {
            now_ms.saturating_sub(delta)
        } else {
            now_ms.saturating_add(delta)
        });
    }

    // Bare integer epoch.
    if value.chars().all(|c| c.is_ascii_digit()) {
        let n: u128 = value.parse().with_context(|| format!("invalid epoch time `{value}`"))?;
        // Heuristic: values below 10^12 are seconds; above are milliseconds.
        // (10^12 ms ≈ 2001-09 in seconds would be wrong; 10^12 seconds is year ~33658.)
        return Ok(if n < 1_000_000_000_000 {
            n.saturating_mul(1000)
        } else {
            n
        });
    }

    // Date-only YYYY-MM-DD.
    if value.len() == 10 && value.as_bytes().get(4) == Some(&b'-') && value.as_bytes().get(7) == Some(&b'-') {
        let start = parse_rfc3339_to_epoch_ms(&format!("{value}T00:00:00Z"))
            .with_context(|| format!("invalid date `{value}`"))?;
        if date_end_of_day {
            // Inclusive end-of-day == exclusive next midnight − 1 ms (UTC days are 86_400_000 ms).
            return Ok(start.saturating_add(86_400_000).saturating_sub(1));
        }
        return Ok(start);
    }

    // RFC3339 / ISO-8601.
    parse_rfc3339_to_epoch_ms(value).with_context(|| {
        format!("invalid time `{value}`: expected relative (30m/6h/2d), RFC3339, YYYY-MM-DD, or epoch integer")
    })
}

/// Wall-clock now as epoch milliseconds.
pub fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Parse an RFC3339 timestamp to epoch ms without pulling in a full chrono
/// feature set. Accepts fractional seconds and `Z` / numeric offsets.
fn parse_rfc3339_to_epoch_ms(s: &str) -> Option<u128> {
    // chrono's parse_from_rfc3339 handles the full grammar; it is available
    // with the workspace `std`-only chrono feature.
    let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    let secs = dt.timestamp();
    if secs < 0 {
        return None;
    }
    let ms = (secs as u128)
        .saturating_mul(1000)
        .saturating_add((dt.timestamp_subsec_millis()) as u128);
    Some(ms)
}

/// Human-readable truncation notice for stderr. `limit` is the cap that was
/// applied (`0` means unlimited — this returns `None`).
pub fn truncation_notice(result: &QueryResult, limit: usize) -> Option<String> {
    if !result.truncated || limit == 0 {
        return None;
    }
    Some(format!(
        "bossctl: showing last {shown} of {total} matching lines (result truncated; raise --tail / -n, or narrow with --since/--until / field filters)",
        shown = result.lines.len(),
        total = result.matched_total,
    ))
}

/// Whether a line matches after applying the same filters used by follow mode
/// (no time window — live lines are always "now").
pub fn follow_line_matches(line: &str, filter: &LogFilter) -> bool {
    let mut live = filter.clone();
    live.since_ms = None;
    live.until_ms = None;
    line_matches(line, &live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut f = std::fs::File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn query_respects_tail_limit_and_reports_truncation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.jsonl");
        write_lines(&path, &["l0", "l1", "l2", "l3", "l4"]);
        let result = query_log_files(&[path], &LogFilter::default(), 2).unwrap();
        assert_eq!(result.lines, vec!["l3", "l4"]);
        assert_eq!(result.matched_total, 5);
        assert!(result.truncated);
        assert!(truncation_notice(&result, 2).unwrap().contains("last 2 of 5"));
    }

    #[test]
    fn query_unlimited_when_limit_zero() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.jsonl");
        write_lines(&path, &["a", "b", "c"]);
        let result = query_log_files(&[path], &LogFilter::default(), 0).unwrap();
        assert_eq!(result.lines, vec!["a", "b", "c"]);
        assert!(!result.truncated);
        assert!(truncation_notice(&result, 0).is_none());
    }

    #[test]
    fn target_filter_does_not_match_message_body() {
        // The incident failure mode: grepping a module name matched unrelated
        // "events socket: hook event received" records. Field filter must not.
        let body_hit = r#"{"timestamp":"2026-07-26T00:00:00Z","level":"INFO","fields":{"message":"events socket: hook event received","target_hint":"boss_engine::events"},"target":"boss_engine::events_socket"}"#;
        let real = r#"{"timestamp":"2026-07-26T00:00:01Z","level":"INFO","fields":{"message":"dispatch ok"},"target":"boss_engine::dispatch"}"#;
        let filter = LogFilter {
            target: Some("boss_engine::dispatch".into()),
            ..Default::default()
        };
        assert!(!line_matches(body_hit, &filter));
        assert!(line_matches(real, &filter));
    }

    #[test]
    fn target_filter_matches_module_prefix() {
        let line = r#"{"timestamp":"2026-07-26T00:00:00Z","level":"INFO","fields":{"message":"x"},"target":"boss_engine::app::server"}"#;
        let filter = LogFilter {
            target: Some("boss_engine::app".into()),
            ..Default::default()
        };
        assert!(line_matches(line, &filter));
        let other = LogFilter {
            target: Some("boss_engine::ap".into()),
            ..Default::default()
        };
        // Prefix must end on a `::` boundary — `ap` is not a module prefix of `app`.
        assert!(!line_matches(line, &other));
    }

    #[test]
    fn level_filter_is_case_insensitive() {
        let line = r#"{"timestamp":"2026-07-26T00:00:00Z","level":"ERROR","fields":{"message":"x"},"target":"t"}"#;
        let filter = LogFilter {
            level: Some("error".into()),
            ..Default::default()
        };
        assert!(line_matches(line, &filter));
        let no = LogFilter {
            level: Some("warn".into()),
            ..Default::default()
        };
        assert!(!line_matches(line, &no));
    }

    #[test]
    fn field_filter_checks_top_level_and_fields_object() {
        let trace = r#"{"timestamp":"2026-07-26T00:00:00Z","level":"INFO","fields":{"message":"x","execution_id":"exec_abc"},"target":"t"}"#;
        let dispatch = r#"{"ts_epoch_ms":1,"stage":"pane_spawned","execution_id":"exec_abc"}"#;
        let other = r#"{"ts_epoch_ms":1,"stage":"pane_spawned","execution_id":"exec_zzz"}"#;
        let filter = LogFilter {
            fields: vec![("execution_id".into(), "exec_abc".into())],
            ..Default::default()
        };
        assert!(line_matches(trace, &filter));
        assert!(line_matches(dispatch, &filter));
        assert!(!line_matches(other, &filter));
    }

    #[test]
    fn execution_or_run_id_matches_either_field() {
        let by_exec = r#"{"ts_epoch_ms":1,"execution_id":"exec_abc"}"#;
        let by_run = r#"{"ts_epoch_ms":1,"run_id":"exec_abc"}"#;
        let other = r#"{"ts_epoch_ms":1,"execution_id":"exec_zzz"}"#;
        let filter = LogFilter {
            execution_or_run_id: Some("exec_abc".into()),
            ..Default::default()
        };
        assert!(line_matches(by_exec, &filter));
        assert!(line_matches(by_run, &filter));
        assert!(!line_matches(other, &filter));
    }

    #[test]
    fn time_window_filters_by_timestamp_and_ts_epoch_ms() {
        let a = r#"{"timestamp":"2026-07-26T01:00:00Z","level":"INFO","fields":{"message":"a"},"target":"t"}"#;
        // 2026-07-26T02:00:00Z as epoch ms.
        let in_window_ms = parse_rfc3339_to_epoch_ms("2026-07-26T02:00:00Z").unwrap();
        let b = format!(r#"{{"ts_epoch_ms":{in_window_ms},"stage":"x"}}"#);
        let outside_ms = parse_rfc3339_to_epoch_ms("2026-07-26T03:00:00Z").unwrap();
        let c = format!(r#"{{"ts_epoch_ms":{outside_ms},"stage":"y"}}"#);
        let since = parse_rfc3339_to_epoch_ms("2026-07-26T00:30:00Z").unwrap();
        let until = parse_rfc3339_to_epoch_ms("2026-07-26T02:30:00Z").unwrap();
        let filter = LogFilter {
            since_ms: Some(since),
            until_ms: Some(until),
            ..Default::default()
        };
        assert!(line_matches(a, &filter));
        assert!(line_matches(&b, &filter), "ts_epoch_ms inside window must match");
        assert!(!line_matches(&c, &filter), "ts_epoch_ms outside window must not match");
        // Undated non-json cannot be placed in a time window.
        assert!(!line_matches("not json at all", &filter));
    }

    #[test]
    fn parse_time_spec_relative_and_epoch() {
        let now = 10_000_000_000_u128;
        assert_eq!(parse_time_spec("30m", now, true, false).unwrap(), now - 30 * 60_000);
        assert_eq!(parse_time_spec("2h", now, true, false).unwrap(), now - 2 * 3_600_000);
        assert_eq!(parse_time_spec("15s", now, false, false).unwrap(), now + 15_000);
        assert_eq!(
            parse_time_spec("1700000000", now, true, false).unwrap(),
            1_700_000_000_000
        );
        assert_eq!(
            parse_time_spec("1700000000000", now, true, false).unwrap(),
            1_700_000_000_000
        );
    }

    #[test]
    fn parse_time_spec_rfc3339_and_date() {
        let now = 0u128;
        let ms = parse_time_spec("2026-07-26T06:20:00Z", now, true, false).unwrap();
        assert_eq!(ms, parse_rfc3339_to_epoch_ms("2026-07-26T06:20:00Z").unwrap());
        // --since style: start of day.
        let day_start = parse_time_spec("2026-07-26", now, true, false).unwrap();
        assert_eq!(day_start, parse_rfc3339_to_epoch_ms("2026-07-26T00:00:00Z").unwrap());
        // --until style: inclusive end of day (next midnight exclusive − 1 ms).
        let day_end = parse_time_spec("2026-07-26", now, true, true).unwrap();
        let next_midnight = parse_rfc3339_to_epoch_ms("2026-07-27T00:00:00Z").unwrap();
        assert_eq!(day_end, next_midnight - 1);
        // End-of-day includes a late timestamp that start-of-day alone would exclude.
        let late = r#"{"timestamp":"2026-07-26T23:59:59Z","level":"INFO","fields":{"message":"late"},"target":"t"}"#;
        let until_filter = LogFilter {
            since_ms: Some(day_start),
            until_ms: Some(day_end),
            ..Default::default()
        };
        assert!(line_matches(late, &until_filter));
        let too_late =
            r#"{"timestamp":"2026-07-27T00:00:00Z","level":"INFO","fields":{"message":"next"},"target":"t"}"#;
        assert!(!line_matches(too_late, &until_filter));
    }

    #[test]
    fn query_spans_multiple_files_chronologically() {
        let dir = TempDir::new().unwrap();
        let older = dir.path().join("older.jsonl");
        let newer = dir.path().join("newer.jsonl");
        write_lines(&older, &["old-1", "old-2"]);
        write_lines(&newer, &["new-1"]);
        let result = query_log_files(&[older, newer], &LogFilter::default(), 2).unwrap();
        assert_eq!(result.lines, vec!["old-2", "new-1"]);
        assert_eq!(result.matched_total, 3);
        assert!(result.truncated);
    }

    #[test]
    fn missing_files_are_skipped() {
        let result = query_log_files(&[PathBuf::from("/nonexistent/log.jsonl")], &LogFilter::default(), 10).unwrap();
        assert!(result.lines.is_empty());
        assert!(result.paths_scanned.is_empty());
    }
}
