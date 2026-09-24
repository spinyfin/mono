//! Prompt composition for `answer_agent` executions from document content and
//! comment-thread context. See `comment-triggered-document-revisions.md`.

use crate::work::{FeedbackTarget, WorkDb, WorkExecution, parse_pr_doc_artifact_id};

/// Compose the initial prompt for an `answer_agent` execution.
/// `execution.work_item_id` is the comment id (see
/// `WorkDb::create_answer_agent_execution`); this resolves it back to the
/// comment, its doc owner, the doc's full content (fetched via `gh api` at
/// the doc's own branch/ref — the leased workspace checkout is at whatever
/// default ref cube gave it, not necessarily this branch, so the doc text is
/// embedded directly rather than read from disk; see the answer-agent
/// capability table's "read code in a leased checkout" vs. "read the
/// commented-on document" distinction), and any prior thread entries
/// (non-empty on a `thread_turn > 0` re-entered follow-up).
///
/// Falls back to the generic implementer prompt — logging a warning — if the
/// comment or its doc owner can no longer be resolved (raced/deleted
/// mid-flight), mirroring the triage/reviewer fallback pattern above: a
/// weaker prompt is better than no spawn at all.
pub(super) async fn compose_answer_agent_prompt(work_db: &WorkDb, execution: &WorkExecution) -> String {
    let comment_id = &execution.work_item_id;
    let fallback = |reason: &str| -> String {
        tracing::warn!(
            execution_id = %execution.id,
            comment_id = %comment_id,
            reason,
            "answer_agent execution: could not compose the answer-agent prompt; \
             falling back to a minimal generic prompt",
        );
        format!(
            "You are a read-only answer agent (see your CLAUDE.md for the full \
             read-only mandate). The engine could not resolve the comment this run was \
             spawned for ({reason}). Post a single reply via `{cmd}` explaining that you \
             were unable to load the question, then stop.",
            cmd = crate::answer_agent::THREAD_REPLY_COMMAND,
        )
    };

    let comment = match work_db.get_comment(comment_id) {
        Ok(Some(c)) => c,
        Ok(None) => return fallback("comment not found"),
        Err(err) => return fallback(&format!("failed to load comment: {err}")),
    };
    let target = match work_db.resolve_feedback_target(&comment.artifact_kind, &comment.artifact_id) {
        Ok(Some(target)) => target,
        Ok(None) => return fallback("comment's artifact has no feedback target"),
        Err(err) => return fallback(&format!("resolve_feedback_target failed: {err}")),
    };
    if let FeedbackTarget::PullRequestImplementation { .. } = &target {
        return compose_guide_answer_prompt(work_db, &comment, &target, &execution.id).await;
    }
    let FeedbackTarget::RepositoryDocument(doc_owner) = target else {
        return fallback("comment's artifact has no design/investigation doc owner");
    };

    let doc_content = match parse_pr_doc_artifact_id(&comment.artifact_id) {
        Some((repo, branch, path)) => match boss_design_doc_fetcher::fetch_design_doc(&repo, &path, &branch).await {
            boss_design_doc_fetcher::DocFetchOutcome::Content(text) => Some((path, text)),
            boss_design_doc_fetcher::DocFetchOutcome::DocMissing => {
                tracing::warn!(
                    execution_id = %execution.id,
                    comment_id = %comment_id,
                    repo, branch, path,
                    "answer_agent execution: doc no longer exists at this ref; \
                     the agent will answer from the comment's anchor context alone",
                );
                None
            }
            boss_design_doc_fetcher::DocFetchOutcome::FetchFailed { reason } => {
                tracing::warn!(
                    execution_id = %execution.id,
                    comment_id = %comment_id,
                    repo, branch, path, reason,
                    "answer_agent execution: doc fetch failed; \
                     the agent will answer from the comment's anchor context alone",
                );
                None
            }
        },
        // Only `pr_doc` artifacts reach here (`resolve_doc_owner` scopes to
        // that kind), so this is unreachable in practice; degrade gracefully.
        None => None,
    };

    let thread = work_db.list_comment_thread_entries(comment_id).unwrap_or_default();

    let mut prompt = String::new();
    prompt.push_str(
        "You are a read-only \"mini-coordinator\" answer agent, spawned to answer one \
         reviewer question left as a comment on a design/investigation document. Your \
         CLAUDE.md states the full read-only mandate and the one command you may run to \
         reply — read it before doing anything else.\n\n",
    );
    prompt.push_str(&format!(
        "## The question\n\n\
         Document: `{path}` (task {task_id}, `{task_kind}`)\n\n",
        path = doc_content
            .as_ref()
            .map(|(p, _)| p.as_str())
            .unwrap_or(comment.artifact_id.as_str()),
        task_id = doc_owner.task_id,
        task_kind = doc_owner.task_kind,
    ));
    prompt.push_str(&format!(
        "Quoted section (the highlighted span, with surrounding context):\n> {prefix}[[{exact}]]{suffix}\n\n",
        prefix = comment.anchor.prefix,
        exact = comment.anchor.exact,
        suffix = comment.anchor.suffix,
    ));
    prompt.push_str(&format!("Comment:\n> {body}\n\n", body = comment.body));

    if !thread.is_empty() {
        prompt.push_str("## Prior thread on this comment\n\n");
        for entry in &thread {
            prompt.push_str(&format!(
                "**{}** ({}):\n{}\n\n",
                entry.entry_kind, entry.author, entry.body
            ));
        }
    }

    match &doc_content {
        Some((_, text)) => {
            prompt.push_str("## Full document content\n\n");
            prompt.push_str("```markdown\n");
            prompt.push_str(text);
            prompt.push_str("\n```\n\n");
        }
        None => {
            prompt.push_str(
                "## Full document content\n\n\
                 Not available (fetch failed or the doc no longer exists at this ref) — \
                 answer from the quoted section above, and use your leased workspace / \
                 read-only tools if you need more context.\n\n",
            );
        }
    }

    prompt.push_str(&format!(
        "## Your task\n\n\
         Answer the question above as thoroughly and accurately as you can. You may read \
         anything the Boss coordinator can see and read code in your leased workspace, but \
         you may not edit, push, or mutate any state. When you have a complete answer, post \
         it with:\n\n\
         ```\n{cmd} --body \"<your comprehensive answer>\"\n```\n\n\
         Post exactly one reply, then stop. Your answer may include a concrete proposed edit \
         as a prose sketch, but you have no mechanism to apply it — do not attempt to.\n",
        cmd = crate::answer_agent::THREAD_REPLY_COMMAND,
    ));

    prompt
}

async fn compose_guide_answer_prompt(
    work_db: &WorkDb,
    comment: &boss_protocol::WorkComment,
    target: &FeedbackTarget,
    execution_id: &str,
) -> String {
    let FeedbackTarget::PullRequestImplementation {
        root_task_id,
        canonical_pr,
        ..
    } = target
    else {
        return String::new();
    };
    let thread = work_db.list_comment_thread_entries(&comment.id).unwrap_or_default();
    let context = comment.guide_context.as_ref();
    let guide_markdown = context
        .and_then(|ctx| work_db.get_pr_review_guide_version(&ctx.version_id).ok().flatten())
        .map(|version| version.markdown);
    let mut prompt = String::new();
    prompt.push_str(
        "You are a read-only \"mini-coordinator\" answer agent, spawned to answer one \
         reviewer question left as a comment on a PR review guide. The guide is an \
         immutable explanation of one comparison; the current PR may have moved on. \
         Investigate current code and distinguish old behavior in the reply. Your \
         CLAUDE.md states the full read-only mandate and the one command you may run to \
         reply — read it before doing anything else.\n\n",
    );
    prompt.push_str(&format!(
        "## The question\n\n\
         Current PR: `{canonical_pr}` (root task {root_task_id})\n"
    ));
    if let Some(capture) = work_db
        .get_latest_pr_review_guide_source_capture(root_task_id)
        .ok()
        .flatten()
    {
        let pr_number = boss_github::pr_url::pr_number_from_url(canonical_pr);
        prompt.push_str(&format!(
            "Current captured PR head SHA: `{head}` (comparison `{comparison}`",
            head = capture.packet.head_sha,
            comparison = capture.comparison_id,
        ));
        if let Some(n) = pr_number {
            prompt.push_str(&format!(", pull request #{n}"));
        }
        prompt.push_str("). Your leased checkout is positioned on this PR head; inspect that code as current.\n");
    }
    if let Some(ctx) = context {
        prompt.push_str(&format!(
            "Original guide version: `{version}` (comparison `{comparison}`, head `{head}`)\n\n",
            version = ctx.version_id,
            comparison = ctx.comparison_id,
            head = ctx.head_sha,
        ));
    }
    prompt.push_str(&format!(
        "Quoted section (the highlighted span, with surrounding context):\n> {prefix}[[{exact}]]{suffix}\n\n",
        prefix = comment.anchor.prefix,
        exact = comment.anchor.exact,
        suffix = comment.anchor.suffix,
    ));
    prompt.push_str(&format!("Comment:\n> {body}\n\n", body = comment.body));
    if !thread.is_empty() {
        prompt.push_str("## Prior thread on this comment\n\n");
        for entry in &thread {
            prompt.push_str(&format!(
                "**{}** ({}):\n{}\n\n",
                entry.entry_kind, entry.author, entry.body
            ));
        }
    }
    match guide_markdown {
        Some(text) => {
            prompt.push_str("## Original guide content (immutable quoted version)\n\n```markdown\n");
            prompt.push_str(&text);
            prompt.push_str("\n```\n\n");
        }
        None => {
            tracing::warn!(
                execution_id,
                comment_id = %comment.id,
                "answer_agent execution: guide version content unavailable"
            );
            prompt.push_str(
                "## Original guide content\n\nNot available — answer from the quoted section and current code.\n\n",
            );
        }
    }
    prompt.push_str(&format!(
        "## Your task\n\n\
         Answer the question above as thoroughly and accurately as you can. Inspect the \
         current PR implementation, not only the quoted guide. If the guide describes \
         behavior that the current code no longer has, say so. You may not edit, push, \
         or mutate any state. When you have a complete answer, post it with:\n\n\
         ```\n{cmd} --body \"<your comprehensive answer>\"\n```\n\n\
         Post exactly one reply, then stop.\n",
        cmd = crate::answer_agent::THREAD_REPLY_COMMAND,
    ));
    prompt
}

#[cfg(test)]
mod tests {
    use super::compose_answer_agent_prompt;
    use crate::test_support::{create_active_chore, create_product, open_db, seed_review_guide_series};
    use crate::work::{CreateCommentInput, PublishReviewGuideOutcome, WorkItemPatch};
    use boss_protocol::CommentAnchor;

    #[tokio::test]
    async fn guide_answer_prompt_carries_original_and_current_pr_head() {
        let (_dir, db) = open_db();
        let root = create_active_chore(&db, &create_product(&db), "impl");
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/acme/widget/pull/9".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let (series, comparison) = seed_review_guide_series(&db, &root);
        let attempt = db
            .create_pr_review_guide_attempt(&series, &comparison, "review-guide-v1")
            .unwrap();
        let PublishReviewGuideOutcome::Published(version) = db
            .publish_pr_review_guide_version(&attempt.id, "# Guide\n\nOriginal quote", "raw")
            .unwrap()
        else {
            panic!("expected published guide")
        };
        let comment = db
            .create_comment_with_guide_version(
                CreateCommentInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(&series)
                    .anchor(CommentAnchor {
                        exact: "Original quote".into(),
                        ..Default::default()
                    })
                    .body("why does this retry?")
                    .author("user:test")
                    .doc_version("hash")
                    .plain_text_projection_version(1)
                    .build(),
                Some(&version.id),
            )
            .unwrap();
        let execution = db
            .create_answer_agent_execution(&comment.id, "https://github.com/acme/widget")
            .unwrap();
        let prompt = compose_answer_agent_prompt(&db, &execution).await;
        assert!(
            prompt.contains("https://github.com/acme/widget/pull/9"),
            "prompt must name the current PR:\n{prompt}"
        );
        assert!(
            prompt.contains("Current captured PR head SHA: `head`"),
            "prompt must name the current captured head:\n{prompt}"
        );
        assert!(
            prompt.contains("pull request #9"),
            "prompt must name the PR number:\n{prompt}"
        );
        assert!(
            prompt.contains(&format!("Original guide version: `{}`", version.id)),
            "prompt must name the original version:\n{prompt}"
        );
        assert!(
            prompt.contains(&format!("comparison `{comparison}`")),
            "prompt must name the original comparison:\n{prompt}"
        );
        assert!(
            prompt.contains("head `head`"),
            "prompt must name the original comparison head:\n{prompt}"
        );
    }
}
