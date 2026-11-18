//! Publishes committed cancels to `engine-audit.log` and the dispatch timeline.
//!
//! Every cancel writes a record in its own transaction
//! (`boss_work_db::ExecutionCancelRecord`), so a rolled-back cancel leaves
//! nothing to publish. [`drain`] writes the `execution_cancelled` audit record
//! and the `execution_cancelled` dispatch event that `bossctl dispatch
//! diagnose` reads for each undelivered one, then acknowledges it. The durable
//! record is the only path to either log, for every cancel, so no caller has
//! to remember to write the timeline event after its transaction commits.
//!
//! The subscriber drains at startup, on every cancel notification, and
//! periodically in case a notification was dropped. Delivery is at least
//! once: a record stays pending until both writes succeed, so a crash or a
//! failed write repeats the record, which keeps the time the cancel was
//! originally written.

use std::sync::Arc;

use boss_event_bus::{EventBus, EventKind, TopicFilter};

use crate::dispatch_events::{DispatchEvent, DispatchEventSink};
use crate::work::{ExecutionCancelRecord, WorkDb};

/// How often the subscriber drains without having been notified.
const BACKSTOP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// The timeline event for a committed cancel. Stamped with the time the cancel
/// was written, not the time it is published.
pub fn timeline_event(record: &ExecutionCancelRecord) -> DispatchEvent {
    let mut event = crate::dispatch_events::execution_cancelled_event(
        &record.execution_id,
        &record.work_item_id,
        &record.from_status,
        &record.actor,
        &record.reason,
        record.work_item_moved,
    );
    event.details["queued_only"] = serde_json::Value::Bool(record.queued_only);
    event.details["peer_pid"] = serde_json::json!(record.peer_pid);
    if let Ok(secs) = record.cancelled_at.parse::<u128>() {
        event.ts_epoch_ms = secs * 1000;
    }
    event
}

fn record_audit(record: &ExecutionCancelRecord) -> std::io::Result<()> {
    crate::audit::try_record_event(
        "execution_cancelled",
        &serde_json::json!({
            "execution_id": record.execution_id,
            "work_item_id": record.work_item_id,
            "from_status": record.from_status,
            "actor": record.actor,
            "reason": record.reason,
            "work_item_moved": record.work_item_moved,
            "queued_only": record.queued_only,
            "peer_pid": record.peer_pid,
            "cancelled_at": record.cancelled_at,
        }),
    )
}

/// Publish every committed cancel that has not been published yet. Returns
/// how many were published. A record whose audit or timeline write failed
/// stays pending for the next drain; the others are still published.
pub async fn drain(db: &WorkDb, sink: &dyn DispatchEventSink) -> anyhow::Result<usize> {
    let records = db.pending_cancel_records()?;
    let mut published = 0;
    for record in &records {
        if let Err(error) = record_audit(record) {
            tracing::warn!(%error, execution_id = %record.execution_id, "cancel audit write failed; will retry");
            continue;
        }
        if let Err(error) = sink.try_emit(timeline_event(record)).await {
            tracing::warn!(%error, execution_id = %record.execution_id, "cancel timeline write failed; will retry");
            continue;
        }
        db.acknowledge_cancel_record(&record.execution_id)?;
        published += 1;
    }
    Ok(published)
}

/// Subscribe before the first drain so startup and live cancels share one path.
pub fn spawn_subscriber(
    db: Arc<WorkDb>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    event_bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()> {
    boss_event_bus::spawn_supervised("cancel_timeline", move || {
        let db = Arc::clone(&db);
        let dispatch_events = Arc::clone(&dispatch_events);
        let mut subscription = event_bus.subscribe(TopicFilter::kind(EventKind::ExecutionCancelled));
        async move {
            let mut interval = tokio::time::interval(BACKSTOP_INTERVAL);
            loop {
                if let Err(error) = drain(&db, dispatch_events.as_ref()).await {
                    tracing::warn!(%error, "could not publish committed cancels");
                }
                tokio::select! {
                    _ = interval.tick() => {},
                    event = subscription.recv() => { if event.is_none() { break; } },
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_events::{JsonlFileSink, RecordingDispatchEventSink, Stage};
    use crate::test_support::{create_ready_chore_execution, create_test_chore_manual, create_test_product};
    use crate::work::{CancelExecutionOpts, StatusActor};

    fn audit_records_for(execution_id: &str) -> Vec<serde_json::Value> {
        let path = crate::audit::default_audit_log_path().expect("a test process resolves its isolated audit path");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["event"] == "execution_cancelled" && record["execution_id"] == execution_id)
            .collect()
    }

    /// Drive one future to completion from a synchronous test, so the audit
    /// lock is never held across an await point.
    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn ready_execution(db: &WorkDb) -> boss_protocol::WorkExecution {
        let product = create_test_product(db);
        let chore = create_test_chore_manual(db, product.id, "cancel");
        create_ready_chore_execution(db, chore.id)
    }

    fn record() -> ExecutionCancelRecord {
        ExecutionCancelRecord {
            execution_id: "exec_1".into(),
            work_item_id: "task_1".into(),
            from_status: "ready".into(),
            actor: "engine".into(),
            reason: "dependent transitioned to blocked".into(),
            work_item_moved: false,
            queued_only: false,
            peer_pid: None,
            cancelled_at: "1700000000".into(),
        }
    }

    #[test]
    fn the_timeline_event_keeps_the_time_the_cancel_was_written() {
        let event = timeline_event(&record());
        assert_eq!(event.stage, Stage::ExecutionCancelled.as_str());
        assert_eq!(event.ts_epoch_ms, 1_700_000_000_000);
        assert_eq!(event.details["actor"], "engine");
        assert_eq!(event.details["prior_status"], "ready");
        assert_eq!(event.details["reason"], "dependent transitioned to blocked");
    }

    /// A cancel from a path with no sink of its own gets both records, with
    /// the actor and reason it named, and is published exactly once.
    #[test]
    fn a_committed_cancel_reaches_the_audit_log_and_the_timeline_once() {
        let _audit = crate::audit::lock_audit_globals_for_tests();
        let db = WorkDb::open_in_memory().unwrap();
        let execution = ready_execution(&db);
        db.cancel_execution_with(
            &execution.id,
            CancelExecutionOpts {
                reason: Some("coordinator cancelled it".to_owned()),
                actor: StatusActor::Boss,
                ..CancelExecutionOpts::default()
            },
        )
        .unwrap();
        assert!(
            audit_records_for(&execution.id).is_empty(),
            "nothing is published before the drain"
        );

        let sink = RecordingDispatchEventSink::new();
        assert_eq!(block_on(drain(&db, &sink)).unwrap(), 1);
        assert_eq!(
            block_on(drain(&db, &sink)).unwrap(),
            0,
            "an acknowledged cancel is not repeated"
        );

        let records = audit_records_for(&execution.id);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["actor"], "boss");
        assert_eq!(records[0]["reason"], "coordinator cancelled it");
        assert_eq!(records[0]["from_status"], "ready");
        assert_eq!(records[0]["work_item_moved"], true);
        let events = block_on(sink.events());
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].execution_id, execution.id);
        assert_eq!(events[0].details["actor"], "boss");
        assert_eq!(events[0].details["reason"], "coordinator cancelled it");
    }

    /// A cancel issued over RPC is audited with `queued_only` and the issuing
    /// pid, and reaches the timeline from the same record.
    #[test]
    fn an_rpc_cancel_is_audited_with_queued_only_and_peer_pid() {
        let _audit = crate::audit::lock_audit_globals_for_tests();
        let db = WorkDb::open_in_memory().unwrap();
        let execution = ready_execution(&db);
        db.cancel_execution_with(
            &execution.id,
            CancelExecutionOpts {
                queued_only: true,
                peer_pid: Some(4242),
                ..CancelExecutionOpts::default()
            },
        )
        .unwrap();

        let sink = RecordingDispatchEventSink::new();
        assert_eq!(block_on(drain(&db, &sink)).unwrap(), 1);

        assert_eq!(block_on(sink.events()).len(), 1);
        let records = audit_records_for(&execution.id);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["queued_only"], true);
        assert_eq!(records[0]["peer_pid"], 4242);
    }

    /// A sink whose write fails, so the cancel it was given must stay pending.
    struct FailingSink;

    #[async_trait::async_trait]
    impl DispatchEventSink for FailingSink {
        async fn emit(&self, _event: DispatchEvent) {}

        async fn try_emit(&self, _event: DispatchEvent) -> Result<(), String> {
            Err("disk full".to_owned())
        }
    }

    /// A failed timeline write must not acknowledge the cancel: the next
    /// drain publishes it.
    #[test]
    fn a_failed_write_leaves_the_cancel_pending_until_a_drain_succeeds() {
        let _audit = crate::audit::lock_audit_globals_for_tests();
        let db = WorkDb::open_in_memory().unwrap();
        let execution = ready_execution(&db);
        db.cancel_execution(&execution.id).unwrap();

        assert_eq!(block_on(drain(&db, &FailingSink)).unwrap(), 0);
        assert_eq!(db.pending_cancel_records().unwrap().len(), 1);

        let sink = RecordingDispatchEventSink::new();
        assert_eq!(block_on(drain(&db, &sink)).unwrap(), 1);
        assert!(db.pending_cancel_records().unwrap().is_empty());
        assert_eq!(block_on(sink.events()).len(), 1);
    }

    /// The path that used to skip the timeline because its caller was to
    /// write the event: a preemption/teardown cancel committed with no caller
    /// emit at all still reaches the timeline.
    #[test]
    fn a_running_execution_teardown_reaches_the_timeline_without_a_caller_emit() {
        let _audit = crate::audit::lock_audit_globals_for_tests();
        let db = WorkDb::open_in_memory().unwrap();
        let execution = ready_execution(&db);
        assert!(db.cancel_running_execution(&execution.id).unwrap());

        let sink = RecordingDispatchEventSink::new();
        assert_eq!(block_on(drain(&db, &sink)).unwrap(), 1);
        let events = block_on(sink.events());
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].execution_id, execution.id);
    }

    #[tokio::test]
    async fn the_subscriber_makes_a_cancel_readable_by_dispatch_diagnose() {
        let db = Arc::new(WorkDb::open_in_memory().unwrap());
        let execution = ready_execution(&db);
        db.cancel_execution(&execution.id).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(JsonlFileSink::new(dir.path()));
        let handle = spawn_subscriber(db.clone(), sink, db.event_bus().clone());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !db.pending_cancel_records().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        handle.abort();
        let events = crate::dispatch_reader::read_execution(dir.path(), &execution.id)
            .unwrap()
            .events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, Stage::ExecutionCancelled.as_str());
        assert_eq!(events[0].details["actor"], "engine");
        assert_eq!(events[0].details["prior_status"], "ready");
    }
}
