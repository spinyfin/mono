//! Periodic reconciler for staged `review_verdict` proposals.
//!
//! Submission leaves a verdict `proposed` so GitHub probes and remediation
//! task creation do not block the worker socket. This sweep (and the
//! post-submit kick in `app::proposals`) applies those proposals: one
//! verdict/cycle update per batch, a revision while the origin PR is open,
//! or a follow-up against `main` if it has merged.

use std::sync::Arc;
use std::time::Duration;

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::DispatchEventSink;
use crate::work::{GhPrStateChecker, ReviewVerdictApplyStats, WorkDb};

/// Bridge committed verdicts and task changes to work-tree invalidations.
/// Database-side producers use this same bridge for transitions whose
/// callers may have no frontend request (dependency and dispatch recovery).
pub fn spawn_notifications(
    event_bus: Arc<boss_event_bus::EventBus>,
    publisher: Arc<dyn crate::coordinator::ExecutionPublisher>,
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
    publisher: &dyn crate::coordinator::ExecutionPublisher,
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

/// Cadence for the crash-recovery pass. Fresh submissions are applied
/// immediately from the proposal handler; this interval only covers
/// engine restarts and a failed first attempt.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

impl crate::sweep_loop::SweepOutcome for ReviewVerdictApplyStats {
    fn has_activity(&self) -> bool {
        self.applied > 0 || self.failed > 0 || self.superseded > 0
    }

    fn log(&self) {
        tracing::info!(
            applied = self.applied,
            failed = self.failed,
            created_work = self.created_work,
            superseded = self.superseded,
            "review-verdict apply sweep: applied staged review_verdict proposals",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_work_sweep_loop(
        work_db,
        coordinator,
        dispatch_events,
        interval,
        |work_db, coordinator, _dispatch_events| Box::pin(run_one_pass(work_db, coordinator)),
    )
}

/// Apply every still-proposed `review_verdict`. Kicks the scheduler when
/// a revision or follow-up was created so it does not wait for the next
/// heartbeat.
pub async fn run_one_pass(work_db: &WorkDb, coordinator: Arc<ExecutionCoordinator>) -> ReviewVerdictApplyStats {
    let work_db_clone = work_db.clone();
    let stats =
        match tokio::task::spawn_blocking(move || work_db_clone.apply_pending_review_verdicts(&GhPrStateChecker)).await
        {
            Ok(Ok(stats)) => stats,
            Ok(Err(error)) => {
                tracing::warn!(
                    ?error,
                    "review-verdict apply sweep: apply query failed; skipping this pass"
                );
                ReviewVerdictApplyStats::default()
            }
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "review-verdict apply sweep: apply task joined with error; skipping this pass"
                );
                ReviewVerdictApplyStats::default()
            }
        };
    if stats.created_work > 0 {
        coordinator.kick();
    }
    stats
}

#[cfg(test)]
mod notification_tests {
    use super::*;

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
