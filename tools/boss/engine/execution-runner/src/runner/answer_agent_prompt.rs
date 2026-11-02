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
        pr_lifecycle,
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
    // `Some(true)` only when `cube workspace goto --pr` actually succeeded
    // for this execution (stamped by the coordinator right after the goto
    // attempt — see `set_answer_agent_run_positioning`). `None`/`Some(false)`
    // means the leased checkout is a fresh `cube change create` off the
    // default base, not the PR head — the prompt below must not claim
    // otherwise, and the guide answer-agent CLAUDE.md conditions the same
    // claim on this signal (see `render_answer_agent_claude_md`).
    let workspace_positioned = work_db
        .get_answer_agent_run_by_execution(execution_id)
        .ok()
        .flatten()
        .and_then(|run| run.workspace_positioned);
    let checkout_positioned_on_pr_head = workspace_positioned == Some(true);
    // Done roots include closed-unmerged chores. This lifecycle is a hint,
    // not merge evidence; permit inspection without promising PR inclusion.
    let inspect_default_branch =
        *pr_lifecycle == boss_protocol::DocOwnerPrLifecycle::Merged && workspace_positioned != Some(false);
    let mut prompt = String::new();
    prompt.push_str(
        "You are a read-only \"mini-coordinator\" answer agent, spawned to answer one \
         reviewer question left as a comment on a PR review guide. The guide is an \
         immutable explanation of one comparison; the current PR may have moved on. \
         Use the checkout guidance below to distinguish current code from the guide. Your \
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
            "Latest captured comparison head SHA: `{head}` (comparison `{comparison}`",
            head = capture.packet.head_sha,
            comparison = capture.comparison_id,
        ));
        if let Some(n) = pr_number {
            prompt.push_str(&format!(", pull request #{n}"));
        }
        // This is the head the most recent stored review-guide capture
        // compared against — an immutable packet that may lag behind a
        // newer push. It is NOT necessarily the checkout identity; see the
        // `checkout_positioned_on_pr_head` sentence below for that.
        prompt.push_str(").\n");
    }
    if checkout_positioned_on_pr_head {
        prompt.push_str(
            "Your leased checkout is positioned on the current PR head (via `cube workspace goto --pr`); \
             inspect that code as current.\n",
        );
    } else if inspect_default_branch {
        prompt.push_str(
            "Your leased checkout is a fresh change off the default base branch. If the PR merged, \
             that branch includes the merged change and may be inspected as current code. The task \
             is done, but that alone does not prove a merge: closed-unmerged chores are also done. \
             Inspect the default-branch implementation and distinguish what you can verify there \
             from the captured PR comparison. If the PR closed unmerged or its changes are absent, \
             state that the current PR code was not available to you.\n",
        );
    } else {
        prompt.push_str(
            "Your leased checkout is a fresh change off the default base branch, NOT the PR head — \
             positioning onto the PR head was not possible (e.g. the PR is no longer open, or is not yet \
             confirmed open). Do not assume your checkout matches the PR above, and do not treat it as \
             current PR code. Answer from the quoted guide, the captured comparison above, and the thread \
             below, and state explicitly in your reply that the current PR code was not available to you.\n",
        );
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
                "## Original guide content\n\nNot available — answer from the quoted section and the evidence permitted by the checkout guidance above.\n\n",
            );
        }
    }
    let task_instruction = if checkout_positioned_on_pr_head {
        "Answer the question above as thoroughly and accurately as you can. Inspect the \
         current PR implementation, not only the quoted guide. If the guide describes \
         behavior that the current code no longer has, say so."
    } else if inspect_default_branch {
        "Answer the question using the quoted guide and the current default-branch implementation. \
         Verify whether the described change is present before treating it as merged PR code; \
         explain any difference or uncertainty in your reply."
    } else {
        "Answer the question above as thoroughly and accurately as you can, from the quoted \
         guide, the captured comparison head above, and the prior thread. Your checkout is not \
         the PR head, so do not inspect it as if it were current PR code; state in your reply \
         that the current PR code was not available to you."
    };
    prompt.push_str(&format!(
        "## Your task\n\n\
         {task_instruction} You may not edit, push, \
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
    use crate::test_support::{
        add_guide_comment, create_active_chore, create_product, open_db, seed_published_guide, seed_review_guide_series,
    };
    use crate::work::{ExecutionKind, ExecutionStatus, PublishReviewGuideOutcome};
    use boss_protocol::{CommentAnchor, CreateCommentInput, WorkItemPatch};

    fn seed_guide_comment(db: &WorkDb) -> (String, boss_protocol::WorkComment) {
        let (root, series) = seed_published_guide(db, 9);
        let comment = add_guide_comment(db, &series, "why does this retry forever?");
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
        // No `AnswerAgentRun` was ever stamped `workspace_positioned = true`
        // for this execution (the coordinator's dispatch/goto path never ran
        // in this unit test), so the prompt must not claim the checkout is on
        // the PR head — it would be a fresh `cube change create` checkout in
        // production too, under the exact same "no run row" condition.
        assert!(
            !prompt.contains("Your leased checkout is positioned on the current PR head"),
            "prompt must not claim PR-head positioning when it never happened:\n{prompt}"
        );
        assert!(
            prompt.contains("Your leased checkout is a fresh change off the default base branch"),
            "prompt must state the checkout is NOT on the PR head:\n{prompt}"
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

    /// When the coordinator's goto did succeed (`workspace_positioned =
    /// true` stamped on the bound `AnswerAgentRun`), the prompt must claim
    /// PR-head positioning — and the original guide head, the latest
    /// capture head, and the checkout's positioning statement are three
    /// distinct pieces of information that must not be conflated.
    #[tokio::test]
    async fn guide_answer_prompt_states_positioning_only_when_goto_actually_succeeded() {
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
        // Mirror the coordinator's real bind-then-stamp sequence: a run is
        // created, bound to this execution, then stamped `positioned = true`
        // exactly as `set_answer_agent_run_positioning` does right after a
        // successful `cube workspace goto --pr`.
        let run = db
            .create_answer_agent_run(&comment.id, "pr_review_guide", &series, "hash", 0)
            .unwrap();
        db.bind_answer_agent_run_execution(&run.id, &execution.id).unwrap();
        db.set_answer_agent_run_positioning(&execution.id, true).unwrap();

        let prompt = compose_answer_agent_prompt(&db, &execution).await;
        assert!(
            prompt.contains("Your leased checkout is positioned on the current PR head"),
            "prompt must claim PR-head positioning once it actually happened:\n{prompt}"
        );
        assert!(
            !prompt.contains("Your leased checkout is a fresh change off the default base branch"),
            "prompt must not also claim the fallback state:\n{prompt}"
        );
    }

    /// A done root permits default-branch inspection but is not merge proof.
    #[tokio::test]
    async fn guide_answer_prompt_allows_default_branch_inspection_for_merged_lifecycle() {
        let (_dir, db) = open_db();
        let root = create_active_chore(&db, &create_product(&db), "impl");
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("done".to_owned()),
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
        assert!(prompt.contains("If the PR merged"));
        assert!(prompt.contains("current default-branch implementation"));
        assert!(prompt.contains("does not prove a merge"));
        assert!(!prompt.contains("so do not inspect it"));
        assert!(
            prompt.contains("Your leased checkout is a fresh change off the default base branch"),
            "a merged/closed PR's target is never `Open`, so goto never ran; the prompt must say so:\n{prompt}"
        );
        assert!(
            !prompt.contains("Your leased checkout is positioned on the current PR head"),
            "prompt must not claim positioning that never happened:\n{prompt}"
        );
        assert!(
            !prompt.contains("fetch"),
            "the answer agent has no tool to fetch PR code; the prompt must not tell it to:\n{prompt}"
        );
        assert!(
            !prompt.contains("Inspect the current PR implementation"),
            "when not positioned, the task block must not tell the agent to inspect its \
             working copy as if it were current PR code:\n{prompt}"
        );

        // A failed goto is explicit contrary evidence even if the task has
        // since become done. Preserve the unavailable-code instructions.
        let run = db
            .create_answer_agent_run(&comment.id, "pr_review_guide", &series, "hash", 0)
            .unwrap();
        db.bind_answer_agent_run_execution(&run.id, &execution.id).unwrap();
        db.set_answer_agent_run_positioning(&execution.id, false).unwrap();
        let prompt = compose_answer_agent_prompt(&db, &execution).await;
        assert!(prompt.contains("current PR code was not available to you"));
        assert!(!prompt.contains("current default-branch implementation"));
        assert!(!prompt.contains("Investigate current code"));
    }
}

pub mod rules {
    //! Read-only "mini-coordinator" answer agent (P3a of
    //! `tools/boss/docs/designs/comment-triggered-document-revisions.md`).
    //!
    //! This module owns the answer agent's **worker-facing** surface: the
    //! read-only agent-rules file it is spawned with, and the one command name it
    //! is allowed to run that changes state (posting its reply). The
    //! *capability-restricted dispatch* itself — the `dontAsk` permission mode, the
    //! `permissions.allow` allowlist, and the deny belt — lives in
    //! [`crate::worker_setup`], keyed on [`crate::worker_setup::WorkerKind::AnswerAgent`],
    //! so the whole permission surface is assembled in one place. The two are
    //! linked by [`THREAD_REPLY_COMMAND`] (the sole allowlisted mutation) so the
    //! allowlist and the prose can never drift apart.
    //!
    //! ## Enforcement model (resolves the design's open question)
    //!
    //! The design (§ Risks) left open whether the read-only sandbox should be a
    //! *hard-coded reduced tool table* (recommended) or a *runtime capability-token
    //! check on every RPC*. This implements the former, using Claude Code's native
    //! deny-by-default `dontAsk` permission mode:
    //!
    //! - The agent is launched with `--permission-mode dontAsk` (forced at the
    //!   dispatch layer — never `--dangerously-skip-permissions`, which would
    //!   bypass settings, and never `--permission-mode auto`, which auto-approves).
    //! - `dontAsk` auto-denies every tool call except those matching
    //!   `permissions.allow` and built-in read-only Bash commands — a true
    //!   allowlist, not a blocklist.
    //! - A comprehensive `permissions.deny` belt (deny always wins over allow)
    //!   covers the known-catastrophic mutating surfaces as defense-in-depth.
    //!
    //! No new per-RPC token machinery is introduced. This mirrors how the revision
    //! PreToolUse guard blocks at the tool-availability layer, applied to a much
    //! smaller allowed set.

    /// The single state-mutating command the answer agent is permitted to run:
    /// post its comprehensive reply as an engine-authored thread entry on the
    /// comment. Every other write/push/mutate surface is denied.
    ///
    /// This is the one entry in the answer agent's `permissions.allow` allowlist
    /// beyond read-only tools (see
    /// engine worker-setup answer-agent allowlist). The command itself — a
    /// `boss` subcommand that appends an `entry_kind='answer'` thread entry and
    /// flips the comment `answering → answered` — is implemented in P3b (spawn +
    /// reply). Naming it here as a single constant keeps the P3a allowlist and the
    /// P3b reply path referencing one source of truth.
    ///
    /// SECURITY (for the P3b implementer): the allowlist entry is
    /// `Bash({THREAD_REPLY_COMMAND}:*)`, which permits ANY arguments/flags to this
    /// command (Claude Code blocks chaining/`$(...)`, but not in-command flags).
    /// The sandbox therefore trusts this command to be strictly single-purpose —
    /// post one reply to the run's own target comment thread. Do NOT give it
    /// side-effecting flags (arbitrary `--comment-id`, `--status`, a `--body-file`
    /// that reads any path, etc.); anything it accepts is inside the sandbox.
    pub const THREAD_REPLY_COMMAND: &str = "boss comment reply";

    /// Render the read-only agent-rules file (`CLAUDE.md`) for an answer-agent
    /// worker. Modeled on [`crate::pr_review::render_reviewer_claude_md`] but for
    /// the answer agent's mandate: read anything the coordinator can see, read code
    /// in a leased checkout, and post exactly one thread reply — never edit, push,
    /// open a PR, or mutate task/comment/cube state.
    ///
    /// Returned in place of the Standard worker `CLAUDE.md` (which is
    /// PR-deliverable-oriented and would be actively wrong here) by
    /// [`crate::worker_setup::render_claude_md`] when
    /// `worker_kind == WorkerKind::AnswerAgent`.
    pub fn render_answer_agent_claude_md(
        lease_id: &str,
        workspace_path: &str,
        checkout_positioned_on_pr_head: bool,
    ) -> String {
        let checkout_positioning_note = if checkout_positioned_on_pr_head {
            "For a PR-guide question the leased checkout is the\n           \
         current PR head — inspect that implementation, not only the quoted\n           \
         guide."
        } else {
            "For a PR-guide question, if positioning the leased checkout on the\n           \
         current PR head was possible the initial prompt says so explicitly; \
         otherwise\n           \
         the checkout is a fresh change off the default base, NOT the PR head — \
         do not\n           \
         assume it matches the PR. Follow the initial prompt's lifecycle guidance: \
         merged changes may be inspected on the default branch; distinguish \
         verified code from the captured comparison in your reply."
        };
        format!(
            "# Boss answer-agent rules\n\
         \n\
         You are running inside a Boss-managed **answer-agent** session. The\n\
         engine spawned you to answer one reviewer question left as a comment\n\
         in that comment's thread. Your initial prompt identifies the target:\n\
         a design/investigation document, or a PR review guide (an immutable\n\
         explanation of one comparison; the current PR may have moved on).\n\
         You are a read-only mini-coordinator: you can read everything the Boss\n\
         coordinator can see and read code in a leased checkout, but you change\n\
         nothing except by posting your reply.\n\
         \n\
         ## Read-only mandate (HARD CONSTRAINT)\n\
         \n\
         **Your only state-changing action is posting your reply to the comment\n\
         thread.** Tool calls for anything else are denied at the dispatch layer\n\
         (deny-by-default `dontAsk` permission mode + an explicit deny list) —\n\
         this is enforced, not advisory.\n\
         \n\
         Forbidden (tool calls for these are denied):\n\
         \n\
         - Editing or writing ANY file (`Edit`, `Write`, `NotebookEdit`).\n\
         - Committing or pushing (`jj git push`, `git push`).\n\
         - Opening, updating, merging, closing, editing, or commenting on a PR\n\
           (`gh pr …`, `cube pr create`, `cube pr update`).\n\
         - Creating or mutating tasks, chores, revisions, or projects, or\n\
           changing comment status by any means other than posting your reply\n\
           (the reply command flips this comment `answering → answered` as its\n\
           own bundled side effect — that is the one sanctioned exception).\n\
         - Leasing, releasing, or otherwise mutating cube state (`cube …`). The\n\
           engine already handed you a read-only checkout — use it as-is.\n\
         \n\
         Anything you would \"fix\", describe in your reply instead. Your reply\n\
         may include concrete proposed edits (a sketch, an embedded diff), but\n\
         you have no mechanism to apply them — it is prose, not a patch.\n\
         \n\
         ## Posting your reply\n\
         \n\
         When you have answered the question, post exactly one reply with\n\
         `{reply_cmd}` (the sole write you are permitted). Do not post more than\n\
         one reply.\n\
         \n\
         ## What you can read\n\
         \n\
         - The commented-on document or review guide, the comment, and its\n\
           full thread. {checkout_positioning_note}\n\
         - Product/project/task/execution/PR state via the coordinator's\n\
           read-only query layer.\n\
         - Code in your leased workspace — use `Read`, `Grep`, `Glob`, and\n\
           read-only shell (`cat`, `grep`, `jj log`, `jj show`, `jj diff`).\n\
         \n\
         ## Your workspace\n\
         \n\
         - Workspace path: `{workspace_path}`\n\
         - Cube lease id: `{lease}`\n\
         \n\
         The workspace is a read-only checkout. Lease held for the lifetime of\n\
         this run by the engine. Do not lease, release, or mutate cube state.\n\
         \n\
         {absolute_paths}\
         \n\
         ## Boundaries\n\
         \n\
         - Do not modify files inside or outside your workspace. Other\n\
           workspaces belong to other workers.\n\
         - Do not modify cube's database, lease state, or workspace registry.\n\
         - `~/Library/Application Support/Boss/` is coordinator/engine-only.\n\
           Never read, write, or touch it.\n",
            reply_cmd = THREAD_REPLY_COMMAND,
            workspace_path = workspace_path,
            lease = lease_id,
            absolute_paths = crate::prompt_fragments::absolute_paths_fragment(),
            checkout_positioning_note = checkout_positioning_note,
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn claude_md_states_read_only_mandate_and_reply_command() {
            let md = render_answer_agent_claude_md("lease-1", "/ws/path", true);
            assert!(md.contains("Read-only mandate"));
            assert!(md.contains(THREAD_REPLY_COMMAND));
            assert!(md.contains("/ws/path"));
            assert!(md.contains("lease-1"));
            // It must NOT tell the agent a PR is the deliverable — that is the
            // Standard-worker contract and is actively wrong here.
            assert!(!md.contains("PR is the deliverable"));
            assert!(!md.contains("cube pr create --branch"));
            assert!(md.contains("design/investigation document"));
            assert!(md.contains("PR review guide"));
            assert!(md.contains("current PR head"));
        }

        /// When the checkout is not positioned on the PR head (fresh
        /// `cube change create` fallback), CLAUDE.md must not claim it is.
        #[test]
        fn claude_md_does_not_claim_pr_head_when_not_positioned() {
            let md = render_answer_agent_claude_md("lease-1", "/ws/path", false);
            assert!(!md.contains("the leased checkout is the"));
            assert!(md.contains("fresh change off the default base"));
        }
    }
}
