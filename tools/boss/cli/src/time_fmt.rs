//! Shared local-time rendering for human CLI output.
//!
//! The workspace `chrono` dependency ships without the `clock` feature, so
//! there is no portable in-process "what's my local timezone" lookup
//! without a new external dependency. Shelling out to `date +%z` (POSIX,
//! identical on BSD and GNU `date`) gets this host's *current* UTC offset
//! with zero new dependencies. Applying that one fixed offset to every
//! timestamp is an approximation — a timestamp on the other side of a DST
//! transition from "now" can be off by an hour — so callers that print a
//! report header state the offset in use, and `boss cost` callers who need
//! exactness can pass `--utc`.
//!
//! The offset is memoised in a `OnceLock` so `date` runs at most once per
//! process, even when `boss task show` formats many archived timestamps.

use std::process::Command;
use std::sync::OnceLock;

use crate::CliError;

pub(crate) struct DisplayTz {
    pub(crate) offset_s: i32,
    pub(crate) label: String,
}

fn detect_local_utc_offset_seconds_uncached() -> Option<i32> {
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

pub(crate) fn detect_local_utc_offset_seconds() -> Option<i32> {
    static OFFSET: OnceLock<Option<i32>> = OnceLock::new();
    *OFFSET.get_or_init(detect_local_utc_offset_seconds_uncached)
}

pub(crate) fn offset_suffix(offset_s: i32) -> String {
    let sign = if offset_s < 0 { '-' } else { '+' };
    let abs = offset_s.unsigned_abs();
    format!("{sign}{:02}:{:02}", abs / 3_600, (abs % 3_600) / 60)
}

pub(crate) fn resolve_display_tz(force_utc: bool) -> DisplayTz {
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

pub(crate) fn format_epoch(epoch_s: i64, tz: &DisplayTz) -> String {
    let shifted = epoch_s.saturating_add(i64::from(tz.offset_s));
    let utc_form = boss_engine_utils::iso8601::format_epoch_iso8601(shifted);
    format!("{}{}", utc_form.trim_end_matches('Z'), offset_suffix(tz.offset_s))
}

/// Format a stored epoch-seconds string for human output. Falls back to the
/// raw value when it does not parse as an `i64` (legacy non-epoch rows).
/// Parse a `--since`/`--until` bound: an RFC3339 timestamp, or a
/// relative duration ago (`24h`, `7d`, `2w`) resolved against `now`.
pub(crate) fn parse_epoch_bound(input: &str, now: i64) -> Result<i64, CliError> {
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

pub(crate) fn resolve_window(since: &str, until: Option<&str>) -> Result<(i64, i64), CliError> {
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

pub(crate) fn format_stored_epoch(raw: &str) -> String {
    let Ok(epoch_s) = raw.parse::<i64>() else {
        return raw.to_owned();
    };
    format_epoch(epoch_s, &resolve_display_tz(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_suffix_formats_negative_zero_and_positive() {
        assert_eq!(offset_suffix(-19_800), "-05:30");
        assert_eq!(offset_suffix(0), "+00:00");
        assert_eq!(offset_suffix(3_600), "+01:00");
    }

    #[test]
    fn format_epoch_applies_the_supplied_offset() {
        let tz = DisplayTz {
            offset_s: -3_600,
            label: "test".to_owned(),
        };
        assert_eq!(format_epoch(0, &tz), "1969-12-31T23:00:00-01:00");
    }

    #[test]
    fn format_stored_epoch_falls_back_for_non_integer_legacy_values() {
        assert_eq!(format_stored_epoch("not-an-epoch"), "not-an-epoch");
    }

    #[test]
    fn format_stored_epoch_uses_the_shared_local_renderer() {
        let raw = "1787115212";
        let expected = format_epoch(1_787_115_212, &resolve_display_tz(false));
        assert_eq!(format_stored_epoch(raw), expected);
    }

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
}
