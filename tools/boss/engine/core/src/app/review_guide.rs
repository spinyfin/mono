//! `GetReviewGuideSummary` / `GetReviewGuideContent` / `RetryReviewGuide` RPC
//! handlers. Modeled on `app::projects::handle_resolve_project_design_doc`
//! (summary/content query shape) and `app::proposals::handle_submit_proposal`
//! (idempotent retry shape). See
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`.

use super::*;

pub(super) async fn handle_generate_review_guide(ctx: Dispatch, req: FrontendRequest) {
    let FrontendRequest::GenerateReviewGuide {
        root_task_id,
        idempotency_token,
    } = req
    else {
        unreachable!()
    };
    let result = generate_review_guide(&ctx, &root_task_id, idempotency_token.as_deref()).await;
    match result {
        Ok((resolved_root_id, outcome)) => {
            crate::work::notify_review_guide_changed(
                &ctx.work_db,
                &ctx.server_state.publisher,
                &resolved_root_id,
                "review_guide_generation_queued",
            )
            .await;
            let (attempt, already_requested) = match outcome {
                crate::work::RetryReviewGuideOutcome::Created(attempt) => (attempt, false),
                crate::work::RetryReviewGuideOutcome::AlreadyRequested(attempt) => (attempt, true),
                crate::work::RetryReviewGuideOutcome::NoComparison => unreachable!(),
            };
            send_response(
                &ctx.sink,
                &ctx.request_id,
                FrontendEvent::ReviewGuideRetryQueued {
                    // Echo the requested card so its in-flight guard clears even
                    // when it was a revision resolved to a different chain root.
                    root_task_id,
                    attempt: wire_attempt(attempt),
                    already_requested,
                },
            );
        }
        Err(error) => send_work_error(&ctx.sink, &ctx.request_id, &error),
    }
}

async fn generate_review_guide(
    ctx: &Dispatch,
    task_id: &str,
    idempotency_token: Option<&str>,
) -> anyhow::Result<(String, crate::work::RetryReviewGuideOutcome)> {
    let db = &ctx.work_db;
    let root_id = {
        let conn = db.connect()?;
        crate::work::chain_root(&conn, task_id)?
    };
    let task = match db.get_work_item(&root_id)? {
        boss_protocol::WorkItem::Task(task) | boss_protocol::WorkItem::Chore(task) => task,
        _ => anyhow::bail!("review guides require a task with a PR"),
    };
    let pr_url = task
        .pr_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("work item has no PR URL; cannot generate a review guide"))?;
    anyhow::ensure!(
        task.repo_remote_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty()),
        "work item has no repository remote; cannot generate a review guide"
    );
    if db
        .get_pr_review_guide_summary_for_root(&root_id)?
        .is_none_or(|summary| summary.selected_comparison_id.is_none())
    {
        ctx.server_state
            .completion_handler
            .capture_review_guide_source_manually(&root_id, pr_url)
            .await?;
    }
    let outcome = db.generate_pr_review_guide(&root_id, idempotency_token)?;
    anyhow::ensure!(
        !matches!(outcome, crate::work::RetryReviewGuideOutcome::NoComparison),
        "source capture did not select a comparison; cannot generate a review guide"
    );
    Ok((root_id, outcome))
}

pub(super) async fn handle_get_review_guide_summary(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetReviewGuideSummary { root_task_id } = req else {
        unreachable!()
    };
    match work_db.get_pr_review_guide_summary_for_root(&root_task_id) {
        Ok(summary) => send_response(
            &sink,
            &request_id,
            FrontendEvent::ReviewGuideSummary {
                summary: summary.map(crate::work::to_wire_review_guide_summary),
            },
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_get_review_guide_content(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetReviewGuideContent { version_id } = req else {
        unreachable!()
    };
    match work_db.get_pr_review_guide_version(&version_id) {
        Ok(version) => send_response(
            &sink,
            &request_id,
            FrontendEvent::ReviewGuideContent {
                version_id,
                content: version.map(wire_version),
            },
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_retry_review_guide(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RetryReviewGuide {
        root_task_id,
        idempotency_token,
    } = req
    else {
        unreachable!()
    };
    match work_db.retry_pr_review_guide(
        &root_task_id,
        idempotency_token.as_deref(),
        boss_review_guide::PROMPT_VERSION,
    ) {
        Ok(crate::work::RetryReviewGuideOutcome::Created(attempt)) => {
            crate::work::notify_review_guide_changed(
                &work_db,
                &server_state.publisher,
                &root_task_id,
                "review_guide_retry_queued",
            )
            .await;
            send_response(
                &sink,
                &request_id,
                FrontendEvent::ReviewGuideRetryQueued {
                    root_task_id: root_task_id.clone(),
                    attempt: wire_attempt(attempt),
                    already_requested: false,
                },
            )
        }
        Ok(crate::work::RetryReviewGuideOutcome::AlreadyRequested(attempt)) => send_response(
            &sink,
            &request_id,
            FrontendEvent::ReviewGuideRetryQueued {
                root_task_id: root_task_id.clone(),
                attempt: wire_attempt(attempt),
                already_requested: true,
            },
        ),
        Ok(crate::work::RetryReviewGuideOutcome::NoComparison) => send_work_error(
            &sink,
            &request_id,
            "no source comparison has been captured for this PR yet; nothing to regenerate from",
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

fn wire_version(version: crate::work::PrReviewGuideVersion) -> boss_protocol::ReviewGuideVersion {
    boss_protocol::ReviewGuideVersion::builder()
        .id(version.id)
        .series_id(version.series_id)
        .comparison_id(version.comparison_id)
        .attempt_id(version.attempt_id)
        .markdown(version.markdown)
        .content_hash(version.content_hash)
        .prompt_version(version.prompt_version)
        .generated_at(version.generated_at)
        .build()
}

fn wire_attempt(attempt: crate::work::PrReviewGuideAttempt) -> boss_protocol::ReviewGuideAttempt {
    boss_protocol::ReviewGuideAttempt::builder()
        .id(attempt.id)
        .series_id(attempt.series_id)
        .comparison_id(attempt.comparison_id)
        .request_epoch(attempt.request_epoch)
        .status(attempt.status)
        .maybe_error(attempt.error)
        .maybe_provider_usage_json(attempt.provider_usage_json)
        .build()
}

#[cfg(test)]
mod tests {
    #[test]
    fn attempt_diagnostics_preserve_native_usage() {
        let usage = r#"{"codex:rollout":{"total_token_usage":{"cached_input_tokens":8}}}"#;
        let attempt = crate::work::PrReviewGuideAttempt::builder()
            .id("attempt")
            .series_id("series")
            .comparison_id("comparison")
            .request_epoch(1)
            .ordinal(1)
            .status("succeeded")
            .prompt_version("test")
            .retries(0)
            .created_at("now")
            .provider_usage_json(usage)
            .build();
        let wire = super::wire_attempt(attempt);
        let encoded = serde_json::to_value(wire).unwrap();
        assert_eq!(encoded["provider_usage_json"], usage);
    }
}
