//! Re-exports the extracted [`boss_dispatch_events`] transport crate and adds
//! the Boothby-specific sinks, which need `crate::boothby_events` (a
//! core-level type) and so cannot live in the low-level transport crate
//! without introducing a reverse dependency edge.

pub use boss_dispatch_events::*;

use std::sync::Arc;

use async_trait::async_trait;
pub use boss_dispatch_events::{DispatchEvent, DispatchEventSink};

/// Fans one [`DispatchEvent`] out to multiple sinks. `ServerState` holds a
/// single `Arc<dyn DispatchEventSink>`; this lets that stay a single trait
/// object while `boothby.md`'s design calls for "the fan-out
/// `DispatchEventSink`" feeding both the production JSONL stream
/// ([`boss_dispatch_events::JsonlFileSink`]) and Boothby's event-trigger
/// queue ([`BoothbyEventSink`]).
pub struct FanOutDispatchEventSink {
    sinks: Vec<Arc<dyn DispatchEventSink>>,
}

impl FanOutDispatchEventSink {
    pub fn new(sinks: Vec<Arc<dyn DispatchEventSink>>) -> Self {
        Self { sinks }
    }
}

#[async_trait]
impl DispatchEventSink for FanOutDispatchEventSink {
    async fn emit(&self, event: DispatchEvent) {
        for sink in &self.sinks {
            sink.emit(event.clone()).await;
        }
    }
}

/// Arms Boothby's event-trigger queue ([`crate::boothby_events::BoothbyEventQueue`])
/// when a dispatch stage in [`crate::boothby_events::BOOTHBY_TRIGGER_STAGES`]
/// fires. Any other stage — including every stage that exists today outside
/// that list, and any stage added in the future — is a silent no-op, per the
/// design's "unknown stages are no-ops" rule (`boothby.md` §"Risks" #6).
pub struct BoothbyEventSink {
    queue: Arc<crate::boothby_events::BoothbyEventQueue>,
}

impl BoothbyEventSink {
    pub fn new(queue: Arc<crate::boothby_events::BoothbyEventQueue>) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl DispatchEventSink for BoothbyEventSink {
    async fn emit(&self, event: DispatchEvent) {
        let is_trigger_stage = crate::boothby_events::BOOTHBY_TRIGGER_STAGES
            .iter()
            .any(|stage| stage.as_str() == event.stage);
        if is_trigger_stage {
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            self.queue.arm(
                now,
                &format!("{}{}", crate::boothby_events::BOOTHBY_EVENT_TRIGGER_PREFIX, event.stage),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_dispatch_events::{Outcome, RecordingDispatchEventSink, Stage};

    #[tokio::test]
    async fn fan_out_sink_forwards_to_every_sink() {
        let a = Arc::new(RecordingDispatchEventSink::new());
        let b = Arc::new(RecordingDispatchEventSink::new());
        let fan_out = FanOutDispatchEventSink::new(vec![a.clone(), b.clone()]);

        fan_out
            .emit(DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-1"))
            .await;

        assert_eq!(a.events().await.len(), 1);
        assert_eq!(b.events().await.len(), 1);
    }

    #[tokio::test]
    async fn boothby_event_sink_arms_the_queue_for_a_trigger_stage() {
        let queue = crate::boothby_events::BoothbyEventQueue::new(Arc::new(tokio::sync::Notify::new()));
        let sink = BoothbyEventSink::new(queue.clone());

        sink.emit(DispatchEvent::new(Stage::DeadPidReconcile, Outcome::Ok, "exec-1"))
            .await;

        assert_eq!(
            queue.take_due(boss_engine_utils::epoch_time::now_epoch_secs() + 10_000, 0),
            Some("event:dead_pid_reconcile".to_owned()),
        );
    }

    #[tokio::test]
    async fn boothby_event_sink_ignores_a_non_trigger_stage() {
        let queue = crate::boothby_events::BoothbyEventQueue::new(Arc::new(tokio::sync::Notify::new()));
        let sink = BoothbyEventSink::new(queue.clone());

        sink.emit(DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "exec-1"))
            .await;

        assert_eq!(
            queue.take_due(boss_engine_utils::epoch_time::now_epoch_secs() + 10_000, 0),
            None,
            "pane_spawned is not a Boothby trigger stage"
        );
    }
}
