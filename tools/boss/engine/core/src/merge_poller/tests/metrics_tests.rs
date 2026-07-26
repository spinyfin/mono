use super::*;

#[test]
fn throttle_factor_is_unthrottled_above_low_water() {
    assert_eq!(throttle_factor_for(5000, 500), 1.0);
    assert_eq!(
        throttle_factor_for(500, 500),
        1.0,
        "exactly at the mark is not yet throttled"
    );
}

#[test]
fn throttle_factor_scales_up_as_budget_drains() {
    let half = throttle_factor_for(250, 500);
    assert!(
        half > 1.0 && half < 8.0,
        "midway drained must throttle, but not at max: {half}"
    );
    let near_empty = throttle_factor_for(1, 500);
    assert!(
        near_empty > half,
        "closer to empty must throttle harder than halfway: {near_empty} vs {half}"
    );
}

#[test]
fn throttle_factor_caps_at_max_when_exhausted() {
    assert_eq!(throttle_factor_for(0, 500), 8.0);
    assert_eq!(
        throttle_factor_for(-1, 500),
        8.0,
        "a negative reading is still fully exhausted"
    );
}

#[test]
fn parse_rate_limit_remaining_reads_the_graphql_field() {
    let body = serde_json::json!({"data": {"rateLimit": {"remaining": 42}}});
    assert_eq!(parse_rate_limit_remaining(&body), Some(42));
}

#[test]
fn parse_rate_limit_remaining_absent_is_none() {
    let body = serde_json::json!({"data": {"repo0": {}}});
    assert_eq!(parse_rate_limit_remaining(&body), None);
}

#[test]
fn is_rate_limit_error_matches_githubs_message_case_insensitively() {
    assert!(is_rate_limit_error(
        "GraphQL: API rate limit already exceeded for user ID 401512"
    ));
    assert!(is_rate_limit_error("api RATE LIMIT exceeded"));
}

#[test]
fn is_rate_limit_error_does_not_match_generic_network_failures() {
    assert!(!is_rate_limit_error("connection reset by peer"));
    assert!(!is_rate_limit_error("could not resolve to a Resource"));
}

#[test]
fn record_conflict_class_counter_increments_per_product_per_class() {
    let registry = crate::metrics::Registry::new();
    record_conflict_class_counter(&registry, "acme", "lockfile");
    record_conflict_class_counter(&registry, "acme", "lockfile");
    record_conflict_class_counter(&registry, "acme", "semantic");
    record_conflict_class_counter(&registry, "other_co", "lockfile");

    assert_eq!(registry.counter_value("conflict.acme.lockfile.classified"), Some(2));
    assert_eq!(registry.counter_value("conflict.acme.semantic.classified"), Some(1));
    assert_eq!(registry.counter_value("conflict.other_co.lockfile.classified"), Some(1));
}

#[test]
fn sanitize_metric_name_component_lowercases_and_replaces_invalid_chars() {
    assert_eq!(sanitize_metric_name_component("Acme-Corp"), "acme_corp");
    assert_eq!(sanitize_metric_name_component("already_ok_123"), "already_ok_123");
    assert_eq!(sanitize_metric_name_component(""), "_");
}

#[test]
fn record_conflict_class_counter_tolerates_unsanitary_product_id() {
    // A product id with characters outside the registry's allowed
    // charset must not produce an invalid dynamic metric name.
    let registry = crate::metrics::Registry::new();
    record_conflict_class_counter(&registry, "Acme Corp!", "lockfile");
    assert_eq!(
        registry.counter_value("conflict.acme_corp_.lockfile.classified"),
        Some(1)
    );
}
