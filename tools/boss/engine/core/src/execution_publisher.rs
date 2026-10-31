//! The `executions.<id>` / product-topic publication contract and the post-commit
//! review-guide notification built on it.

use async_trait::async_trait;
use boss_protocol::FrontendEvent;

/// Sink for `executions.<id>` topic invalidations. The engine wires this
/// to the topic broker; tests use a no-op or recording double.
#[async_trait]
pub trait ExecutionPublisher: Send + Sync {
    async fn publish(&self, execution_id: &str, work_item_id: &str, status: &str, reason: &str);

    /// Publish a work-tree invalidation on the work item's product
    /// topic so subscribers (the kanban view) re-fetch and pick up
    /// status changes the coordinator drove from a non-request path
    /// — e.g., the auto-advance of `tasks.status` to `'active'` that
    /// happens inside `start_execution_run`.
    async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str);

    /// Like [`Self::publish_work_item_changed`], but for a change that touched
    /// only the runtime fields of this one card: its dispatch wait, PR poll
    /// state, merge-queue position or doc pointer. No card was added,
    /// removed, re-parented or re-statused and no dependency edge moved, so
    /// subscribers may re-read just this card instead of the whole product
    /// board. A site must call this only when its write meets that bar;
    /// anything uncertain stays on `publish_work_item_changed`, which is the
    /// full-board invalidation.
    ///
    /// The default forwards to `publish_work_item_changed`: a publisher that
    /// does not tell the classes apart treats every change as structural.
    async fn publish_work_item_runtime_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.publish_work_item_changed(product_id, work_item_id, reason).await;
    }

    /// Push a typed [`FrontendEvent`] verbatim on the work item's
    /// product topic. Used for activity-feed events such as
    /// `ConflictResolutionStarted` / `Succeeded` / `Failed` /
    /// `Abandoned` (design Q8) where subscribers need the full
    /// payload, not just a "refetch" hint.
    async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent);

    /// Nudge the execution scheduler to drain its ready queue. Called
    /// by the merge-poller's conflict-detection path after inserting a
    /// `conflict_resolution` execution so the worker is dispatched
    /// promptly rather than waiting for the next opportunistic kick.
    /// Default is a no-op — only the production `BrokerExecutionPublisher`
    /// overrides this.
    fn kick_scheduler(&self) {}
}

#[derive(Default)]
pub struct NoopExecutionPublisher;

#[async_trait]
impl ExecutionPublisher for NoopExecutionPublisher {
    async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
    async fn publish_work_item_changed(&self, _: &str, _: &str, _: &str) {}
    async fn publish_frontend_event_on_product(&self, _: &str, _: FrontendEvent) {}
}

use crate::work::WorkDb;
use std::sync::Arc;

/// Broadcast a work-item invalidation for `root_task_id` after a review-guide
/// lifecycle transition that did not itself touch the `tasks` row (guide
/// state lives in `pr_review_guide_source_series` / `_attempts` /
/// `_versions`, keyed by `root_task_id`, not in `tasks`). Mirrors the
/// `"task_doc_pointer_set"` / `"design_doc_pointer_set"` precedent in
/// `completion/pr_transition.rs`: a derived projection changed on read, so
/// subscribers (the kanban view) need a refetch hint, not a payload push.
/// Best-effort — a failure to resolve the task's product only means the
/// card catches up on its next unrelated refresh instead of immediately.
/// Takes the publisher directly (rather than `Arc<ServerState>`) so both a
/// `Dispatch` request handler (`server_state.publisher`) and
/// `WorkerCompletionHandler` (its own `publisher` field) can share it.
pub(crate) async fn notify_review_guide_changed(
    work_db: &WorkDb,
    publisher: &Arc<dyn ExecutionPublisher>,
    root_task_id: &str,
    reason: &str,
) {
    match work_db.get_work_item(root_task_id) {
        Ok(item) => {
            publisher
                .publish_work_item_changed(item.product_id(), root_task_id, reason)
                .await;
        }
        Err(err) => {
            tracing::warn!(
                root_task_id,
                reason,
                ?err,
                "notify_review_guide_changed: failed to resolve root task's product; skipping broadcast"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{RecordingPublisher, create_active_chore, create_product, open_db};

    #[tokio::test]
    async fn publishes_the_root_cards_product_invalidation_with_the_given_reason() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "guide publication");
        let recorder = Arc::new(RecordingPublisher::default());
        let publisher: Arc<dyn ExecutionPublisher> = recorder.clone();

        notify_review_guide_changed(&db, &publisher, &root, "review_guide_ready").await;

        assert_eq!(
            *recorder.events.lock().await,
            vec![(product, root, "review_guide_ready".to_owned())]
        );
    }

    #[tokio::test]
    async fn an_unresolvable_root_task_is_skipped_without_publishing() {
        let (_dir, db) = open_db();
        let recorder = Arc::new(RecordingPublisher::default());
        let publisher: Arc<dyn ExecutionPublisher> = recorder.clone();

        notify_review_guide_changed(&db, &publisher, "task_does_not_exist", "review_guide_failed").await;

        assert!(recorder.events.lock().await.is_empty());
    }
}
