use super::*;
use crate::driver::AgentDriver;

#[test]
fn agy_quota_error_escalates_without_nudge_or_redispatch_budget() {
    let driver = crate::driver::agy::AgyDriver::default();
    // Synthetic ERROR_MESSAGE row: the native step enum is known, but the
    // pane studies did not intentionally exhaust the signed-in account.
    let row = driver.normalize_transcript_entry(serde_json::json!({
        "step_index": 7, "source": "MODEL", "type": "ERROR_MESSAGE", "status": "ERROR",
        "error": "You have exhausted your quota on this model."
    }));
    let error = driver
        .extract_error_from_transcript(&[row])
        .expect("halting quota error");
    assert_eq!(driver.classify_error(&error), WorkerErrorClass::QuotaExhausted);
    let policy_class = worker_error_class_to_policy(driver.classify_error(&error));
    let policy = RecoveryPolicy::default();
    for attempts in 0..=policy.max_attempts() {
        assert_eq!(
            policy.decide(policy_class, attempts),
            RecoveryDecision::Escalate {
                reason: EscalateReason::Permanent
            },
        );
    }
}

#[test]
fn agy_eligibility_service_failure_uses_bounded_transient_recovery() {
    let driver = crate::driver::agy::AgyDriver::default();
    let class = driver.classify_error("Eligibility check failed: failed to fetch user info: 500 Internal Server Error");
    assert_eq!(class, WorkerErrorClass::EligibilityServiceFailure);
    let policy_class = worker_error_class_to_policy(class);
    assert_eq!(policy_class, ErrorClass::Transient);
    let policy = RecoveryPolicy::default();
    assert!(matches!(
        policy.decide(policy_class, 0),
        RecoveryDecision::Resume { attempt: 1, .. }
    ));
    assert_eq!(
        policy.decide(policy_class, policy.max_attempts()),
        RecoveryDecision::Escalate {
            reason: EscalateReason::RetriesExhausted
        },
    );
}
