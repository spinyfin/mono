//! `GetReviewGuideSummary` / `GetReviewGuideContent` / `RetryReviewGuide` RPC
//! handlers. Modeled on `app::projects::handle_resolve_project_design_doc`
//! (summary/content query shape) and `app::proposals::handle_submit_proposal`
//! (idempotent retry shape). See
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`.

use super::*;

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
                summary: summary.map(wire_summary),
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
                content: version.map(wire_version),
            },
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_retry_review_guide(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
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
        Ok(crate::work::RetryReviewGuideOutcome::Created(attempt)) => send_response(
            &sink,
            &request_id,
            FrontendEvent::ReviewGuideRetryQueued {
                attempt: wire_attempt(attempt),
                already_requested: false,
            },
        ),
        Ok(crate::work::RetryReviewGuideOutcome::AlreadyRequested(attempt)) => send_response(
            &sink,
            &request_id,
            FrontendEvent::ReviewGuideRetryQueued {
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

fn wire_summary(summary: crate::work::PrReviewGuideSummary) -> boss_protocol::ReviewGuideSummary {
    boss_protocol::ReviewGuideSummary::builder()
        .series_id(summary.series_id)
        .root_task_id(summary.root_task_id)
        .canonical_pr_url(summary.canonical_pr_url)
        .lifecycle(summary.lifecycle)
        .request_epoch(summary.request_epoch)
        .maybe_selected_comparison_id(summary.selected_comparison_id)
        .maybe_readable_version_id(summary.readable_version_id)
        .build()
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
