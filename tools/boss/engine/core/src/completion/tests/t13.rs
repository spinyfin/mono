//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).
//!
//! Theme: PR-created-declaration worker-proposal seam
//! (`pr_created_proposals_seam`, design implementation task 12 — the last
//! of the per-seam migrations). Mirrors the `automation_outcome_proposals_seam`
//! tests in `t11.rs`, adapted to the PR-detection ladder's shape: instead of
//! one `finalize_*` entry point, `pr_created_from_proposal` is consulted from
//! both `on_stop_inner` and `recheck_for_pr`, and every OTHER source that
//! reaches the shared `finalize_pr_transition` funnel counts as a fallback.

use super::*;

fn enable_pr_created_seam() -> (Arc<crate::feature_flags::FeatureFlagsStore>, TempDir) {
    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("pr_created_proposals_seam", true).unwrap();
    (flags, flags_dir)
}

/// Proposal path wins over the cold-reconstruction ladder — and, because
/// this test never populates `StagedPrUrlCache` (the default empty cache,
/// exactly what an engine restart between the worker's `cube pr create` and
/// this Stop would leave behind), it also demonstrates the seam's durability
/// win: finalization succeeds from the durable `pr_created` proposal row
/// alone, with no in-memory staging evidence at all.
#[tokio::test]
async fn pr_created_proposals_first_uses_proposal_url_surviving_an_empty_staging_cache() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    // If the ladder ran at all, the detector would supply THIS (different)
    // URL — proves the proposal, not the cold path, decided the outcome.
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/999"));

    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/500"),
        "expected finalization via the proposal's URL, got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        0,
        "the pr_created proposal must short-circuit the cold-reconstruction ladder entirely",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => assert_eq!(t.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/500")),
        other => panic!("expected chore, got {other:?}"),
    }
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "the proposal covered the URL; the legacy ladder must never fire",
    );
}

/// No `pr_created` proposal exists: the legacy staging-cache / cold-
/// reconstruction ladder still runs exactly as before, and the fallback hit
/// is counted — this seam's explicit exit criterion.
#[tokio::test]
async fn pr_created_proposals_first_falls_back_to_the_ladder_and_counts_the_hit() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/12"));

    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/12"),
        "expected the legacy ladder to still finalize via the detector, got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        1,
        "no proposal existed — the cold detector must run"
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => assert_eq!(t.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/12")),
        other => panic!("expected chore, got {other:?}"),
    }
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(1),
        "no proposal existed, so the legacy ladder fired and must count as a fallback hit",
    );
}

/// Even with an existing `pr_created` proposal present, the flag defaulting
/// off must reproduce the exact pre-seam behavior: the legacy ladder always
/// decides, the proposal is never consulted, and nothing is counted.
#[tokio::test]
async fn pr_created_proposals_first_flag_off_matches_pre_migration_behavior_exactly() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    // Distinct from the proposal's URL, so a wrongly-proposal-first path
    // would produce a detectably different (wrong) answer.
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/12"));

    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/12"),
        "flag off: must decide via the legacy ladder, not the proposal, got {outcome:?}",
    );
    assert_eq!(detector.call_count(), 1);
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "with the flag off nothing is counted",
    );
}

/// Merge-poller mirror: `recheck_for_pr` also reads the `pr_created`
/// proposal before its own staging-cache / SHA-delta / cold-reconstruction
/// chain, and skips the detector entirely when a proposal covers it.
#[tokio::test]
async fn pr_created_proposals_first_via_recheck_for_pr_uses_proposal_and_skips_detector() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/999"));

    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/500"),
        "expected pr-recheck to finalize via the proposal's URL, got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        0,
        "the proposal must short-circuit the cold detector on recheck too"
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "the proposal covered the URL; the legacy ladder must never fire",
    );
}

/// A `pr_created` proposal rejected at submission (wrong repo/URL shape)
/// carries no usable URL — the legacy ladder runs as the counted fallback,
/// exactly like the "no proposal at all" case.
#[tokio::test]
async fn pr_created_proposals_first_rejected_proposal_falls_back_to_the_ladder() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    // A PR URL from an unrelated repo fails `validate_pr_url`'s product-repo
    // gate inside `apply_pr_created` — the proposal is submitted but ends up
    // `Rejected`, not `Applied`.
    let rejected = db
        .submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
            execution_id: &execution_id,
            work_item_id: &chore_id,
            kind: ProposalKind::PrCreated,
            payload_json: r#"{"pr_url":"https://github.com/some-other-org/other-repo/pull/1"}"#,
            idempotency_key: "key-1",
        })
        .unwrap()
        .unwrap();
    assert_eq!(rejected.proposal.state, ProposalState::Rejected);

    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/12"));
    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/12"),
        "a rejected proposal carries no usable URL; expected the legacy ladder to decide, got {outcome:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(1),
        "a rejected proposal still counts as 'the proposal did not cover this finalization'",
    );
}
