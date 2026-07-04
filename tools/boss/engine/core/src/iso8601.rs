//! Small ISO-8601 / epoch conversion helpers shared across
//! boss-engine-core. All date arithmetic delegates to `chrono` (already
//! a crate dependency); these functions only add the thin, call-site
//! specific acceptance rules chrono doesn't provide out of the box.
//!
//! Two parsers are exposed on purpose — they differ only in how
//! permissive they are, both routing through chrono:
//!
//! * [`parse_iso8601_to_epoch`] is **strict** — it requires a canonical
//!   `YYYY-MM-DDTHH:MM:SS[.fff]Z` value and rejects everything else
//!   (`+00:00` offsets, already-numeric strings, impossible dates) so
//!   the DB timestamp migration leaves those rows untouched.
//! * [`parse_iso8601_lenient`] is **lenient** — it accepts any RFC 3339
//!   value (any offset, `Z` or `+HH:MM`), matching what the GitHub API
//!   can return.

/// Format `epoch_secs` as a fixed-width ISO-8601 UTC string
/// (`YYYY-MM-DDTHH:MM:SSZ`). Because the format is fixed-width,
/// lexicographic string ordering matches chronological ordering — the
/// stale-worker sweep, for instance, builds a cutoff timestamp with this
/// and compares `last_event_at < cutoff` directly, with no date parsing.
///
/// Negative epochs (pre-1970) format correctly. Panics only for an
/// epoch outside chrono's supported range (roughly ±262,000 years),
/// which no real system/DB timestamp can reach.
pub(crate) fn format_epoch_iso8601(epoch_secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(epoch_secs, 0)
        .expect("epoch within chrono's supported range")
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Strict parse of a canonical UTC timestamp
/// (`YYYY-MM-DDTHH:MM:SS[.fff]Z`, with a space also accepted in place of
/// the `T` separator) into Unix epoch seconds. Returns `None` for any
/// other shape — already-numeric strings, non-`Z` (offset) timestamps,
/// impossible calendar dates, or otherwise malformed values — so the DB
/// timestamp migration leaves those rows untouched.
///
/// The trailing-`Z` requirement is enforced here (chrono's own RFC 3339
/// parser would accept `+00:00` offsets, which this call site must
/// reject); chrono does the field parsing and date arithmetic. Fractional
/// seconds are truncated to whole seconds.
pub(crate) fn parse_iso8601_to_epoch(value: &str) -> Option<i64> {
    // Require an explicit trailing `Z`: this rejects numeric strings and
    // offset forms like `+00:00` before chrono ever sees the value.
    let body = value.trim().strip_suffix('Z')?;
    // Accept either `T` or a space between date and time; fractional
    // seconds are optional. chrono validates the calendar date and ranges.
    let naive = chrono::NaiveDateTime::parse_from_str(body, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(body, "%Y-%m-%d %H:%M:%S%.f"))
        .ok()?;
    Some(naive.and_utc().timestamp())
}

/// Lenient parse of an RFC 3339 datetime string (e.g.
/// `"2026-05-17T10:00:00Z"`) to Unix seconds, used for GitHub API
/// timestamps. Unlike [`parse_iso8601_to_epoch`] it accepts any RFC 3339
/// offset (including `Z` and `+HH:MM`), matching what the GitHub API can
/// return. Fractional seconds are truncated to whole seconds.
pub(crate) fn parse_iso8601_lenient(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_epoch_iso8601 ────────────────────────────────────────────────
    //
    // Expected strings are taken from an independent epoch->UTC converter,
    // NOT derived by re-reading the civil-date algorithm under test.

    #[test]
    fn format_epoch_unix_epoch_is_1970() {
        assert_eq!(format_epoch_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_epoch_known_modern_instant() {
        // 1700000000 == 2023-11-14T22:13:20Z (a well-known round epoch).
        assert_eq!(format_epoch_iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn format_epoch_leap_day() {
        // 2024 is a leap year; 1709164800 is exactly midnight of Feb 29.
        // Also exercises the m <= 2 February year-adjustment branch.
        assert_eq!(format_epoch_iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn format_epoch_year_month_boundary() {
        // One second before 2024-01-01T00:00:00Z (1704067200) rolls the
        // year, month, and day all back to the final second of 2023.
        assert_eq!(format_epoch_iso8601(1_704_067_199), "2023-12-31T23:59:59Z");
        assert_eq!(format_epoch_iso8601(1_704_067_200), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn format_epoch_january_year_adjust_branch() {
        // A January instant exercises the m <= 2 branch with a non-zero
        // time-of-day. 1736944245 == 2025-01-15T12:30:45Z.
        assert_eq!(format_epoch_iso8601(1_736_944_245), "2025-01-15T12:30:45Z");
    }

    #[test]
    fn format_epoch_negative_pre_1970_wraps_time_of_day() {
        // Negative epochs must use Euclidean div/rem so the time-of-day
        // wraps to a positive value rather than going negative. One second
        // before the epoch is 1969-12-31T23:59:59Z.
        assert_eq!(format_epoch_iso8601(-1), "1969-12-31T23:59:59Z");
        // A full day before the epoch is exactly midnight of 1969-12-31.
        assert_eq!(format_epoch_iso8601(-86_400), "1969-12-31T00:00:00Z");
    }

    // ── round-trip (parse <-> format) ───────────────────────────────────────

    #[test]
    fn round_trip_parse_then_format() {
        // A parse followed by a format must reproduce the original string.
        let s = "2026-05-07T20:04:11Z";
        let ts = parse_iso8601_to_epoch(s).expect("valid timestamp");
        assert_eq!(format_epoch_iso8601(ts), s);
    }

    // ── parse_iso8601_to_epoch (strict) ─────────────────────────────────────

    #[test]
    fn parse_strict_handles_canonical_shapes() {
        // Reference epochs cross-checked with `date -u -d '...' +%s`.
        assert_eq!(parse_iso8601_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_to_epoch("2026-05-07T18:55:45Z"), Some(1_778_180_145));
        // Fractional seconds are truncated, not rounded.
        assert_eq!(parse_iso8601_to_epoch("2026-05-07T18:55:45.000Z"), Some(1_778_180_145));
        assert_eq!(parse_iso8601_to_epoch("2026-05-07T18:55:45.999Z"), Some(1_778_180_145));
        // Already-canonical numeric strings are left untouched.
        assert_eq!(parse_iso8601_to_epoch("1778180145"), None);
        assert_eq!(parse_iso8601_to_epoch(""), None);
        // Non-UTC suffixes aren't supported (engine only writes Z).
        assert_eq!(parse_iso8601_to_epoch("2026-05-07T18:55:45+00:00"), None);
        // Malformed values fall through.
        assert_eq!(parse_iso8601_to_epoch("2026/05/07T18:55:45Z"), None);
        assert_eq!(parse_iso8601_to_epoch("2026-13-07T18:55:45Z"), None);
        // Impossible calendar dates are rejected (chrono validates the
        // day-of-month), leaving such rows untouched rather than silently
        // rolling them into the next month.
        assert_eq!(parse_iso8601_to_epoch("2026-02-30T00:00:00Z"), None);
        // A space in place of the `T` separator is accepted.
        assert_eq!(parse_iso8601_to_epoch("2026-05-07 18:55:45Z"), Some(1_778_180_145));
    }

    // ── parse_iso8601_lenient (GitHub) ──────────────────────────────────────

    #[test]
    fn parse_lenient_known_epoch() {
        // 1970-01-01T00:00:00Z == 0
        assert_eq!(parse_iso8601_lenient("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_lenient_known_date() {
        // 2026-05-17T10:00:00Z == 1_779_012_000
        let ts = parse_iso8601_lenient("2026-05-17T10:00:00Z").expect("valid timestamp");
        assert_eq!(ts, 1_779_012_000, "ts={ts}");
    }

    #[test]
    fn parse_lenient_rejects_malformed() {
        assert_eq!(parse_iso8601_lenient("not-a-date"), None);
        assert_eq!(parse_iso8601_lenient(""), None);
    }

    // ── strict and lenient agree on canonical GitHub-shaped input ───────────

    #[test]
    fn strict_and_lenient_agree_on_canonical_utc() {
        for s in ["1970-01-01T00:00:00Z", "2026-05-17T10:00:00Z", "2023-11-14T22:13:20Z"] {
            assert_eq!(
                parse_iso8601_to_epoch(s),
                parse_iso8601_lenient(s),
                "disagreement on {s}"
            );
        }
    }
}
