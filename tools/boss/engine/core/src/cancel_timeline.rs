//! Publishes transactionally persisted cancels to the dispatch timeline.
//!
//! Startup drains recovery-migration cancels, and a periodic drain covers
//! dropped bus notifications. A crash after append but before acknowledgement
//! can repeat an event; the durable record retains its original timestamp.

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

/// Read committed cancels that still need their timeline mirror.
#[work_db_boundary::instrument]
impl crate::work::WorkDb {
    pub(crate) fn pending_cancel_events(&self) -> anyhow::Result<Vec<DispatchEvent>> {
        let conn = self.connect()?;
        let mut stmt =
            conn.prepare("SELECT event_json FROM execution_cancel_events WHERE delivered = 0 ORDER BY execution_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    fn acknowledge_cancel_event(&self, id: &str) -> anyhow::Result<()> {
        self.connect()?.execute(
            "UPDATE execution_cancel_events SET delivered = 1 WHERE execution_id = ?1",
            [id],
        )?;
        Ok(())
    }
}

/// Drain durable cancels through the same sink read by dispatch diagnose.
pub(crate) async fn drain(db: &crate::work::WorkDb, sink: &dyn DispatchEventSink) -> anyhow::Result<()> {
    for event in db.pending_cancel_events()? {
        let id = event.execution_id.clone();
        sink.emit(event).await;
        db.acknowledge_cancel_event(&id)?;
    }
    Ok(())
}

/// Subscribe before the first drain so startup and live cancels share one path.
pub fn spawn_subscriber(
    db: Arc<crate::work::WorkDb>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    event_bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()> {
    boss_event_bus::spawn_supervised("cancel_timeline", move || {
        let db = Arc::clone(&db);
        let dispatch_events = Arc::clone(&dispatch_events);
        let mut subscription = event_bus.subscribe(TopicFilter::kind(EventKind::ExecutionCancelled));
        async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
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
    async fn a_committed_cancel_is_readable_by_dispatch_diagnose() {
        use crate::test_support::{create_ready_chore_execution, create_test_chore_manual, create_test_product};
        let db = Arc::new(crate::work::WorkDb::open_in_memory().unwrap());
        let product = create_test_product(&db);
        let id = create_test_chore_manual(&db, product.id, "cancel").id;
        let execution = create_ready_chore_execution(&db, id);
        db.cancel_execution(&execution.id).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(crate::dispatch_events::JsonlFileSink::new(dir.path()));
        let handle = spawn_subscriber(db.clone(), sink, db.event_bus().clone());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !db.pending_cancel_events().unwrap().is_empty() {
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
        assert!(db.pending_cancel_events().unwrap().is_empty());
    }
}
