//! Completion tests for the `pr_created` proposal seam (`pr_created_proposals_seam`).

use super::*;
use crate::work::SubmitWorkerProposalInput;

const PR_URL: &str = "https://github.com/spinyfin/mono/pull/4201";

fn submit_pr_created(db: &WorkDb, execution_id: &str, chore_id: &str, pr_url: &str) {
    let payload_json = serde_json::json!({ "pr_url": pr_url }).to_string();
    let idempotency_key = format!("{execution_id}:pr_created:{pr_url}");
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id,
            work_item_id: chore_id,
            kind: ProposalKind::PrCreated,
            payload_json: &payload_json,
            idempotency_key: &idempotency_key,
        })
        .unwrap()
        .expect("pr_created submission accepted");
    assert_eq!(outcome.proposal.state, boss_protocol::ProposalState::Applied);
}

fn new_flags(dir: &Path, enable_seam: bool) -> Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        dir.join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    if enable_seam {
        flags.set("pr_created_proposals_seam", true).unwrap();
    }
    flags
}

fn new_metrics() -> Arc<Registry> {
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    metrics
}

fn expected_branch(db: &WorkDb, execution_id: &str) -> String {
    let exec = db.get_execution(execution_id).unwrap();
    expected_branch_name(execution_id, &exec.branch_naming, exec.worker_branch_prefix.as_deref())
}

#[tokio::test]
async fn on_stop_uses_applied_pr_created_proposal_and_skips_legacy_ladder() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);

    let flags = new_flags(workspace.path(), true);
    let metrics = new_metrics();
    let detector = StubPrDetector::ok(None);
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch(&db, &execution_id)));

    let outcome = handler.on_stop(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::ReviewerEnqueued {
            pr_url: PR_URL.to_owned()
        }
    );
    assert_eq!(
        detector.calls.lock().unwrap().len(),
        0,
        "pr_created proposal must skip the cold-path detector"
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "primary pr_created proposal path must not count a fallback hit"
    );
}

#[tokio::test]
async fn recheck_for_pr_uses_applied_pr_created_proposal_and_skips_legacy_ladder() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);

    let flags = new_flags(workspace.path(), true);
    let metrics = new_metrics();
    let detector = StubPrDetector::ok(None);
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch(&db, &execution_id)));

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::ReviewerEnqueued {
            pr_url: PR_URL.to_owned()
        }
    );
    assert_eq!(
        detector.calls.lock().unwrap().len(),
        0,
        "recheck_for_pr must skip the cold-path detector when pr_created proposal is present"
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "primary pr_created proposal path must not count a fallback hit"
    );
}

#[tokio::test]
async fn on_stop_falls_back_to_legacy_ladder_and_counts_fallback_hit_when_no_pr_created_proposal() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());

    let flags = new_flags(workspace.path(), true);
    let metrics = new_metrics();
    let detector = StubPrDetector::ok_status(PrStatus::Fresh { url: PR_URL.to_owned() });
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::ReviewerEnqueued {
            pr_url: PR_URL.to_owned()
        }
    );
    assert_eq!(detector.calls.lock().unwrap().len(), 1);
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(1),
        "legacy ladder hit with pr_created_proposals_seam on must increment fallback_hit.pr_created"
    );
}

#[tokio::test]
async fn recheck_for_pr_counts_fallback_hit_when_no_pr_created_proposal() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());

    let flags = new_flags(workspace.path(), true);
    let metrics = new_metrics();
    let detector = StubPrDetector::ok_status(PrStatus::Fresh { url: PR_URL.to_owned() });
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone());

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::ReviewerEnqueued {
            pr_url: PR_URL.to_owned()
        }
    );
    assert_eq!(detector.calls.lock().unwrap().len(), 1);
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(1),
        "recheck fallback with pr_created_proposals_seam on must increment fallback_hit.pr_created"
    );
}

#[tokio::test]
async fn on_stop_ignores_pr_created_proposal_when_flag_is_off() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);

    let flags = new_flags(workspace.path(), false);
    let metrics = new_metrics();
    let detector = StubPrDetector::ok(None);
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;

    assert_eq!(outcome, StopOutcome::AwaitingInput);
    assert_eq!(
        detector.calls.lock().unwrap().len(),
        1,
        "when pr_created_proposals_seam is off, Stop must fall through to legacy ladder"
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "fallback counter must not increment when pr_created_proposals_seam is off"
    );
}

#[tokio::test]
async fn on_stop_rejects_pr_created_proposal_on_branch_mismatch() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);

    let flags = new_flags(workspace.path(), true);
    let metrics = new_metrics();
    let detector = StubPrDetector::ok(None);
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok("boss/unrelated-branch"));

    let outcome = handler.on_stop(&execution_id).await;

    assert_eq!(outcome, StopOutcome::AwaitingInput);
    assert_eq!(
        detector.calls.lock().unwrap().len(),
        1,
        "branch-mismatched pr_created proposal must not finalize and must fall through"
    );
}

#[tokio::test]
async fn on_stop_defers_pr_created_proposal_when_run_done_seam_requires_declaration() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);

    let flags = new_flags(workspace.path(), true);
    flags.set("run_done_proposals_seam", true).unwrap();
    let metrics = new_metrics();
    let detector = StubPrDetector::ok(None);
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch(&db, &execution_id)));

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::AwaitingRunDoneDeclaration { .. }),
        "pr_created proposal must defer for run_done declaration when run_done_proposals_seam is on; got {outcome:?}"
    );
    assert_eq!(
        detector.calls.lock().unwrap().len(),
        0,
        "deferred pr_created proposal must not fall through to cold detector"
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0)
    );
}

const PARENT_PR: &str = "https://github.com/spinyfin/mono/pull/2090";
const HEAD_BEFORE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_AFTER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn counter(metrics: &Registry) -> Option<u64> {
    metrics.counter_value("worker_proposals.fallback_hit.pr_created")
}

#[tokio::test]
async fn revision_proposal_matching_bound_pr_with_moved_head_finalizes_without_detector() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), PARENT_PR, HEAD_BEFORE);
    submit_pr_created(&db, &execution_id, &revision_id, PARENT_PR);
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(HEAD_AFTER.into())).await;
    let metrics = new_metrics();
    let detector = StubPrDetector::ok(None);
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(new_flags(workspace.path(), true))
        .with_metrics(metrics.clone())
        .with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == PARENT_PR),
        "{outcome:?}"
    );
    assert_eq!(detector.calls.lock().unwrap().len(), 0);
    assert_eq!(counter(&metrics), Some(0));
}

#[tokio::test]
async fn revision_proposal_without_head_movement_cannot_finalize_on_a_declaration_alone() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), PARENT_PR, HEAD_BEFORE);
    submit_pr_created(&db, &execution_id, &revision_id, PARENT_PR);
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(HEAD_BEFORE.into())).await;
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(new_flags(workspace.path(), true))
        .with_metrics(new_metrics())
        .with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        !matches!(
            outcome,
            StopOutcome::PrDetected { .. } | StopOutcome::ReviewerEnqueued { .. }
        ),
        "an unmoved head must not pass the contribution gate: {outcome:?}"
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => assert_ne!(t.status, TaskStatus::InReview),
        other => panic!("expected task, got {other:?}"),
    }
}

#[tokio::test]
async fn revision_proposal_for_a_different_pr_than_the_bound_one_is_ignored() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), PARENT_PR, HEAD_BEFORE);
    submit_pr_created(
        &db,
        &execution_id,
        &revision_id,
        "https://github.com/spinyfin/mono/pull/2091",
    );
    // Applying a proposal binds its URL to the task; clear that so the
    // revision's bound PR is the chain root's, as when the worker's URL is wrong.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = NULL WHERE id = ?1",
            rusqlite::params![revision_id],
        )
        .unwrap();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(new_flags(workspace.path(), true))
        .with_metrics(new_metrics());
    let execution = db.get_execution(&execution_id).unwrap();

    assert_eq!(
        handler.pr_created_proposal_lookup(&execution, "test").await,
        PrCreatedProposalLookup::Unusable
    );
}

#[tokio::test]
async fn proposal_lookup_distinguishes_absent_unusable_and_transient() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let execution = db.get_execution(&execution_id).unwrap();
    let build = |verifier: Arc<StubBranchVerifier>| {
        TestHarness::new(db.clone(), StubPrDetector::ok(None))
            .handler
            .with_feature_flags(new_flags(workspace.path(), true))
            .with_branch_verifier(verifier)
    };
    assert_eq!(
        build(StubBranchVerifier::ok("x"))
            .pr_created_proposal_lookup(&execution, "t")
            .await,
        PrCreatedProposalLookup::Absent
    );
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);
    assert_eq!(
        build(StubBranchVerifier::ok("boss/unrelated-branch"))
            .pr_created_proposal_lookup(&execution, "t")
            .await,
        PrCreatedProposalLookup::Unusable
    );
    assert_eq!(
        build(StubBranchVerifier::err("GitHub unavailable"))
            .pr_created_proposal_lookup(&execution, "t")
            .await,
        PrCreatedProposalLookup::Transient
    );
}

#[tokio::test]
async fn run_done_resolution_reports_transient_not_none_found_when_proposal_is_unverifiable() {
    use crate::run_done_declaration::DeclaredPrResolution;
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);
    let execution = db.get_execution(&execution_id).unwrap();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(new_flags(workspace.path(), true))
        .with_metrics(new_metrics())
        .with_branch_verifier(StubBranchVerifier::err("GitHub unavailable"));

    let resolution = handler.resolve_declared_pr_url(&execution).await;

    assert!(
        matches!(resolution, DeclaredPrResolution::Transient { .. }),
        "an unverifiable applied proposal must not become a definitive no-PR: {resolution:?}"
    );
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Failed,
        "a transient verification failure must not terminalize"
    );
}

#[tokio::test]
async fn run_done_resolution_uses_verified_proposal_before_the_legacy_ladder() {
    use crate::run_done_declaration::DeclaredPrResolution;
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);
    let execution = db.get_execution(&execution_id).unwrap();
    let detector = StubPrDetector::ok(None);
    let metrics = new_metrics();
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(new_flags(workspace.path(), true))
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch(&db, &execution_id)));

    let resolution = handler.resolve_declared_pr_url(&execution).await;

    assert!(
        matches!(resolution, DeclaredPrResolution::Bound { ref pr_url, .. } if pr_url == PR_URL),
        "{resolution:?}"
    );
    assert_eq!(detector.calls.lock().unwrap().len(), 0);
    assert_eq!(counter(&metrics), Some(0));
}

#[tokio::test]
async fn unusable_applied_proposal_still_counts_a_fallback_hit_when_the_legacy_path_resolves() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);
    let metrics = new_metrics();
    let detector = StubPrDetector::ok_status(PrStatus::Fresh { url: PR_URL.to_owned() });
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(new_flags(workspace.path(), true))
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok("boss/unrelated-branch"));

    let outcome = handler.on_stop(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::ReviewerEnqueued {
            pr_url: PR_URL.to_owned()
        }
    );
    assert_eq!(detector.calls.lock().unwrap().len(), 1);
    assert_eq!(counter(&metrics), Some(1));
}

#[tokio::test]
async fn transient_verifier_failure_neither_finalizes_nor_counts_a_fallback_hit() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);
    let metrics = new_metrics();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(new_flags(workspace.path(), true))
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::err("GitHub unavailable"));

    let outcome = handler.on_stop(&execution_id).await;

    assert_eq!(outcome, StopOutcome::AwaitingInput);
    assert_eq!(counter(&metrics), Some(0));
}

#[tokio::test]
async fn fallback_hit_is_counted_once_per_execution_across_passes() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let execution = db.get_execution(&execution_id).unwrap();
    let metrics = new_metrics();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(new_flags(workspace.path(), true))
        .with_metrics(metrics.clone());

    handler.record_pr_created_fallback_hit(&execution, "stop_staged", PR_URL);
    handler.record_pr_created_fallback_hit(&execution, "stop_staged", PR_URL);
    assert_eq!(counter(&metrics), Some(1), "repeat passes for one source count once");

    handler.record_pr_created_fallback_hit(&execution, "stop_detector", PR_URL);
    assert_eq!(counter(&metrics), Some(2), "a distinct source counts separately");
}

#[tokio::test]
async fn recheck_holds_a_verified_proposal_for_the_run_done_declaration() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);
    let flags = new_flags(workspace.path(), true);
    flags.set("run_done_proposals_seam", true).unwrap();
    let detector = StubPrDetector::ok(None);
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(flags)
        .with_metrics(new_metrics())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch(&db, &execution_id)));

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::AwaitingRunDoneDeclaration { .. }),
        "{outcome:?}"
    );
    assert_eq!(detector.calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn nudge_recheck_prefers_the_verified_proposal_url() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);
    let execution = db.get_execution(&execution_id).unwrap();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(new_flags(workspace.path(), true))
        .with_metrics(new_metrics())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch(&db, &execution_id)));

    assert_eq!(
        handler.current_verified_or_bound_pr_url(&execution).await.as_deref(),
        Some(PR_URL)
    );
}

#[tokio::test]
async fn seam_flag_has_no_effect_while_the_master_proposals_switch_is_off() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    submit_pr_created(&db, &execution_id, &chore_id, PR_URL);
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("pr_created_proposals_seam", true).unwrap();
    let metrics = new_metrics();
    let detector = StubPrDetector::ok(None);
    let handler = TestHarness::new(db.clone(), detector.clone())
        .handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;

    assert_eq!(outcome, StopOutcome::AwaitingInput);
    assert_eq!(detector.calls.lock().unwrap().len(), 1);
    assert_eq!(counter(&metrics), Some(0));
}
