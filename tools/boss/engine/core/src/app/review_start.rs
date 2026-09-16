//! Explicit operator review admission and PR-number disambiguation.

use super::*;

/// `bossctl review start --pr <n>`: re-enqueue the automated review
/// pipeline on demand, using the same batch admission as automatic review
/// when fanout is enabled, and the unchanged legacy path otherwise.
pub(super) async fn handle_trigger_pr_review(ctx: Dispatch, req: FrontendRequest) {
    handle_trigger_pr_review_with(ctx, req, &GhPrStateChecker, |url| async move {
        boss_github::pr_files::fetch_pr_view_json(&url, "state,baseRefOid,headRefOid,files,additions,deletions").await
    })
    .await;
}

async fn handle_trigger_pr_review_with<F, Fut>(
    ctx: Dispatch,
    req: FrontendRequest,
    pr_checker: &dyn crate::work::PrStateChecker,
    fetch_metadata: F,
) where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<serde_json::Value>>,
{
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::TriggerPrReview { pr_number, repo } = req else {
        unreachable!()
    };
    {
        let matches = match work_db.find_work_items_by_pr(pr_number) {
            Ok(matches) => matches,
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
                return;
            }
        };
        // Repo filter (when given) matches by substring against the PR
        // URL — same disambiguation shape `boss task by-pr --repo` uses:
        // the same PR number can exist in more than one repo, and the PR
        // URL (not any per-task repo override) is authoritative for which
        // repo a PR lives in.
        let matches: Vec<_> = match repo.as_deref().filter(|r| !r.is_empty()) {
            Some(repo_filter) => matches
                .into_iter()
                .filter(|m| m.owner.pr_url.as_deref().is_some_and(|url| url.contains(repo_filter)))
                .collect(),
            None => matches,
        };
        let owner = match matches.len() {
            0 => {
                let scope = repo
                    .as_deref()
                    .map(|r| format!(" in a repo matching {r:?}"))
                    .unwrap_or_default();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("no work item bound to PR #{pr_number}{scope}"),
                    },
                );
                return;
            }
            1 => matches.into_iter().next().expect("len checked == 1").owner,
            n => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("PR #{pr_number} is ambiguous across {n} repos — pass --repo to disambiguate"),
                    },
                );
                return;
            }
        };
        let result = if server_state.feature_flags.is_enabled("review_batch_fanout") {
            async {
                let pr_url = owner.pr_url.as_deref().ok_or_else(|| anyhow::anyhow!("task has no PR URL"))?;
                let metadata = fetch_metadata(pr_url.to_owned()).await
                    .map_err(|err| anyhow::anyhow!("cannot start review batch: PR metadata unavailable: {err}"))?;
                match metadata.get("state").and_then(serde_json::Value::as_str) {
                    Some("OPEN") => {}
                    Some(state) => anyhow::bail!("cannot start review batch: PR is {state}"),
                    None => anyhow::bail!("cannot start review batch: PR metadata omitted open state"),
                }
                let input = crate::completion::review_batch_input_from_metadata(&work_db, &owner.id, pr_url, metadata)?;
                match work_db.request_pre_merge_review_batch_for_pool(input, usize::from(server_state.review_pool_size))? {
                    crate::work::ReviewBatchDispatch::Created { batch, executions }
                    | crate::work::ReviewBatchDispatch::ExistingBatch { batch, executions } => {
                        tracing::info!(batch_id = %batch.id, generation = batch.generation, "review start: admitted review batch");
                        executions.into_iter().next().ok_or_else(|| anyhow::anyhow!("review batch has no executions"))
                    }
                    other => anyhow::bail!("cannot start review batch: unexpected admission outcome {other:?}"),
                }
            }.await
        } else {
            work_db.request_pr_review(&owner.id, pr_checker)
        };
        match result {
            Ok(execution) => {
                tracing::info!(
                    work_item_id = %owner.id,
                    execution_id = %execution.id,
                    pr_number,
                    "review start: re-enqueued pr_review execution",
                );
                server_state.execution_coordinator.kick();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::PrReviewTriggered {
                        execution,
                        work_item_id: owner.id,
                        pr_url: owner.pr_url.unwrap_or_default(),
                    },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "review_start_tests.rs"]
mod tests;
