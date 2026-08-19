//! Resolve a local wall-clock time in an IANA timezone to a UTC epoch
//! instant, with an explicit, tested policy for the two DST edge cases.
//!
//! Originally written for the automation scheduler
//! (`boss-engine-automation-schedule`), which needs this to turn a cron
//! field's wall-clock hour/minute into a UTC occurrence. `boss-engine-driver-quota`
//! needs the identical resolution for a different wall-clock string (a
//! provider's own "resets at" clause), so this lives here rather than in
//! either caller, per the repo's prefer-crates-for-distinct-units
//! convention — a second independent implementation is exactly how the two
//! policies would drift apart.

use chrono::{Duration, MappedLocalTime, NaiveDateTime, TimeZone};
use chrono_tz::Tz;

/// Maximum minutes [`resolve_local_to_utc`] will advance a non-existent
/// (spring-forward gap) wall-clock time looking for the next valid instant.
/// Real DST gaps are ≤120 minutes; 240 is a generous safety bound.
pub const MAX_GAP_ADVANCE_MINUTES: i64 = 240;

/// Resolve a local wall-clock `naive` time in `tz` to a UTC epoch second,
/// applying the DST policy: gap → next valid instant (run later); fold →
/// earliest of the two instants (fire once).
pub fn resolve_local_to_utc(naive: NaiveDateTime, tz: Tz) -> Option<i64> {
    match tz.from_local_datetime(&naive) {
        MappedLocalTime::Single(dt) => Some(dt.timestamp()),
        MappedLocalTime::Ambiguous(earliest, _latest) => Some(earliest.timestamp()),
        MappedLocalTime::None => {
            // Spring-forward gap: this wall time does not exist. Advance
            // minute-by-minute to the first instant that does (the gap's
            // far edge), so the caller's occurrence is once, slightly later.
            let mut candidate = naive;
            for _ in 0..MAX_GAP_ADVANCE_MINUTES {
                candidate += Duration::minutes(1);
                match tz.from_local_datetime(&candidate) {
                    MappedLocalTime::Single(dt) => return Some(dt.timestamp()),
                    MappedLocalTime::Ambiguous(earliest, _) => return Some(earliest.timestamp()),
                    MappedLocalTime::None => continue,
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::America::Los_Angeles;

    /// Spring-forward: on 2026-03-08 LA clocks jump 02:00→03:00, so 02:30
    /// does not exist. Resolution must advance to the next valid instant
    /// (03:00 PDT), not return `None`.
    #[test]
    fn spring_forward_gap_advances_to_next_valid_instant() {
        let naive = NaiveDateTime::parse_from_str("2026-03-08 02:30:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let epoch = resolve_local_to_utc(naive, Los_Angeles).expect("gap must resolve, not fail");
        let resolved = Los_Angeles.timestamp_opt(epoch, 0).single().unwrap();
        assert_eq!(format!("{}", resolved.format("%H:%M")), "03:00");
    }

    /// Fall-back: on 2026-11-01 LA clocks repeat 01:00–01:59. Resolution
    /// must pick the earliest (PDT) instant, not the later (PST) one.
    #[test]
    fn fall_back_fold_picks_earliest_instant() {
        let naive = NaiveDateTime::parse_from_str("2026-11-01 01:30:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let epoch = resolve_local_to_utc(naive, Los_Angeles).expect("fold must resolve, not fail");
        // 01:30 PDT (the earlier of the two 01:30s) is 08:30 UTC.
        let utc = chrono::Utc.timestamp_opt(epoch, 0).single().unwrap();
        assert_eq!(format!("{}", utc.format("%H:%M")), "08:30");
    }

    /// An ordinary, unambiguous wall time resolves directly.
    #[test]
    fn ordinary_time_resolves_directly() {
        let naive = NaiveDateTime::parse_from_str("2026-07-15 14:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let epoch = resolve_local_to_utc(naive, Los_Angeles).unwrap();
        let utc = chrono::Utc.timestamp_opt(epoch, 0).single().unwrap();
        // 14:00 PDT (UTC-7) = 21:00 UTC.
        assert_eq!(format!("{}", utc.format("%H:%M")), "21:00");
    }
}
