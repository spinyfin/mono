use super::*;

/// LinkedIn-org reclassification: a PR in `linkedin-multiproduct`
/// with `Owner Approval` pending and no other failing check should
/// surface as CI clean + review required, not CI in-flight. Without
/// the reclassification at the aggregation layer the card reads
/// "Required CI checks in progress" when the real situation is
/// "waiting for owner review", which is what the issue asks to fix.
#[test]
fn owner_approval_pending_in_linkedin_org_routes_to_review() {
    let rollup = serde_json::json!([
        check_run("ci/build", "COMPLETED", "SUCCESS"),
        check_run("Owner Approval", "IN_PROGRESS", ""),
    ]);
    let body = json_doc("OPEN", "", "MERGEABLE", "CLEAN", ("base-1", "head-1"), &[], rollup);
    let probe = parse_probe_json("https://github.com/linkedin-multiproduct/mono/pull/1", &body, None).unwrap();
    let open = match probe.state {
        PrLifecycleState::Open(open) => open,
        other => panic!("expected Open, got {other:?}"),
    };
    assert_eq!(
        open.ci,
        OpenPrCiStatus::Clean,
        "Owner Approval pending must not contribute to CI status",
    );
    assert_eq!(
        probe.review,
        PrReviewState::Required,
        "Owner Approval pending must surface as review-required",
    );
}

/// Dominance rule: even when GitHub's `reviewDecision` reports
/// `APPROVED` (the code-review side is satisfied), a pending
/// `Owner Approval` check still gates merge and must show the
/// PR as awaiting required review.
#[test]
fn owner_approval_pending_overrides_github_approved_decision() {
    let rollup = serde_json::json!([check_run("Owner Approval", "IN_PROGRESS", ""),]);
    let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
        "OPEN",
        "",
        "MERGEABLE",
        "CLEAN",
        ("base-1", "head-1"),
        &[],
        rollup,
    ))
    .unwrap();
    doc["reviewDecision"] = serde_json::json!("APPROVED");
    doc["reviews"] = serde_json::json!([
        {"author": {"login": "alice"}, "state": "APPROVED"},
    ]);
    let probe = parse_probe_json("https://github.com/linkedin-eng/foo/pull/2", &doc.to_string(), None).unwrap();
    assert_eq!(probe.review, PrReviewState::Required);
}

/// `ChangesRequested` is a stronger negative signal than a pending
/// owner-approval check; preserve it rather than overriding to
/// `Required` so the user still sees who blocked the PR.
#[test]
fn owner_approval_pending_preserves_changes_requested() {
    let rollup = serde_json::json!([check_run("Owner Approval", "IN_PROGRESS", ""),]);
    let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
        "OPEN",
        "",
        "MERGEABLE",
        "CLEAN",
        ("base-1", "head-1"),
        &[],
        rollup,
    ))
    .unwrap();
    doc["reviewDecision"] = serde_json::json!("CHANGES_REQUESTED");
    doc["reviews"] = serde_json::json!([
        {"author": {"login": "bob"}, "state": "CHANGES_REQUESTED"},
    ]);
    let probe = parse_probe_json(
        "https://github.com/linkedin-multiproduct/mono/pull/3",
        &doc.to_string(),
        None,
    )
    .unwrap();
    assert_eq!(
        probe.review,
        PrReviewState::ChangesRequested {
            reviewers: vec!["bob".to_owned()]
        },
    );
}

/// Successful Owner Approval is a no-op for the review axis — the
/// GitHub verdict (here `Unknown` since `reviewDecision` is unset)
/// stands.
#[test]
fn owner_approval_success_does_not_override_review() {
    let rollup = serde_json::json!([
        check_run("Owner Approval", "COMPLETED", "SUCCESS"),
        check_run("ci/build", "COMPLETED", "SUCCESS"),
    ]);
    let body = json_doc("OPEN", "", "MERGEABLE", "CLEAN", ("base-1", "head-1"), &[], rollup);
    let probe = parse_probe_json("https://github.com/linkedin-multiproduct/mono/pull/4", &body, None).unwrap();
    let open = match probe.state {
        PrLifecycleState::Open(open) => open,
        other => panic!("expected Open, got {other:?}"),
    };
    assert_eq!(open.ci, OpenPrCiStatus::Clean);
    assert_eq!(probe.review, PrReviewState::Unknown);
}

/// Failed Owner Approval (ACL rejection) is reported as
/// `ChangesRequested` with no reviewer identity, and is removed
/// from the CI axis so the engine's CI-fix flow doesn't try to
/// auto-remediate a human-approval refusal.
#[test]
fn owner_approval_failure_becomes_changes_requested() {
    let rollup = serde_json::json!([check_run("Owner Approval", "COMPLETED", "FAILURE"),]);
    let body = json_doc("OPEN", "", "MERGEABLE", "CLEAN", ("base-1", "head-1"), &[], rollup);
    let probe = parse_probe_json("https://github.com/linkedin-eng/foo/pull/5", &body, None).unwrap();
    let open = match probe.state {
        PrLifecycleState::Open(open) => open,
        other => panic!("expected Open, got {other:?}"),
    };
    assert_eq!(
        open.ci,
        OpenPrCiStatus::Clean,
        "Owner Approval failure must not show as a CI failure",
    );
    assert_eq!(probe.review, PrReviewState::ChangesRequested { reviewers: Vec::new() },);
}

/// Outside the configured LinkedIn orgs, an `Owner Approval` check
/// is left in the CI rollup and behaves like any other required
/// check — this guards against the reclassification leaking into
/// repos where the check doesn't have ACL semantics.
#[test]
fn owner_approval_in_other_org_stays_a_ci_check() {
    let rollup = serde_json::json!([check_run("Owner Approval", "IN_PROGRESS", ""),]);
    let body = json_doc("OPEN", "", "MERGEABLE", "CLEAN", ("base-1", "head-1"), &[], rollup);
    let probe = parse_probe_json("https://github.com/spinyfin/mono/pull/6", &body, None).unwrap();
    let open = match probe.state {
        PrLifecycleState::Open(open) => open,
        other => panic!("expected Open, got {other:?}"),
    };
    assert_eq!(
        open.ci,
        OpenPrCiStatus::InFlight,
        "non-LinkedIn org: Owner Approval contributes to CI as normal",
    );
    assert_eq!(probe.review, PrReviewState::Unknown);
}

/// Org matching is case-insensitive on the URL owner segment;
/// GitHub preserves casing for org slugs but the engine should
/// tolerate drift in user-supplied URLs.
#[test]
fn linkedin_org_match_is_case_insensitive() {
    let rollup = serde_json::json!([check_run("owner approval", "IN_PROGRESS", ""),]);
    let body = json_doc("OPEN", "", "MERGEABLE", "CLEAN", ("base-1", "head-1"), &[], rollup);
    let probe = parse_probe_json("https://github.com/LinkedIn-Multiproduct/mono/pull/7", &body, None).unwrap();
    let open = match probe.state {
        PrLifecycleState::Open(open) => open,
        other => panic!("expected Open, got {other:?}"),
    };
    assert_eq!(open.ci, OpenPrCiStatus::Clean);
    assert_eq!(probe.review, PrReviewState::Required);
}

/// A LinkedIn-org PR without an `Owner Approval` check at all
/// (e.g. an older PR that predates the gate) is treated as having
/// no review-signal verdict — both axes behave as normal.
#[test]
fn linkedin_org_without_owner_approval_is_unchanged() {
    let rollup = serde_json::json!([check_run("ci/build", "COMPLETED", "SUCCESS"),]);
    let body = json_doc("OPEN", "", "MERGEABLE", "CLEAN", ("base-1", "head-1"), &[], rollup);
    let probe = parse_probe_json("https://github.com/linkedin-multiproduct/mono/pull/8", &body, None).unwrap();
    let open = match probe.state {
        PrLifecycleState::Open(open) => open,
        other => panic!("expected Open, got {other:?}"),
    };
    assert_eq!(open.ci, OpenPrCiStatus::Clean);
    assert_eq!(probe.review, PrReviewState::Unknown);
}

#[test]
fn owner_from_pr_url_extracts_owner_segment() {
    assert_eq!(
        super::owner_from_pr_url("https://github.com/linkedin-multiproduct/mono/pull/1"),
        Some("linkedin-multiproduct"),
    );
    assert_eq!(
        super::owner_from_pr_url("https://github.com/spinyfin/mono/pull/568"),
        Some("spinyfin"),
    );
    assert_eq!(super::owner_from_pr_url("not-a-url"), None);
}

#[test]
fn pr_labels_opt_out_recognises_label_regardless_of_case() {
    assert!(super::pr_labels_opt_out(&["boss/no-auto-rebase".into()]));
    assert!(super::pr_labels_opt_out(&["Boss/No-Auto-Rebase".into()]));
    assert!(super::pr_labels_opt_out(&[
        "needs-review".into(),
        "BOSS/NO-AUTO-REBASE".into(),
    ]));
    assert!(!super::pr_labels_opt_out(&["needs-review".into()]));
    assert!(!super::pr_labels_opt_out(&[]));
}

/// `normalize_leaf` folds the two GraphQL rollup leaf shapes into one
/// verdict bucket. Legacy `StatusContext` leaves dispatch on `state`;
/// modern `CheckRun` leaves combine `status` + `conclusion`. Missing
/// fields must be tolerated (no panic) rather than misclassified as a
/// failure.
#[test]
fn normalize_leaf_maps_both_shapes() {
    // --- StatusContext shape (legacy commit-status: `state`, no
    // `conclusion`). ---
    let sc_success = serde_json::json!({
        "__typename": "StatusContext",
        "context": "buildkite/mono",
        "state": "SUCCESS",
    });
    assert_eq!(leaf_tag(&super::normalize_leaf(&sc_success)), "Pass");

    let sc_failure = serde_json::json!({
        "__typename": "StatusContext",
        "context": "buildkite/mono",
        "state": "FAILURE",
    });
    assert_eq!(leaf_tag(&super::normalize_leaf(&sc_failure)), "Fail:FAILURE",);

    let sc_pending = serde_json::json!({
        "__typename": "StatusContext",
        "context": "buildkite/mono",
        "state": "PENDING",
    });
    assert_eq!(leaf_tag(&super::normalize_leaf(&sc_pending)), "InFlight");

    // StatusContext detected by shape (has `state`, no `conclusion`)
    // even without `__typename`.
    let sc_no_typename = serde_json::json!({
        "context": "legacy/check",
        "state": "SUCCESS",
    });
    assert_eq!(leaf_tag(&super::normalize_leaf(&sc_no_typename)), "Pass");

    // --- CheckRun shape (`status` + `conclusion`). ---
    // In-progress with empty/absent conclusion → InFlight.
    let cr_in_progress = serde_json::json!({
        "__typename": "CheckRun",
        "name": "ci/test",
        "status": "IN_PROGRESS",
        "conclusion": serde_json::Value::Null,
    });
    assert_eq!(leaf_tag(&super::normalize_leaf(&cr_in_progress)), "InFlight",);

    // Completed + failing conclusion → Fail (uppercased, preserved).
    let cr_fail = serde_json::json!({
        "__typename": "CheckRun",
        "name": "ci/test",
        "status": "COMPLETED",
        "conclusion": "failure",
    });
    assert_eq!(leaf_tag(&super::normalize_leaf(&cr_fail)), "Fail:FAILURE");

    // Completed + passing conclusion → Pass.
    let cr_pass = serde_json::json!({
        "__typename": "CheckRun",
        "name": "ci/test",
        "status": "COMPLETED",
        "conclusion": "SUCCESS",
    });
    assert_eq!(leaf_tag(&super::normalize_leaf(&cr_pass)), "Pass");

    // Completed + unknown conclusion → InFlight (don't mis-fail).
    let cr_unknown = serde_json::json!({
        "name": "ci/test",
        "status": "COMPLETED",
        "conclusion": "MYSTERY",
    });
    assert_eq!(leaf_tag(&super::normalize_leaf(&cr_unknown)), "InFlight");

    // Entirely missing fields must not panic; an empty object has no
    // `state`/`conclusion` so it falls to the CheckRun path with an
    // empty conclusion → InFlight.
    let empty = serde_json::json!({});
    assert_eq!(leaf_tag(&super::normalize_leaf(&empty)), "InFlight");
}

/// `classify_ci` collapses the required-check leaves into a single
/// `OpenPrCiStatus`, consulting `combined_state` only when the rollup is
/// empty. The cases below pin the behaviours the engine depends on:
/// the empty-rollup fallback, fast-fail (a terminal failure surfaces
/// `Failing`), InFlight-dominates-Fail (a terminal failure mixed with an
/// in-flight check holds at `InFlight` so no moot fix worker spawns),
/// latest-leaf-per-name for re-runs, and the not-required filter.
#[test]
fn classify_ci_collapses_leaves() {
    // Empty rollup → consult combined_state.
    assert_eq!(super::classify_ci(&[], None), OpenPrCiStatus::Clean);
    assert_eq!(super::classify_ci(&[], Some("pending")), OpenPrCiStatus::InFlight,);
    assert_eq!(super::classify_ci(&[], Some("failure")), OpenPrCiStatus::InFlight,);
    assert_eq!(super::classify_ci(&[], Some("error")), OpenPrCiStatus::InFlight,);
    assert_eq!(super::classify_ci(&[], Some("success")), OpenPrCiStatus::Clean,);

    // Fast-fail: a single terminal failure surfaces `Failing`, carrying
    // the parsed provider + job id.
    let failing = [serde_json::json!({
        "name": "buildkite/mono",
        "status": "COMPLETED",
        "conclusion": "FAILURE",
        "targetUrl": "https://buildkite.com/acme/mono/builds/42#job-uuid",
        "isRequired": true,
    })];
    assert_eq!(
        super::classify_ci(&failing, None),
        OpenPrCiStatus::Failing {
            failures: vec![RequiredCheckFailure {
                name: "buildkite/mono".into(),
                conclusion: "FAILURE".into(),
                target_url: "https://buildkite.com/acme/mono/builds/42#job-uuid".into(),
                provider: CiProvider::Buildkite,
                provider_job_id: Some("job-uuid".into()),
            }],
        },
    );

    // Mixed: a terminal failure alongside an in-flight required check
    // surfaces `Failing` IMMEDIATELY (fast-fail). `Fail` dominates
    // `InFlight` for terminal failures — see the function's doc comment
    // and the T1150 regression note. Hiding a real failure until the
    // slowest check finishes defeats fast detection; anti-phantom
    // protection lives in the reconcile/withdraw path, not here.
    let mixed = [
        serde_json::json!({
            "name": "ci/test",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
            "isRequired": true,
        }),
        serde_json::json!({
            "name": "ci/lint",
            "status": "IN_PROGRESS",
            "conclusion": serde_json::Value::Null,
            "isRequired": true,
        }),
    ];
    assert_eq!(
        super::classify_ci(&mixed, None),
        OpenPrCiStatus::Failing {
            failures: vec![RequiredCheckFailure {
                name: "ci/test".into(),
                conclusion: "FAILURE".into(),
                target_url: "".into(),
                provider: CiProvider::Other,
                provider_job_id: None,
            }],
        },
    );

    // Re-runs of the same check: only the latest leaf counts. An earlier
    // FAILURE followed by a later SUCCESS for the same name → Clean.
    let rerun_cleared = [
        serde_json::json!({
            "name": "ci/test",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
            "isRequired": true,
        }),
        serde_json::json!({
            "name": "ci/test",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "isRequired": true,
        }),
    ];
    assert_eq!(super::classify_ci(&rerun_cleared, None), OpenPrCiStatus::Clean,);

    // A non-required failing check does not gate: it's filtered out, so a
    // rollup whose only required check passes is Clean.
    let optional_fail = [
        serde_json::json!({
            "name": "third-party/lint",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
            "isRequired": false,
        }),
        serde_json::json!({
            "name": "ci/test",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "isRequired": true,
        }),
    ];
    assert_eq!(super::classify_ci(&optional_fail, None), OpenPrCiStatus::Clean,);
}

/// A Trunk merge-queue eviction flips Trunk's own bookkeeping check
/// (`"Trunk Merge Queue (main)"`, posted by the `trunk-io` app) to
/// failure on the PR head. That check must never be treated as an
/// ordinary required-check failure — the Trunk-eviction path
/// (`ci_watch::on_trunk_queue_eviction_detected`, driven by
/// `TrunkQueueProbe`) is the authoritative handler for that signal, and
/// double-firing here would spawn a duplicate, misleading
/// `pr_branch_ci` remediation from a check that isn't a real CI run.
#[test]
fn classify_ci_excludes_trunk_merge_queue_check() {
    // The Trunk check is the ONLY failing check → the PR must read
    // Clean, not Failing, so `on_ci_failure_detected` never fires.
    let only_trunk_check_failing = [serde_json::json!({
        "name": "Trunk Merge Queue (main)",
        "status": "COMPLETED",
        "conclusion": "FAILURE",
        "isRequired": true,
    })];
    assert_eq!(
        super::classify_ci(&only_trunk_check_failing, None),
        OpenPrCiStatus::Clean
    );

    // A real required-check failure alongside the Trunk check still
    // surfaces Failing — but the failure list must not include the
    // Trunk check itself.
    let mixed = [
        serde_json::json!({
            "name": "Trunk Merge Queue (main)",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
            "isRequired": true,
        }),
        serde_json::json!({
            "name": "ci/test",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
            "isRequired": true,
        }),
    ];
    assert_eq!(
        super::classify_ci(&mixed, None),
        OpenPrCiStatus::Failing {
            failures: vec![RequiredCheckFailure {
                name: "ci/test".into(),
                conclusion: "FAILURE".into(),
                target_url: "".into(),
                provider: CiProvider::Other,
                provider_job_id: None,
            }],
        },
    );
}

#[test]
fn leaf_matches_check_name_empty_names_is_false() {
    // Empty `names` short-circuits to false without inspecting the leaf —
    // even a leaf that would otherwise match.
    let leaf = serde_json::json!({"name": "anything"});
    assert!(!leaf_matches_check_name(&leaf, &[]));
}

#[test]
fn leaf_matches_check_name_matches_on_name_field() {
    let leaf = serde_json::json!({"name": "Visual Review"});
    assert!(leaf_matches_check_name(&leaf, &["visual review"]));
}

#[test]
fn leaf_matches_check_name_falls_back_to_context() {
    // No `name` field → uses `context` (StatusContext shape).
    let leaf = serde_json::json!({"context": "ci/codecov"});
    assert!(leaf_matches_check_name(&leaf, &["ci/codecov"]));
}

#[test]
fn leaf_matches_check_name_is_case_insensitive() {
    let leaf = serde_json::json!({"name": "CI/Test"});
    assert!(leaf_matches_check_name(&leaf, &["ci/test"]));
}

#[test]
fn leaf_matches_check_name_empty_or_absent_name_is_false() {
    let empty_name = serde_json::json!({"name": ""});
    assert!(!leaf_matches_check_name(&empty_name, &["something"]));

    let no_name_field = serde_json::json!({"other": "x"});
    assert!(!leaf_matches_check_name(&no_name_field, &["something"]));
}

#[test]
fn leaf_matches_check_name_no_match_is_false() {
    let leaf = serde_json::json!({"name": "ci/test"});
    assert!(!leaf_matches_check_name(&leaf, &["ci/lint", "ci/build"]));
}

// ── record_conflict_class_counter (Layer 0 / T5 per-product counters) ──
