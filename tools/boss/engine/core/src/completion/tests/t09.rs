//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).

use super::*;

/// Pins the wiring `dispatch_worker_event_fanout` depends on
/// (`app::worker_events::pre_pass_delivered_a_probe`'s pure predicate is
/// pinned separately) once it reaches [`WorkerCompletionHandler::on_stop_with_turn_end_deferrable`]:
/// when the pre-completion probe pass delivered a probe for this boundary,
/// the generic PR-detection/nudge/park decision must be withheld —
/// `DeferredForProbeTurn`, no `PROBE_NO_PR` nudge queued — but `stop_seen`
/// must still be stamped, because the merge poller's SHA-delta gate keys on
/// it regardless of whether this boundary's terminal decision ran.
///
/// Counterfactual: [`pre_pass_not_delivering_lets_the_stop_decision_run_and_still_stamps_stop_seen`].
#[tokio::test]
async fn pre_pass_delivering_a_probe_defers_the_stop_decision_but_still_stamps_stop_seen() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    assert!(
        !db.execution_stop_seen(&execution_id).unwrap(),
        "precondition: stop_seen must be unset before this execution's first Stop",
    );

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let outcome = handler
        .on_stop_with_turn_end_deferrable(&execution_id, None, /* defer_finalization */ true)
        .await;

    assert_eq!(
        outcome,
        StopOutcome::DeferredForProbeTurn,
        "a boundary whose pre-pass delivered a probe must withhold the terminal decision",
    );
    assert!(
        db.execution_stop_seen(&execution_id).unwrap(),
        "stop_seen must be stamped even when the terminal decision is deferred — the merge \
         poller's SHA-delta gate must see this boundary regardless",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no PROBE_NO_PR nudge must be queued while the terminal decision is deferred, got {:?}",
        probes.snapshot(),
    );
}

/// Counterfactual to
/// [`pre_pass_delivering_a_probe_defers_the_stop_decision_but_still_stamps_stop_seen`]:
/// when the pre-pass did not deliver anything (`defer_finalization = false`,
/// mirroring `pre_pass_delivered_a_probe` returning `false`), the generic
/// PR-detection/nudge/park decision must run exactly as it always has —
/// `PROBE_NO_PR` queued, `AwaitingInput` — and `stop_seen` is stamped either
/// way.
#[tokio::test]
async fn pre_pass_not_delivering_lets_the_stop_decision_run_and_still_stamps_stop_seen() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let outcome = handler
        .on_stop_with_turn_end_deferrable(&execution_id, None, /* defer_finalization */ false)
        .await;

    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "a boundary whose pre-pass did not deliver must run the terminal decision as before",
    );
    assert!(
        db.execution_stop_seen(&execution_id).unwrap(),
        "stop_seen must be stamped on the normal (non-deferred) path too",
    );
    let queued = probes.snapshot();
    assert_eq!(
        queued.len(),
        1,
        "the produce-a-PR nudge must fire when the terminal decision is not deferred, got {queued:?}",
    );
    assert_eq!(queued[0].0, execution_id);
    assert_eq!(queued[0].1, PROBE_NO_PR);
}
