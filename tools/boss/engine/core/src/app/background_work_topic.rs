//! Push channel for the global background-work snapshot.
//!
//! The toolbar badge used to be fed by the app polling `list_engine_attempts`.
//! Here the engine instead publishes the snapshot on
//! [`TOPIC_ENGINE_BACKGROUND_WORK`] whenever one of its inputs changes. Inputs
//! announce themselves with [`boss_event_bus::Event::BackgroundWorkChanged`]
//! (see `WorkDb::notify_background_work_changed`); a supervised task turns
//! each announcement into one recompute, however many sessions are subscribed,
//! and pushes only when the result differs from the last one. A session that
//! subscribes is served the last computed snapshot from memory.
use super::*;
use boss_protocol::{BackgroundWorkItem, TOPIC_ENGINE_BACKGROUND_WORK};
use std::time::Duration;

/// How long to wait before recomputing after a failed snapshot read, so a
/// transient database error does not leave the badge stale until the next
/// unrelated change. Runs only after an error; a healthy engine never waits.
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(10);

fn snapshot_envelope(items: Vec<BackgroundWorkItem>) -> FrontendEventEnvelope {
    FrontendEventEnvelope::push(FrontendEvent::BackgroundWorkSnapshot { items })
}

impl TopicBroker {
    /// Recompute the snapshot and fan it out to subscribers if it changed.
    /// Returns when the publisher should next recompute without being told: the
    /// moment a still-hidden planner run turns old enough to be listed, or a
    /// retry after a failed read.
    pub(super) async fn refresh_background_work(&self, db: &WorkDb) -> Option<Duration> {
        // Held across the publish so an older snapshot can never land after a
        // newer one, and a subscribing session sees one or the other.
        let mut last = self.background_work_snapshot.lock().await;
        match crate::background_work::snapshot(db) {
            Ok(items) => {
                if last.as_ref() != Some(&items) {
                    *last = Some(items.clone());
                    self.publish(TOPIC_ENGINE_BACKGROUND_WORK, snapshot_envelope(items))
                        .await;
                }
            }
            Err(err) => {
                tracing::warn!(?err, "background_work snapshot failed; keeping the last one published");
                return Some(RETRY_AFTER_ERROR);
            }
        }
        drop(last);
        crate::background_work::next_planner_eligibility(db)
            .inspect_err(|err| tracing::warn!(?err, "could not find the next planner run to become eligible"))
            .unwrap_or(Some(RETRY_AFTER_ERROR))
    }

    /// Send the current snapshot to one session that has just subscribed.
    pub(super) async fn replay_background_work(&self, db: &WorkDb, sink: &SessionSink) {
        let mut last = self.background_work_snapshot.lock().await;
        if last.is_none() {
            match crate::background_work::snapshot(db) {
                Ok(items) => *last = Some(items),
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        "background_work snapshot failed; subscriber gets none until the next change"
                    );
                    return;
                }
            }
        }
        if let Some(items) = last.clone() {
            sink.enqueue(snapshot_envelope(items));
        }
    }
}

impl ServerState {
    /// Keep `engine.background_work` current: recompute on every
    /// [`boss_event_bus::Event::BackgroundWorkChanged`], and once more when a
    /// planner run that is still too young to list becomes old enough. Nothing
    /// polls: the loop is parked on the bus (or on that one deadline).
    ///
    /// Supervised like the other broadcasters; each (re)start recomputes
    /// before waiting, which closes any gap a panic left.
    pub fn spawn_background_work_publisher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak_state = Arc::downgrade(self);
        boss_event_bus::spawn_supervised("background_work_publisher", move || {
            let weak_state = weak_state.clone();
            async move {
                let Some(state) = weak_state.upgrade() else {
                    return;
                };
                // Subscribe before the first recompute so a change landing
                // between the two is not lost.
                let mut changes = state.event_bus.subscribe(boss_event_bus::TopicFilter::kind(
                    boss_event_bus::EventKind::BackgroundWorkChanged,
                ));
                let mut wake_in = state.topic_broker.refresh_background_work(&state.work_db).await;
                drop(state);
                loop {
                    let next = match wake_in {
                        Some(delay) => tokio::time::timeout(delay, changes.recv()).await,
                        None => Ok(changes.recv().await),
                    };
                    // A timeout is the planner age gate (or error retry) elapsing.
                    if matches!(next, Ok(None)) {
                        break;
                    }
                    let Some(state) = weak_state.upgrade() else {
                        return;
                    };
                    wake_in = state.topic_broker.refresh_background_work(&state.work_db).await;
                }
                tracing::warn!("background-work publisher: event bus closed; the snapshot will no longer push");
            }
        })
    }
}
