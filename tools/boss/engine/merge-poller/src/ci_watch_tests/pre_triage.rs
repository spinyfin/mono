use super::helpers::*;

// ----- Phase 9 #28: pre-triage classification permutations ----------

#[test]
fn pre_triage_all_startup_failure_routes_to_retrigger() {
    let fs = [failure("a", "STARTUP_FAILURE"), failure("b", "STARTUP_FAILURE")];
    assert_eq!(classify_pre_triage(&fs), "retrigger");
}

#[test]
fn pre_triage_mixed_startup_and_cancelled_routes_to_retrigger() {
    let fs = [failure("a", "STARTUP_FAILURE"), failure("b", "CANCELLED")];
    assert_eq!(classify_pre_triage(&fs), "retrigger");
}

#[test]
fn pre_triage_one_real_failure_routes_to_fix() {
    let fs = [failure("a", "STARTUP_FAILURE"), failure("b", "FAILURE")];
    assert_eq!(classify_pre_triage(&fs), "fix");
}

#[test]
fn pre_triage_all_failure_routes_to_fix() {
    let fs = [failure("a", "FAILURE"), failure("b", "TIMED_OUT")];
    assert_eq!(classify_pre_triage(&fs), "fix");
}

#[test]
fn pre_triage_action_required_routes_to_fix() {
    // ACTION_REQUIRED isn't unambiguous infra — it needs a human or
    // a worker triage decision, so it stays on the fix path.
    let fs = [failure("a", "ACTION_REQUIRED")];
    assert_eq!(classify_pre_triage(&fs), "fix");
}

#[test]
fn pre_triage_empty_defaults_to_fix() {
    assert_eq!(classify_pre_triage(&[]), "fix");
}
