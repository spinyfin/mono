//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).
//!
//! One `#[tokio::test]` per Stop-hook finalizer that terminalizes a
//! parked-live execution and therefore owns driver teardown —
//! `finalize_pr_transition`, `finalize_pr_review_pass`,
//! `finalize_no_op_completion`, and `finalize_idle_park` — following the
//! established one-test-per-call-site convention already used for
//! `force_release` (`t01::force_release_tears_down_driver_workspace`),
//! `finalize_gone_execution` (`execution_liveness.rs`), and
//! `record_run_completion` (`coordinator_tests/dispatch.rs`). A missing
//! `driver_teardown::teardown_driver_workspace` call at any of these sites is
//! otherwise silent — only visible by grepping engine logs in production —
//! so each test asserts the call count directly via
//! `driver_teardown::test_hooks::{reset, count}`.

use super::*;

#[tokio::test]
async fn finalize_pr_transition_tears_down_driver_workspace() {
    crate::driver_teardown::test_hooks::reset();

    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/1449"));

    let handler = TestHarness::new(db.clone(), detector)
        .handler
        // Bypass the reviewer so on_stop lands directly in
        // `finalize_pr_transition`'s in_review path (not PendingReview).
        .with_max_review_cycles(0);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { .. }),
        "expected PrDetected; got {outcome:?}",
    );
    assert_eq!(
        crate::driver_teardown::test_hooks::count(),
        1,
        "finalize_pr_transition must invoke driver teardown exactly once",
    );

    // Idempotency: a re-fired Stop on the now-terminal execution hits the
    // `Ok(None) => AlreadyTerminal` early return and must not tear down again.
    let outcome2 = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome2, StopOutcome::AlreadyTerminal),
        "expected AlreadyTerminal on re-fire; got {outcome2:?}",
    );
    assert_eq!(
        crate::driver_teardown::test_hooks::count(),
        1,
        "a re-fired Stop on an already-terminal execution must not tear down driver workspace again",
    );
}

#[tokio::test]
async fn finalize_pr_review_pass_tears_down_driver_workspace() {
    crate::driver_teardown::test_hooks::reset();

    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    let json = clean_review_result_json(pr_url);
    let (_dir, db, _product_id, _chore_id, pr_review_exec_id, _pr_url) =
        pr_review_exec_fixture(workspace.path(), Some(&json));

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker());

    let outcome = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewPassCompleted { .. }),
        "expected ReviewPassCompleted; got {outcome:?}",
    );
    assert_eq!(
        crate::driver_teardown::test_hooks::count(),
        1,
        "finalize_pr_review_pass must invoke driver teardown exactly once",
    );

    // Idempotency: re-firing Stop on the now-terminal reviewer execution
    // must not tear down a second time.
    let outcome2 = handler.on_stop(&pr_review_exec_id).await;
    assert!(
        matches!(outcome2, StopOutcome::AlreadyTerminal),
        "expected AlreadyTerminal on re-fire; got {outcome2:?}",
    );
    assert_eq!(
        crate::driver_teardown::test_hooks::count(),
        1,
        "a re-fired Stop on an already-terminal reviewer execution must not tear down again",
    );
}

#[tokio::test]
async fn finalize_no_op_completion_tears_down_driver_workspace() {
    crate::driver_teardown::test_hooks::reset();

    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "## Summary\nNothing left to do; the working copy has no diff.\n\nNO_CHANGES_NEEDED\n",
    );
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::NoChangesNeeded { .. }),
        "expected NoChangesNeeded; got {outcome:?}",
    );
    assert_eq!(
        crate::driver_teardown::test_hooks::count(),
        1,
        "finalize_no_op_completion must invoke driver teardown exactly once",
    );

    // Idempotency: a re-fired Stop on the now-closed no-op must not tear
    // down again.
    let outcome2 = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome2, StopOutcome::AlreadyTerminal),
        "expected AlreadyTerminal on re-fire; got {outcome2:?}",
    );
    assert_eq!(
        crate::driver_teardown::test_hooks::count(),
        1,
        "a re-fired Stop on an already-closed no-op must not tear down driver workspace again",
    );
}

#[tokio::test]
async fn finalize_idle_park_tears_down_driver_workspace() {
    crate::driver_teardown::test_hooks::reset();

    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_max_unproductive_nudges(2);

    // The legitimate produce-a-PR nudge fires up to the cap, then the third
    // Stop trips the breaker and finalizes via `finalize_idle_park`.
    let _o1 = handler.on_stop(&execution_id).await;
    let _o2 = handler.on_stop(&execution_id).await;
    let o3 = handler.on_stop(&execution_id).await;
    assert!(
        matches!(o3, StopOutcome::NudgeBreakerParked { .. }),
        "breaker must bound the no-PR nudge after the cap; got {o3:?}",
    );
    assert_eq!(
        crate::driver_teardown::test_hooks::count(),
        1,
        "finalize_idle_park must invoke driver teardown exactly once",
    );

    // Idempotency: a further Stop on the now-abandoned execution must not
    // tear down again.
    let o4 = handler.on_stop(&execution_id).await;
    assert!(
        matches!(o4, StopOutcome::AlreadyTerminal),
        "expected AlreadyTerminal on re-fire; got {o4:?}",
    );
    assert_eq!(
        crate::driver_teardown::test_hooks::count(),
        1,
        "a re-fired Stop on an already-abandoned execution must not tear down driver workspace again",
    );
}
