//! Prompt composition for `pr_review_guide` executions. Split out of
//! `runner::prompt` (which sits at the repo's file-size limit) to keep this
//! addition reviewable on its own — see the design at
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`.

use std::path::Path;

use anyhow::Context as _;

use crate::work::{WorkDb, WorkExecution};

/// Compose the full review-guide prompt for a `pr_review_guide` execution:
/// the exact versioned template (`boss_review_guide::render_prompt`) plus
/// the embedded, revision-pinned source context
/// (`boss_review_guide::render_source_context`) and the orientation inputs
/// (`boss_review_guide::render_guide_context`). `execution.work_item_id` is
/// the comparison id (see `WorkDb::create_pr_review_guide_execution`) — not
/// a task — so this loads the immutable packet directly rather than going
/// through the generic work-item path. No async work: the packet is already
/// durably stored, so there is nothing left to fetch over the network.
///
/// `workspace_path` is the pinned read-only cube workspace; the prompt names
/// its git object store explicitly (`git --git-dir <store> …`) because the
/// worker environment deliberately omits `GIT_DIR` — an ambient `GIT_DIR` at
/// cube's shared store turns every bare `git` into a store operation.
pub(super) fn compose_review_guide_prompt(
    work_db: &WorkDb,
    execution: &WorkExecution,
    workspace_path: &Path,
) -> anyhow::Result<String> {
    let comparison_id = &execution.work_item_id;
    let capture = match work_db.get_pr_review_guide_comparison_by_id(comparison_id) {
        Ok(Some(capture)) => capture,
        Ok(None) => {
            tracing::warn!(
                execution_id = %execution.id,
                comparison_id,
                "review_guide execution: comparison not found; the run will have no source material",
            );
            return Ok(
                "The engine could not load the source comparison for this review-guide run. \
                    State that essential context could not be obtained; do not invent content."
                    .to_owned(),
            );
        }
        Err(err) => {
            tracing::warn!(
                execution_id = %execution.id,
                comparison_id,
                error = %err,
                "review_guide execution: failed to load comparison",
            );
            return Ok(
                "The engine could not load the source comparison for this review-guide run. \
                    State that essential context could not be obtained; do not invent content."
                    .to_owned(),
            );
        }
    };
    let packet = capture.packet;
    let git_dir = crate::conflict_diagnosis::resolve_git_dir(workspace_path)
        .with_context(|| {
            format!(
                "resolving the pinned git object store for review-guide workspace {}",
                workspace_path.display()
            )
        })?
        .display()
        .to_string();
    let metadata = boss_review_guide::PromptMetadata::builder()
        .pr_url(&packet.canonical_pr_url)
        .repository(&packet.base_repository)
        .pr_title(&packet.title)
        // Source links use the comparison's merge base rather than the base tip.
        .base_sha(&packet.merge_base_sha)
        .head_sha(&packet.head_sha)
        .git_dir(&git_dir)
        .build();
    let mut prompt = match work_db.review_guide_update_context(&execution.id)? {
        Some((old_head, previous)) => boss_review_guide::render_update_prompt(&metadata, &old_head, &previous),
        None => boss_review_guide::render_prompt(&metadata),
    };
    prompt.push_str("\n\n");
    prompt.push_str(&boss_review_guide::render_source_context(&packet));
    // Every guide opens with an orientation built from the originating task,
    // its revisions and its project. A guide without those inputs cannot
    // orient anyone, so a missing one aborts the run with the reason instead
    // of letting the model write around the gap.
    let context = work_db
        .review_guide_task_context(&capture.root_task_id, &packet.canonical_pr_url, &packet.head_sha)
        .with_context(|| {
            format!(
                "review-guide execution {} cannot be started: the orientation context for {} is unavailable",
                execution.id, packet.canonical_pr_url
            )
        })?;
    prompt.push('\n');
    prompt.push_str(&boss_review_guide::render_guide_context(&context));
    // Guide executions point to a comparison, not a task. The captured
    // series already carries the canonical root resolved at source capture.
    // Reuse the gallery's chain-wide query so revisions contribute evidence.
    let attachments = work_db.list_work_attachments_for_chain(&capture.root_task_id)?;
    prompt.push_str("\n### Registered screenshots\n\n");
    if attachments.is_empty() {
        prompt.push_str("No screenshots are registered for this task or its revisions.\n");
    } else {
        prompt.push_str("Screenshots registered for this task and its revisions:\n\n");
        // Serialize only review metadata, never local paths or evidence URLs.
        // JSON quoting also keeps multiline captions from creating headings.
        let manifest: Vec<_> = attachments
            .iter()
            .map(|attachment| {
                serde_json::json!({
                    "id": attachment.id,
                    "caption": attachment.caption,
                    "pixel_width": attachment.pixel_width,
                    "pixel_height": attachment.pixel_height,
                    "image_available": !attachment.is_reclaimed(),
                })
            })
            .collect();
        prompt.push_str(&serde_json::to_string_pretty(&manifest)?);
        prompt.push('\n');
    }
    Ok(prompt)
}
