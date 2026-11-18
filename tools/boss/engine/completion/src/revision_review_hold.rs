//! The head a finished revision delivered, and the review hold taken on it.
//!
//! A revision whose run delivered is held `active` until the automated review
//! of what it pushed reaches a decision. The hold is released by the review
//! of one specific pull-request head, the one recorded on the revision's
//! finished run. So a completion that takes the hold has to know that head,
//! and has to leave the hold owned by something that will decide it.
//!
//! Every completion that takes a revision's review hold records the head the
//! run finished at before taking it, and settles the hold against the
//! pre-merge batch for that head, so the hold never depends on a later
//! admission call to name its head.
//!
//! - [`WorkerCompletionHandler::delivered_revision_head`] reads the head a
//!   declared delivery finished at. The Stop boundary has its own reads.
//!   On either path a delivery whose head cannot be read from any source
//!   does not complete: it fails with a named diagnostic
//!   ([`WorkerCompletionHandler::fail_delivery_with_unreadable_head`]),
//!   because a hold with no head is one nothing can release.
//! - [`settle_revision_hold`] records the head a batch was admitted at as the
//!   one the revision is held for, and releases the revision when that batch
//!   has already reached its decision.

use super::*;
use crate::work::ReviewHoldSettlement;

impl WorkerCompletionHandler {
    /// The pull-request head a revision's delivery finished at.
    ///
    /// The direct read is preferred, as at the Stop boundary. `verified_head`
    /// is the head GitHub reported in the answer that verified the delivery
    /// mergeable (see `ConflictDeliveryDecision::Proceed`); it stands in when
    /// the direct read fails or does not answer within
    /// [`super::run_done_declaration::DECLARED_DELIVERY_NETWORK_BUDGET`], so
    /// one failed read does not fail a delivery GitHub has just described.
    /// `None` only when neither source has a head.
    pub(super) async fn delivered_revision_head(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
        verified_head: Option<String>,
    ) -> Option<String> {
        let budget = super::run_done_declaration::DECLARED_DELIVERY_NETWORK_BUDGET;
        match tokio::time::timeout(budget, self.fetch_pr_head_after(execution, pr_url)).await {
            Ok(Some(head)) => return Some(head),
            Ok(None) => {}
            Err(_) => tracing::warn!(
                execution_id = %execution.id,
                pr_url,
                budget_secs = budget.as_secs(),
                "revision delivery: the direct read of the pull request's head did not answer in time",
            ),
        }
        match verified_head {
            Some(head) => {
                tracing::info!(
                    execution_id = %execution.id,
                    pr_url,
                    head = %head,
                    "revision delivery: using the head GitHub reported when it verified the delivery mergeable",
                );
                Some(head)
            }
            None => None,
        }
    }

    /// Fail a revision's delivery whose pull-request head cannot be read,
    /// at either completion boundary.
    ///
    /// The run is over either way. What cannot be decided is which review
    /// its push is held for, so completing it would leave the revision
    /// `active` with nothing live and nothing able to release it. It fails
    /// the same way a delivery whose mergeability cannot be verified does
    /// (`conflict_delivery_decision`): the task is blocked as a worker
    /// failure carrying this diagnostic, and an attention item names it.
    pub(super) async fn fail_delivery_with_unreadable_head(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
    ) -> StopOutcome {
        let detail = format!(
            "Execution `{}` finished its run delivering to {pr_url}, but the pull request's head commit \
             could not be read: GitHub's mergeability answer carried no head and the direct read of the head \
             failed or timed out. A finished revision is held for the review of one specific head, so without \
             it nothing could release the hold. The run has ended and its slot and lease are released. Check \
             that the pull request can be read (`gh pr view {pr_url} --json headRefOid`), then start this \
             revision again.",
            execution.id
        );
        if !self
            .finalize_worker_failure(execution, &detail, WorkerFailureBasis::DeliveredWithoutPr)
            .await
        {
            return StopOutcome::DbError;
        }
        if let Err(err) = self
            .file_execution_attention(
                execution,
                RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND,
                "Revision delivered but the pull request's head could not be read",
                detail.clone(),
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "revision delivery (head unreadable): failed to file attention item",
            );
        }
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            pr_url,
            pr_head_before = execution.pr_head_before.as_deref().unwrap_or("(none recorded)"),
            "revision delivery: the pull request's head could not be read; failed the run with a durable \
             task failure instead of holding the revision for an unknown head",
        );
        StopOutcome::RunDoneDeclaredWithoutDelivery { detail }
    }
}

/// Settle the review hold of the finished revision `revision_id` against
/// `target_sha`, the head a pre-merge batch was admitted at.
///
/// Records that head as the one the revision is held for and, when the batch
/// has already reached its decision, releases the revision (see
/// [`crate::work::WorkDb::settle_revision_review_hold`]). Logs what the hold
/// now rests on, and publishes the board change when the revision moved.
/// Returns `true` when it was released.
///
/// A free function so the declared-delivery post-effects, which hold only the
/// collaborators they cloned, can call it too.
pub(super) async fn settle_revision_hold(
    work_db: &crate::work::WorkDb,
    publisher: &dyn ExecutionPublisher,
    revision_id: &str,
    target_sha: &str,
    source: &'static str,
) -> bool {
    match work_db.settle_revision_review_hold(revision_id, target_sha) {
        Ok(ReviewHoldSettlement::Released { batch_id, batch_status }) => {
            tracing::info!(
                revision_id,
                target_sha,
                batch_id = %batch_id,
                batch_status = %batch_status,
                source,
                "revision review hold: the review of this head had already reached its decision; \
                 released the revision to in_review",
            );
            if let Ok(work_item) = work_db.get_work_item(revision_id) {
                publisher
                    .publish_work_item_changed(work_item.product_id(), revision_id, "reviewer_fallback_advanced")
                    .await;
            }
            publisher.kick_scheduler();
            true
        }
        Ok(ReviewHoldSettlement::AwaitingBatch { batch_id }) => {
            tracing::info!(
                revision_id,
                target_sha,
                batch_id = %batch_id,
                source,
                "revision review hold: held for this head until its review batch reaches a decision",
            );
            false
        }
        Ok(settlement @ (ReviewHoldSettlement::NoBatch | ReviewHoldSettlement::NotHeld)) => {
            tracing::info!(
                revision_id,
                target_sha,
                ?settlement,
                source,
                "revision review hold: nothing to settle against this head",
            );
            false
        }
        Err(err) => {
            tracing::warn!(
                revision_id,
                target_sha,
                source,
                ?err,
                "revision review hold: could not settle the hold; the deferred-admission sweep retries it",
            );
            false
        }
    }
}
