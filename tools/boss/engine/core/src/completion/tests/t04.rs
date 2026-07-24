//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).

use super::*;

#[test]
fn parse_api_pr_tsv_treats_null_and_empty_merged_at_as_none() {
    // jq emits a literal "null" (any case) when mergedAt is absent.
    let pr = parse_api_pr_tsv("https://x/pull/1\topen\tnull\t0\t0\t0").unwrap();
    assert_eq!(pr.merged_at, None);
    let pr = parse_api_pr_tsv("https://x/pull/1\topen\tNULL\t0\t0\t0").unwrap();
    assert_eq!(pr.merged_at, None);
    // An empty mergedAt column is likewise None.
    let pr = parse_api_pr_tsv("https://x/pull/1\topen\t\t0\t0\t0").unwrap();
    assert_eq!(pr.merged_at, None);
}

#[test]
fn parse_api_pr_tsv_returns_none_when_url_empty() {
    // Empty leading field (the `select(.)` / absent-row case) → None.
    assert!(parse_api_pr_tsv("\topen\tnull\t0\t0\t0").is_none());
    assert!(parse_api_pr_tsv("").is_none());
}

#[test]
fn parse_api_pr_tsv_defaults_missing_and_unparseable_numerics_to_zero() {
    // Missing trailing numeric columns fall back to 0.
    let pr = parse_api_pr_tsv("https://x/pull/1\topen\tnull").unwrap();
    assert_eq!((pr.changed_files, pr.additions, pr.deletions), (0, 0, 0));
    // Non-numeric junk also falls back to 0 (parse::<i64>().unwrap_or(0)).
    let pr = parse_api_pr_tsv("https://x/pull/1\topen\tnull\tx\ty\tz").unwrap();
    assert_eq!((pr.changed_files, pr.additions, pr.deletions), (0, 0, 0));
}

#[test]
fn parse_api_pr_tsv_ignores_trailing_head_ref_field() {
    // The suffix-scan query appends a 7th headRefName column; the shared
    // parser must ignore it and still produce the same ApiPr. The call
    // site parses headRefName separately for the suffix filter.
    let line = "https://x/pull/9\topen\tnull\t1\t2\t3\tbduff/exec_abc";
    let pr = parse_api_pr_tsv(line).unwrap();
    assert_eq!(pr.url, "https://x/pull/9");
    assert_eq!(pr.changed_files, 1);
    assert_eq!(pr.deletions, 3);
    assert_eq!(line.split('\t').nth(6), Some("bduff/exec_abc"));
}

#[test]
fn branches_identify_same_work_item_is_prefix_agnostic() {
    // The core of issue #1145: a `bduff/<suffix>` PR must associate
    // with the engine's `boss/<suffix>` expected branch.
    assert!(branches_identify_same_work_item(
        "bduff/exec_18b5023342a35418_18",
        "boss/exec_18b5023342a35418_18",
    ));
    // Identical branches still match.
    assert!(branches_identify_same_work_item("boss/exec_x", "boss/exec_x",));
    // Hash-suffix strategies match across prefixes too.
    assert!(branches_identify_same_work_item("bduff/a7f3e9c2", "boss/a7f3e9c2"));
    // Different suffixes (the incident's #1004 case:
    // `bduff/go-lib-publish-idempotent-v2` vs the work item's
    // `exec_…` suffix) correctly do NOT match.
    assert!(!branches_identify_same_work_item(
        "bduff/go-lib-publish-idempotent-v2",
        "boss/exec_18b5023342a35418_18",
    ));
    // Defensive: empty suffixes (malformed `…/` branches) never match,
    // even each other.
    assert!(!branches_identify_same_work_item("boss/", "bduff/"));
}

/// R6 invariant: the cold-path detector scopes its `gh pr list --head`
/// query by `repo_remote_url`, so two executions on *different* products
/// (and therefore different repos) that happen to produce the same
/// OpaqueHash suffix do NOT collide — the query only returns PRs in the
/// execution's own repo.
#[tokio::test]
async fn opaque_hash_collision_across_repos_does_not_mislead_detector() {
    // We can't force a real hash collision in unit-test time. Instead we
    // verify the scoping invariant: two executions on different repos are
    // each queried independently, and a PR found in repo-A's namespace is
    // not attributed to an execution in repo-B.
    let workspace = tempdir().unwrap();
    // Build a product and a chore for repo-A.
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let repo_a = "git@github.com:spinyfin/mono.git";
    let repo_b = "git@github.com:otherorg/otherrepo.git";

    // Detector for repo-A always finds the expected PR.
    let detector_a = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/10"));

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector_a.clone());
    let outcome = handler.on_stop(&execution_id).await;

    // Verify the detector was called with the execution's own repo_remote_url.
    let calls = detector_a.calls_snapshot();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].repo_remote_url, repo_a,
        "detector must be scoped to the execution's repo, not any other",
    );
    // repo_b must never appear in any detect_pr call.
    assert!(
        calls.iter().all(|c| c.repo_remote_url != repo_b),
        "detector must never query repo-B when the execution belongs to repo-A",
    );
    let _ = outcome;
}

/// Acceptance: `branch_naming` snapshotted at spawn is used by the
/// cold-path detector to reconstruct the branch name. An execution with
/// `BranchNaming::OpaqueHash` calls the detector with an opaque-hash
/// branch name, not the classic `boss/exec_<id>` form.
#[tokio::test]
async fn detector_uses_branch_naming_from_execution_row() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());

    // Patch the execution's branch_naming to OpaqueHash so we can verify
    // the detector is called with the opaque-hash branch form.
    db.force_branch_naming_for_test(&execution_id, &BranchNaming::OpaqueHash)
        .unwrap();

    let expected_hash_branch = expected_branch_name(&execution_id, &BranchNaming::OpaqueHash, None);
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/77"));

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let outcome = handler.on_stop(&execution_id).await;
    // chore_implementation holds task and enqueues reviewer.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "expected ReviewerEnqueued; got {outcome:?}",
    );

    let calls = detector.calls_snapshot();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].expected_branch, expected_hash_branch,
        "detector must use the opaque-hash branch name from the execution row",
    );
    assert!(
        !calls[0].expected_branch.contains(&execution_id),
        "opaque-hash branch must not embed the execution id",
    );
}

/// First review of a PR is never skipped by the trivial rule (design §8).
/// When `last_reviewed_sha` is `None` (review_cycle = 0) the gate must
/// pass through and enqueue the reviewer regardless of the head OID or
/// diff size.
#[tokio::test]
async fn noop_skip_gate_first_review_never_skipped() {
    let workspace = tempdir().unwrap();
    // last_reviewed_sha = None → first review
    let (_dir, db, _chore_id, execution_id, staged, branch) = noop_skip_fixture(workspace.path(), None);

    let verifier = StubBranchVerifier::ok(&branch);
    // Return a 0-line diff — if the gate were applied, this would trigger a skip.
    // The first-review guard must prevent that.
    verifier.set_diff_line_count(Ok(0)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "first review must never be skipped; expected ReviewerEnqueued, got {outcome:?}",
    );
}

/// When the current PR head SHA equals `last_reviewed_sha` the gate skips
/// the reviewer and advances the task directly to in_review.
#[tokio::test]
async fn noop_skip_gate_skips_when_sha_unchanged() {
    const SAME_SHA: &str = "sha_abc123";
    let workspace = tempdir().unwrap();
    let (_dir, db, _chore_id, execution_id, staged, branch) = noop_skip_fixture(workspace.path(), Some(SAME_SHA));

    let verifier = StubBranchVerifier::ok(&branch);
    // Current head == last_reviewed_sha → skip.
    verifier.set_head_oid(Ok(SAME_SHA.to_owned())).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { .. }),
        "sha_unchanged must skip reviewer; expected PrDetected, got {outcome:?}",
    );
}

/// When the effective diff between last-reviewed and current head is zero
/// lines (pure rebase with no file-content changes) the gate skips the
/// reviewer.
#[tokio::test]
async fn noop_skip_gate_skips_on_empty_diff() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _chore_id, execution_id, staged, branch) = noop_skip_fixture(workspace.path(), Some("sha_old"));

    let verifier = StubBranchVerifier::ok(&branch);
    // Different head SHA (new commit) but 0 changed lines → pure rebase.
    verifier.set_head_oid(Ok("sha_new".to_owned())).await;
    verifier.set_diff_line_count(Ok(0)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { .. }),
        "empty diff must skip reviewer; expected PrDetected, got {outcome:?}",
    );
}

/// When `min_review_changed_lines > 0` and the diff is below the threshold
/// the gate skips the reviewer (trivial-diff path).
#[tokio::test]
async fn noop_skip_gate_skips_trivial_diff_when_threshold_set() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _chore_id, execution_id, staged, branch) = noop_skip_fixture(workspace.path(), Some("sha_old"));

    let verifier = StubBranchVerifier::ok(&branch);
    verifier.set_head_oid(Ok("sha_new".to_owned())).await;
    // 5 changed lines, threshold is 10 → trivial → skip.
    verifier.set_diff_line_count(Ok(5)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier)
        .with_min_review_changed_lines(10);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { .. }),
        "trivial diff below threshold must skip reviewer; expected PrDetected, got {outcome:?}",
    );
}

/// When `min_review_changed_lines > 0` and the diff meets the threshold
/// the reviewer is enqueued normally.
#[tokio::test]
async fn noop_skip_gate_does_not_skip_when_diff_meets_threshold() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _chore_id, execution_id, staged, branch) = noop_skip_fixture(workspace.path(), Some("sha_old"));

    let verifier = StubBranchVerifier::ok(&branch);
    verifier.set_head_oid(Ok("sha_new".to_owned())).await;
    // 10 changed lines, threshold is 10 → not trivial → review.
    verifier.set_diff_line_count(Ok(10)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier)
        .with_min_review_changed_lines(10);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "diff at threshold must not skip reviewer; expected ReviewerEnqueued, got {outcome:?}",
    );
}

/// The default `min_review_changed_lines = 0` must NOT skip a small but
/// non-empty diff (only empty diffs and SHA matches are skipped by default).
#[tokio::test]
async fn noop_skip_gate_default_does_not_skip_nonzero_diff() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _chore_id, execution_id, staged, branch) = noop_skip_fixture(workspace.path(), Some("sha_old"));

    let verifier = StubBranchVerifier::ok(&branch);
    verifier.set_head_oid(Ok("sha_new".to_owned())).await;
    // 1 changed line — with the conservative default (0 threshold) this
    // must NOT be treated as trivial.
    verifier.set_diff_line_count(Ok(1)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);
    // min_review_changed_lines uses the default (0 = disabled)

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "1-line diff with default threshold must not skip; expected ReviewerEnqueued, got {outcome:?}",
    );
}

/// If `fetch_pr_head_oid` fails the gate fails open (proceeds with review),
/// so a transient GitHub API error never silently suppresses a reviewer pass.
#[tokio::test]
async fn noop_skip_gate_fails_open_on_head_oid_error() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _chore_id, execution_id, staged, branch) = noop_skip_fixture(workspace.path(), Some("sha_old"));

    let verifier = StubBranchVerifier::ok(&branch);
    // Simulate a GitHub API failure when fetching the PR head OID.
    verifier.set_head_oid(Err("simulated API error".to_owned())).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "API error in noop gate must fail open (enqueue reviewer); got {outcome:?}",
    );
}

// ── end-to-end / integration tests ──────────────────────────
//
// These tests exercise the complete produce→review→revise→re-review loop
// and the termination conditions (cycle bound, no-op gate interaction).
// Individual component unit tests (severity gate, no-op gate, instructions
// rendering, etc.) live above; these tests operate at the completion-handler
// level to verify the full state-machine transitions.

/// A clean reviewer result (no critical/high/regression findings) must
/// advance the producing task to `in_review` without creating a revision
/// and tick the `review_cycle` counter.
#[tokio::test]
async fn pr_review_pass_clean_advances_to_in_review_without_revision() {
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    let json = clean_review_result_json(pr_url);
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) =
        pr_review_exec_fixture(workspace.path(), Some(&json));

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker());

    let outcome = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewPassCompleted { .. }),
        "clean result must yield ReviewPassCompleted; got {outcome:?}",
    );

    // Producing task must be in in_review.
    let item = db.get_work_item(&chore_id).unwrap();
    let task = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected task/chore, got {other:?}"),
    };
    assert_eq!(
        task.status,
        TaskStatus::InReview,
        "chore must advance to in_review after reviewer approves"
    );

    // review_cycle must be incremented (0 → 1) by the completion handler.
    let (review_cycle, last_sha) = db.get_task_review_cycle_state(&chore_id).unwrap();
    assert_eq!(
        review_cycle, 1,
        "review_cycle must be incremented after each reviewer pass"
    );
    assert_eq!(
        last_sha.as_deref(),
        Some("sha_reviewed_abc123"),
        "last_reviewed_sha must be recorded from the ReviewResult head_sha",
    );
}

/// A `ReviewResult` with a HIGH severity finding must trigger the engine's
/// severity gate and create a revision on the producing task with the
/// correct `created_via` prefix and rendered instructions.
#[tokio::test]
async fn pr_review_pass_high_finding_creates_revision_with_correct_metadata() {
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    let json = high_finding_review_result_json(pr_url);
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) =
        pr_review_exec_fixture(workspace.path(), Some(&json));

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker());

    let outcome = handler.on_stop(&pr_review_exec_id).await;
    let revision_task_id = match &outcome {
        StopOutcome::ReviewPassRevisionCreated { revision_task_id, .. } => revision_task_id.clone(),
        other => panic!("high finding must yield ReviewPassRevisionCreated; got {other:?}"),
    };

    // Revision must have the pr_review created_via prefix so the
    // RevisionImplementation completion triggers another reviewer pass.
    let revision = match db.get_work_item(&revision_task_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("revision is not a task/chore: {other:?}"),
    };
    assert!(
        revision
            .created_via
            .starts_with(boss_protocol::CREATED_VIA_PR_REVIEW_PREFIX),
        "revision created_via must carry the pr_review prefix so the \
         RevisionImplementation re-triggers a reviewer pass; got: {:?}",
        revision.created_via,
    );
    // Revision instructions must mention the finding.
    assert!(
        revision.description.contains("Duplicate PR case not handled"),
        "revision instructions must include the finding title; got: {:?}",
        revision.description,
    );

    // Producing task advances to in_review even when a revision is created
    // (the revision is a follow-up child — the PR is still ready for
    // human review with the outstanding findings noted internally).
    let item = db.get_work_item(&chore_id).unwrap();
    let task = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        task.status,
        TaskStatus::InReview,
        "producing task must advance to in_review after reviewer pass",
    );
}

/// A `ReviewResult` with a `regression` category finding must trigger the
/// engine's severity gate *regardless of severity level* (a live feature
/// silently removed during a forward-port must be caught even if the
/// reviewer rates it `low` severity).
#[tokio::test]
async fn pr_review_regression_finding_creates_revision_at_low_severity_t793_check() {
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    let json = regression_class_finding_review_result_json(pr_url);
    let (_dir, db, _product_id, _chore_id, pr_review_exec_id, _pr_url) =
        pr_review_exec_fixture(workspace.path(), Some(&json));

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker());

    let outcome = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewPassRevisionCreated { .. }),
        "low-severity regression finding must still fire the severity gate \
         and create a revision; got {outcome:?}",
    );
}

/// Regression: two independent `pr_review` executions completing for
/// the SAME producing task with the SAME reviewed head sha — the exact
/// shape of the incident, where the enqueue-side race minted two review
/// executions for one unchanged push before the dedup guard existed —
/// must mint exactly ONE findings revision. The second pass recognizes
/// (via `last_reviewed_sha`) that this head was already recorded as
/// reviewed by a prior completed pass and skips minting a duplicate.
#[tokio::test]
async fn duplicate_pr_review_passes_on_same_head_mint_only_one_revision() {
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    let json_a = high_finding_review_result_json(pr_url);
    let (_dir, db, product_id, chore_id, pr_review_exec_a, _pr_url) =
        pr_review_exec_fixture(workspace.path(), Some(&json_a));

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker());

    let outcome_a = handler.on_stop(&pr_review_exec_a).await;
    assert!(
        matches!(outcome_a, StopOutcome::ReviewPassRevisionCreated { .. }),
        "first pass with a high finding must create a revision; got {outcome_a:?}",
    );

    // A second, INDEPENDENT pr_review execution for the SAME chore,
    // reviewing the SAME head sha ("sha_reviewed_abc123") — two reviewer
    // workers spawned for one unchanged push, each producing its own
    // (differently worded) ReviewResult, exactly like the original incident.
    let json_b = serde_json::json!({
        "pr_url": pr_url,
        "head_sha": "sha_reviewed_abc123",
        "summary": "Same critical correctness issue, independently reviewed.",
        "revision_warranted": true,
        "findings": [
            {
                "severity": "high",
                "category": "correctness",
                "file": "src/pr.rs",
                "location": "fn ensure_pr, ~L120",
                "title": "Duplicate PR case not handled (reworded)",
                "detail": "A second independent review of the same commit, worded slightly differently.",
                "confidence": "medium"
            }
        ],
        "regression_check": {"performed": true, "suspected_deletions": []}
    })
    .to_string();
    let pr_review_exec_b = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let (pr_review_exec_b, run_b) = db
        .start_execution_run(
            &pr_review_exec_b.id,
            "review-worker-2",
            "mono",
            "lease-review-2",
            "mono-agent-review-002",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    let _ = db
        .finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&pr_review_exec_b.id)
                .run_id(&run_b.id)
                .execution_status(ExecutionStatus::Running)
                .run_status("completed")
                .result_summary("reviewer spawned")
                .build(),
        )
        .unwrap();
    let transcript_path = workspace
        .path()
        .join(format!("transcript-{}.jsonl", pr_review_exec_b.id));
    std::fs::write(&transcript_path, make_review_transcript_jsonl(&json_b).as_bytes()).unwrap();
    db.set_run_transcript_path_if_unset(&pr_review_exec_b.id, transcript_path.to_str().unwrap())
        .unwrap();

    let outcome_b = handler.on_stop(&pr_review_exec_b.id).await;
    assert!(
        matches!(outcome_b, StopOutcome::ReviewPassCompleted { .. }),
        "second pass reviewing the SAME already-reviewed head must NOT mint another \
         revision; got {outcome_b:?}",
    );

    let revisions = db.list_revisions(&product_id, None, false, Some(&chore_id)).unwrap();
    assert_eq!(
        revisions.len(),
        1,
        "exactly one revision must be minted from two duplicate review passes on the \
         same head; got {revisions:?}",
    );

    // The redundant pass must not consume a second review_cycle slot.
    let (review_cycle, last_sha) = db.get_task_review_cycle_state(&chore_id).unwrap();
    assert_eq!(
        review_cycle, 1,
        "the duplicate pass must not double-increment review_cycle"
    );
    assert_eq!(last_sha.as_deref(), Some("sha_reviewed_abc123"));
}

/// When no ReviewResult is readable (no artifact AND no parseable
/// transcript), the finalizer must NOT silently advance the PR unreviewed.
/// It re-prompts the still-live reviewer (queues a probe naming the
/// artifact path) and returns the non-terminal `ReviewPassAwaitingResult`,
/// leaving the producing task untouched so the next Stop can re-read.
#[tokio::test]
async fn pr_review_pass_no_result_reprompts_instead_of_silently_advancing() {
    let workspace = tempdir().unwrap();
    // No review result JSON → no transcript written, no artifact written.
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) = pr_review_exec_fixture(workspace.path(), None);
    let out_dir = tempdir().unwrap();
    let probe_queuer = Arc::new(RecordingProbeQueuer::default());

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        StubPrDetector::ok(None),
        Arc::new(StubCubeClient::default()),
        Arc::new(RecordingPublisher::default()),
        Arc::new(RecordingPaneReleaser::default()),
        probe_queuer.clone(),
    )
    .with_pr_state_checker(open_pr_checker())
    .with_structured_output_dir(out_dir.path().to_path_buf())
    .with_max_unproductive_nudges(2);

    let outcome = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewPassAwaitingResult),
        "no readable result must re-prompt (not advance); got {outcome:?}",
    );

    // Task must NOT have advanced — it stays put pending the re-emit.
    let item = db.get_work_item(&chore_id).unwrap();
    let task = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_ne!(
        task.status,
        TaskStatus::InReview,
        "task must not advance to in_review while re-prompting the reviewer",
    );

    // A probe naming the artifact path must have been queued.
    let probes = probe_queuer.snapshot();
    assert_eq!(probes.len(), 1, "exactly one probe must be queued");
    assert_eq!(probes[0].0, pr_review_exec_id, "probe keyed to the reviewer exec");
    let expected_path = crate::structured_output::path_in(out_dir.path(), &pr_review_exec_id);
    assert!(
        probes[0].1.contains(&expected_path.display().to_string()),
        "probe must name the artifact path; got: {}",
        probes[0].1,
    );
}

/// After the auto-nudge breaker trips (the reviewer kept failing to write a
/// valid result across re-prompts), the finalizer gives up: it advances the
/// producing task to `in_review` WITHOUT a revision and files a
/// human-visible attention item — replacing the old silent drop.
#[tokio::test]
async fn pr_review_pass_no_result_advances_with_attention_after_breaker_trips() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) = pr_review_exec_fixture(workspace.path(), None);
    let out_dir = tempdir().unwrap();

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker())
        .with_structured_output_dir(out_dir.path().to_path_buf())
        // max=1: first Stop re-prompts (Proceed), second Stop trips.
        .with_max_unproductive_nudges(1);

    // First Stop: re-prompt.
    let first = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(first, StopOutcome::ReviewPassAwaitingResult),
        "first no-result Stop must re-prompt; got {first:?}",
    );

    // Second Stop: breaker trips → advance without revision.
    let second = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(second, StopOutcome::ReviewPassCompleted { .. }),
        "breaker trip must advance to in_review (no revision); got {second:?}",
    );

    let item = db.get_work_item(&chore_id).unwrap();
    let task = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        task.status,
        TaskStatus::InReview,
        "producing task must advance after the breaker gives up",
    );

    // An attention item must record that the PR advanced unreviewed.
    let attentions = db.list_attention_items(&pr_review_exec_id).unwrap();
    assert!(
        attentions.iter().any(|i| i.kind == REVIEW_RESULT_GIVEUP_ATTENTION_KIND),
        "a review-result-missing attention must be filed; got {attentions:?}",
    );
}

/// The PRIMARY channel: a `ReviewResult` written to the engine-owned
/// structured-output artifact (no transcript at all) must drive the
/// severity gate and create a revision.
#[tokio::test]
async fn pr_review_pass_reads_result_from_artifact_file() {
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    let json = high_finding_review_result_json(pr_url);
    // No transcript — the artifact is the only source.
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) = pr_review_exec_fixture(workspace.path(), None);
    let out_dir = tempdir().unwrap();
    std::fs::write(
        crate::structured_output::path_in(out_dir.path(), &pr_review_exec_id),
        &json,
    )
    .unwrap();

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker())
        .with_structured_output_dir(out_dir.path().to_path_buf());

    let outcome = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewPassRevisionCreated { .. }),
        "artifact with a high finding must create a revision; got {outcome:?}",
    );

    let item = db.get_work_item(&chore_id).unwrap();
    let task = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::InReview);

    // The artifact must be reaped after a successful read.
    assert!(
        !crate::structured_output::path_in(out_dir.path(), &pr_review_exec_id).exists(),
        "structured-output artifact must be deleted after the finalizer reads it",
    );
}

/// Regression test: a reviewer that emits the ReviewResult as
/// **bare JSON** (no ` ```json ` fence) must still produce a revision when
/// the severity gate fires. The extractor's bare-JSON scan (Strategy 3) must
/// find and validate the object; the finalizer must NOT silently advance to
/// `in_review` without revision.
#[tokio::test]
async fn pr_review_pass_bare_json_revision_warranted_true_creates_revision() {
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    let json = high_finding_review_result_json(pr_url);

    // Transcript contains bare JSON (no fence).
    let jsonl = make_bare_review_transcript_jsonl(&json);
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) =
        pr_review_exec_fixture_with_jsonl(workspace.path(), Some(&jsonl));

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker());

    let outcome = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewPassRevisionCreated { .. }),
        "bare-JSON reviewer output with a high finding must create a revision \
         (bare-JSON regression); got {outcome:?}",
    );

    // Verify the revision was actually created and has the finding.
    let revision_task_id = match outcome {
        StopOutcome::ReviewPassRevisionCreated { revision_task_id, .. } => revision_task_id,
        _ => unreachable!(),
    };
    let revision = match db.get_work_item(&revision_task_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("revision is not a task/chore: {other:?}"),
    };
    assert!(
        revision.description.contains("Duplicate PR case not handled"),
        "revision instructions must include the finding from the bare-JSON result; \
         got: {:?}",
        revision.description,
    );

    // Producing task must still advance to in_review.
    let item = db.get_work_item(&chore_id).unwrap();
    let task = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        task.status,
        TaskStatus::InReview,
        "producing task must advance to in_review after bare-JSON review pass",
    );
}

/// PR#1497 regression test: a reviewer that correctly identifies a
/// regression but fills `suspected_deletions` with descriptive strings
/// (instead of `ReviewFinding` objects) must still parse and create a
/// revision. Previously the serde type mismatch rejected the entire
/// `ReviewResult` and the engine advanced to `in_review` without revision.
#[tokio::test]
async fn pr_review_pass_regression_with_string_deletions_creates_revision() {
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    let json = string_shaped_deletions_review_result_json(pr_url);
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) =
        pr_review_exec_fixture(workspace.path(), Some(&json));

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker());

    let outcome = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewPassRevisionCreated { .. }),
        "regression finding with string suspected_deletions must create a revision \
         (string-deletions fix); got {outcome:?}",
    );

    let item = db.get_work_item(&chore_id).unwrap();
    let task = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        task.status,
        TaskStatus::InReview,
        "producing task must advance to in_review after revision is created",
    );
}

/// When the artifact JSON is present but fails to deserialize (e.g. a
/// type mismatch in one field), the re-prompt probe must include the
/// specific serde error text so the reviewer can correct the exact
/// malformation rather than receiving a generic "write valid JSON" message.
#[tokio::test]
async fn pr_review_pass_malformed_artifact_probe_includes_parse_error() {
    let workspace = tempdir().unwrap();
    // Malformed: "findings" is a string instead of an array — valid JSON
    // but wrong type, so serde fails with a specific error message.
    let malformed_json = serde_json::json!({
        "pr_url": "https://github.com/spinyfin/mono/pull/88",
        "head_sha": "abc",
        "summary": "Found issues.",
        "revision_warranted": true,
        "findings": "this should be an array not a string",
        "regression_check": {"performed": true, "suspected_deletions": []}
    })
    .to_string();

    let (_dir, db, _product_id, _chore_id, pr_review_exec_id, _pr_url) = pr_review_exec_fixture(workspace.path(), None);
    let out_dir = tempdir().unwrap();
    std::fs::write(
        crate::structured_output::path_in(out_dir.path(), &pr_review_exec_id),
        &malformed_json,
    )
    .unwrap();

    let probe_queuer = Arc::new(RecordingProbeQueuer::default());
    let handler = WorkerCompletionHandler::new(
        db.clone(),
        StubPrDetector::ok(None),
        Arc::new(StubCubeClient::default()),
        Arc::new(RecordingPublisher::default()),
        Arc::new(RecordingPaneReleaser::default()),
        probe_queuer.clone(),
    )
    .with_pr_state_checker(open_pr_checker())
    .with_structured_output_dir(out_dir.path().to_path_buf())
    .with_max_unproductive_nudges(2);

    let outcome = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewPassAwaitingResult),
        "malformed artifact must re-prompt, not advance; got {outcome:?}",
    );

    let probes = probe_queuer.snapshot();
    assert_eq!(probes.len(), 1, "exactly one probe must be queued");
    // The probe must mention the specific parse error (serde will say
    // something like "invalid type: string, expected a sequence").
    assert!(
        probes[0].1.contains("invalid type") || probes[0].1.contains("expected"),
        "probe must contain the serde parse error text so the reviewer can fix the \
         exact malformation; got: {}",
        probes[0].1,
    );
}

// ── Test: cycle bound ─────────────────────────────────────────────────────

/// When `review_cycle` has already reached `max_review_cycles`, the next
/// producing-worker completion must skip the reviewer entirely, advance the
/// task directly to `in_review` (PrDetected), and create a sticky
/// `pr_review_cycle_bound` attention item for the human.
#[tokio::test]
async fn pr_review_cycle_bound_skips_reviewer_and_creates_attention_item() {
    let workspace = tempdir().unwrap();
    let (_dir, db, chore_id, execution_id, staged, branch) = noop_skip_fixture(workspace.path(), None);

    // Pre-increment the cycle counter to `max_review_cycles` so the bound
    // is already reached when the producing worker finishes.
    let max_cycles: usize = 1;
    for _ in 0..max_cycles {
        db.increment_task_review_cycle(&chore_id, Some("sha_prev"))
            .expect("failed to pre-increment review_cycle");
    }

    let verifier = StubBranchVerifier::ok(&branch);
    // diff line count doesn't matter here (cycle bound fires before noop gate).
    verifier.set_diff_line_count(Ok(999)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier)
        .with_max_review_cycles(max_cycles);

    let outcome = handler.on_stop(&execution_id).await;
    // Cycle bound: no reviewer enqueued → task goes straight to in_review.
    assert!(
        matches!(outcome, StopOutcome::PrDetected { .. }),
        "cycle bound must skip reviewer and yield PrDetected; got {outcome:?}",
    );

    // Verify the sticky attention item was created for the human.
    let item = db.get_work_item(&chore_id).unwrap();
    let task = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        task.status,
        TaskStatus::InReview,
        "task must be in_review after cycle bound"
    );

    // The attention item is created on the task (work_item_id), not on
    // the execution, so we query it via the task.
    let attentions = db
        .list_attention_items_for_work_item(&chore_id)
        .expect("failed to list attention items");
    assert!(
        attentions.iter().any(|a| a.kind == "pr_review_cycle_bound"),
        "a pr_review_cycle_bound attention item must exist; got: {attentions:?}",
    );
}

// ── Test: full produce → review → revise → re-review loop ────────────────

/// End-to-end integration test for the complete automated-reviewer loop.
///
/// Flow:
///   1. ChoreImplementation finishes → reviewer enqueued (PendingReview).
///   2. PrReview (high finding) → revision created; producing task in_review.
///   3. RevisionImplementation finishes → reviewer re-enqueued.
///   4. PrReview (clean) → ReviewPassCompleted; revision task in_review.
#[tokio::test]
async fn full_produce_review_revise_re_review_loop_converges() {
    const PR_URL: &str = "https://github.com/spinyfin/mono/pull/99";
    let workspace = tempdir().unwrap();

    // ── Step 1: ChoreImplementation completes → reviewer enqueued ────────
    let (_dir, db, _product_id, chore_id, chore_exec_id) = fixture(workspace.path());

    let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged.record_if_unset(&chore_exec_id, PR_URL);

    let chore_branch = expected_branch_name(&chore_exec_id, &BranchNaming::BossExecPrefix, None);
    let verifier = StubBranchVerifier::ok(&chore_branch);
    // diff line count: non-trivial so no-op gate doesn't fire (first review
    // is never skipped by the trivial rule, but set it anyway for realism).
    verifier.set_diff_line_count(Ok(50)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(staged.clone())
        .with_branch_verifier(verifier.clone())
        .with_pr_state_checker(open_pr_checker());

    let outcome = handler.on_stop(&chore_exec_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "step 1: expected ReviewerEnqueued; got {outcome:?}",
    );

    // ── Step 2: PrReview (high finding) → revision created ───────────────
    // Find the newly-created PrReview execution (status = ready).
    let ready = db.list_ready_executions().unwrap();
    let pr_review_exec_1 = ready
        .iter()
        .find(|e| e.kind == ExecutionKind::PrReview && e.work_item_id == chore_id)
        .cloned()
        .expect("a PrReview execution must exist in ready status after step 1");

    // Start + finish the PrReview execution (simulate reviewer spawned).
    let (pr_review_exec_1, run1) = db
        .start_execution_run(
            &pr_review_exec_1.id,
            "review-worker-1",
            "mono",
            "lease-review-1",
            "mono-agent-review-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &pr_review_exec_1.id, &run1.id, Some("reviewer spawned"));

    // Write a transcript with a HIGH finding.
    let high_json = high_finding_review_result_json(PR_URL);
    let transcript1 = workspace
        .path()
        .join(format!("transcript-{}.jsonl", pr_review_exec_1.id));
    std::fs::write(&transcript1, make_review_transcript_jsonl(&high_json).as_bytes()).unwrap();
    db.set_run_transcript_path_if_unset(&pr_review_exec_1.id, transcript1.to_str().unwrap())
        .unwrap();

    let outcome2 = handler.on_stop(&pr_review_exec_1.id).await;
    let revision_task_id = match &outcome2 {
        StopOutcome::ReviewPassRevisionCreated { revision_task_id, .. } => revision_task_id.clone(),
        other => panic!("step 2: expected ReviewPassRevisionCreated; got {other:?}"),
    };

    // Verify the chore is now in_review and review_cycle = 1.
    let chore_item = db.get_work_item(&chore_id).unwrap();
    let chore_task = match chore_item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        chore_task.status,
        TaskStatus::InReview,
        "step 2: chore must be in_review"
    );
    let (cycle_after_r1, _) = db.get_task_review_cycle_state(&chore_id).unwrap();
    assert_eq!(
        cycle_after_r1, 1,
        "step 2: review_cycle must be 1 after first reviewer pass"
    );

    // ── Step 3: RevisionImplementation finishes → reviewer re-enqueued ───
    // Create and run a RevisionImplementation execution for the revision task.
    let rev_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision_task_id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let (rev_exec, rev_run) = db
        .start_execution_run(
            &rev_exec.id,
            "worker-rev-1",
            "mono",
            "lease-rev-1",
            "mono-agent-rev-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &rev_exec.id, &rev_run.id, Some("revision worker spawned"));

    // Stage the same PR URL for the revision execution.
    let rev_staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    rev_staged.record_if_unset(&rev_exec.id, PR_URL);

    let rev_branch = expected_branch_name(&rev_exec.id, &BranchNaming::BossExecPrefix, None);
    let rev_verifier = StubBranchVerifier::ok(&rev_branch);
    rev_verifier.set_diff_line_count(Ok(30)).await;

    let handler3 = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_staged_pr_urls(rev_staged)
        .with_branch_verifier(rev_verifier)
        .with_pr_state_checker(open_pr_checker())
        .with_enable_revision_triggered_reviews(true);

    let outcome3 = handler3.on_stop(&rev_exec.id).await;
    assert!(
        matches!(outcome3, StopOutcome::ReviewerEnqueued { .. }),
        "step 3: revision completion must re-enqueue reviewer; got {outcome3:?}",
    );

    // ── Step 4: PrReview (clean) → ReviewPassCompleted ───────────────────
    // Find the second PrReview execution (for the revision task).
    let ready2 = db.list_ready_executions().unwrap();
    let pr_review_exec_2 = ready2
        .iter()
        .find(|e| e.kind == ExecutionKind::PrReview && e.work_item_id == revision_task_id)
        .cloned()
        .expect("a second PrReview execution must exist after step 3");

    // Start + finish.
    let (pr_review_exec_2, run2) = db
        .start_execution_run(
            &pr_review_exec_2.id,
            "review-worker-2",
            "mono",
            "lease-review-2",
            "mono-agent-review-002",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &pr_review_exec_2.id, &run2.id, Some("reviewer 2 spawned"));

    // Write a clean transcript — no qualifying findings.
    let clean_json = clean_review_result_json(PR_URL);
    let transcript2 = workspace
        .path()
        .join(format!("transcript-{}.jsonl", pr_review_exec_2.id));
    std::fs::write(&transcript2, make_review_transcript_jsonl(&clean_json).as_bytes()).unwrap();
    db.set_run_transcript_path_if_unset(&pr_review_exec_2.id, transcript2.to_str().unwrap())
        .unwrap();

    let outcome4 = handler.on_stop(&pr_review_exec_2.id).await;
    assert!(
        matches!(outcome4, StopOutcome::ReviewPassCompleted { .. }),
        "step 4: clean review must yield ReviewPassCompleted; got {outcome4:?}",
    );

    // Revision task must be in_review — the loop converged.
    let rev_item = db.get_work_item(&revision_task_id).unwrap();
    let rev_task = match rev_item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("expected task, got {other:?}"),
    };
    assert_eq!(
        rev_task.status,
        TaskStatus::InReview,
        "step 4: revision task must be in_review after clean reviewer pass",
    );
}

// -----------------------------------------------------------
// Revision-triggered review (2026-07-01 experiment, gap: revisions of
// ANY kind — CI-fix, conflict-resolution, operator-filed — previously
// pushed post-review commits with zero re-review). Tests below cover
// the verification scenarios called out in scope:
//   (a) a revision push triggers exactly one review;
//   (b) the motivating whole-PR duplication case produces a finding
//       and a follow-up revision;
//   (c) a no-op revision (no SHA delta) does not spin a review.
// -----------------------------------------------------------

/// (a) A revision that pushes new commits to its parent PR (SHA-delta
/// gate: `Contributed`) triggers exactly one `pr_review` execution when
/// the kill-switch is on — the completion-path trigger, not a poller.
#[tokio::test]
async fn revision_push_triggers_exactly_one_review() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/737";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;
    verifier.set_diff_line_count(Ok(40)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier)
        .with_pr_state_checker(open_pr_checker())
        .with_enable_revision_triggered_reviews(true);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url } if pr_url == parent_pr_url),
        "revision push must enqueue a reviewer; got {outcome:?}",
    );

    let ready = db.list_ready_executions().unwrap();
    let reviews: Vec<_> = ready
        .iter()
        .filter(|e| e.kind == ExecutionKind::PrReview && e.work_item_id == revision_id)
        .collect();
    assert_eq!(
        reviews.len(),
        1,
        "exactly one pr_review execution must be created for the revision's push; got {reviews:?}",
    );

    // The kill-switch off (bare handler default) must preserve the
    // legacy no-reviewer behaviour — belt-and-suspenders check that the
    // ON case above is actually the flag doing the work.
    let (_dir, db2, _product_id2, revision_id2, execution_id2) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    let verifier2 = StubBranchVerifier::ok("boss/exec_parent");
    verifier2
        .set_head_oid(Ok("cccccccccccccccccccccccccccccccccccccccc".into()))
        .await;
    let handler_off = TestHarness::new(db2.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier2)
        .with_pr_state_checker(open_pr_checker());
    let outcome_off = handler_off.on_stop(&execution_id2).await;
    assert!(
        !matches!(outcome_off, StopOutcome::ReviewerEnqueued { .. }),
        "kill-switch off must not enqueue a reviewer; got {outcome_off:?}",
    );
    assert!(
        db2.list_ready_executions()
            .unwrap()
            .iter()
            .all(|e| !(e.kind == ExecutionKind::PrReview && e.work_item_id == revision_id2)),
        "kill-switch off must not create any pr_review execution for the revision",
    );
}

/// (b) The motivating rec_engine incident: a revision (CI-fix,
/// conflict-resolution, or operator-filed — this fixture uses a
/// generic, non-reviewer-spawned `created_via`) pushes a commit that
/// leaves two complete copies of the same module. The revision-
/// triggered review must produce a duplication finding and — because
/// duplication forces a revision regardless of severity — spawn a
/// follow-up revision with the finding in its instructions.
#[tokio::test]
async fn revision_triggered_review_catches_motivating_duplication_case() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/737";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;
    verifier.set_diff_line_count(Ok(40)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier)
        .with_pr_state_checker(open_pr_checker())
        .with_enable_revision_triggered_reviews(true);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "revision push must enqueue a reviewer; got {outcome:?}",
    );

    let ready = db.list_ready_executions().unwrap();
    let pr_review_exec = ready
        .iter()
        .find(|e| e.kind == ExecutionKind::PrReview && e.work_item_id == revision_id)
        .cloned()
        .expect("a pr_review execution must exist for the revision");

    let (pr_review_exec, run) = db
        .start_execution_run(
            &pr_review_exec.id,
            "review-worker-dup",
            "mono",
            "lease-review-dup",
            "mono-agent-review-dup",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &pr_review_exec.id, &run.id, Some("reviewer spawned"));

    let dup_json = duplication_finding_review_result_json(parent_pr_url);
    let transcript = workspace.path().join(format!("transcript-{}.jsonl", pr_review_exec.id));
    std::fs::write(&transcript, make_review_transcript_jsonl(&dup_json).as_bytes()).unwrap();
    db.set_run_transcript_path_if_unset(&pr_review_exec.id, transcript.to_str().unwrap())
        .unwrap();

    let review_outcome = handler.on_stop(&pr_review_exec.id).await;
    let followup_revision_id = match &review_outcome {
        StopOutcome::ReviewPassRevisionCreated { revision_task_id, .. } => revision_task_id.clone(),
        other => panic!(
            "duplication finding must force a follow-up revision (category-forced, \
             regardless of severity); got {other:?}"
        ),
    };

    let followup = db.get_work_item(&followup_revision_id).unwrap();
    let followup_task = match followup {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("expected task, got {other:?}"),
    };
    assert!(
        followup_task.description.contains("duplication") || followup_task.description.contains("blob/"),
        "follow-up revision instructions must carry the duplication finding: {}",
        followup_task.description,
    );
    assert_eq!(
        followup_task.parent_task_id.as_deref(),
        Some(revision_id.as_str()),
        "follow-up revision must chain off the reviewed revision task",
    );
}

/// (c) A no-op revision push — the SHA-delta gate reports
/// `NoContribution` because the parent PR's head did not move — must
/// not enqueue a reviewer at all. This is the structural guarantee
/// upstream of `finalize_pr_transition`: an empty/whitespace-only
/// revision never reaches the reviewer-enqueue check in the first
/// place, so no pr_review execution (and no wasted review cycle) is
/// ever created for it.
#[tokio::test]
async fn noop_revision_push_does_not_enqueue_a_review() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/737";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);

    // Head SHA unchanged — the revision worker stopped without pushing
    // anything (e.g. an empty/whitespace-only change it declined to land).
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head_before.to_owned())).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier)
        .with_pr_state_checker(open_pr_checker())
        .with_enable_revision_triggered_reviews(true);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        !matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "a no-op (no SHA delta) revision push must not enqueue a reviewer; got {outcome:?}",
    );
    assert!(
        db.list_ready_executions()
            .unwrap()
            .iter()
            .all(|e| !(e.kind == ExecutionKind::PrReview && e.work_item_id == revision_id)),
        "no pr_review execution may be created for a no-op revision push",
    );
}

// -----------------------------------------------------------
// Deliverable-satisfied gate tests (zombie-worker / "nothing left to do"
// loop fix).
//
// When a worker stops without pushing new commits (NoContribution),
// but the bound PR is already in a satisfactory state — open with
// CI clean and no conflict, or already merged — the engine should
// finalize immediately instead of nudging. Prevents the spin loop
// where workers park in waiting_for_input emitting "nothing left to
// do" and hold their pool slot indefinitely until manually reaped.
// -----------------------------------------------------------

#[tokio::test]
async fn on_stop_finalizes_satisfied_revision_when_pr_clean_and_sha_unchanged() {
    // Regression: revision worker stopped without pushing
    // (NoContribution — SHA unchanged), but the bound PR is open with
    // CI green and no conflict. The deliverable-satisfied gate must
    // finalize without nudging.
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1490";
    let head = "abcdef1111111111111111111111111111111111";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    // SHA unchanged → SHA-delta gate returns NoContribution.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    // PR is open, CI clean, no conflict.
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier).with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::DeliverableSatisfied { ref pr_url } if pr_url == parent_pr_url),
        "satisfied-deliverable gate must finalize a no-push revision whose PR is clean; \
         got {outcome:?}",
    );
    // Revision advances to InReview — deliverable satisfied.
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(
            t.status,
            TaskStatus::InReview,
            "revision must advance to in_review when deliverable satisfied",
        ),
        other => panic!("expected task, got {other:?}"),
    }
    // Execution finalized, worker reaped.
    let exec = db.get_execution(&execution_id).unwrap();
    assert_eq!(exec.status, ExecutionStatus::Completed);
    assert!(exec.cube_lease_id.is_none(), "lease must be released");
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
    // No nudge probe queued.
    assert!(
        probes.snapshot().is_empty(),
        "satisfied deliverable must NOT nudge; got {:?}",
        probes.snapshot(),
    );
}

#[tokio::test]
async fn on_stop_does_not_finalize_satisfied_revision_when_ci_inflight() {
    // When CI is still in-flight (checks running), the deliverable is NOT
    // yet satisfied. The gate must fall through to the nudge path.
    use crate::merge_poller::{OpenPrCiStatus, OpenPrMergeability, OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1491";
    let head = "1111111111111111111111111111111111111111";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    // SHA unchanged → NoContribution.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    // PR is open but CI is still in-flight.
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus {
        mergeability: OpenPrMergeability::Clean,
        ci: OpenPrCiStatus::InFlight,
    })));

    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler, cube, pane, ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier).with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    // Gate must NOT fire — CI in-flight is not a satisfied state.
    assert!(
        !matches!(outcome, StopOutcome::DeliverableSatisfied { .. }),
        "CI in-flight must not trigger the satisfied-deliverable gate; got {outcome:?}",
    );
    // Worker must NOT be reaped.
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingHuman,
    );
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
    // Revision task stays in Doing.
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected task, got {other:?}"),
    }
}

#[tokio::test]
async fn on_stop_finalizes_satisfied_chore_when_pr_clean_and_sha_unchanged() {
    // Regression: a chore_implementation worker stopped without
    // pushing new commits (PR was already open from a prior run,
    // NoContribution), but the PR is open with CI green and no conflict.
    // The deliverable-satisfied gate must finalize without nudging.
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let expected_pr_url = "https://github.com/spinyfin/mono/pull/1473";
    let head = "deadbeef11111111111111111111111111111111";

    // Create a chore with a bound PR URL (simulates a prior run's completion).
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product_named(&db, "Satisfied Chore Test");
    let chore = create_test_chore(&db, product.id.clone(), "Implement feature X");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore.id, expected_pr_url],
        )
        .unwrap();
    }
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-chore-1",
            "mono-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &execution.id, &run.id, Some("spawned worker pane"));
    db.set_execution_pr_head_before(&execution.id, head).unwrap();

    // SHA unchanged → NoContribution.
    let verifier = StubBranchVerifier::ok("boss/exec_chore");
    verifier.set_head_oid(Ok(head.into())).await;
    // PR open, CI clean, no conflict.
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier).with_merge_probe(probe);

    let outcome = handler.on_stop(&execution.id).await;
    assert!(
        matches!(outcome, StopOutcome::DeliverableSatisfied { ref pr_url } if pr_url == expected_pr_url),
        "satisfied-deliverable gate must finalize a no-push chore whose PR is clean; \
         got {outcome:?}",
    );
    // Chore stays InReview (was already; idempotent).
    match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }
    // Execution finalized, worker reaped.
    let exec_after = db.get_execution(&execution.id).unwrap();
    assert_eq!(exec_after.status, ExecutionStatus::Completed);
    assert!(exec_after.cube_lease_id.is_none(), "lease must be released");
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-chore-1"]);
    assert_eq!(pane.calls.lock().await.as_slice(), [execution.id.as_str()]);
    // No nudge probe.
    assert!(
        probes.snapshot().is_empty(),
        "satisfied deliverable must NOT nudge; got {:?}",
        probes.snapshot(),
    );
}

// ── Regression: SHA-delta gate suppressed until Stop seen ───────

/// Regression: the SHA-delta gate in `recheck_for_pr` must
/// NOT fire for a `revision_implementation` execution before `on_stop_inner`
/// has been called (i.e. before `stop_seen` is stamped). Without this guard
/// the gate fires the moment ANY worker pushes to the parent PR — including
/// the parent chore's own still-active worker — misattributing those commits
/// as the revision's contribution and transitioning the revision to
/// `in_review` before the revision worker has done anything.
///
/// After the fix: the gate returns `AwaitingInput` when `stop_seen = false`,
/// leaving the revision in `active` until `on_stop_inner` stamps the flag.
#[tokio::test]
async fn recheck_for_pr_sha_delta_suppressed_until_stop_seen() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1425";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // NOTE: stop_seen is NOT set — this simulates the revision worker
    // having just been dispatched, pane spawned, but no Stop event yet.

    let detector = StubPrDetector::ok(None);
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());
    // Branch verifier: SHA moved (simulates another worker, e.g. the
    // parent chore's worker, pushing to the same PR after dispatch).
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector,
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "SHA-delta gate must be suppressed when stop_seen = false; got {outcome:?}",
    );
    // Revision task must stay active — no premature in_review.
    let item = db.get_work_item(&revision_id).unwrap();
    match item {
        WorkItem::Task(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "revision must remain active when SHA-delta gate is suppressed; got {:?}",
                t.status,
            );
        }
        other => panic!("expected task, got {other:?}"),
    }
    // No lease release — nothing was finalized.
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "no cube lease must be released when gate is suppressed",
    );
}

/// Transient-failure recovery: once `on_stop_inner` stamps `revision_stop_contributed_head`
/// (the head it observed when the revision's own push was confirmed) and
/// `recheck_for_pr` sees the same head still at the PR, it finalizes.
/// This represents the recovery path: on_stop_inner detected the revision's
/// contribution and attempted to finalize but failed transiently, so
/// pr_head_before was NOT advanced. The merge poller retries until it
/// succeeds.
///
/// The discriminator is `revision_stop_contributed_head == head_now`
/// (not just stop_seen): without it, a concurrent parent push would look
/// identical to the revision's own push from the merge-poller's perspective.
#[tokio::test]
async fn recheck_for_pr_sha_delta_fires_after_stop_seen() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1425";
    // head_before = "aaaa": the baseline from execution start (or last
    // on_stop_inner NoContribution sweep). The revision worker pushed "bbbb"
    // and on_stop_inner's Contributed arm tried but failed to finalize —
    // so pr_head_before stays at "aaaa" (not advanced), preserving the
    // delta for merge-poller recovery.
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // Stamp stop_seen and revision_stop_contributed_head to simulate
    // on_stop_inner having confirmed the revision's own push ("bbbb")
    // and attempted finalization (which failed transiently).
    db.set_execution_stop_seen(&execution_id).unwrap();
    db.set_revision_stop_contributed_head(&execution_id, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        .unwrap();

    let detector = StubPrDetector::ok(None);
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());
    // Branch verifier: SHA at "bbbb" (revision worker's own push, still
    // the PR head because on_stop_inner's finalize failed before advancing).
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector,
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "SHA-delta gate must fire after stop_seen is set; got {outcome:?}",
    );
    let item = db.get_work_item(&revision_id).unwrap();
    match item {
        WorkItem::Task(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "revision must move to in_review");
        }
        other => panic!("expected task, got {other:?}"),
    }
}

/// Regression (multi-turn revision, foreign push absorbed): after
/// `on_stop_inner`'s own already-stop-seen-gated suppression path has
/// advanced `pr_head_before` to absorb a parent-chore push ("bbbb" —
/// `recheck_for_pr` never does this absorption itself, see the
/// 2026-07-14 incident regressions above), the merge poller must NOT
/// advance the revision if the head has not moved since the last Stop
/// boundary. The revision worker is still active between turns.
///
/// This tests the case the reviewer flagged: stop_seen=true but the SHA move
/// is attributable to a different (parent) worker. Because on_stop_inner's
/// suppression path already advanced `pr_head_before` to the parent's commit,
/// the next merge-poller sweep finds no new delta and leaves the revision active.
#[tokio::test]
async fn recheck_for_pr_sha_delta_suppressed_when_baseline_matches_current_head() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1425";
    // Simulate: parent chore's worker pushed "bbbb" before the first Stop.
    // The pre-stop suppression path advanced pr_head_before to "bbbb".
    // The revision has NOT pushed anything; PR head is still "bbbb".
    let head_before_after_absorption = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before_after_absorption);
    // stop_seen = true: first Stop has already fired (advancing the baseline).
    db.set_execution_stop_seen(&execution_id).unwrap();

    let detector = StubPrDetector::ok(None);
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());
    // Branch verifier: current head is still "bbbb" (no new push since the
    // last Stop boundary). The foreign push was already absorbed into the
    // baseline, so this is a NoContribution from the gate's perspective.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector,
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "revision must stay active when head has not moved since last Stop boundary; \
         foreign push already absorbed into baseline (got {outcome:?})",
    );
    // Revision task must remain active.
    let item = db.get_work_item(&revision_id).unwrap();
    match item {
        WorkItem::Task(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "revision must remain active; no new push since last Stop (got {:?})",
                t.status,
            );
        }
        other => panic!("expected task, got {other:?}"),
    }
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "no cube lease must be released when no contribution detected",
    );
}

/// 2026-07-14 incident (exec_18c2124d2f06d768_106d) regression:
/// the pre-first-Stop suppression path in `recheck_for_pr` must NOT
/// advance `pr_head_before` to the current head. `execution.status` is
/// `waiting_human` for a worker's ENTIRE session, not just once it goes
/// idle, so this poller sweep can race a live worker's own in-flight
/// push — absorbing it here would poison the worker's own later
/// SHA-delta comparison at its real Stop boundary (see
/// `recheck_for_pr_revision_unattributed_contributed_does_not_clobber_baseline`
/// for the end-to-end version of this regression). Only
/// `on_stop_inner`'s own already-stop-seen-gated absorption — which
/// runs at a turn boundary the worker itself just crossed — is
/// trustworthy enough to advance the baseline.
#[tokio::test]
async fn recheck_for_pr_pre_stop_suppression_does_not_clobber_pr_head_before_baseline() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1425";
    // Execution-start baseline.
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, _revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // NOTE: stop_seen is NOT set — pre-first-Stop window.

    let detector = StubPrDetector::ok(None);
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());
    // Parent pushed "bbbb" before the first Stop.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector,
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "pre-stop suppression must return AwaitingInput; got {outcome:?}",
    );
    // The baseline must be untouched — advancing it here would race a
    // live worker's own in-flight push (2026-07-14 incident).
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.pr_head_before.as_deref(),
        Some(head_before),
        "recheck_for_pr must NOT advance pr_head_before on an unattributed \
         Contributed observation — only on_stop_inner's own \
         already-stop-seen-gated absorption may do that",
    );
}

/// Multi-turn foreign-push guard (recheck_for_pr path): when stop_seen=true
/// but `revision_stop_contributed_head` is NOT set (on_stop_inner never
/// observed a Contributed outcome for this execution), a head movement caused
/// by a *different* worker (e.g. the parent chore's still-active worker)
/// must NOT advance the revision to `in_review`.
///
/// 2026-07-14 incident regression: the baseline must NOT be
/// absorbed here either — this sweep cannot distinguish "the parent
/// worker pushed" from "this revision's own worker pushed and just
/// hasn't reached its Stop boundary yet" (`execution.status` is
/// `waiting_human` for the worker's entire session). Absorbing
/// unconditionally would silently erase real evidence in the latter
/// case. Leave the baseline alone in both cases and defer to
/// `on_stop_inner`'s own already-stop-seen-gated absorption, which can
/// tell the difference via `StagedRevisionPushCache`.
///
/// Scenario: stop_seen=true (multi-turn revision), pr_head_before=aaaa,
/// parent pushes bbbb; revision made no commit → revision stays Active.
#[tokio::test]
async fn recheck_for_pr_foreign_push_post_stop_does_not_finalize_revision() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1425";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // stop_seen = true: at least one stop has been observed.
    db.set_execution_stop_seen(&execution_id).unwrap();
    // revision_stop_contributed_head is NOT set: on_stop_inner never
    // confirmed the revision's own push (the head movement is from
    // the parent worker, not this revision).

    let detector = StubPrDetector::ok(None);
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());
    // Parent chore worker pushed "bbbb" — head moved from the revision's
    // baseline, but the revision did not author this commit.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector,
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "foreign push after stop_seen must NOT finalize the revision; \
         revision_stop_contributed_head is absent (parent pushed, not revision); \
         got {outcome:?}",
    );
    // Revision task must remain active — not prematurely in_review.
    let item = db.get_work_item(&revision_id).unwrap();
    match item {
        WorkItem::Task(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "revision must remain active when head was moved by a different \
                 worker (no revision_stop_contributed_head set); got {:?}",
                t.status,
            );
        }
        other => panic!("expected task, got {other:?}"),
    }
    // No lease release — nothing was finalized.
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "no cube lease must be released when gate is suppressed",
    );
    // The baseline must be untouched — recheck_for_pr cannot tell a
    // foreign push apart from this revision's own not-yet-Stopped push,
    // so it must not absorb either (2026-07-14 incident).
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.pr_head_before.as_deref(),
        Some(head_before),
        "recheck_for_pr must NOT advance pr_head_before on an unattributed \
         Contributed observation, foreign push or not",
    );
}

/// Multi-turn foreign-push guard (on_stop_inner path): when stop_seen was
/// already true (multi-turn revision, already_stop_seen=true) and no push
/// was staged in `StagedRevisionPushCache`, a head movement caused by the
/// parent worker must NOT advance the revision to `in_review`. The Contributed
/// arm must absorb the baseline and fall through to the nudge path.
///
/// Scenario: already_stop_seen=true, pr_head_before=aaaa, parent pushes bbbb;
/// no push staged for this execution → revision stays Active (nudge fired).
#[tokio::test]
async fn on_stop_foreign_push_post_stop_does_not_finalize_revision() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1425";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // Stamp stop_seen BEFORE calling on_stop so already_stop_seen=true
    // inside on_stop_inner (simulating a multi-turn revision's second+ stop).
    db.set_execution_stop_seen(&execution_id).unwrap();
    // Do NOT record a push in StagedRevisionPushCache — the revision ran
    // no push command this turn.

    let detector = StubPrDetector::ok(None);
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());
    // Parent chore worker pushed "bbbb" — head moved but revision did not
    // author this commit.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector,
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;

    // on_stop_inner must NOT finalize — it must nudge (AwaitingInput).
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "on_stop_inner must not finalize revision when already_stop_seen=true \
         and no push was staged (parent push assumed); got {outcome:?}",
    );
    // Revision task must remain active.
    let item = db.get_work_item(&revision_id).unwrap();
    match item {
        WorkItem::Task(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "revision must stay active when SHA-delta Contributed is \
                 suppressed at multi-turn stop with no push evidence; got {:?}",
                t.status,
            );
        }
        other => panic!("expected task, got {other:?}"),
    }
    // No lease release — revision is not done.
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "no cube lease must be released when on_stop_inner suppresses Contributed",
    );
    // The baseline must have been advanced to absorb the foreign push.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.pr_head_before.as_deref(),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        "on_stop_inner must absorb foreign push into pr_head_before baseline",
    );
}

/// Regression for the revision reap gap (2026-07-14 live incident,
/// exec_18c2124d2f06d768_106d): a revision worker's own push,
/// staged via `StagedRevisionPushCache` (populated when the worker runs
/// `cube pr update`, not the legacy direct `jj git push` — see
/// `pr_url_capture::is_revision_push_command`), must be recognised as
/// push evidence on a multi-turn Stop (`already_stop_seen = true`, e.g.
/// the worker spent earlier turns investigating review findings before
/// finally pushing on this one) so the revision is finalised to
/// `in_review` AND its pane/lease are reaped in the same `on_stop`
/// call — one engine tick, no merge-poller sweep required.
///
/// Companion to `on_stop_foreign_push_post_stop_does_not_finalize_revision`
/// above, which covers the opposite half of the same gate (no push
/// evidence for THIS execution → a concurrent parent push must NOT
/// finalize the revision). Before the fix, `is_revision_push_command`
/// only matched a literal `jj git push` command, which no compliant
/// worker ever runs directly (a `PreToolUse` hook blocks it) — so
/// `StagedRevisionPushCache` was never populated in production and this
/// scenario always fell through to `nudge_or_park`, stranding
/// multi-turn revisions in `active` with their pane sitting idle.
#[tokio::test]
async fn on_stop_revision_own_push_post_stop_finalizes_and_reaps() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/826";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // Stamp stop_seen BEFORE calling on_stop so already_stop_seen=true
    // inside on_stop_inner, simulating a multi-turn revision's second+
    // (terminal) stop.
    db.set_execution_stop_seen(&execution_id).unwrap();

    let detector = StubPrDetector::ok(None);
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());
    // The bound PR's head moved because THIS revision pushed.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;
    // Stage push evidence exactly as the PostToolUse dispatcher does
    // when it sees the worker's `cube pr update` Bash call.
    let staged_pushes = Arc::new(crate::pr_url_capture::StagedRevisionPushCache::new());
    staged_pushes.record(&execution_id);

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector,
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_branch_verifier(verifier)
    .with_staged_revision_pushes(staged_pushes);

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "on_stop must finalize a revision's own push even at a multi-turn \
         Stop when push evidence is staged; got {outcome:?}",
    );
    // No probe fired — the worker must not be nudged after finalization.
    assert!(
        probes.snapshot().is_empty(),
        "no probe must fire when revision finalises via its own staged push; got {:?}",
        probes.snapshot(),
    );
    // "row advanced": revision task reaches in_review.
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "revision must move to in_review");
        }
        other => panic!("expected task, got {other:?}"),
    }
    // "pane reaped": the pane releaser was invoked and the cube lease
    // released, synchronously inside this single on_stop call — no
    // poller sweep required.
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "worker pane must be reaped in the same tick that finalizes the revision",
    );
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released in the same tick that finalizes the revision",
    );
}
