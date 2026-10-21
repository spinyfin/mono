//! Cancel provenance on the dispatch timeline: every cancel the completion
//! handler performs records who cancelled the execution and why as an
//! `execution_cancelled` event (`crate::work::execution_cancelled_event`),
//! with the status that was terminalized and whether the owning row moved.
//! Before this only the merge poller's cancel reached the timeline, so a
//! cancelled row's history stopped at `pane_spawned` with nothing saying
//! who stopped it.

use super::*;

fn cancelled_events(events: &[crate::dispatch_events::DispatchEvent]) -> Vec<&crate::dispatch_events::DispatchEvent> {
    events
        .iter()
        .filter(|event| event.stage == crate::dispatch_events::Stage::ExecutionCancelled.as_str())
        .collect()
}

/// A `cancel_and_release` teardown (drag back to Backlog, work-item delete)
/// is an operator action: actor `human`, the caller's reason verbatim, the
/// prior status, exactly once, and not again for an already-terminal row.
#[tokio::test]
async fn cancel_and_release_records_provenance_on_the_dispatch_timeline() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture(workspace.path());
    let (TestHarness { handler, .. }, dispatch_events) =
        TestHarness::with_recording_dispatch_events(db.clone(), StubPrDetector::ok(None));
    let prior_status = db.get_execution(&execution_id).unwrap().status;
    let reason = "kanban drag: active → todo (dragged back to Backlog)";

    handler.cancel_and_release(&chore_id, &execution_id, reason).await;

    let events = dispatch_events.events().await;
    let cancelled = cancelled_events(&events);
    assert_eq!(cancelled.len(), 1, "exactly one execution_cancelled event: {events:?}");
    let event = cancelled[0];
    assert_eq!(event.execution_id, execution_id);
    assert_eq!(event.work_item_id.as_deref(), Some(chore_id.as_str()));
    assert_eq!(event.details["actor"], "human");
    assert_eq!(event.details["reason"], reason);
    assert_eq!(event.details["prior_status"], prior_status.as_str());
    assert!(event.details["work_item_moved"].is_boolean());

    // Idempotent: a second teardown of the already-terminal row cancels
    // nothing and must not fabricate a second provenance event.
    handler.cancel_and_release(&chore_id, &execution_id, reason).await;
    assert_eq!(cancelled_events(&dispatch_events.events().await).len(), 1);
}

/// `force_stop_execution_as` (the merge poller stopping a worker whose task
/// reached Review) records the actor the caller passed, so the timeline
/// tells an engine stop from an operator's.
#[tokio::test]
async fn force_stop_records_provenance_on_the_dispatch_timeline() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture(workspace.path());
    let (TestHarness { handler, .. }, dispatch_events) =
        TestHarness::with_recording_dispatch_events(db.clone(), StubPrDetector::ok(None));
    let reason = "task auto-transitioned to in_review (CI green); worker stopped";

    handler
        .force_stop_execution_as(&execution_id, crate::work::CancelActor::Engine, reason)
        .await;

    let events = dispatch_events.events().await;
    let cancelled = cancelled_events(&events);
    assert_eq!(cancelled.len(), 1, "{events:?}");
    let event = cancelled[0];
    assert_eq!(event.execution_id, execution_id);
    assert_eq!(event.work_item_id.as_deref(), Some(chore_id.as_str()));
    assert_eq!(event.details["actor"], "engine");
    assert_eq!(event.details["reason"], reason);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Cancelled
    );
}

/// The plain operator stop (`bossctl agents stop`, the app's stop) is
/// recorded as `human` / "explicit stop" without the caller naming it.
#[tokio::test]
async fn plain_force_stop_is_recorded_as_an_operator_stop() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _chore_id, execution_id) = fixture(workspace.path());
    let (TestHarness { handler, .. }, dispatch_events) =
        TestHarness::with_recording_dispatch_events(db.clone(), StubPrDetector::ok(None));

    handler.force_stop_execution(&execution_id).await;

    let events = dispatch_events.events().await;
    let cancelled = cancelled_events(&events);
    assert_eq!(cancelled.len(), 1, "{events:?}");
    assert_eq!(cancelled[0].details["actor"], "human");
    assert_eq!(cancelled[0].details["reason"], "explicit stop");
}
