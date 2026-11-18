//! Cancel provenance: every cancel the completion handler performs stages a
//! durable record of who cancelled the execution and why, with the status
//! that was terminalized and whether the owning row moved. The engine's cancel
//! publisher turns that record into the `execution_cancelled` timeline event
//! and audit entry, so a cancelled row's history never stops at
//! `pane_spawned` and no caller has to emit the event itself.

use super::*;

fn cancel_records_for(db: &WorkDb, execution_id: &str) -> Vec<crate::work::ExecutionCancelRecord> {
    db.pending_cancel_records()
        .unwrap()
        .into_iter()
        .filter(|record| record.execution_id == execution_id)
        .collect()
}

/// A `cancel_and_release` teardown (drag back to Backlog, work-item delete)
/// records the actor the caller resolved, the caller's reason verbatim and the
/// prior status, exactly once, and not again for an already-terminal row.
#[tokio::test]
async fn cancel_and_release_records_provenance() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture(workspace.path());
    let (TestHarness { handler, .. }, _dispatch_events) =
        TestHarness::with_recording_dispatch_events(db.clone(), StubPrDetector::ok(None));
    let prior_status = db.get_execution(&execution_id).unwrap().status;
    let reason = "kanban drag: active → todo (dragged back to Backlog)";

    handler
        .cancel_and_release(&chore_id, &execution_id, boss_protocol::StatusActor::Human, reason)
        .await;

    let records = cancel_records_for(&db, &execution_id);
    assert_eq!(records.len(), 1, "exactly one cancel record: {records:?}");
    let record = &records[0];
    assert_eq!(record.work_item_id, chore_id);
    assert_eq!(record.actor, "human");
    assert_eq!(record.reason, reason);
    assert_eq!(record.from_status, prior_status.as_str());

    // Idempotent: a second teardown of the already-terminal row cancels
    // nothing and must not fabricate a second record.
    handler
        .cancel_and_release(&chore_id, &execution_id, boss_protocol::StatusActor::Human, reason)
        .await;
    assert_eq!(cancel_records_for(&db, &execution_id).len(), 1);
}

/// `force_stop_execution_as` (the merge poller stopping a worker whose task
/// reached Review) records the actor the caller passed, so the timeline
/// tells an engine stop from a person's.
#[tokio::test]
async fn force_stop_records_provenance() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture(workspace.path());
    let (TestHarness { handler, .. }, _dispatch_events) =
        TestHarness::with_recording_dispatch_events(db.clone(), StubPrDetector::ok(None));
    let reason = "task auto-transitioned to in_review (CI green); worker stopped";

    handler
        .force_stop_execution_as(&execution_id, boss_protocol::StatusActor::Engine, reason)
        .await;

    let records = cancel_records_for(&db, &execution_id);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].work_item_id, chore_id);
    assert_eq!(records[0].actor, "engine");
    assert_eq!(records[0].reason, reason);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Cancelled
    );
}

/// A stop issued from the coordinator session resolves to `boss`, and that
/// actor — not `human` — reaches the cancel record, the audit log and the
/// park text.
#[tokio::test]
async fn coordinator_stop_is_recorded_as_boss() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture(workspace.path());
    let (TestHarness { handler, .. }, _dispatch_events) =
        TestHarness::with_recording_dispatch_events(db.clone(), StubPrDetector::ok(None));

    handler
        .force_stop_execution_as(&execution_id, boss_protocol::StatusActor::Boss, "explicit stop")
        .await;

    let records = cancel_records_for(&db, &execution_id);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].actor, "boss");
    assert_eq!(records[0].reason, "explicit stop");
    let parked = match db.get_work_item(&chore_id).unwrap() {
        crate::work::WorkItem::Task(task) | crate::work::WorkItem::Chore(task) => task,
        other => panic!("expected a task/chore, got {other:?}"),
    };
    assert!(
        parked
            .dispatch_failed_error
            .as_deref()
            .unwrap_or_default()
            .contains("cancelled by the coordinator (explicit stop)"),
        "{:?}",
        parked.dispatch_failed_error
    );
}
