use super::*;

#[test]
fn only_legacy_pr_anchor_failures_are_rearmed() {
    for suffix in [
        "does not exist in the shared store; the bound-PR checkout must write it before reconciliation",
        "must resolve to exactly one change; it is conflicted",
    ] {
        assert!(is_legacy_pr_anchor_blocker(&format!(
            "PR head bookmark pr/139 {suffix}"
        )));
        assert!(!is_legacy_pr_anchor_blocker(&format!(
            "PR head bookmark pr/not-a-number {suffix}"
        )));
    }
    assert!(!is_legacy_pr_anchor_blocker(
        "cannot repair pr/139 from GitHub: unavailable"
    ));
    assert!(!is_legacy_pr_anchor_blocker(
        "recovery bookmark boss-recovery/exec_test must resolve to exactly one change"
    ));
}
