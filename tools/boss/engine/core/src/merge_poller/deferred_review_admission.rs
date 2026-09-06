//! Retry pre-merge review admission for tasks held in PendingReview.

use super::*;

/// Retry pre-merge review admission for tasks held in PendingReview with no
/// live batch. Distinguishes a new head (create a batch or stay deferred)
/// from a completed batch on the current SHA (advance to human Review).
pub(crate) async fn sweep_deferred_review_admission(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    wanted_pr_urls: Option<&std::collections::HashSet<&str>>,
    review_pool_size: usize,
    outcome: &mut SweepOutcome,
) {
    let candidates = match work_db.list_tasks_awaiting_pre_merge_review_admission() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list deferred review-admission tasks");
            return;
        }
    };
    for candidate in candidates {
        if wanted_pr_urls.is_some_and(|wanted| !wanted.contains(candidate.pr_url.as_str())) {
            continue;
        }
        if candidate.repo_remote_url.is_empty() {
            tracing::warn!(
                task_id = %candidate.task_id,
                pr_url = %candidate.pr_url,
                "merge poller: deferred review admission has no repo URL; leaving the hold"
            );
            crate::completion::file_admission_deferred_attention(work_db, &candidate.task_id, &candidate.pr_url);
            outcome.review_admission_still_deferred += 1;
            continue;
        }
        match crate::completion::enqueue_review_batch(
            work_db,
            &candidate.task_id,
            &candidate.repo_remote_url,
            &candidate.pr_url,
            review_pool_size,
        )
        .await
        {
            Ok(crate::work::ReviewBatchDispatch::Created { batch, executions }) => {
                tracing::info!(
                    task_id = %candidate.task_id,
                    batch_id = %batch.id,
                    leaf_executions = executions.len(),
                    pr_url = %candidate.pr_url,
                    "merge poller: deferred pre-merge review batch admitted"
                );
                publisher.kick_scheduler();
                publisher
                    .publish_work_item_changed(&candidate.product_id, &candidate.task_id, "review_admission_recovered")
                    .await;
                let _ = work_db.resolve_external_tracker_attention(
                    &candidate.task_id,
                    crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND,
                );
                outcome.review_admission_recovered += 1;
            }
            Ok(crate::work::ReviewBatchDispatch::ExistingBatch { batch, .. }) => {
                if matches!(
                    batch.status,
                    boss_protocol::ReviewBatchStatus::Completed | boss_protocol::ReviewBatchStatus::Failed
                ) {
                    match work_db.advance_pending_review_task_to_in_review(&candidate.task_id) {
                        Ok(true) => {
                            tracing::info!(
                                task_id = %candidate.task_id,
                                batch_id = %batch.id,
                                pr_url = %candidate.pr_url,
                                "merge poller: deferred-admission hold matched an already-settled \
                                 batch for this head; advancing to in_review"
                            );
                            publisher
                                .publish_work_item_changed(
                                    &candidate.product_id,
                                    &candidate.task_id,
                                    "reviewer_fallback_advanced",
                                )
                                .await;
                            outcome.reviewer_fallback_advanced += 1;
                        }
                        Ok(false) => {}
                        Err(err) => tracing::warn!(
                            task_id = %candidate.task_id,
                            ?err,
                            "merge poller: failed to advance settled deferred-admission hold"
                        ),
                    }
                }
                let _ = work_db.resolve_external_tracker_attention(
                    &candidate.task_id,
                    crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND,
                );
            }
            Ok(crate::work::ReviewBatchDispatch::LegacyExecution(_)) => {
                let _ = work_db.resolve_external_tracker_attention(
                    &candidate.task_id,
                    crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND,
                );
            }
            Ok(crate::work::ReviewBatchDispatch::AlreadyReviewed) => {
                // The verdict `already_reviewed_at_head` matched may be keyed
                // by `candidate.cycle_root_id` rather than the task itself
                // (a revision task's batch-leaf verdicts live on the cycle
                // root) — use the source-aware advance so the EXISTS
                // subquery looks in the right place instead of silently
                // matching zero rows forever.
                match work_db.advance_pending_review_task_to_in_review_with_verdict_source(
                    &candidate.task_id,
                    &candidate.cycle_root_id,
                ) {
                    Ok(true) => {
                        tracing::info!(
                            task_id = %candidate.task_id,
                            pr_url = %candidate.pr_url,
                            "merge poller: deferred-admission hold's current head already has an \
                             informative review verdict; advancing to in_review instead of re-reviewing"
                        );
                        publisher
                            .publish_work_item_changed(
                                &candidate.product_id,
                                &candidate.task_id,
                                "reviewer_fallback_advanced",
                            )
                            .await;
                        outcome.reviewer_fallback_advanced += 1;
                    }
                    Ok(false) => {
                        // The verdict-EXISTS predicate still didn't match
                        // (or a live non-review execution blocked the
                        // advance) even after resolving the cycle root — a
                        // genuine wedge, not the previously-silent id
                        // mismatch. File the deferred-admission marker so
                        // the candidate query's marker arm (rather than the
                        // 10-minute staleness arm) keeps re-surfacing it,
                        // and log at warn so it is operator-visible instead
                        // of an unbounded silent `gh pr view` loop.
                        tracing::warn!(
                            task_id = %candidate.task_id,
                            cycle_root_id = %candidate.cycle_root_id,
                            pr_url = %candidate.pr_url,
                            "merge poller: already-reviewed deferred-admission hold did not advance \
                             (no matching verdict, or a live non-review execution is blocking); \
                             filing an attention marker so the hold stays visible"
                        );
                        crate::completion::file_admission_deferred_attention(
                            work_db,
                            &candidate.task_id,
                            &candidate.pr_url,
                        );
                        outcome.review_admission_still_deferred += 1;
                        continue;
                    }
                    Err(err) => tracing::warn!(
                        task_id = %candidate.task_id,
                        ?err,
                        "merge poller: failed to advance already-reviewed deferred-admission hold"
                    ),
                }
                let _ = work_db.resolve_external_tracker_attention(
                    &candidate.task_id,
                    crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND,
                );
            }
            Ok(crate::work::ReviewBatchDispatch::AdmissionDeferred) => {
                crate::completion::file_admission_deferred_attention(work_db, &candidate.task_id, &candidate.pr_url);
                outcome.review_admission_still_deferred += 1;
            }
            Err(err) => {
                tracing::warn!(
                    task_id = %candidate.task_id,
                    pr_url = %candidate.pr_url,
                    ?err,
                    "merge poller: deferred review admission retry failed"
                );
            }
        }
    }
}
