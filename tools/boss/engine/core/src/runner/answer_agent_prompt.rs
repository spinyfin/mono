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
    let fallback = |reason: &str| -> String { fallback_prompt(&execution.id, comment_id, reason) };

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

/// A diagnosable fallback prompt for when the comment/target this run was
/// spawned for can no longer be resolved — a weaker prompt is better than an
/// empty one, which would spawn a worker with no instructions at all.
fn fallback_prompt(execution_id: &str, comment_id: &str, reason: &str) -> String {
    tracing::warn!(
        execution_id,
        comment_id,
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
        return fallback_prompt(
            execution_id,
            &comment.id,
            "resolved feedback target is not a PR implementation target",
        );
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
    use super::*;
    use crate::test_support::{create_active_chore, create_product, open_db, seed_review_guide_series};
    use crate::work::{ExecutionKind, ExecutionStatus, PublishReviewGuideOutcome};
    use boss_protocol::{CommentAnchor, CreateCommentInput, WorkItemPatch};

    fn seed_guide_comment(db: &WorkDb) -> (String, boss_protocol::WorkComment) {
        let root = create_active_chore(db, &create_product(db), "impl");
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/acme/widget/pull/9".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let (series, comparison) = seed_review_guide_series(db, &root);
        let attempt = db
            .create_pr_review_guide_attempt(&series, &comparison, "review-guide-v1")
            .unwrap();
        let PublishReviewGuideOutcome::Published(_) = db
            .publish_pr_review_guide_version(&attempt.id, "# Guide\n\nOriginal quote", "raw")
            .unwrap()
        else {
            panic!("expected published guide")
        };
        let version_id = db
            .get_pr_review_guide_summary_for_root(&root)
            .unwrap()
            .expect("summary")
            .readable_version_id
            .expect("readable version");
        let comment = db
            .create_comment_with_guide_version(
                CreateCommentInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .anchor(CommentAnchor {
                        exact: "Original quote".into(),
                        ..Default::default()
                    })
                    .body("why does this retry forever?")
                    .author("user:test")
                    .doc_version("hash")
                    .plain_text_projection_version(1)
                    .build(),
                Some(&version_id),
            )
            .unwrap();
        (root, comment)
    }

    fn execution_for(comment_id: &str) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_answer_01")
            .work_item_id(comment_id)
            .kind(ExecutionKind::AnswerAgent)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:acme/widget.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    #[tokio::test]
    async fn guide_answer_prompt_carries_pr_identity_and_guide_content() {
        let (_dir, db) = open_db();
        let (root, comment) = seed_guide_comment(&db);
        let execution = execution_for(&comment.id);

        let prompt = compose_answer_agent_prompt(&db, &execution).await;

        assert!(
            prompt.contains("https://github.com/acme/widget/pull/9"),
            "prompt must carry the canonical current PR:\n{prompt}",
        );
        assert!(prompt.contains(&root), "prompt must carry the root task id:\n{prompt}",);
        let ctx = comment
            .guide_context
            .as_ref()
            .expect("seeded guide comment carries guide_context");
        assert!(
            prompt.contains("## Original guide content (immutable quoted version)"),
            "prompt must use the guide-content heading:\n{prompt}",
        );
        assert!(
            prompt.contains("# Guide"),
            "prompt must embed the published guide markdown:\n{prompt}",
        );
        assert!(
            !prompt.contains("Not available"),
            "guide-content branch must not fall back:\n{prompt}",
        );
        assert!(
            prompt.contains(&ctx.version_id),
            "prompt must carry version_id {}:\n{prompt}",
            ctx.version_id,
        );
        assert!(
            prompt.contains(&ctx.comparison_id),
            "prompt must carry comparison_id {}:\n{prompt}",
            ctx.comparison_id,
        );
        assert!(
            prompt.contains(&ctx.head_sha),
            "prompt must carry head_sha {}:\n{prompt}",
            ctx.head_sha,
        );
        assert!(
            prompt.contains("why does this retry forever?"),
            "prompt must carry the comment body:\n{prompt}",
        );
    }

    #[tokio::test]
    async fn guide_answer_prompt_falls_back_when_guide_version_missing() {
        let (_dir, db) = open_db();
        let (_root, mut comment) = seed_guide_comment(&db);
        // Simulate an unresolvable guide version: the missing-guide-version
        // branch of `compose_guide_answer_prompt` must still produce a
        // diagnosable prompt, not an empty section.
        if let Some(ctx) = comment.guide_context.as_mut() {
            ctx.version_id = "missing-version".to_owned();
        }
        let target = db
            .resolve_feedback_target(&comment.artifact_kind, &comment.artifact_id)
            .unwrap()
            .expect("feedback target");
        let prompt = compose_guide_answer_prompt(&db, &comment, &target, "exec_answer_02").await;
        assert!(
            prompt.contains("Original guide content\n\nNot available"),
            "missing guide version must fall back to a diagnosable placeholder:\n{prompt}",
        );
    }

    #[tokio::test]
    async fn fallback_prompt_used_when_feedback_target_is_not_pr_implementation() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let task = create_active_chore(&db, &product, "some-chore");
        // A comment whose artifact resolves to no feedback target at all —
        // `compose_answer_agent_prompt`'s own `OutOfScope` branch already
        // returns fallback text; this exercises `compose_guide_answer_prompt`
        // directly with a non-PR-implementation target to prove its own
        // fallback (not `String::new()`) fires too.
        let comment = db
            .create_comment_with_guide_version(
                CreateCommentInput::builder()
                    .artifact_kind("work_item")
                    .artifact_id(task.clone())
                    .anchor(CommentAnchor {
                        exact: "the whole task".into(),
                        ..Default::default()
                    })
                    .body("hello")
                    .author("user:test")
                    .doc_version("hash")
                    .plain_text_projection_version(1)
                    .build(),
                None,
            )
            .unwrap();
        let bogus_target = FeedbackTarget::RepositoryDocument(boss_protocol::DocOwner {
            task_id: task.clone(),
            task_kind: boss_protocol::TaskKind::Chore,
            chain_root_id: task,
            pr_url: None,
            pr_lifecycle: boss_protocol::DocOwnerPrLifecycle::NoPr,
        });
        let prompt = compose_guide_answer_prompt(&db, &comment, &bogus_target, "exec_answer_03").await;
        assert!(
            prompt.contains("could not resolve the comment this run was spawned for"),
            "non-PR-implementation target must produce a diagnosable fallback, not an empty prompt:\n{prompt}",
        );
    }
}
