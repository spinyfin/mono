//! Automatic landed-code post-merge review trigger.

use super::*;

/// Trigger the automatic landed-code post-merge review (parent project:
/// "Multi-agent code review" — an automatic post-merge review for large or
/// complex PRs). Eligibility and target-batch creation both run off durably
/// persisted state, so redundant calls (a late-PR recheck racing the main
/// sweep, a retried pass) are no-ops rather than duplicate reviews:
///
/// - **Deep-production eligibility** is read from the cycle root's most
///   recent pre-merge review batch's frozen [`boss_protocol::ReviewClassification`]
///   rather than recomputed — only a `Deep` profile (large/complex PR)
///   qualifies. A cycle root with no pre-merge batch on record (batch-mode
///   review disabled, or a manual/human-driven PR) has nothing to gate
///   eligibility on and is skipped.
/// - **Merge-SHA batch creation** is keyed on `(cycle_root_id, PostMerge,
///   merge_sha)` — [`WorkDb::create_post_merge_review_batch`] enforces the
///   same one-batch-per-immutable-target uniqueness
///   [`WorkDb::create_pre_merge_review_batch`] does, and this function
///   checks [`WorkDb::review_batch_for_target`] first so a batch that
///   already exists (this call already ran, or is running concurrently) is
///   read back rather than re-created.
///
/// Uses the actual merge-commit SHA (`probe.merge_commit_oid`) as the
/// batch's target — the only SHA guaranteed reachable via `jj git fetch`
/// after a squash merge and head-branch deletion. When GitHub omits it
/// (rare), the pass is skipped rather than falling back to the PR's own
/// head SHA, which `cube workspace goto --revision` would then be unable
/// to fetch or position on.
pub(crate) async fn maybe_trigger_post_merge_review(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) {
    let cycle_root_id = work_db.review_cycle_root_id(&candidate.work_item_id);

    let pre_merge_batch = match work_db.review_batches_for_cycle_root(&cycle_root_id) {
        Ok(batches) => batches
            .into_iter()
            .find(|batch| batch.phase == boss_protocol::ReviewBatchPhase::PreMerge),
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                cycle_root_id = %cycle_root_id,
                ?err,
                "merge poller: failed to list review batches for post-merge eligibility check",
            );
            return;
        }
    };
    let Some(pre_merge_batch) = pre_merge_batch else {
        return; // no pre-merge classification on record; nothing to gate eligibility on
    };
    if pre_merge_batch.classification.profile != boss_protocol::ReviewProfile::Deep {
        return; // only Deep (large/complex) PRs get an automatic post-merge review
    }

    // Only `merge_commit_oid` is usable as a positioning target: it is the
    // frozen merge commit reachable from the default branch, so `cube
    // workspace goto --revision` can always fetch and land on it. A bare
    // `head_ref_oid` fallback would hand that SHA to `goto` instead, and
    // after a squash merge (plus GitHub's automatic head-branch deletion)
    // that commit sits on no remote ref at all — `jj git fetch` cannot
    // bring it down, so positioning would fail deterministically, burn the
    // batch's one `pr_review_recovery` retry, and terminal-fail with a
    // `pr_review_quorum_failed` attention. A PR reported merged with no
    // `mergeCommit` from GitHub is rare enough that skipping this pass
    // beats scheduling one that cannot position.
    let Some(merge_sha) = probe.merge_commit_oid.clone() else {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "merge poller: PR reported merged with no merge commit SHA (head SHA is not a safe \
             positioning fallback); skipping post-merge review",
        );
        return;
    };

    match work_db.review_batch_for_target(&cycle_root_id, boss_protocol::ReviewBatchPhase::PostMerge, &merge_sha) {
        Ok(Some(_)) => return, // already triggered for this merge — idempotent no-op
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                cycle_root_id = %cycle_root_id,
                ?err,
                "merge poller: failed to check for an existing post-merge review batch",
            );
            return;
        }
    }

    let Some(pr_number) = stored_pr_number(&candidate.pr_url) else {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "merge poller: could not parse PR number for post-merge review batch",
        );
        return;
    };

    let repo_remote_url = match work_db.get_work_item(&candidate.work_item_id) {
        Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => work_db.repo_remote_url_for_tripwire(&task),
        _ => None,
    };
    let Some(repo_remote_url) = repo_remote_url else {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            "merge poller: could not resolve repo_remote_url for post-merge review batch",
        );
        return;
    };

    if probe.base_ref_oid.is_none() {
        // Unlike merge/head SHA, PR number, and repo_remote_url above, a
        // missing `base_ref_oid` does not abort the trigger — nothing in the
        // post-merge path reads `base_sha` today. But silently defaulting to
        // `""` plants an unlabelled sentinel a future reader of the batch row
        // cannot distinguish from a real (if empty) value, so log the same
        // warn shape as the sibling guards to keep the gap visible.
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "merge poller: PR reported merged with no base ref SHA; recording an empty base_sha on the post-merge review batch",
        );
    }

    let input = crate::work::ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root_id)
        .base_sha(probe.base_ref_oid.clone().unwrap_or_default())
        .classification(pre_merge_batch.classification.clone())
        .phase(boss_protocol::ReviewBatchPhase::PostMerge)
        .pr_number(pr_number)
        .pr_url(candidate.pr_url.clone())
        .target_sha(merge_sha.clone())
        .merge_sha(merge_sha)
        .build();

    match work_db.create_post_merge_review_batch(input, &repo_remote_url) {
        Ok(crate::work::ReviewBatchDispatch::Created { batch, .. }) => {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                batch_id = %batch.id,
                pr_url = %candidate.pr_url,
                merge_sha = %batch.target_sha,
                "merge poller: post-merge review batch created for a Deep-classified PR",
            );
            publisher.kick_scheduler();
        }
        Ok(crate::work::ReviewBatchDispatch::ExistingBatch { .. }) => {}
        Ok(crate::work::ReviewBatchDispatch::LegacyExecution(_)) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                "merge poller: post-merge review dispatch unexpectedly returned a legacy execution",
            );
        }
        Ok(crate::work::ReviewBatchDispatch::AdmissionDeferred) => {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "merge poller: post-merge review batch deferred because the review pool is at capacity",
            );
        }
        Ok(crate::work::ReviewBatchDispatch::AlreadyReviewed) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                "merge poller: post-merge review dispatch unexpectedly returned AlreadyReviewed",
            );
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "merge poller: failed to create post-merge review batch",
            );
        }
    }
}
