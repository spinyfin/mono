//! Small shared helpers for reading numeric configuration from environment
//! variables with a fallback default.
//!
//! Several engine sweeps expose a "read a numeric env var, fall back to a
//! default" knob (backup interval/retention, heartbeat cadence, trace
//! rotation thresholds). Each had independently reimplemented the same
//! `std::env::var(KEY).ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT)`
//! shape. These helpers consolidate that pattern so the parsing, trimming,
//! and fallback behaviour stay consistent across call sites.
//!
//! The value is trimmed before parsing so a stray surrounding whitespace in
//! an env value does not silently fall back to the default.

use std::str::FromStr;
use std::time::Duration;

/// Read `key` from the environment and parse it as `T`, falling back to
/// `default` when the variable is unset, empty, or does not parse.
///
/// The raw value is trimmed before parsing.
pub(crate) fn env_parsed_or<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<T>().ok())
        .unwrap_or(default)
}

/// Read `key` as a whole number of seconds and return it as a [`Duration`],
/// falling back to `default` when unset/unparseable.
pub(crate) fn env_duration_secs(key: &str, default: Duration) -> Duration {
    env_duration_secs_min(key, default, 0)
}

/// Like [`env_duration_secs`] but additionally rejects any parsed value
/// below `min_secs`, falling back to `default`. Used where a too-small
/// interval would be harmful (e.g. a `0` heartbeat interval would busy-loop),
/// so such a value must fall back to the safe default rather than be honoured.
pub(crate) fn env_duration_secs_min(key: &str, default: Duration, min_secs: u64) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs >= min_secs)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `body` with `key` set to `value`, restoring the prior value
    /// afterwards. Each test uses a unique key so concurrent tests do not
    /// interfere with one another's process-global environment.
    fn with_env_var(key: &str, value: Option<&str>, body: impl FnOnce()) {
        let prev = std::env::var_os(key);
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        body();
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn env_parsed_or_reads_valid_value() {
        with_env_var("BOSS_TEST_ENV_PARSE_VALID", Some("42"), || {
            assert_eq!(env_parsed_or::<u64>("BOSS_TEST_ENV_PARSE_VALID", 7), 42);
        });
    }

    #[test]
    fn env_parsed_or_falls_back_when_missing() {
        with_env_var("BOSS_TEST_ENV_PARSE_MISSING", None, || {
            assert_eq!(env_parsed_or::<usize>("BOSS_TEST_ENV_PARSE_MISSING", 7), 7);
        });
    }

    #[test]
    fn env_parsed_or_falls_back_when_unparseable() {
        with_env_var("BOSS_TEST_ENV_PARSE_BAD", Some("not-a-number"), || {
            assert_eq!(env_parsed_or::<u64>("BOSS_TEST_ENV_PARSE_BAD", 7), 7);
        });
    }

    #[test]
    fn env_parsed_or_trims_surrounding_whitespace() {
        with_env_var("BOSS_TEST_ENV_PARSE_TRIM", Some("  99  "), || {
            assert_eq!(env_parsed_or::<u64>("BOSS_TEST_ENV_PARSE_TRIM", 7), 99);
        });
    }

    #[test]
    fn env_duration_secs_reads_valid_value() {
        with_env_var("BOSS_TEST_ENV_DUR_VALID", Some("120"), || {
            assert_eq!(
                env_duration_secs("BOSS_TEST_ENV_DUR_VALID", Duration::from_secs(5)),
                Duration::from_secs(120)
            );
        });
    }

    #[test]
    fn env_duration_secs_falls_back_when_missing() {
        with_env_var("BOSS_TEST_ENV_DUR_MISSING", None, || {
            assert_eq!(
                env_duration_secs("BOSS_TEST_ENV_DUR_MISSING", Duration::from_secs(5)),
                Duration::from_secs(5)
            );
        });
    }

    #[test]
    fn env_duration_secs_falls_back_when_unparseable() {
        with_env_var("BOSS_TEST_ENV_DUR_BAD", Some("soon"), || {
            assert_eq!(
                env_duration_secs("BOSS_TEST_ENV_DUR_BAD", Duration::from_secs(5)),
                Duration::from_secs(5)
            );
        });
    }

    #[test]
    fn env_duration_secs_min_honours_value_at_or_above_min() {
        with_env_var("BOSS_TEST_ENV_DUR_MIN_OK", Some("10"), || {
            assert_eq!(
                env_duration_secs_min("BOSS_TEST_ENV_DUR_MIN_OK", Duration::from_secs(300), 1),
                Duration::from_secs(10)
            );
        });
    }

    #[test]
    fn env_duration_secs_min_rejects_zero_below_min() {
        // A zero value is below the min-of-1 guard and must fall back to the
        // default (mirrors heartbeat_interval's nonzero requirement).
        with_env_var("BOSS_TEST_ENV_DUR_MIN_ZERO", Some("0"), || {
            assert_eq!(
                env_duration_secs_min("BOSS_TEST_ENV_DUR_MIN_ZERO", Duration::from_secs(300), 1),
                Duration::from_secs(300)
            );
        });
    }
}
