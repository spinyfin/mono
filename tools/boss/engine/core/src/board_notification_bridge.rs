//! Bridge from committed work-item changes on the event bus to board
//! `work_invalidated` pushes.
//!
//! Database-side writers (dependency and dispatch recovery, revision settle,
//! review-verdict application) often run with no frontend request to publish
//! on their behalf. They stage an [`boss_event_bus::Event`] in their
//! transaction instead, and this bridge turns each committed one into the
//! push that makes the app refetch.

use std::sync::Arc;

/// Forward every committed `WorkItemChanged` and `ReviewVerdictApplied` event
/// to the board as a work-item invalidation.
pub fn spawn_notifications(
    event_bus: Arc<boss_event_bus::EventBus>,
    publisher: Arc<dyn boss_execution_contracts::ExecutionPublisher>,
) -> tokio::task::JoinHandle<()> {
    // Subscribe before returning so an immediately applied verdict is not lost.
    let subscription = event_bus.subscribe(boss_event_bus::TopicFilter::kinds([
        boss_event_bus::EventKind::ReviewVerdictApplied,
        boss_event_bus::EventKind::WorkItemChanged,
    ]));
    boss_task_scope::spawn(async move {
        forward_notifications(subscription, publisher.as_ref()).await;
    })
}

async fn forward_notifications(
    mut subscription: boss_event_bus::Subscription,
    publisher: &dyn boss_execution_contracts::ExecutionPublisher,
) {
    while let Some(event) = subscription.recv().await {
        let (product_id, task_id, reason) = match event {
            boss_event_bus::Event::ReviewVerdictApplied { product_id, task_id } => {
                (product_id, task_id, "review_verdict_applied")
            }
            boss_event_bus::Event::WorkItemChanged { product_id, task_id } => {
                (product_id, task_id, "work_item_changed")
            }
            _ => continue,
        };
        publisher.publish_work_item_changed(&product_id, &task_id, reason).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn task_changes_reach_the_board_through_the_registered_bridge() {
        let bus = Arc::new(boss_event_bus::EventBus::new());
        let publisher = Arc::new(crate::test_support::RecordingPublisher::default());
        let handle = spawn_notifications(bus.clone(), publisher.clone());
        bus.publish(boss_event_bus::Event::WorkItemChanged {
            product_id: "product".into(),
            task_id: "task".into(),
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !publisher.events.lock().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *publisher.events.lock().await,
            vec![("product".into(), "task".into(), "work_item_changed".into())]
        );
        handle.abort();
        let _ = handle.await;
    }
}
