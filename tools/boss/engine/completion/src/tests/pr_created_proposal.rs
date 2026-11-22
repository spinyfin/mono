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

fn new_flags(enable_seam: bool) -> Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    if enable_seam {
        flags.set("pr_created_proposals_seam", true).unwrap();
    }
    // Keep the tempdir alive for the duration of the test via forget only if needed;
    // FeatureFlagsStore::set writes synchronously, and flags_dir is only for disk persistence.
    std::mem::forget(flags_dir);
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

    let flags = new_flags(true);
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

    let flags = new_flags(true);
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

    let flags = new_flags(true);
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

    let flags = new_flags(true);
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

    let flags = new_flags(false);
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

    let flags = new_flags(true);
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

    let flags = new_flags(true);
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
