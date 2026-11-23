//! Merge-click rebase path for `blocked: ci_failure` / `blocked: ci_failure_exhausted`
//! cards whose PR branch is behind its base.
//!
//! When a PR's CI failed for reasons already fixed on its base branch, clicking
//! Merge reuses [`boss_engine_runtime::conflict_watch::rebase_for_ci_failure_merge`]
//! (the same mechanical rebase rung used for conflicts) rather than building a
//! second rebase path, then arms the merge (`gh pr merge --auto --squash` or
//! Trunk queue) so GitHub / Trunk merges only if CI on the rebased head goes
//! green.

use std::time::Duration;

use super::review::{
    AfterRebaseRecord, DirectMergeArmRequest, TrunkQueueMergeRequest, arm_direct_github_merge,
    handle_trunk_queue_merge, refuse_merge_when_ready, remember_merge_request,
};
use super::*;

const REBASED_HEAD_ATTEMPTS: usize = 10;
const REBASED_HEAD_DELAY: Duration = Duration::from_millis(150);

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(super) struct CiFailureMergeRebaseContext {
    pub server_state: Arc<ServerState>,
    pub work_db: Arc<WorkDb>,
    pub sink: Arc<SessionSink>,
    pub request_id: String,
    pub product_id: String,
    pub product_name: String,
    pub product_slug: String,
    pub work_item_id: String,
    pub pr_url: String,
    pub blocked_reason: String,
    pub mechanism: boss_engine_runtime::merge_mechanism::MergeMechanism,
    pub confirmed_revisions: Vec<boss_protocol::OpenMergeRevision>,
}

pub(super) async fn handle_ci_failure_merge_rebase(ctx: CiFailureMergeRebaseContext) {
    let CiFailureMergeRebaseContext {
        server_state,
        work_db,
        sink,
        request_id,
        product_id,
        product_name,
        product_slug,
        work_item_id,
        pr_url,
        blocked_reason,
        mechanism,
        confirmed_revisions,
    } = ctx;
    let merge_probe = server_state.merge_probe.clone();
    let cube_client = server_state.cube_client.clone();
    let publisher = server_state.publisher.clone();
    let mechanical_rebase_enabled = server_state
        .feature_flags
        .is_enabled("conflict_ladder_mechanical_rebase");

    boss_task_scope::spawn(async move {
        let probe = match merge_probe.probe(&pr_url).await {
            Ok(probe) => probe,
            Err(err) => {
                refuse_merge_when_ready(
                    &sink,
                    &request_id,
                    &work_item_id,
                    format!("merge_when_ready: failed to inspect PR before rebase: {err:#}"),
                );
                return;
            }
        };
        if !matches!(probe.state, boss_engine_merge_poller::PrLifecycleState::Open(_)) {
            refuse_merge_when_ready(
                &sink,
                &request_id,
                &work_item_id,
                format!("merge_when_ready: PR is no longer open ({:?})", probe.state),
            );
            return;
        }
        let behind = match merge_probe.is_pr_behind_base(&pr_url, &probe).await {
            Ok(behind) => behind,
            Err(err) => {
                refuse_merge_when_ready(
                    &sink,
                    &request_id,
                    &work_item_id,
                    format!("merge_when_ready: failed to check whether PR is behind its base branch: {err:#}"),
                );
                return;
            }
        };
        if !behind {
            refuse_merge_when_ready(
                &sink,
                &request_id,
                &work_item_id,
                format!(
                    "merge_when_ready: CI is failing on an up-to-date branch (status: blocked, reason: {blocked_reason}); nothing to rebase"
                ),
            );
            return;
        }
        if !mechanical_rebase_enabled {
            refuse_merge_when_ready(
                &sink,
                &request_id,
                &work_item_id,
                "merge_when_ready: mechanical rebase is disabled; rebase manually".to_owned(),
            );
            return;
        }
        // Guard against double-clicking Merge before GitHub surfaces the head
        // pushed by an already-completed rebase click.
        if let Ok(Some(existing)) = work_db.get_ci_merge_rebase(&work_item_id, &pr_url)
            && probe.head_ref_oid.is_some()
            && existing.head_sha_before == probe.head_ref_oid
        {
            refuse_merge_when_ready(
                &sink,
                &request_id,
                &work_item_id,
                "merge_when_ready: a rebase onto base is already awaiting CI for this PR".to_owned(),
            );
            return;
        }

        let base_branch = probe
            .base_ref_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("main")
            .to_owned();
        let candidate = boss_work_db::PendingMergeCheck {
            work_item_id: work_item_id.clone(),
            product_id: product_id.clone(),
            pr_url: pr_url.clone(),
        };
        let pr_checker = boss_work_db::StaticPrStateChecker(boss_work_db::PrOpenState::Open);
        tracing::info!(
            %work_item_id,
            %pr_url,
            %blocked_reason,
            %base_branch,
            "merge_when_ready: CI-blocked PR is behind base; running mechanical rebase rung",
        );

        let outcome = boss_engine_runtime::conflict_watch::rebase_for_ci_failure_merge(
            &work_db,
            publisher.as_ref(),
            cube_client.as_ref(),
            &pr_checker,
            &candidate,
            &probe,
            &blocked_reason,
        )
        .await;

        match outcome {
            boss_engine_runtime::conflict_watch::CiMergeRebaseOutcome::RebasedAndPushed => {
                if let Err(err) = work_db.abandon_active_ci_remediations_for_work_item(
                    &work_item_id,
                    "superseded_by_merge_rebase",
                    false,
                ) {
                    tracing::warn!(
                        %work_item_id,
                        ?err,
                        "merge_when_ready: failed to abandon pre-rebase CI remediation attempts; continuing",
                    );
                }
                let rebased_head =
                    match wait_for_rebased_head(merge_probe.as_ref(), &pr_url, probe.head_ref_oid.as_deref()).await {
                        Ok(sha) => sha,
                        Err(message) => {
                            refuse_merge_when_ready(&sink, &request_id, &work_item_id, message);
                            return;
                        }
                    };
                let after_rebase = AfterRebaseRecord {
                    base_branch: base_branch.clone(),
                    head_sha_before: probe.head_ref_oid.clone(),
                    rebased_head_sha: Some(rebased_head),
                };
                match mechanism {
                    boss_engine_runtime::merge_mechanism::MergeMechanism::Direct => {
                        arm_direct_github_merge(
                            DirectMergeArmRequest::builder()
                                .work_db(work_db)
                                .sink(sink)
                                .request_id(request_id)
                                .product_id(product_id)
                                .product_name(product_name)
                                .product_slug(product_slug)
                                .work_item_id(work_item_id)
                                .pr_url(pr_url)
                                .confirmed_revisions(confirmed_revisions)
                                .publisher(publisher)
                                .event_bus(server_state.event_bus.clone())
                                .direct_merge_executor(server_state.direct_merge_executor.clone())
                                .publish_reason("ci_merge_rebase_armed")
                                .always_publish(true)
                                .release_deferred_conflict_on_refusal(false)
                                .kick_reconciler_on_error(true)
                                .after_rebase(after_rebase)
                                .build(),
                        )
                        .await;
                    }
                    boss_engine_runtime::merge_mechanism::MergeMechanism::TrunkQueue { target_branch } => {
                        handle_trunk_queue_merge(
                            server_state.clone(),
                            work_db,
                            sink,
                            request_id,
                            TrunkQueueMergeRequest::builder()
                                .product_id(product_id)
                                .work_item_id(work_item_id)
                                .pr_url(pr_url)
                                .target_branch(target_branch)
                                .retire_stale_and_retry(true)
                                .kick_pr_reconciler(true)
                                .publish_reason("ci_merge_rebase_trunk_submitted")
                                .after_rebase(after_rebase)
                                .build(),
                        )
                        .await;
                    }
                }
            }
            boss_engine_runtime::conflict_watch::CiMergeRebaseOutcome::HandedOffToConflictLadder { spawned } => {
                tracing::info!(
                    %work_item_id,
                    %pr_url,
                    spawned,
                    "merge_when_ready: CI-blocked rebase hit conflicts; handed off to conflict ladder",
                );
                match &mechanism {
                    boss_engine_runtime::merge_mechanism::MergeMechanism::Direct => {
                        let started = work_db
                            .start_merge_conflict_resolution(&work_item_id, &pr_url, &base_branch)
                            .unwrap_or(false);
                        if started
                            && let Ok(confirmation) =
                                work_db.remember_confirmed_merge_revisions(&work_item_id, &pr_url, &confirmed_revisions)
                            && !work_db
                                .attach_confirmed_merge_revisions(&work_item_id, &pr_url, &confirmation)
                                .unwrap_or(false)
                        {
                            let _ = work_db.forget_confirmed_merge_revisions(&confirmation);
                        }
                    }
                    boss_engine_runtime::merge_mechanism::MergeMechanism::TrunkQueue { target_branch } => {
                        if let Err(message) =
                            ensure_trunk_intent_superseded_by_conflict(&work_db, &work_item_id, &pr_url, target_branch)
                        {
                            refuse_merge_when_ready(
                                &sink,
                                &request_id,
                                &work_item_id,
                                format!(
                                    "{message}; click Merge again after the conflict is resolved to submit to the Trunk queue"
                                ),
                            );
                            return;
                        }
                    }
                }
                remember_merge_request(&work_db, &work_item_id, &pr_url, None);
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MergeWhenReadyAccepted {
                        work_item_id: work_item_id.clone(),
                        pr_url: pr_url.clone(),
                        action: merge_when_ready::MergeAction::Requested.as_str().to_owned(),
                    },
                );
                publisher
                    .publish_work_item_changed(&product_id, &work_item_id, "merge_conflict_resolution_requested")
                    .await;
                server_state.event_bus.publish(Event::PrReconcileRequested { pr_url });
            }
            boss_engine_runtime::conflict_watch::CiMergeRebaseOutcome::HaltedForSignoff => {
                refuse_merge_when_ready(
                    &sink,
                    &request_id,
                    &work_item_id,
                    "merge_when_ready: rebase halted for deletion sign-off; review the flagged deletions before merging"
                        .to_owned(),
                );
            }
            boss_engine_runtime::conflict_watch::CiMergeRebaseOutcome::LockfileRegenerationFailed => {
                refuse_merge_when_ready(
                    &sink,
                    &request_id,
                    &work_item_id,
                    "merge_when_ready: rebase hit a lockfile conflict whose regeneration failed; resolve it before merging"
                        .to_owned(),
                );
            }
            boss_engine_runtime::conflict_watch::CiMergeRebaseOutcome::DeferredForLiveWriter => {
                refuse_merge_when_ready(
                    &sink,
                    &request_id,
                    &work_item_id,
                    "merge_when_ready: a worker is currently active on this PR's branch; wait for it to finish before rebasing to merge"
                        .to_owned(),
                );
            }
            boss_engine_runtime::conflict_watch::CiMergeRebaseOutcome::MechanicalRungsUnavailable => {
                refuse_merge_when_ready(
                    &sink,
                    &request_id,
                    &work_item_id,
                    "merge_when_ready: could not lease a workspace to rebase the PR; try again".to_owned(),
                );
            }
            boss_engine_runtime::conflict_watch::CiMergeRebaseOutcome::WorkspacePreparationFailed => {
                refuse_merge_when_ready(
                    &sink,
                    &request_id,
                    &work_item_id,
                    "merge_when_ready: workspace preparation failed while rebasing the PR".to_owned(),
                );
            }
            boss_engine_runtime::conflict_watch::CiMergeRebaseOutcome::Failed(reason) => {
                refuse_merge_when_ready(
                    &sink,
                    &request_id,
                    &work_item_id,
                    format!("merge_when_ready: rebase onto {base_branch} failed: {reason}"),
                );
            }
        }
    });
}

/// Wait until GitHub's PR head differs from the pre-rebase SHA. `gh pr view`
/// can lag a cube push; recording the stale SHA would consume the one-shot
/// rebase marker against the old head's red CI.
async fn wait_for_rebased_head(
    merge_probe: &dyn MergeProbe,
    pr_url: &str,
    head_sha_before: Option<&str>,
) -> Result<String, String> {
    let Some(before) = head_sha_before.filter(|s| !s.is_empty()) else {
        let probe = merge_probe
            .probe(pr_url)
            .await
            .map_err(|err| format!("merge_when_ready: rebased, but failed to read the new PR head: {err:#}"))?;
        return probe
            .head_ref_oid
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "merge_when_ready: rebased, but GitHub returned an empty head SHA".to_owned());
    };
    for attempt in 0..REBASED_HEAD_ATTEMPTS {
        match merge_probe.probe(pr_url).await {
            Ok(probe) => {
                if let Some(head) = probe.head_ref_oid.filter(|s| !s.is_empty() && s != before) {
                    return Ok(head);
                }
            }
            Err(err) => {
                tracing::warn!(
                    %pr_url,
                    attempt,
                    ?err,
                    "merge_when_ready: failed to read PR head while waiting for the rebased SHA"
                );
            }
        }
        if attempt + 1 < REBASED_HEAD_ATTEMPTS {
            tokio::time::sleep(REBASED_HEAD_DELAY).await;
        }
    }
    Err("merge_when_ready: rebased onto base, but GitHub still reports the pre-rebase head; try again".to_owned())
}

/// Create (or stamp) an active Trunk intent in `boss:superseded_by_conflict`
/// so the existing cancel/resubmit path submits after the conflict fix lands.
fn ensure_trunk_intent_superseded_by_conflict(
    work_db: &WorkDb,
    work_item_id: &str,
    pr_url: &str,
    target_branch: &str,
) -> Result<(), String> {
    let coords = boss_engine_merge_poller::trunk_merge::parse_trunk_pr_coordinates(pr_url)
        .map_err(|err| format!("merge_when_ready: {err}"))?;
    let insert_input = boss_work_db::TrunkMergeIntentInsertInput::builder()
        .work_item_id(work_item_id)
        .pr_url(pr_url)
        .pr_number(coords.number as i64)
        .repo(format!("{}/{}", coords.owner, coords.repo))
        .target_branch(target_branch)
        .build();
    match work_db.insert_trunk_merge_intent(insert_input) {
        Ok(Some(intent)) => {
            if let Err(err) = work_db.record_trunk_merge_intent_state(
                &intent.id,
                boss_engine_merge_poller::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT,
            ) {
                return Err(format!(
                    "merge_when_ready: recorded a trunk merge intent but could not mark it superseded-by-conflict: {err}"
                ));
            }
            Ok(())
        }
        Ok(None) => {
            boss_engine_merge_poller::trunk_merge::mark_trunk_intent_superseded_by_conflict(work_db, work_item_id);
            if work_db
                .get_active_trunk_merge_intent(work_item_id)
                .ok()
                .flatten()
                .is_none()
            {
                return Err(
                    "merge_when_ready: could not record a trunk merge intent for conflict continuation".to_owned(),
                );
            }
            Ok(())
        }
        Err(err) => Err(format!("merge_when_ready: failed to record trunk merge intent: {err}")),
    }
}

#[cfg(test)]
#[path = "review_ci_rebase_tests.rs"]
mod tests;
