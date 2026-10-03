use crate::parse::parse_grok_billing;
use boss_protocol::{DriverQuotaFailureKind, DriverQuotaOutcome, DriverQuotaWindow};

// HTTP 200 from the production credits endpoint on 2026-10-03.
// Nonessential string values are redacted; no credentials or account IDs retained.
const ZERO_USAGE_BODY: &str = r#"{
  "config": {
    "currentPeriod": {
      "type": "USAGE_PERIOD_TYPE_WEEKLY",
      "start": "2026-10-02T22:22:42.845319+00:00",
      "end": "2026-10-09T22:22:42.845319+00:00"
    },
    "onDemandCap": {"val": 0},
    "onDemandUsed": {"val": 0},
    "isUnifiedBillingUser": true,
    "prepaidBalance": {"val": 0},
    "topUpMethod": "<redacted>",
    "billingPeriodStart": "<redacted>",
    "billingPeriodEnd": "2026-10-09T22:22:42.845319+00:00"
  }
}"#;

fn assert_zero(body: &serde_json::Value) {
    let DriverQuotaOutcome::Reading(reading) = parse_grok_billing(&body.to_string()) else {
        panic!("expected zero-usage reading");
    };
    assert_eq!(reading.used_percent, 0.0);
    assert_eq!(reading.window, DriverQuotaWindow::Weekly);
    assert_eq!(
        reading.resets_at_epoch_s,
        Some(
            chrono::DateTime::parse_from_rfc3339("2026-10-09T22:22:42Z")
                .unwrap()
                .timestamp()
        )
    );
    assert_eq!(reading.resets_at_text, None);
}

fn unparseable(body: &str) -> String {
    match parse_grok_billing(body) {
        DriverQuotaOutcome::Unavailable { kind, reason } => {
            assert_eq!(kind, DriverQuotaFailureKind::Unparseable);
            reason
        }
        other => panic!("expected unparseable, got {other:?}"),
    }
}

#[test]
fn real_billing_body_with_omitted_zero_parses_wrapped_and_bare() {
    let body: serde_json::Value = serde_json::from_str(ZERO_USAGE_BODY).unwrap();
    assert_zero(&body);
    assert_zero(&body["config"]);
}

#[test]
fn malformed_percent_is_not_defaulted_to_zero() {
    for value in [
        serde_json::json!("0"),
        serde_json::json!(null),
        serde_json::json!(false),
        serde_json::json!({}),
    ] {
        let mut body: serde_json::Value = serde_json::from_str(ZERO_USAGE_BODY).unwrap();
        body["config"]["creditUsagePercent"] = value;
        unparseable(&body.to_string());
    }
}

#[test]
fn incomplete_or_unknown_billing_structure_is_not_zero() {
    for key in ["currentPeriod", "onDemandCap", "isUnifiedBillingUser"] {
        let mut body: serde_json::Value = serde_json::from_str(ZERO_USAGE_BODY).unwrap();
        body["config"].as_object_mut().unwrap().remove(key);
        unparseable(&body.to_string());
    }
    for (key, value) in [
        ("type", "USAGE_PERIOD_TYPE_UNKNOWN"),
        ("start", "invalid"),
        ("end", "2026-01-01T00:00:00Z"),
    ] {
        let mut body: serde_json::Value = serde_json::from_str(ZERO_USAGE_BODY).unwrap();
        body["config"]["currentPeriod"][key] = serde_json::json!(value);
        unparseable(&body.to_string());
    }
    for body in [
        r#"{}"#,
        r#"{"config":{}}"#,
        r#"{"config":null}"#,
        "null",
        "[]",
        r#"{"error":"secret-auth-detail"}"#,
        r#"{"config":{"renamedPercent":0}}"#,
    ] {
        unparseable(body);
    }
}

#[test]
fn failure_diagnostics_contain_keys_without_values() {
    let reason = unparseable(
        r#"{"account":"private-account","config":{"creditUsagePercent":"private-value","newField":"private-detail"}}"#,
    );
    assert!(reason.contains(r#"top-level keys: ["account", "config"]"#));
    assert!(reason.contains(r#"config keys: ["creditUsagePercent", "newField"]"#));
    assert!(!reason.contains("private-"));
    let reason = unparseable(r#"{"error":"private-auth-error"}"#);
    assert!(reason.contains(r#"top-level keys: ["error"]"#));
    assert!(reason.contains("config keys: absent or not an object"));
    assert!(!reason.contains("private-"));
    assert!(!unparseable(r#"{"config": private-invalid-json}"#).contains("private-"));
}
