//! Regression tests for a produce-a-PR nudge cycle that is overtaken by a
//! staged, branch-verified PR.
//!
//! A worker can exhaust its wait, take three sweep-delivered `no_pr` nudges,
//! then open a PR. The merge poller's next pass must hold for a run_done
//! declaration (or finalize), never replay the stale `no_pr` intent through
//! the breaker and fail the attempt with "No PR was produced."

use super::*;
use crate::work::ExecutionStatus;

const PR_URL: &str = "https://github.com/spinyfin/mono/pull/86";

fn declaration_flags(path: &std::path::Path) -> Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        path.join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("run_done_proposals_seam", true).unwrap();
    flags
}

fn lapse_wait(waits: &crate::wait_registry::WaitRegistry, execution_id: &str) {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    waits
        .declare(execution_id, "eval still running".into(), None, 60, now - 61)
        .unwrap();
}

/// Wait expiry, then three sweep-delivered produce-a-PR nudges. Breaker sits
/// at the cap; the next identical `no_pr` replay would trip.
async fn three_sweep_no_pr_nudges_after_wait(
    handler: &WorkerCompletionHandler,
    waits: &crate::wait_registry::WaitRegistry,
    breaker: &crate::nudge_breaker::NudgeBreaker,
    execution_id: &str,
) {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    waits
        .declare(execution_id, "eval still running".into(), None, 60, now)
        .unwrap();
    assert!(matches!(
        handler.on_stop(execution_id).await,
        StopOutcome::WorkerWaitPending { .. }
    ));
    lapse_wait(waits, execution_id);
    for count in 1..=3 {
        let outcome = handler.recheck_background_nudge(execution_id).await;
        assert!(
            matches!(outcome, Some(StopOutcome::AwaitingInput)),
            "sweep {count} must deliver a produce-a-PR nudge; got {outcome:?}",
        );
        assert_eq!(breaker.counts(execution_id), Some((count, count)));
    }
    assert!(
        handler
            .pending_background_nudge_execution_ids()
            .contains(&execution_id.to_owned()),
        "the produce-a-PR intent must stay retained after the third sweep so a quiet later \
         pass would otherwise replay it",
    );
}

fn stage_verified_pr(execution_id: &str) -> Arc<crate::pr_url_capture::StagedPrUrlCache> {
    let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged.record_if_unset(execution_id, PR_URL);
    staged.mark_branch_verified(execution_id, PR_URL);
    staged
}

/// The incident sequence: wait expiry → three sweep-delivered `no_pr`
/// nudges → a PR is staged and verified against the execution branch → a
/// quiet merge-poller pass (`recheck_for_pr` then `recheck_background_nudge`).
/// Must yield `AwaitingRunDoneDeclaration`, never `NudgeBreakerParked`.
#[tokio::test]
async fn staged_pr_after_three_no_pr_nudges_holds_for_declaration_on_quiet_sweep() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let TestHarness {
        handler, probes, cube, ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let waits = Arc::new(crate::wait_registry::WaitRegistry::new());
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    let flags_dir = tempdir().unwrap();
    let handler = handler
        .with_wait_registry(waits.clone())
        .with_nudge_breaker(breaker.clone())
        .with_staged_pr_urls(staged.clone())
        .with_feature_flags(declaration_flags(flags_dir.path()))
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    three_sweep_no_pr_nudges_after_wait(&handler, &waits, &breaker, &execution_id).await;
    let probes_at_cap = probes.snapshot().len();

    staged.record_if_unset(&execution_id, PR_URL);
    staged.mark_branch_verified(&execution_id, PR_URL);

    let recheck = handler.recheck_for_pr(&execution_id).await;
    assert!(
        matches!(recheck, StopOutcome::AwaitingRunDoneDeclaration { .. }),
        "pr-recheck of a verified staged PR must hold for a run_done declaration; got {recheck:?}",
    );
    let replay = handler.recheck_background_nudge(&execution_id).await;
    assert!(
        !matches!(replay, Some(StopOutcome::NudgeBreakerParked { .. })),
        "a quiet sweep must not trip the breaker after a PR was staged; got {replay:?}",
    );
    assert!(
        matches!(replay, None | Some(StopOutcome::AwaitingRunDoneDeclaration { .. })),
        "after retiring the produce-a-PR intent the replay must hold or drop, not nudge; got {replay:?}",
    );

    assert_eq!(
        probes.snapshot().len(),
        probes_at_cap,
        "a staged PR must not produce a fourth produce-a-PR nudge",
    );
    assert_eq!(breaker.counts(&execution_id), None, "the no_pr cycle must be reset");
    assert!(
        db.get_execution(&execution_id).unwrap().status.is_live(),
        "the execution must stay live while waiting for a declaration",
    );
    assert_eq!(
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => t.status,
            other => panic!("expected a task/chore, got {other:?}"),
        },
        TaskStatus::Active,
    );
    assert!(cube.release_calls.lock().await.is_empty());
}

/// Same setup, but the quiet pass is only `recheck_background_nudge` — the
/// path that actually tripped in the incident by replaying the captured
/// `no_pr` fingerprint. It must re-derive from the staged URL rather than
/// trust the retained intent.
#[tokio::test]
async fn recheck_background_nudge_rederives_from_staged_pr_instead_of_replaying_no_pr() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let waits = Arc::new(crate::wait_registry::WaitRegistry::new());
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    let flags_dir = tempdir().unwrap();
    let handler = handler
        .with_wait_registry(waits.clone())
        .with_nudge_breaker(breaker.clone())
        .with_staged_pr_urls(staged.clone())
        .with_feature_flags(declaration_flags(flags_dir.path()))
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    three_sweep_no_pr_nudges_after_wait(&handler, &waits, &breaker, &execution_id).await;
    staged.record_if_unset(&execution_id, PR_URL);
    staged.mark_branch_verified(&execution_id, PR_URL);

    let outcome = handler.recheck_background_nudge(&execution_id).await;
    assert!(
        matches!(outcome, Some(StopOutcome::AwaitingRunDoneDeclaration { .. })),
        "recheck must re-derive a declaration hold from the staged PR; got {outcome:?}",
    );
    assert!(
        !matches!(outcome, Some(StopOutcome::NudgeBreakerParked { .. })),
        "replaying a stale no_pr intent must not trip the breaker after a PR was staged",
    );
    assert_eq!(breaker.counts(&execution_id), None);
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());
    assert!(cube.release_calls.lock().await.is_empty());
}

/// Cover the pr-recheck staged-URL deferral on its own: with the breaker
/// already at the cap, `recheck_for_pr` seeing a verified staged URL must
/// retire the produce-a-PR cycle and hold, not leave a replayable `no_pr`
/// intent behind.
#[tokio::test]
async fn pr_recheck_staged_url_retires_no_pr_intent_at_breaker_cap() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let waits = Arc::new(crate::wait_registry::WaitRegistry::new());
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    let flags_dir = tempdir().unwrap();
    let handler = handler
        .with_wait_registry(waits.clone())
        .with_nudge_breaker(breaker.clone())
        .with_staged_pr_urls(staged.clone())
        .with_feature_flags(declaration_flags(flags_dir.path()))
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    three_sweep_no_pr_nudges_after_wait(&handler, &waits, &breaker, &execution_id).await;
    staged.record_if_unset(&execution_id, PR_URL);
    staged.mark_branch_verified(&execution_id, PR_URL);
    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::AwaitingRunDoneDeclaration { .. }),
        "pr-recheck staged-URL arm must hold for a declaration; got {outcome:?}",
    );
    assert_eq!(breaker.counts(&execution_id), None);
    assert!(
        !handler.pending_background_nudge_execution_ids().contains(&execution_id),
        "pr-recheck must forget the retained produce-a-PR intent",
    );
    let replay = handler.recheck_background_nudge(&execution_id).await;
    assert_eq!(
        replay, None,
        "with the intent retired, a later background recheck must be a no-op; got {replay:?}",
    );
}

/// A breaker trip that happens while a PR is staged must name that PR. The
/// clause is computed from current state at trip time, never from a
/// replayed `bound_pr_url = None`.
#[tokio::test]
async fn breaker_trip_with_a_staged_pr_never_says_no_pr_was_produced() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let staged = stage_verified_pr(&execution_id);
    let handler = handler.with_staged_pr_urls(staged);
    let execution = db.get_execution(&execution_id).unwrap();

    let outcome = handler
        .fail_for_unproductive_nudges(&execution, 3, None, "no new commit, PR, or state change")
        .await;
    let StopOutcome::NudgeBreakerParked { reason } = outcome else {
        panic!("expected NudgeBreakerParked, got {outcome:?}");
    };
    assert!(
        !reason.contains("No PR was produced"),
        "failure text must not deny a PR that is already staged:\n{reason}",
    );
    assert!(
        reason.contains(PR_URL),
        "failure text must name the staged PR:\n{reason}",
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}
