//! Prompt composition for `pr_review_guide` executions. Split out of
//! `runner::prompt` (which sits at the repo's file-size limit) to keep this
//! addition reviewable on its own — see the design at
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`.

use crate::work::{WorkDb, WorkExecution};

/// Compose the full review-guide prompt for a `pr_review_guide` execution:
/// the exact versioned template (`boss_review_guide::render_prompt`) plus
/// the embedded, revision-pinned source context
/// (`boss_review_guide::render_source_context`). `execution.work_item_id` is
/// the comparison id (see `WorkDb::create_pr_review_guide_execution`) — not
/// a task — so this loads the immutable packet directly rather than going
/// through the generic work-item path. No async work: the packet is already
/// durably stored, so there is nothing left to fetch over the network.
pub(super) fn compose_review_guide_prompt(work_db: &WorkDb, execution: &WorkExecution) -> String {
    let comparison_id = &execution.work_item_id;
    let capture = match work_db.get_pr_review_guide_comparison_by_id(comparison_id) {
        Ok(Some(capture)) => capture,
        Ok(None) => {
            tracing::warn!(
                execution_id = %execution.id,
                comparison_id,
                "review_guide execution: comparison not found; the run will have no source material",
            );
            return "The engine could not load the source comparison for this review-guide run. \
                    State that essential context could not be obtained; do not invent content."
                .to_owned();
        }
        Err(err) => {
            tracing::warn!(
                execution_id = %execution.id,
                comparison_id,
                error = %err,
                "review_guide execution: failed to load comparison",
            );
            return "The engine could not load the source comparison for this review-guide run. \
                    State that essential context could not be obtained; do not invent content."
                .to_owned();
        }
    };
    let packet = capture.packet;
    let metadata = boss_review_guide::PromptMetadata {
        pr_url: &packet.canonical_pr_url,
        repository: &packet.base_repository,
        pr_title: &packet.title,
        // Before-side links validate against `merge_base_sha`, not the
        // observed base-branch tip. Advertising the merge base here is
        // what lets a model following "Use revision-pinned source links"
        // pass `reference_repository_matches`.
        base_sha: &packet.merge_base_sha,
        head_sha: &packet.head_sha,
    };
    let mut prompt = boss_review_guide::render_prompt(&metadata);
    prompt.push_str("\n\n");
    prompt.push_str(&boss_review_guide::render_source_context(&packet));
    prompt
}
