//! Merge-click rebase path for `blocked: ci_failure` / `blocked: ci_failure_exhausted`
//! cards whose PR branch is behind its base.
//!
//! When a PR's CI failed for reasons already fixed on its base branch, clicking
//! Merge reuses [`boss_engine_runtime::conflict_watch::rebase_for_ci_failure_merge`]
//! (the same mechanical rebase rung used for conflicts) rather than building a
//! second rebase path, then arms the merge (`gh pr merge --auto --squash` or
//! Trunk queue) so GitHub / Trunk merges only if CI on the rebased head goes
//! green.

use super::review::{refuse_merge_when_ready, remember_merge_request};
use super::*;

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
    let direct_merge_executor = server_state.direct_merge_executor.clone();
    let trunk_client = server_state.trunk_client.clone();
    let publisher = server_state.publisher.clone();
    let event_bus = server_state.event_bus.clone();

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
                match mechanism {
                    boss_engine_runtime::merge_mechanism::MergeMechanism::Direct => {
                        arm_direct_merge_after_ci_rebase(
                            DirectArmContext {
                                work_db,
                                sink,
                                request_id,
                                product_id,
                                product_name,
                                product_slug,
                                work_item_id,
                                pr_url,
                                base_branch,
                                head_sha_before: probe.head_ref_oid,
                                confirmed_revisions,
                            },
                            direct_merge_executor,
                            publisher,
                            event_bus,
                        )
                        .await;
                    }
                    boss_engine_runtime::merge_mechanism::MergeMechanism::TrunkQueue { target_branch } => {
                        arm_trunk_merge_after_ci_rebase(
                            TrunkArmContext {
                                work_db,
                                sink,
                                request_id,
                                product_id,
                                work_item_id,
                                pr_url,
                                base_branch,
                                target_branch,
                                head_sha_before: probe.head_ref_oid,
                            },
                            trunk_client,
                            publisher,
                            event_bus,
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
                if matches!(mechanism, boss_engine_runtime::merge_mechanism::MergeMechanism::Direct) {
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
                event_bus.publish(Event::PrReconcileRequested { pr_url });
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

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct DirectArmContext {
    work_db: Arc<WorkDb>,
    sink: Arc<SessionSink>,
    request_id: String,
    product_id: String,
    product_name: String,
    product_slug: String,
    work_item_id: String,
    pr_url: String,
    base_branch: String,
    head_sha_before: Option<String>,
    confirmed_revisions: Vec<boss_protocol::OpenMergeRevision>,
}

async fn arm_direct_merge_after_ci_rebase(
    ctx: DirectArmContext,
    direct_merge_executor: Arc<dyn merge_when_ready::DirectMergeExecutor>,
    publisher: Arc<dyn boss_execution_contracts::ExecutionPublisher>,
    event_bus: Arc<EventBus>,
) {
    let DirectArmContext {
        work_db,
        sink,
        request_id,
        product_id,
        product_name,
        product_slug,
        work_item_id,
        pr_url,
        base_branch,
        head_sha_before,
        confirmed_revisions,
    } = ctx;
    let confirmation = match work_db.remember_confirmed_merge_revisions(&work_item_id, &pr_url, &confirmed_revisions) {
        Ok(confirmation) => confirmation,
        Err(err) => {
            refuse_merge_when_ready(&sink, &request_id, &work_item_id, err.to_string());
            return;
        }
    };
    match direct_merge_executor.execute(&pr_url).await {
        Ok(success) => {
            let detail = boss_engine_storage_values::merge_queue_section::queued_no_position_detail();
            let record = work_db.record_github_merge_intent(
                boss_work_db::GithubMergeIntentInsertInput::builder()
                    .work_item_id(work_item_id.clone())
                    .pr_url(pr_url.clone())
                    .head_sha(success.head_sha.clone())
                    .confirmation_request_id(confirmation.clone())
                    .build(),
                &detail,
            );
            if let Err(err) = record {
                let _ = work_db.forget_confirmed_merge_revisions(&confirmation);
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "merge_when_ready accepted the GitHub request after rebase but could not record its merge intent: {err}"
                        ),
                    },
                );
                return;
            }
            if let Err(err) = work_db.record_ci_merge_rebase(
                &work_item_id,
                &pr_url,
                &base_branch,
                head_sha_before.as_deref(),
                Some(&success.head_sha),
            ) {
                tracing::warn!(%work_item_id, ?err, "merge_when_ready: failed to record post-rebase tracking");
            }
            remember_merge_request(&work_db, &work_item_id, &pr_url, Some(&success.head_sha));
            send_response(
                &sink,
                &request_id,
                FrontendEvent::MergeWhenReadyAccepted {
                    work_item_id: work_item_id.clone(),
                    pr_url: pr_url.clone(),
                    action: merge_when_ready::MergeAction::Requested.as_str().to_owned(),
                },
            );
            event_bus.publish(Event::PrReconcileRequested { pr_url: pr_url.clone() });
            publisher
                .publish_work_item_changed(&product_id, &work_item_id, "ci_merge_rebase_armed")
                .await;
        }
        Err(err) => {
            let message = format!("{err:#}");
            let _ = work_db.forget_confirmed_merge_revisions(&confirmation);
            if boss_engine_runtime::merge_mechanism::is_push_restriction_error(&message) {
                let _ = work_db.create_attention_item(boss_work_db::CreateAttentionItemInput {
                    work_item_id: Some(work_item_id.clone()),
                    kind: boss_engine_runtime::merge_mechanism::PUSH_RESTRICTION_ATTENTION_KIND.to_owned(),
                    title: boss_engine_runtime::merge_mechanism::PUSH_RESTRICTION_ATTENTION_TITLE.to_owned(),
                    body_markdown: boss_engine_runtime::merge_mechanism::render_push_restriction_attention_body(
                        &product_name,
                        &product_slug,
                        &message,
                    ),
                    execution_id: None,
                    status: None,
                    resolved_at: None,
                });
            }
            event_bus.publish(Event::PrReconcileRequested { pr_url });
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!(
                        "merge_when_ready: rebased onto {base_branch}, but arming merge failed: {message}"
                    ),
                },
            );
        }
    }
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct TrunkArmContext {
    work_db: Arc<WorkDb>,
    sink: Arc<SessionSink>,
    request_id: String,
    product_id: String,
    work_item_id: String,
    pr_url: String,
    base_branch: String,
    target_branch: String,
    head_sha_before: Option<String>,
}

async fn arm_trunk_merge_after_ci_rebase(
    ctx: TrunkArmContext,
    trunk_client: boss_trunk_client::TrunkClient,
    publisher: Arc<dyn boss_execution_contracts::ExecutionPublisher>,
    event_bus: Arc<EventBus>,
) {
    let TrunkArmContext {
        work_db,
        sink,
        request_id,
        product_id,
        work_item_id,
        pr_url,
        base_branch,
        target_branch,
        head_sha_before,
    } = ctx;
    let coords = match boss_engine_merge_poller::trunk_merge::parse_trunk_pr_coordinates(&pr_url) {
        Ok(coords) => coords,
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("merge_when_ready: {err}"),
                },
            );
            return;
        }
    };
    let insert_input = boss_work_db::TrunkMergeIntentInsertInput::builder()
        .work_item_id(work_item_id.clone())
        .pr_url(pr_url.clone())
        .pr_number(coords.number as i64)
        .repo(format!("{}/{}", coords.owner, coords.repo))
        .target_branch(target_branch.clone())
        .build();
    let intent = match work_db.insert_trunk_merge_intent(insert_input) {
        Ok(Some(intent)) => intent,
        Ok(None) => {
            // An earlier failed/evicted active intent may still be sitting on
            // the work item; retire it so this fresh post-rebase submission
            // owns the slot.
            if let Ok(Some(existing)) = work_db.get_active_trunk_merge_intent(&work_item_id) {
                if boss_engine_merge_poller::trunk_merge::intent_is_live_in_queue(existing.last_trunk_state.as_deref())
                {
                    if let Err(err) = work_db.record_ci_merge_rebase(
                        &work_item_id,
                        &pr_url,
                        &base_branch,
                        head_sha_before.as_deref(),
                        None,
                    ) {
                        tracing::warn!(%work_item_id, ?err, "merge_when_ready: failed to record post-rebase tracking");
                    }
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::MergeWhenReadyAccepted {
                            work_item_id,
                            pr_url: pr_url.clone(),
                            action: merge_when_ready::MergeAction::TrunkAlreadyEnqueued.as_str().to_owned(),
                        },
                    );
                    event_bus.publish(Event::PrReconcileRequested { pr_url });
                    return;
                }
                let _ = work_db.retire_trunk_merge_intent(&existing.id, "cancelled");
                match work_db.insert_trunk_merge_intent(
                    boss_work_db::TrunkMergeIntentInsertInput::builder()
                        .work_item_id(work_item_id.clone())
                        .pr_url(pr_url.clone())
                        .pr_number(coords.number as i64)
                        .repo(format!("{}/{}", coords.owner, coords.repo))
                        .target_branch(target_branch.clone())
                        .build(),
                ) {
                    Ok(Some(intent)) => intent,
                    _ => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!(
                                    "merge_when_ready: rebased onto {base_branch}, but could not record trunk merge intent"
                                ),
                            },
                        );
                        return;
                    }
                }
            } else {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "merge_when_ready: rebased onto {base_branch}, but trunk merge intent insert was ignored"
                        ),
                    },
                );
                return;
            }
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("merge_when_ready: failed to record trunk merge intent: {err}"),
                },
            );
            return;
        }
    };

    let request = boss_trunk_client::SubmitPullRequestRequest::builder()
        .repo(boss_trunk_client::TrunkRepoRef::new(
            boss_engine_merge_poller::trunk_merge::TRUNK_REPO_HOST,
            coords.owner,
            coords.repo,
        ))
        .pr(boss_trunk_client::TrunkPrRef::new(coords.number))
        .target_branch(target_branch)
        .build();
    match trunk_client.submit_pull_request(&request).await {
        Ok(()) => {
            let detail = boss_engine_merge_poller::trunk_merge::optimistic_pending_detail_json();
            let _ = work_db.set_task_merge_queue_state(&work_item_id, Some("queued"), Some(&detail));
            if let Err(err) =
                work_db.record_ci_merge_rebase(&work_item_id, &pr_url, &base_branch, head_sha_before.as_deref(), None)
            {
                tracing::warn!(%work_item_id, ?err, "merge_when_ready: failed to record post-rebase tracking");
            }
            remember_merge_request(&work_db, &work_item_id, &pr_url, None);
            send_response(
                &sink,
                &request_id,
                FrontendEvent::MergeWhenReadyAccepted {
                    work_item_id: work_item_id.clone(),
                    pr_url: pr_url.clone(),
                    action: merge_when_ready::MergeAction::TrunkEnqueued.as_str().to_owned(),
                },
            );
            event_bus.publish(Event::PrReconcileRequested { pr_url });
            publisher
                .publish_work_item_changed(&product_id, &work_item_id, "ci_merge_rebase_trunk_submitted")
                .await;
        }
        Err(err) => {
            if work_db.delete_trunk_merge_intent(&intent.id).is_err() {
                let _ = work_db.retire_trunk_merge_intent(&intent.id, "cancelled");
            }
            event_bus.publish(Event::PrReconcileRequested { pr_url: pr_url.clone() });
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!(
                        "merge_when_ready: rebased onto {base_branch}, but trunk queue submission failed for {pr_url}: {err}"
                    ),
                },
            );
        }
    }
}

#[cfg(test)]
#[path = "review_ci_rebase_tests.rs"]
mod tests;
