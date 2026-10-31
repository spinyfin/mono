//! Post-commit publication: how engine services announce committed changes.
//!
//! Publication always happens after the transaction that made the change has
//! committed, from the service that owns the publisher — never from inside
//! storage. [`ExecutionPublisher`] is the contract and depends on nothing but
//! the wire event type; it lives outside [`crate::coordinator`] so that code
//! which only needs to publish does not import the execution scheduler
//! (`crate::coordinator::ExecutionPublisher` remains a re-export for the
//! services that already reach it there). [`notify_review_guide_changed`] is
//! the announcement review-guide transitions make once their storage write
//! has committed.

use std::sync::Arc;

use async_trait::async_trait;
use boss_protocol::FrontendEvent;

use crate::work::WorkDb;

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

    use crate::test_support::{
        RecordingPublisher, create_active_chore, create_product, open_db, seed_review_guide_series,
    };
    use crate::work::PublishReviewGuideOutcome;

    /// Records the guide's readable pointer at the instant the publisher is
    /// called, so a test can prove the announcement follows the commit.
    struct ReadablePointerProbe {
        db: WorkDb,
        root_task_id: String,
        readable_at_publish: std::sync::Mutex<Vec<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl ExecutionPublisher for ReadablePointerProbe {
        async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}

        async fn publish_work_item_changed(&self, _: &str, _: &str, _: &str) {
            let summary = self
                .db
                .get_pr_review_guide_summary_for_root(&self.root_task_id)
                .unwrap();
            self.readable_at_publish
                .lock()
                .unwrap()
                .push(summary.and_then(|summary| summary.readable_version_id));
        }

        async fn publish_frontend_event_on_product(&self, _: &str, _: boss_protocol::FrontendEvent) {}
    }

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

    /// The announcement is made by the caller after storage commits, never
    /// from inside the transaction: when the publisher runs, a subscriber that
    /// re-reads the card already sees the published guide.
    #[tokio::test]
    async fn the_announcement_follows_the_commit_that_published_the_guide() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "guide publication");
        let (series_id, comparison_id) = seed_review_guide_series(&db, &root);
        let attempt = db
            .create_pr_review_guide_attempt(&series_id, &comparison_id, boss_review_guide::PROMPT_VERSION)
            .unwrap();
        let execution = db
            .create_pr_review_guide_execution(&comparison_id, "acme/widget")
            .unwrap();
        db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
            .unwrap();
        let probe = Arc::new(ReadablePointerProbe {
            db: db.clone(),
            root_task_id: root.clone(),
            readable_at_publish: Default::default(),
        });
        let publisher: Arc<dyn ExecutionPublisher> = probe.clone();

        let PublishReviewGuideOutcome::Published(version) = db
            .publish_pr_review_guide_version(&attempt.id, "# Guide\n", "raw")
            .unwrap()
        else {
            panic!("the guide must publish");
        };
        notify_review_guide_changed(&db, &publisher, &root, "review_guide_ready").await;

        assert_eq!(
            *probe.readable_at_publish.lock().unwrap(),
            vec![Some(version.id)],
            "a subscriber re-reading on the announcement must see the committed guide"
        );
    }
}
