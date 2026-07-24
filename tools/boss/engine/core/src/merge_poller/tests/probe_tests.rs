use super::*;

/// Mapping table for the parser's `(raw_state × mergeable ×
/// mergeStateStatus)` rules. The truth table here mirrors the
/// design doc's Q1 classification rules and guards against future
/// tweaks rewriting them silently.
#[test]
fn parse_probe_covers_state_mergeable_status_matrix() {
    struct Case {
        label: &'static str,
        state: &'static str,
        merged_at: &'static str,
        mergeable: &'static str,
        merge_state_status: &'static str,
        base_ref_oid: &'static str,
        expect: PrLifecycleState,
        expect_base: Option<&'static str>,
    }
    let cases = [
        Case {
            label: "MERGED carries through even if mergeable is empty",
            state: "MERGED",
            merged_at: "2026-05-09T12:00:00Z",
            mergeable: "",
            merge_state_status: "",
            base_ref_oid: "abc",
            expect: PrLifecycleState::Merged,
            expect_base: Some("abc"),
        },
        Case {
            label: "non-empty mergedAt overrides state=OPEN (edge: GH lag)",
            state: "OPEN",
            merged_at: "2026-05-09T12:00:00Z",
            mergeable: "MERGEABLE",
            merge_state_status: "CLEAN",
            base_ref_oid: "abc",
            expect: PrLifecycleState::Merged,
            expect_base: Some("abc"),
        },
        Case {
            label: "CLOSED without merged falls to ClosedUnmerged",
            state: "CLOSED",
            merged_at: "",
            mergeable: "",
            merge_state_status: "",
            base_ref_oid: "abc",
            expect: PrLifecycleState::ClosedUnmerged,
            expect_base: Some("abc"),
        },
        Case {
            label: "OPEN + MERGEABLE/CLEAN is Clean",
            state: "OPEN",
            merged_at: "",
            mergeable: "MERGEABLE",
            merge_state_status: "CLEAN",
            base_ref_oid: "abc",
            expect: PrLifecycleState::Open(OpenPrStatus::clean()),
            expect_base: Some("abc"),
        },
        Case {
            label: "OPEN + CONFLICTING/DIRTY is Conflict",
            state: "OPEN",
            merged_at: "",
            mergeable: "CONFLICTING",
            merge_state_status: "DIRTY",
            base_ref_oid: "abc",
            expect: PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            expect_base: Some("abc"),
        },
        Case {
            label: "CONFLICTING without DIRTY status falls to Clean (lag protection)",
            state: "OPEN",
            merged_at: "",
            mergeable: "CONFLICTING",
            merge_state_status: "UNKNOWN",
            base_ref_oid: "abc",
            expect: PrLifecycleState::Open(OpenPrStatus::clean()),
            expect_base: Some("abc"),
        },
        Case {
            label: "DIRTY without CONFLICTING falls to Clean (lag protection)",
            state: "OPEN",
            merged_at: "",
            mergeable: "MERGEABLE",
            merge_state_status: "DIRTY",
            base_ref_oid: "abc",
            expect: PrLifecycleState::Open(OpenPrStatus::clean()),
            expect_base: Some("abc"),
        },
        Case {
            label: "UNKNOWN mergeable maps to Unknown (indeterminate; skip conflict transitions)",
            state: "OPEN",
            merged_at: "",
            mergeable: "UNKNOWN",
            merge_state_status: "UNKNOWN",
            base_ref_oid: "abc",
            expect: PrLifecycleState::Open(OpenPrStatus::unknown_mergeability()),
            expect_base: Some("abc"),
        },
        Case {
            label: "BEHIND is mergeable; not a conflict",
            state: "OPEN",
            merged_at: "",
            mergeable: "MERGEABLE",
            merge_state_status: "BEHIND",
            base_ref_oid: "abc",
            expect: PrLifecycleState::Open(OpenPrStatus::clean()),
            expect_base: Some("abc"),
        },
        Case {
            label: "empty base ref is None",
            state: "OPEN",
            merged_at: "",
            mergeable: "MERGEABLE",
            merge_state_status: "CLEAN",
            base_ref_oid: "",
            expect: PrLifecycleState::Open(OpenPrStatus::clean()),
            expect_base: None,
        },
    ];
    for case in cases {
        let body = json_doc(
            case.state,
            case.merged_at,
            case.mergeable,
            case.merge_state_status,
            (case.base_ref_oid, ""),
            &[],
            serde_json::json!([]),
        );
        let probe = parse_probe_json("https://example.test/pr/1", &body, None).unwrap();
        assert_eq!(
            probe.state, case.expect,
            "case `{}`: state mismatch (body: {:?})",
            case.label, body,
        );
        assert_eq!(
            probe.base_ref_oid.as_deref(),
            case.expect_base,
            "case `{}`: base_ref_oid mismatch",
            case.label,
        );
        assert!(
            probe.labels.is_empty(),
            "case `{}`: labels mismatch (none expected)",
            case.label,
        );
    }
}

/// Labels arrive as an array of `{name, …}` objects from gh. Empty
/// stays empty; the conflict-watch opt-out uses these to honour
/// the per-PR `boss/no-auto-rebase` label.
#[test]
fn parse_probe_parses_labels_column() {
    let body = json_doc(
        "OPEN",
        "",
        "MERGEABLE",
        "CLEAN",
        ("abc", ""),
        &["needs-review", "boss/no-auto-rebase"],
        serde_json::json!([]),
    );
    let probe = parse_probe_json("https://example.test/pr/2", &body, None).unwrap();
    assert_eq!(
        probe.labels,
        vec!["needs-review".to_owned(), "boss/no-auto-rebase".to_owned()],
    );

    let body_empty = json_doc(
        "OPEN",
        "",
        "MERGEABLE",
        "CLEAN",
        ("abc", ""),
        &[],
        serde_json::json!([]),
    );
    let probe_empty = parse_probe_json("https://example.test/pr/3", &body_empty, None).unwrap();
    assert!(probe_empty.labels.is_empty());
}

/// [`flatten_batched_pr_node`] reshapes the batched GraphQL response's
/// `{ nodes: [...] }` connections (labels/reviews/statusCheckRollup)
/// into the flat arrays `gh pr view --json` produces, so
/// [`parse_probe_json`] can consume either shape identically. This
/// pins that reshaping and round-trips the result through
/// `parse_probe_json` to confirm the two probe paths agree.
#[test]
fn flatten_batched_pr_node_matches_gh_pr_view_shape() {
    let node = serde_json::json!({
        "state": "OPEN",
        "mergedAt": null,
        "closedAt": null,
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "baseRefOid": "abc",
        "headRefOid": "def",
        "headRefName": "feature",
        "baseRefName": "main",
        "labels": { "nodes": [{ "name": "needs-review" }, { "name": "boss/no-auto-rebase" }] },
        "reviewDecision": "APPROVED",
        "reviews": { "nodes": [{ "author": { "login": "alice" }, "state": "APPROVED" }] },
        "mergeQueueEntry": null,
        "commits": {
            "nodes": [{
                "commit": {
                    "statusCheckRollup": {
                        "contexts": {
                            "nodes": [{
                                "__typename": "CheckRun",
                                "name": "bazel-test",
                                "status": "COMPLETED",
                                "conclusion": "SUCCESS",
                                "detailsUrl": "https://github.com/o/r/actions/runs/1/job/2",
                            }]
                        }
                    }
                }
            }]
        },
    });
    let flat = flatten_batched_pr_node(&node);
    assert_eq!(
        flat["labels"],
        serde_json::json!([{ "name": "needs-review" }, { "name": "boss/no-auto-rebase" }])
    );
    assert_eq!(
        flat["reviews"],
        serde_json::json!([{ "author": { "login": "alice" }, "state": "APPROVED" }])
    );
    assert_eq!(flat["statusCheckRollup"][0]["name"], "bazel-test");
    assert_eq!(flat["mergeable"], "MERGEABLE");

    let probe = parse_probe_json("https://github.com/o/r/pull/1", &flat.to_string(), None).unwrap();
    assert_eq!(probe.state, PrLifecycleState::Open(OpenPrStatus::clean()));
    assert_eq!(
        probe.labels,
        vec!["needs-review".to_owned(), "boss/no-auto-rebase".to_owned()]
    );
    assert_eq!(
        probe.review,
        PrReviewState::Approved {
            reviewers: vec!["alice".to_owned()]
        }
    );
    assert!(!probe.in_merge_queue);
}

/// A PR with no commits (or an empty check-run rollup) must flatten to
/// an empty `statusCheckRollup` array rather than panicking on the
/// missing `commits.nodes[0]` — mirrors a brand-new PR with no CI yet.
#[test]
fn flatten_batched_pr_node_handles_missing_commits() {
    let node = serde_json::json!({
        "state": "OPEN",
        "mergedAt": null,
        "closedAt": null,
        "mergeable": "UNKNOWN",
        "mergeStateStatus": "UNKNOWN",
        "baseRefOid": null,
        "headRefOid": null,
        "headRefName": null,
        "baseRefName": null,
        "labels": { "nodes": [] },
        "reviewDecision": null,
        "reviews": { "nodes": [] },
        "mergeQueueEntry": { "state": "QUEUED" },
        "commits": { "nodes": [] },
    });
    let flat = flatten_batched_pr_node(&node);
    assert_eq!(flat["statusCheckRollup"], serde_json::json!([]));
    assert_eq!(flat["labels"], serde_json::json!([]));

    let probe = parse_probe_json("https://github.com/o/r/pull/2", &flat.to_string(), None).unwrap();
    assert!(
        probe.in_merge_queue,
        "mergeQueueEntry passed through non-null -> in queue"
    );
}

/// A batch consisting entirely of non-canonical PR URLs must fail
/// fast, per-URL, without ever shelling out to `gh` — exercises the
/// `order.is_empty()` early return so a bad URL can't block the whole
/// pass on a subprocess call that was never going to succeed.
#[tokio::test]
async fn probe_batch_via_graphql_rejects_non_canonical_urls_without_a_network_call() {
    let urls = vec!["not-a-pr-url".to_owned(), "https://example.com/o/r/pull/1".to_owned()];
    let out = CommandMergeProbe::new().probe_batch_via_graphql(&urls).await;
    assert_eq!(out.len(), 2);
    assert!(out["not-a-pr-url"].is_err());
    assert!(out["https://example.com/o/r/pull/1"].is_err());
}

/// Two PRs in one repo plus one PR in a second repo must group into
/// exactly two `repository(...)` aliases, with each PR getting its own
/// `pullRequest(...)` alias nested inside the right repo block, and the
/// alias map must let the response walk find each URL back by those
/// same aliases.
#[test]
fn build_batch_query_groups_multiple_prs_per_repo_and_multiple_repos() {
    let urls = vec![
        "https://github.com/acme/widgets/pull/1".to_owned(),
        "https://github.com/acme/widgets/pull/2".to_owned(),
        "https://github.com/acme/gadgets/pull/7".to_owned(),
    ];
    let mut parsed: HashMap<String, (String, String, u64)> = HashMap::new();
    parsed.insert(urls[0].clone(), ("acme".to_owned(), "widgets".to_owned(), 1));
    parsed.insert(urls[1].clone(), ("acme".to_owned(), "widgets".to_owned(), 2));
    parsed.insert(urls[2].clone(), ("acme".to_owned(), "gadgets".to_owned(), 7));

    let (query, alias_map) = build_batch_query(&urls, &parsed, PR_PROBE_FIELDS);

    // BTreeMap ordering of (owner, repo) puts "gadgets" before "widgets".
    assert_eq!(alias_map.len(), 2, "two distinct repos -> two repo aliases");
    let (gadgets_alias, gadgets_prs) = &alias_map[0];
    let (widgets_alias, widgets_prs) = &alias_map[1];
    assert_eq!(gadgets_alias, "repo0");
    assert_eq!(widgets_alias, "repo1");
    assert_eq!(gadgets_prs.len(), 1, "one PR in the gadgets repo");
    assert_eq!(widgets_prs.len(), 2, "two PRs in the widgets repo");
    assert_eq!(gadgets_prs[0], ("pr0".to_owned(), urls[2].clone()));
    assert_eq!(widgets_prs[0], ("pr0".to_owned(), urls[0].clone()));
    assert_eq!(widgets_prs[1], ("pr1".to_owned(), urls[1].clone()));

    // The query text itself must reference every alias and PR number.
    assert!(query.contains("repo0: repository(owner: \"acme\", name: \"gadgets\")"));
    assert!(query.contains("repo1: repository(owner: \"acme\", name: \"widgets\")"));
    assert!(query.contains("pr0: pullRequest(number: 7)"));
    assert!(query.contains("pr0: pullRequest(number: 1)"));
    assert!(query.contains("pr1: pullRequest(number: 2)"));
}

/// The response walk must find each PR node back out by the aliases
/// `build_batch_query` produced, and must surface `None` (rather than
/// panicking or defaulting to `Some`) for a null node — the shape
/// GitHub returns for a force-deleted/transferred PR — while a
/// populated sibling PR in the same batch is unaffected.
#[test]
fn walk_batch_response_finds_nodes_by_alias_and_flags_null_nodes() {
    let alias_map: BatchAliasMap = vec![
        (
            "repo0".to_owned(),
            vec![
                ("pr0".to_owned(), "https://github.com/acme/widgets/pull/1".to_owned()),
                ("pr1".to_owned(), "https://github.com/acme/widgets/pull/2".to_owned()),
            ],
        ),
        (
            "repo1".to_owned(),
            vec![("pr0".to_owned(), "https://github.com/acme/gadgets/pull/7".to_owned())],
        ),
    ];
    let body = serde_json::json!({
        "data": {
            "repo0": {
                "pr0": { "state": "OPEN" },
                "pr1": null,
            },
            "repo1": {
                "pr0": { "state": "MERGED" },
            },
        }
    });

    let walked = walk_batch_response(&body, &alias_map);
    assert_eq!(walked.len(), 3);
    let by_url: HashMap<String, Option<&serde_json::Value>> = walked.into_iter().collect();

    assert_eq!(
        by_url["https://github.com/acme/widgets/pull/1"].map(|v| v["state"].clone()),
        Some(serde_json::json!("OPEN"))
    );
    assert_eq!(
        by_url["https://github.com/acme/widgets/pull/2"], None,
        "null node -> None"
    );
    assert_eq!(
        by_url["https://github.com/acme/gadgets/pull/7"].map(|v| v["state"].clone()),
        Some(serde_json::json!("MERGED"))
    );
}

/// `(state × mergeability × ci-leaf-set × combined-state)` matrix for
/// the CI predicate. Exercises the latest-leaf-per-name collapse, the
/// required/not-required filter, the closed conclusion set from design
/// §Q1 / Phase 8 #21, and the combined-commit-status fallback used to
/// surface EXPECTED (not-yet-submitted) required checks.
#[test]
fn parse_probe_covers_ci_leaf_set_matrix() {
    struct Case {
        label: &'static str,
        rollup: serde_json::Value,
        /// Simulates the legacy commit-status combined state returned by
        /// `GET /repos/{owner}/{repo}/commits/{sha}/status`.
        combined_state: Option<&'static str>,
        expect_ci: OpenPrCiStatus,
    }
    let failing_check = |name: &'static str, conclusion: &'static str, target: &'static str| {
        serde_json::json!({
            "name": name,
            "status": "COMPLETED",
            "conclusion": conclusion,
            "targetUrl": target,
            "isRequired": true,
        })
    };
    let success_check = |name: &'static str| {
        serde_json::json!({
            "name": name,
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "isRequired": true,
        })
    };
    let cases = [
        Case {
            label: "no rollup, no combined state → Clean (no CI configured)",
            rollup: serde_json::json!([]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Clean,
        },
        Case {
            label: "no rollup + combined pending → InFlight (EXPECTED checks not yet submitted)",
            rollup: serde_json::json!([]),
            combined_state: Some("pending"),
            expect_ci: OpenPrCiStatus::InFlight,
        },
        Case {
            label: "no rollup + combined success → Clean (no required checks)",
            rollup: serde_json::json!([]),
            combined_state: Some("success"),
            expect_ci: OpenPrCiStatus::Clean,
        },
        Case {
            label: "no rollup + combined failure → InFlight (conservative; no check details yet)",
            rollup: serde_json::json!([]),
            combined_state: Some("failure"),
            expect_ci: OpenPrCiStatus::InFlight,
        },
        Case {
            label: "all required checks SUCCESS → Clean",
            rollup: serde_json::json!([success_check("ci/build"), success_check("ci/test")]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Clean,
        },
        Case {
            label: "one required check FAILURE → Failing",
            rollup: serde_json::json!([success_check("ci/build"), failing_check("ci/test", "FAILURE", ""),]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "ci/test".into(),
                    conclusion: "FAILURE".into(),
                    target_url: "".into(),
                    provider: CiProvider::Other,
                    provider_job_id: None,
                }],
            },
        },
        Case {
            label: "later leaf wins for the same name (re-run success masks earlier FAILURE)",
            rollup: serde_json::json!([failing_check("ci/test", "FAILURE", ""), success_check("ci/test"),]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Clean,
        },
        Case {
            label: "later leaf wins for the same name (re-run FAILURE masks earlier success)",
            rollup: serde_json::json!([success_check("ci/test"), failing_check("ci/test", "FAILURE", ""),]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "ci/test".into(),
                    conclusion: "FAILURE".into(),
                    target_url: "".into(),
                    provider: CiProvider::Other,
                    provider_job_id: None,
                }],
            },
        },
        Case {
            label: "non-required failing check is ignored",
            rollup: serde_json::json!([
                {
                    "name": "third-party/lint",
                    "status": "COMPLETED",
                    "conclusion": "FAILURE",
                    "isRequired": false,
                },
                success_check("ci/test"),
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Clean,
        },
        Case {
            label: "required check IN_PROGRESS → InFlight (we wait)",
            rollup: serde_json::json!([
                {
                    "name": "ci/test",
                    "status": "IN_PROGRESS",
                    "conclusion": serde_json::Value::Null,
                    "isRequired": true,
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::InFlight,
        },
        Case {
            label: "STARTUP_FAILURE counts as failure (engine pre-triages to retrigger)",
            rollup: serde_json::json!([failing_check("ci/build", "STARTUP_FAILURE", "")]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "ci/build".into(),
                    conclusion: "STARTUP_FAILURE".into(),
                    target_url: "".into(),
                    provider: CiProvider::Other,
                    provider_job_id: None,
                }],
            },
        },
        Case {
            label: "TIMED_OUT counts as failure",
            rollup: serde_json::json!([failing_check("ci/test", "TIMED_OUT", "")]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "ci/test".into(),
                    conclusion: "TIMED_OUT".into(),
                    target_url: "".into(),
                    provider: CiProvider::Other,
                    provider_job_id: None,
                }],
            },
        },
        Case {
            label: "NEUTRAL and SKIPPED are passes (don't gate merge)",
            rollup: serde_json::json!([
                {
                    "name": "ci/changelog",
                    "status": "COMPLETED",
                    "conclusion": "NEUTRAL",
                    "isRequired": true,
                },
                {
                    "name": "ci/coverage",
                    "status": "COMPLETED",
                    "conclusion": "SKIPPED",
                    "isRequired": true,
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Clean,
        },
        // Fast-fail: a terminally-failed required check surfaces `Failing`
        // immediately, even while another required check is still running.
        // This is a prior regression fix: it had these two cases
        // returning `InFlight`, which hid real failures until the slowest
        // check finished. If a future change reintroduces the "wait for
        // all terminal" gate, these cases will fail and catch the regression.
        Case {
            label: "mixed: terminal failure + in-flight → Failing immediately (fast-fail)",
            rollup: serde_json::json!([
                failing_check("ci/test", "FAILURE", ""),
                {
                    "name": "ci/lint",
                    "status": "IN_PROGRESS",
                    "conclusion": serde_json::Value::Null,
                    "isRequired": true,
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "ci/test".into(),
                    conclusion: "FAILURE".into(),
                    target_url: "".into(),
                    provider: CiProvider::Other,
                    provider_job_id: None,
                }],
            },
        },
        Case {
            label: "mixed: terminal failure + queued → Failing immediately (fast-fail)",
            rollup: serde_json::json!([
                failing_check("ci/test", "FAILURE", ""),
                {
                    "name": "ci/lint",
                    "status": "QUEUED",
                    "conclusion": serde_json::Value::Null,
                    "isRequired": true,
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "ci/test".into(),
                    conclusion: "FAILURE".into(),
                    target_url: "".into(),
                    provider: CiProvider::Other,
                    provider_job_id: None,
                }],
            },
        },
        // Regression test for the exact prior-regression scenario: a fast check
        // (e.g. checkleft in 4s) fails terminally while a slow check
        // (e.g. bazel-test) is still running. Must surface Failing at once.
        Case {
            label: "fast-check terminal fail + slow check running → Failing (prior regression)",
            rollup: serde_json::json!([
                failing_check("buildkite/mono/checks", "FAILURE", "https://buildkite.com/acme/mono/builds/99"),
                {
                    "name": "buildkite/mono/bazel-test",
                    "status": "IN_PROGRESS",
                    "conclusion": serde_json::Value::Null,
                    "isRequired": true,
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "buildkite/mono/checks".into(),
                    conclusion: "FAILURE".into(),
                    target_url: "https://buildkite.com/acme/mono/builds/99".into(),
                    provider: CiProvider::Buildkite,
                    provider_job_id: None,
                }],
            },
        },
        Case {
            label: "all terminal, one failure → Failing (terminal gate satisfied)",
            rollup: serde_json::json!([success_check("ci/lint"), failing_check("ci/test", "FAILURE", ""),]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "ci/test".into(),
                    conclusion: "FAILURE".into(),
                    target_url: "".into(),
                    provider: CiProvider::Other,
                    provider_job_id: None,
                }],
            },
        },
        Case {
            label: "Buildkite target URL → provider inferred",
            rollup: serde_json::json!([failing_check(
                "buildkite/mono",
                "FAILURE",
                "https://buildkite.com/anthropic/mono/builds/42#01h-job-uuid",
            )]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "buildkite/mono".into(),
                    conclusion: "FAILURE".into(),
                    target_url: "https://buildkite.com/anthropic/mono/builds/42#01h-job-uuid".into(),
                    provider: CiProvider::Buildkite,
                    provider_job_id: Some("01h-job-uuid".into()),
                }],
            },
        },
        Case {
            label: "GitHub Actions target URL → provider inferred",
            rollup: serde_json::json!([failing_check(
                "gha/build",
                "FAILURE",
                "https://github.com/anthropic/mono/actions/runs/12345/job/67890",
            )]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "gha/build".into(),
                    conclusion: "FAILURE".into(),
                    target_url: "https://github.com/anthropic/mono/actions/runs/12345/job/67890".into(),
                    provider: CiProvider::GithubActions,
                    provider_job_id: Some("67890".into()),
                }],
            },
        },
        // ---- StatusContext leaf shape (legacy commit-status API,
        // used by Buildkite and other CI integrations). These
        // leaves carry `context` + `state` and have NO `status` or
        // `conclusion` field. Pre-fix the parser silently classified
        // every StatusContext leaf as InFlight; the next four cases
        // pin the StatusContext code path so a future regression
        // shows up as a test failure rather than a stuck yellow
        // clock on every chore card.
        Case {
            label: "StatusContext: all SUCCESS → Clean (Buildkite-style rollup)",
            rollup: serde_json::json!([
                {
                    "__typename": "StatusContext",
                    "context": "buildkite/mono",
                    "state": "SUCCESS",
                    "targetUrl": "https://buildkite.com/flunge/mono/builds/91",
                },
                {
                    "__typename": "StatusContext",
                    "context": "buildkite/mono/checks",
                    "state": "SUCCESS",
                    "targetUrl": "https://buildkite.com/flunge/mono/builds/91#abc",
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Clean,
        },
        Case {
            label: "StatusContext: PENDING → InFlight",
            rollup: serde_json::json!([
                {
                    "__typename": "StatusContext",
                    "context": "buildkite/mono",
                    "state": "PENDING",
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::InFlight,
        },
        Case {
            label: "StatusContext: FAILURE → Failing",
            rollup: serde_json::json!([
                {
                    "__typename": "StatusContext",
                    "context": "buildkite/mono",
                    "state": "FAILURE",
                    "targetUrl": "https://buildkite.com/flunge/mono/builds/91#019e",
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "buildkite/mono".into(),
                    conclusion: "FAILURE".into(),
                    target_url: "https://buildkite.com/flunge/mono/builds/91#019e".into(),
                    provider: CiProvider::Buildkite,
                    provider_job_id: Some("019e".into()),
                }],
            },
        },
        Case {
            label: "StatusContext: ERROR is a failure (legacy commit-status crash state)",
            rollup: serde_json::json!([
                {
                    "__typename": "StatusContext",
                    "context": "buildkite/mono",
                    "state": "ERROR",
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    name: "buildkite/mono".into(),
                    conclusion: "ERROR".into(),
                    target_url: "".into(),
                    provider: CiProvider::Other,
                    provider_job_id: None,
                }],
            },
        },
        Case {
            label: "Mixed CheckRun + StatusContext, all green → Clean",
            rollup: serde_json::json!([
                success_check("ci/build"),
                {
                    "__typename": "StatusContext",
                    "context": "buildkite/mono",
                    "state": "SUCCESS",
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Clean,
        },
        Case {
            label: "StatusContext: SUCCESS without __typename (defensive fallback)",
            rollup: serde_json::json!([
                {
                    "context": "legacy/check",
                    "state": "SUCCESS",
                },
            ]),
            combined_state: None,
            expect_ci: OpenPrCiStatus::Clean,
        },
    ];
    for case in cases {
        let body = json_doc(
            "OPEN",
            "",
            "MERGEABLE",
            "CLEAN",
            ("abc", "head-1"),
            &[],
            case.rollup.clone(),
        );
        let probe = parse_probe_json("https://example.test/pr/ci", &body, case.combined_state).unwrap();
        let actual_ci = match probe.state {
            PrLifecycleState::Open(OpenPrStatus { ci, .. }) => ci,
            other => panic!("case `{}`: expected Open, got {other:?}", case.label),
        };
        assert_eq!(actual_ci, case.expect_ci, "case `{}`: CI status mismatch", case.label,);
    }
}

/// GitHub's `commits/{sha}/status` endpoint returns `state:"pending"`
/// for a commit with zero recorded statuses. Without filtering on
/// `total_count` the empty-rollup PR card would render a stuck yellow
/// "waiting for CI" icon for repos that have no checks configured at
/// all. The helper must collapse that case to `None`, which the
/// caller folds into `OpenPrCiStatus::Clean`.
#[test]
fn parse_combined_status_zero_total_count_returns_none() {
    let body = serde_json::json!({"state": "pending", "total_count": 0}).to_string();
    assert_eq!(parse_combined_status_response(&body), None);
}

#[test]
fn parse_combined_status_surfaces_state_when_count_positive() {
    let cases = [
        ("pending", "pending"),
        ("PENDING", "pending"),
        ("success", "success"),
        ("failure", "failure"),
        ("error", "error"),
    ];
    for (input, expected) in cases {
        let body = serde_json::json!({"state": input, "total_count": 1}).to_string();
        assert_eq!(
            parse_combined_status_response(&body),
            Some(expected.to_string()),
            "state={input}",
        );
    }
}

#[test]
fn parse_combined_status_handles_missing_or_empty_fields() {
    // Missing total_count defaults to 0 → treat as no checks.
    let no_count = serde_json::json!({"state": "pending"}).to_string();
    assert_eq!(parse_combined_status_response(&no_count), None);

    // Empty state with positive count → None (defensive).
    let empty_state = serde_json::json!({"state": "", "total_count": 2}).to_string();
    assert_eq!(parse_combined_status_response(&empty_state), None);

    // Malformed JSON → None.
    assert_eq!(parse_combined_status_response("not json"), None);
}

// ── ETag conditional-request plumbing ─────────────────────────────
//
// `gh api -i` mixes line endings: its own status line is `\n`-terminated
// while the header block it copies from the raw HTTP response is
// `\r\n`-terminated. These fixtures preserve that so the parser is
// exercised against the real shape, not an idealized one.

#[test]
fn parse_include_response_extracts_status_and_etag_on_200() {
    let stdout = gh_api_include_body(
        "HTTP/2.0 200 OK",
        &[
            ("Etag", "W/\"abc123\""),
            ("Content-Type", "application/json; charset=utf-8"),
        ],
        "{\"state\":\"success\",\"total_count\":1}",
    );
    let (status, etag, body) = parse_include_response(&stdout).unwrap();
    assert_eq!(status, 200);
    assert_eq!(etag, Some("W/\"abc123\"".to_owned()));
    assert_eq!(body, "{\"state\":\"success\",\"total_count\":1}");
}

#[test]
fn parse_include_response_handles_304_with_no_body() {
    let stdout = gh_api_include_body("HTTP/2.0 304 Not Modified", &[("Etag", "\"abc123\"")], "");
    let (status, etag, body) = parse_include_response(&stdout).unwrap();
    assert_eq!(status, 304);
    assert_eq!(etag, Some("\"abc123\"".to_owned()));
    assert_eq!(body, "");
}

#[test]
fn parse_include_response_is_case_insensitive_on_etag_header_name() {
    let stdout = gh_api_include_body("HTTP/2.0 200 OK", &[("ETAG", "\"xyz\"")], "{}");
    assert_eq!(parse_include_response(&stdout).unwrap().1, Some("\"xyz\"".to_owned()));
}

#[test]
fn parse_include_response_missing_etag_header_yields_none() {
    let stdout = gh_api_include_body("HTTP/2.0 200 OK", &[("Content-Type", "application/json")], "{}");
    assert_eq!(parse_include_response(&stdout).unwrap().1, None);
}

#[test]
fn parse_include_response_garbled_output_yields_none() {
    assert_eq!(parse_include_response(""), None);
    assert_eq!(parse_include_response("not an http response"), None);
}

#[test]
fn classify_conditional_output_maps_200_to_modified_with_etag() {
    let stdout = gh_api_include_body("HTTP/2.0 200 OK", &[("Etag", "\"v1\"")], "{\"state\":\"pending\"}");
    let output = std::process::Output {
        status: test_exit_status(0),
        stdout: stdout.into_bytes(),
        stderr: Vec::new(),
    };
    assert_eq!(
        classify_conditional_output(&output),
        ConditionalGetOutcome::Modified {
            body: "{\"state\":\"pending\"}".to_owned(),
            etag: Some("\"v1\"".to_owned()),
        },
    );
}

#[test]
fn classify_conditional_output_maps_304_to_not_modified_despite_nonzero_exit() {
    // `gh` exits non-zero on a 304 (any non-2xx is an error to it), so
    // the classifier must key off the parsed status line, not `status`.
    let stdout = gh_api_include_body("HTTP/2.0 304 Not Modified", &[("Etag", "\"v1\"")], "");
    let output = std::process::Output {
        status: test_exit_status(1),
        stdout: stdout.into_bytes(),
        stderr: b"gh: HTTP 304".to_vec(),
    };
    assert_eq!(classify_conditional_output(&output), ConditionalGetOutcome::NotModified);
}

#[test]
fn classify_conditional_output_maps_other_status_to_failed() {
    let stdout = gh_api_include_body("HTTP/2.0 500 Internal Server Error", &[], "oops");
    let output = std::process::Output {
        status: test_exit_status(1),
        stdout: stdout.into_bytes(),
        stderr: Vec::new(),
    };
    assert_eq!(classify_conditional_output(&output), ConditionalGetOutcome::Failed);
}

#[test]
fn classify_conditional_output_unparseable_stdout_is_failed() {
    let output = std::process::Output {
        status: test_exit_status(1),
        stdout: Vec::new(),
        stderr: b"gh: could not connect".to_vec(),
    };
    assert_eq!(classify_conditional_output(&output), ConditionalGetOutcome::Failed);
}

#[test]
fn resolve_and_cache_not_modified_replays_cached_state_without_mutating_cache() {
    let cache: std::sync::Mutex<HashMap<String, CachedCommitStatus>> = std::sync::Mutex::new(HashMap::from([(
        "repos/o/r/commits/sha1/status".to_owned(),
        CachedCommitStatus {
            etag: "\"v1\"".to_owned(),
            state: Some("success".to_owned()),
        },
    )]));
    let resolved = resolve_and_cache_combined_state(
        &cache,
        "repos/o/r/commits/sha1/status",
        ConditionalGetOutcome::NotModified,
    );
    assert_eq!(resolved, Some("success".to_owned()));
    // Cache entry is untouched by a replay.
    let entry = cache
        .lock()
        .unwrap()
        .get("repos/o/r/commits/sha1/status")
        .cloned()
        .unwrap();
    assert_eq!(entry.etag, "\"v1\"");
}

#[test]
fn resolve_and_cache_not_modified_with_no_prior_entry_returns_none() {
    let cache: std::sync::Mutex<HashMap<String, CachedCommitStatus>> = std::sync::Mutex::new(HashMap::new());
    let resolved = resolve_and_cache_combined_state(
        &cache,
        "repos/o/r/commits/sha1/status",
        ConditionalGetOutcome::NotModified,
    );
    assert_eq!(resolved, None);
}

#[test]
fn resolve_and_cache_modified_parses_body_and_refreshes_etag() {
    let cache: std::sync::Mutex<HashMap<String, CachedCommitStatus>> = std::sync::Mutex::new(HashMap::from([(
        "repos/o/r/commits/sha1/status".to_owned(),
        CachedCommitStatus {
            etag: "\"stale\"".to_owned(),
            state: Some("pending".to_owned()),
        },
    )]));
    let outcome = ConditionalGetOutcome::Modified {
        body: serde_json::json!({"state": "failure", "total_count": 1}).to_string(),
        etag: Some("\"fresh\"".to_owned()),
    };
    let resolved = resolve_and_cache_combined_state(&cache, "repos/o/r/commits/sha1/status", outcome);
    assert_eq!(resolved, Some("failure".to_owned()));
    let entry = cache
        .lock()
        .unwrap()
        .get("repos/o/r/commits/sha1/status")
        .cloned()
        .unwrap();
    assert_eq!(entry.etag, "\"fresh\"");
    assert_eq!(entry.state, Some("failure".to_owned()));
}

#[test]
fn resolve_and_cache_modified_without_etag_leaves_prior_cache_entry_in_place() {
    // No ETag on the response means there's nothing usable to send as
    // `If-None-Match` next time, so the stale cache entry (if any) is
    // left alone rather than wiped or refreshed with an empty etag.
    let cache: std::sync::Mutex<HashMap<String, CachedCommitStatus>> = std::sync::Mutex::new(HashMap::from([(
        "repos/o/r/commits/sha1/status".to_owned(),
        CachedCommitStatus {
            etag: "\"v1\"".to_owned(),
            state: Some("pending".to_owned()),
        },
    )]));
    let outcome = ConditionalGetOutcome::Modified {
        body: serde_json::json!({"state": "success", "total_count": 1}).to_string(),
        etag: None,
    };
    let resolved = resolve_and_cache_combined_state(&cache, "repos/o/r/commits/sha1/status", outcome);
    assert_eq!(resolved, Some("success".to_owned()));
    let entry = cache
        .lock()
        .unwrap()
        .get("repos/o/r/commits/sha1/status")
        .cloned()
        .unwrap();
    assert_eq!(entry.etag, "\"v1\"", "stale etag must survive an etag-less refresh");
}

#[test]
fn resolve_and_cache_failed_returns_none_and_does_not_mutate_cache() {
    let cache: std::sync::Mutex<HashMap<String, CachedCommitStatus>> = std::sync::Mutex::new(HashMap::from([(
        "repos/o/r/commits/sha1/status".to_owned(),
        CachedCommitStatus {
            etag: "\"v1\"".to_owned(),
            state: Some("success".to_owned()),
        },
    )]));
    let resolved =
        resolve_and_cache_combined_state(&cache, "repos/o/r/commits/sha1/status", ConditionalGetOutcome::Failed);
    assert_eq!(resolved, None);
    let entry = cache
        .lock()
        .unwrap()
        .get("repos/o/r/commits/sha1/status")
        .cloned()
        .unwrap();
    assert_eq!(
        entry.etag, "\"v1\"",
        "a failed fetch must not clobber a good cache entry"
    );
}

/// Conflict pre-empts CI in the joint state (design §Q1 dispatch
/// table); the parser still surfaces both axes so callers can
/// inspect either. The merge_poller sweep only acts on the conflict
/// axis when both fire, but the probe doesn't lose data.
#[test]
fn parse_probe_surfaces_conflict_and_ci_failure_together() {
    let body = json_doc(
        "OPEN",
        "",
        "CONFLICTING",
        "DIRTY",
        ("base-1", "head-1"),
        &[],
        serde_json::json!([{
            "name": "ci/test",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
            "isRequired": true,
        }]),
    );
    let probe = parse_probe_json("https://example.test/pr/both", &body, None).unwrap();
    let open = match probe.state {
        PrLifecycleState::Open(open) => open,
        other => panic!("expected Open, got {other:?}"),
    };
    assert_eq!(open.mergeability, OpenPrMergeability::Conflict);
    assert!(
        matches!(open.ci, OpenPrCiStatus::Failing { .. }),
        "ci must remain Failing alongside Conflict; got {:?}",
        open.ci,
    );
    assert_eq!(probe.head_ref_oid.as_deref(), Some("head-1"));
}

/// `mergeQueueEntry` field: non-null → `in_merge_queue = true`,
/// null / absent → `in_merge_queue = false`.
#[test]
fn parse_probe_detects_merge_queue_entry() {
    // PR in merge queue — mergeQueueEntry is a non-null object.
    let body_in_queue = {
        let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
            "OPEN",
            "",
            "MERGEABLE",
            "CLEAN",
            ("", ""),
            &[],
            serde_json::json!([]),
        ))
        .unwrap();
        doc["mergeQueueEntry"] = serde_json::json!({"state": "QUEUED"});
        doc.to_string()
    };
    let probe = parse_probe_json("https://example.test/pr/mq1", &body_in_queue, None).unwrap();
    assert!(
        probe.in_merge_queue,
        "non-null mergeQueueEntry should set in_merge_queue"
    );

    // PR not in merge queue — mergeQueueEntry is JSON null.
    let body_null = {
        let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
            "OPEN",
            "",
            "MERGEABLE",
            "CLEAN",
            ("", ""),
            &[],
            serde_json::json!([]),
        ))
        .unwrap();
        doc["mergeQueueEntry"] = serde_json::Value::Null;
        doc.to_string()
    };
    let probe_null = parse_probe_json("https://example.test/pr/mq2", &body_null, None).unwrap();
    assert!(
        !probe_null.in_merge_queue,
        "null mergeQueueEntry should clear in_merge_queue"
    );

    // PR not in merge queue — mergeQueueEntry field absent entirely
    // (older gh versions or repos without queue enabled).
    let body_absent = json_doc("OPEN", "", "MERGEABLE", "CLEAN", ("", ""), &[], serde_json::json!([]));
    let probe_absent = parse_probe_json("https://example.test/pr/mq3", &body_absent, None).unwrap();
    assert!(
        !probe_absent.in_merge_queue,
        "absent mergeQueueEntry should clear in_merge_queue",
    );
}

/// `mergeQueueEntry.{state,position,enqueuedAt}` parse through onto the
/// probe's sub-state fields, and clear back to `None` when not queued —
/// this is what the Review card's merging indicator renders as
/// queue-position / relative enqueued time.
#[test]
fn parse_probe_surfaces_merge_queue_sub_state() {
    let body_in_queue = {
        let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
            "OPEN",
            "",
            "MERGEABLE",
            "CLEAN",
            ("", ""),
            &[],
            serde_json::json!([]),
        ))
        .unwrap();
        doc["mergeQueueEntry"] = serde_json::json!({
            "state": "AWAITING_CHECKS",
            "position": 1,
            "enqueuedAt": "2026-07-10T11:54:54Z",
        });
        doc.to_string()
    };
    let probe = parse_probe_json("https://example.test/pr/mqsub1", &body_in_queue, None).unwrap();
    assert!(probe.in_merge_queue);
    assert_eq!(probe.merge_queue_entry_state.as_deref(), Some("AWAITING_CHECKS"));
    assert_eq!(probe.merge_queue_position, Some(1));
    assert_eq!(probe.merge_queue_enqueued_at.as_deref(), Some("2026-07-10T11:54:54Z"));

    let body_not_queued = json_doc("OPEN", "", "MERGEABLE", "CLEAN", ("", ""), &[], serde_json::json!([]));
    let probe_not_queued = parse_probe_json("https://example.test/pr/mqsub2", &body_not_queued, None).unwrap();
    assert!(!probe_not_queued.in_merge_queue);
    assert_eq!(probe_not_queued.merge_queue_entry_state, None);
    assert_eq!(probe_not_queued.merge_queue_position, None);
    assert_eq!(probe_not_queued.merge_queue_enqueued_at, None);
}

/// `autoMergeRequest` non-null → `auto_merge_enabled = true` and its
/// `enabledAt` parses through; null, absent, or an unrelated document
/// clear it back to `false` — this is the "Merge When Ready, not yet
/// queued" signal the Merging section keys off.
#[test]
fn parse_probe_surfaces_auto_merge_request() {
    let body_armed = {
        let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
            "OPEN",
            "",
            "MERGEABLE",
            "CLEAN",
            ("", ""),
            &[],
            serde_json::json!([]),
        ))
        .unwrap();
        doc["autoMergeRequest"] = serde_json::json!({"enabledAt": "2026-07-10T11:54:54Z"});
        doc.to_string()
    };
    let probe = parse_probe_json("https://example.test/pr/amr1", &body_armed, None).unwrap();
    assert!(probe.auto_merge_enabled);
    assert_eq!(probe.auto_merge_enabled_at.as_deref(), Some("2026-07-10T11:54:54Z"));

    let body_null = {
        let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
            "OPEN",
            "",
            "MERGEABLE",
            "CLEAN",
            ("", ""),
            &[],
            serde_json::json!([]),
        ))
        .unwrap();
        doc["autoMergeRequest"] = serde_json::Value::Null;
        doc.to_string()
    };
    let probe_null = parse_probe_json("https://example.test/pr/amr2", &body_null, None).unwrap();
    assert!(!probe_null.auto_merge_enabled);
    assert_eq!(probe_null.auto_merge_enabled_at, None);

    let body_absent = json_doc("OPEN", "", "MERGEABLE", "CLEAN", ("", ""), &[], serde_json::json!([]));
    let probe_absent = parse_probe_json("https://example.test/pr/amr3", &body_absent, None).unwrap();
    assert!(!probe_absent.auto_merge_enabled);
    assert_eq!(probe_absent.auto_merge_enabled_at, None);
}

/// mono#1904 note: while a PR sits in the merge queue,
/// `mergeStateStatus` commonly reads `UNKNOWN` (GitHub is mid-recompute
/// against the synthetic merge commit). `classify_state`'s conflict
/// predicate requires *both* `mergeable == CONFLICTING` and
/// `mergeStateStatus == DIRTY`, so an `UNKNOWN` `mergeStateStatus` can
/// never trip it regardless of `mergeable` — this pins that down for the
/// specific queued case (prior art covers the general
/// UNKNOWN-must-not-be-MERGEABLE rule; this covers the newer
/// UNKNOWN-must-not-be-CONFLICT-while-queued rule from the same design
/// note) so conflict_watch never misfires a `blocked: merge_conflict`
/// flip on a PR that is actually mid-merge-queue-check.
#[test]
fn parse_probe_queued_with_unknown_merge_state_status_never_classifies_conflict() {
    for mergeable in ["MERGEABLE", "UNKNOWN"] {
        let body = {
            let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
                "OPEN",
                "",
                mergeable,
                "UNKNOWN",
                ("base-1", "head-1"),
                &[],
                serde_json::json!([]),
            ))
            .unwrap();
            doc["mergeQueueEntry"] = serde_json::json!({
                "state": "AWAITING_CHECKS",
                "position": 1,
                "enqueuedAt": "2026-07-10T11:54:54Z",
            });
            doc.to_string()
        };
        let probe = parse_probe_json("https://example.test/pr/mqconflict", &body, None).unwrap();
        assert!(
            probe.in_merge_queue,
            "mergeable={mergeable}: queue entry must still parse"
        );
        let PrLifecycleState::Open(open) = &probe.state else {
            panic!("mergeable={mergeable}: expected Open state");
        };
        assert_ne!(
            open.mergeability,
            OpenPrMergeability::Conflict,
            "mergeable={mergeable}: a queued PR with mergeStateStatus=UNKNOWN must never classify as Conflict",
        );
    }
}
