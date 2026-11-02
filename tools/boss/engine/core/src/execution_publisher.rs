//! The `executions.<id>` / product-topic publication contract and the post-commit
//! review-guide notification built on it.

use boss_execution_contracts::ExecutionPublisher;
#[cfg(test)]
pub use boss_execution_contracts::NoopExecutionPublisher;

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
