use super::helpers::*;

// ---------------------------------------------------------------------------
// `classify_noop_validation` — the decision half of the `boss engine ci
// mark-noop` validation gate. Pure over a LIVE probe; these assert that only a
// verified-green probe is honored and that the verdict is always keyed to the
// probe's current head SHA.
// ---------------------------------------------------------------------------

#[test]
fn noop_validation_honors_clean_ci_keyed_to_live_head() {
    let p = probe("https://github.com/foo/bar/pull/1", "sha-green");
    assert_eq!(
        classify_noop_validation(&p),
        NoopValidation::Green {
            head_sha: Some("sha-green".into()),
        },
    );
}

#[test]
fn noop_validation_rejects_failing_ci_and_names_the_checks() {
    let mut p = probe("https://github.com/foo/bar/pull/1", "sha-red");
    p.state = PrLifecycleState::Open(OpenPrStatus::ci_failing(one_failure()));
    match classify_noop_validation(&p) {
        NoopValidation::Rejected { head_sha, status } => {
            assert_eq!(head_sha.as_deref(), Some("sha-red"));
            assert!(
                status.contains("ci/test"),
                "rejection status should name the failing required check: {status}",
            );
        }
        other => panic!("expected Rejected for failing CI, got {other:?}"),
    }
}

#[test]
fn noop_validation_rejects_in_flight_ci() {
    let mut p = probe("https://github.com/foo/bar/pull/1", "sha-pending");
    p.state = PrLifecycleState::Open(OpenPrStatus {
        mergeability: crate::merge_poller::OpenPrMergeability::Clean,
        ci: crate::merge_poller::OpenPrCiStatus::InFlight,
    });
    match classify_noop_validation(&p) {
        NoopValidation::Rejected { head_sha, status } => {
            assert_eq!(head_sha.as_deref(), Some("sha-pending"));
            let lower = status.to_lowercase();
            assert!(
                lower.contains("pending") || lower.contains("in-flight"),
                "rejection status should explain CI is unfinished: {status}",
            );
        }
        other => panic!("expected Rejected for in-flight CI, got {other:?}"),
    }
}

#[test]
fn noop_validation_honors_merged_pr_as_moot() {
    let mut p = probe("https://github.com/foo/bar/pull/1", "sha-merged");
    p.state = PrLifecycleState::Merged;
    assert_eq!(
        classify_noop_validation(&p),
        NoopValidation::Green {
            head_sha: Some("sha-merged".into()),
        },
    );
}

#[test]
fn noop_validation_rejects_closed_unmerged_pr() {
    let mut p = probe("https://github.com/foo/bar/pull/1", "sha-closed");
    p.state = PrLifecycleState::ClosedUnmerged;
    assert!(
        matches!(classify_noop_validation(&p), NoopValidation::Rejected { .. }),
        "a closed-unmerged PR is not a validated-green state",
    );
}

#[test]
fn noop_validation_carries_no_sha_when_github_omits_head_ref_oid() {
    // The verdict reports whatever the LIVE probe says the head is. When
    // GitHub omits headRefOid the claim is still honored on a clean rollup,
    // but there is no SHA to record.
    let mut p = probe("https://github.com/foo/bar/pull/1", "ignored");
    p.head_ref_oid = None;
    assert_eq!(classify_noop_validation(&p), NoopValidation::Green { head_sha: None },);
}
