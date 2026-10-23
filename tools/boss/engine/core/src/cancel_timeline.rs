//! Puts engine-side cancels on the execution's dispatch timeline.
//!
//! A cancel committed deep inside the work DB (a dependency block, a
//! tombstoned remediation, a review-guide replacement, any caller with no
//! dispatch sink of its own) stages an [`Event::ExecutionCancelled`] that is
//! published only after its transaction commits. This subscriber turns each
//! one flagged `on_timeline` into the `execution_cancelled` dispatch event
//! `bossctl dispatch diagnose` reads, so the timeline never stops short of a
//! `cancelled` row. Callers that record the event themselves set
//! [`crate::work::CancelExecutionOpts::caller_records_timeline`] and are
//! skipped here, so a cancel is never recorded twice.

use std::sync::Arc;

use boss_event_bus::{Event, EventBus, EventKind, TopicFilter};

use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};

/// The timeline event for a committed cancel, or `None` when the caller
/// records it (or `event` is not a cancel).
pub fn timeline_event(event: &Event) -> Option<DispatchEvent> {
    let Event::ExecutionCancelled {
        execution_id,
        task_id,
        from_status,
        actor,
        reason,
        work_item_moved,
        queued_only,
        peer_pid,
        on_timeline: true,
    } = event
    else {
        return None;
    };
    Some(
        DispatchEvent::new(Stage::ExecutionCancelled, Outcome::Ok, execution_id)
            .with_work_item(task_id)
            .with_details(serde_json::json!({
                "actor": actor,
                "reason": reason,
                "prior_status": from_status,
                "work_item_moved": work_item_moved,
                "queued_only": queued_only,
                "peer_pid": peer_pid,
            })),
    )
}

/// Subscribe to cancels and write each one to `dispatch_events`.
pub fn spawn_subscriber(
    dispatch_events: Arc<dyn DispatchEventSink>,
    event_bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()> {
    boss_event_bus::spawn_supervised("cancel_timeline", move || {
        let dispatch_events = Arc::clone(&dispatch_events);
        let event_bus = Arc::clone(&event_bus);
        async move {
            let mut subscription = event_bus.subscribe(TopicFilter::kind(EventKind::ExecutionCancelled));
            while let Some(event) = subscription.recv().await {
                if let Some(timeline) = timeline_event(&event) {
                    dispatch_events.emit(timeline).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;

    fn cancelled(on_timeline: bool) -> Event {
        Event::ExecutionCancelled {
            execution_id: "exec_1".into(),
            task_id: "task_1".into(),
            from_status: "ready".into(),
            actor: "engine".into(),
            reason: "dependent transitioned to blocked".into(),
            work_item_moved: false,
            queued_only: false,
            peer_pid: None,
            on_timeline,
        }
    }

    #[test]
    fn a_caller_recorded_cancel_is_not_written_twice() {
        assert!(timeline_event(&cancelled(false)).is_none());
        assert!(timeline_event(&Event::DispatchReady).is_none());
    }

    #[tokio::test]
    async fn a_published_cancel_lands_on_the_execution_timeline() {
        let bus = Arc::new(EventBus::new());
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let _handle = spawn_subscriber(sink.clone(), bus.clone());
        // The subscription is created inside the spawned task; publish until
        // it is live (the bus has no replay).
        for _ in 0..200 {
            bus.publish(cancelled(true));
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if !sink.events_for("exec_1").await.is_empty() {
                break;
            }
        }
        let events = sink.events_for("exec_1").await;
        assert!(!events.is_empty(), "the cancel reached the timeline");
        assert_eq!(events[0].stage, Stage::ExecutionCancelled.as_str());
        assert_eq!(events[0].details["actor"], "engine");
        assert_eq!(events[0].details["prior_status"], "ready");
    }
}
