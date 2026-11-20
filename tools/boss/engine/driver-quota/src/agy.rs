//! agy 1.5.10 local `/usage` JSON: provider fractions, not token accounting.
use boss_protocol::{DriverQuotaFailureKind, DriverQuotaOutcome, DriverQuotaReading, DriverQuotaWindow};

use crate::parse::unavailable;

/// Select Gemini's weekly bucket, never the independent Claude/GPT allowance
/// or the five-hour burst limit. Boss dispatches Gemini models through agy.
pub fn parse_usage_json(stdout: &str) -> DriverQuotaOutcome {
    let malformed = || unavailable(DriverQuotaFailureKind::Unparseable, "Unrecognized agy quota output");
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return malformed();
    };
    if payload.get("status").and_then(|v| v.as_str()) != Some("SUCCESS") {
        return unavailable(
            DriverQuotaFailureKind::ProbeFailed,
            "agy could not report quota; check agy sign-in",
        );
    }
    if payload.pointer("/command/name").and_then(|v| v.as_str()) != Some("usage")
        || payload.get("num_turns").and_then(|v| v.as_u64()) != Some(0)
    {
        return malformed();
    }
    let Some(groups) = payload.pointer("/command/data/groups").and_then(|v| v.as_array()) else {
        return malformed();
    };
    let mut matches = groups
        .iter()
        .filter_map(|group| group.get("buckets").and_then(|v| v.as_array()))
        .flatten()
        .filter(|bucket| bucket.get("id").and_then(|v| v.as_str()) == Some("gemini-weekly"));
    let Some(bucket) = matches.next() else {
        return unavailable(DriverQuotaFailureKind::NotReported, "Quota not reported by agy");
    };
    if matches.next().is_some() || bucket.get("window").and_then(|v| v.as_str()) != Some("weekly") {
        return malformed();
    }
    let Some(remaining) = bucket.get("remaining_fraction").and_then(|v| v.as_f64()) else {
        return malformed();
    };
    if !remaining.is_finite() || !(0.0..=1.0).contains(&remaining) {
        return malformed();
    }
    let reset = match bucket.get("reset_time") {
        None => None,
        Some(value) => match value
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        {
            Some(time) => Some(time.timestamp()),
            None => return malformed(),
        },
    };
    DriverQuotaOutcome::Reading(DriverQuotaReading {
        used_percent: (1.0 - remaining) * 100.0,
        window: DriverQuotaWindow::Weekly,
        resets_at_epoch_s: reset,
        resets_at_text: None,
        source: "agy --print /usage (Gemini weekly)".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("agy_usage_fixture.json")).unwrap()
    }

    #[test]
    fn captured_usage_selects_gemini_weekly_and_converts_remaining_to_used() {
        let DriverQuotaOutcome::Reading(reading) = parse_usage_json(&fixture().to_string()) else {
            panic!("expected reading");
        };
        assert!((reading.used_percent - 0.84230303764343).abs() < 0.000001);
        assert_eq!(reading.window, DriverQuotaWindow::Weekly);
        assert_eq!(reading.resets_at_epoch_s, Some(1795567941));
    }

    #[test]
    fn missing_bucket_is_not_zero_or_another_models_quota() {
        let mut value = fixture();
        value["command"]["data"]["groups"][0]["buckets"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        assert!(matches!(
            parse_usage_json(&value.to_string()),
            DriverQuotaOutcome::Unavailable {
                kind: DriverQuotaFailureKind::NotReported,
                ..
            }
        ));
    }

    #[test]
    fn malformed_fraction_reset_and_window_fail_without_echoing_payload() {
        for (field, value) in [
            ("remaining_fraction", serde_json::json!(-0.1)),
            ("remaining_fraction", serde_json::json!(1.1)),
            ("remaining_fraction", serde_json::Value::Null),
            ("reset_time", serde_json::json!("secret")),
            ("window", serde_json::json!("5h")),
        ] {
            let mut payload = fixture();
            payload["command"]["data"]["groups"][0]["buckets"][0][field] = value;
            let outcome = parse_usage_json(&payload.to_string());
            assert!(matches!(
                outcome,
                DriverQuotaOutcome::Unavailable {
                    kind: DriverQuotaFailureKind::Unparseable,
                    ..
                }
            ));
            assert!(!format!("{outcome:?}").contains("secret"));
        }
    }

    #[test]
    fn absent_fraction_never_defaults_to_zero() {
        let mut payload = fixture();
        payload["command"]["data"]["groups"][0]["buckets"][0]
            .as_object_mut()
            .unwrap()
            .remove("remaining_fraction");
        assert!(matches!(
            parse_usage_json(&payload.to_string()),
            DriverQuotaOutcome::Unavailable { .. }
        ));
    }
}
