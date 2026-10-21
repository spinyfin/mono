//! `FrontendRequest` handlers — markdown-viewer comments.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

use boss_protocol::{
    AnswerAgentRun, COMMENT_STATUS_ACTIVE, COMMENT_STATUS_ANSWERING, COMMENT_STATUS_DISMISSED,
    COMMENT_STATUS_IN_REVISION, COMMENT_STATUS_ORPHANED, COMMENT_STATUS_RESOLVED, CommentThreadEntry, INTENT_REVISION,
    THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP,
};

use crate::utility_model::UtilityTask;

pub(super) async fn handle_comments_create(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsCreate {
        mut input,
        guide_version_id,
    } = req
    else {
        unreachable!()
    };
    // Mirror `handle_comments_list`: `pr_doc` keys are stored with the full
    // git remote URL as the repo component, but callers (CLI `--artifact`,
    // chat, PR links) routinely spell `owner/repo`. Resolve to an existing
    // stored key so a slug-form create lands on the same thread the app uses.
    // Falls back to the supplied id when no equivalent row exists yet.
    if input.artifact_kind == "pr_doc" {
        match work_db.resolve_pr_doc_artifact_id(&input.artifact_id) {
            Ok(resolved) => input.artifact_id = resolved,
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        }
    }
    {
        let artifact_kind = input.artifact_kind.clone();
        let artifact_id = input.artifact_id.clone();
        match work_db.create_comment_with_guide_version(input, guide_version_id.as_deref()) {
            Ok(comment) => {
                let revision = publish_comment_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    &artifact_kind,
                    &artifact_id,
                    "comment_created",
                )
                .await;

                // Classifier is NOT on the create request's critical path
                // (comment-triggered-document-revisions.md § "The classifier"):
                // spawn it detached, mirroring the
                // magic-wand dispatch pattern. The comment starts with
                // `intent` NULL — the transient `classifying` state — until
                // this completes and publishes on `comment_topic`.
                //
                // Review-guide comments are classified too
                // (automatic-pr-review-guides.md § "Comments target the implementation"):
                // the kind is what `revisable_comment_predicate` gates the
                // Revise PR batch on and what routes a question to the
                // answer agent, so a guide comment left unclassified can
                // never reach either path.
                spawn_comment_classifier(&server_state, &work_db, &session_id, &request_id, &comment);

                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::CommentResult { comment });
            }
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

/// Spawn the detached classifier call for a freshly created top-level
/// comment. `crate::comment_classifier::classify` already retries transient
/// failures internally; once those retries are exhausted (no API key,
/// persistent transport failure, or a reply that still doesn't parse), the
/// failure is recorded on the comment row
/// (`intent_classification_failed_at`/`intent_classification_error`) so the
/// UI can show a terminal failed state instead of an indefinite
/// "classifying…" spinner.
fn spawn_comment_classifier(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &WorkComment,
) {
    let server_state = server_state.clone();
    let work_db = work_db.clone();
    let session_id = session_id.to_owned();
    let request_id = request_id.to_owned();
    let comment_id = comment.id.clone();
    let body = comment.body.clone();
    let anchor = comment.anchor.clone();
    let artifact_kind = comment.artifact_kind.clone();
    let artifact_id = comment.artifact_id.clone();

    boss_task_scope::spawn(async move {
        let call = match server_state.utility_model.resolve(UtilityTask::CommentIntent) {
            Ok(call) => call,
            Err(err) => {
                tracing::debug!(
                    comment_id = %comment_id,
                    %err,
                    "comment intent classifier: no utility-model credential; recording terminal classification failure",
                );
                record_classification_failure(
                    &server_state,
                    &work_db,
                    &session_id,
                    &request_id,
                    &ClassifiedCommentRef {
                        comment_id: &comment_id,
                        artifact_kind: &artifact_kind,
                        artifact_id: &artifact_id,
                    },
                    "no utility-model credential configured",
                )
                .await;
                return;
            }
        };

        match crate::comment_classifier::classify(
            &call,
            &body,
            &anchor,
            crate::comment_classifier::ClassifierSubject::from_artifact_kind(&artifact_kind),
        )
        .await
        {
            Ok(result) => match work_db.set_comment_intent(&comment_id, &result.intent, result.confidence) {
                Ok(classified) => {
                    publish_comment_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        &artifact_kind,
                        &artifact_id,
                        "comment_intent_classified",
                    )
                    .await;

                    // Bucket 2 (P3b): a `question`-classified comment spawns the
                    // read-only answer agent. Buckets 1&3 (`revision`) are a
                    // later phase — the comment just stays `active` for now.
                    if classified.intent.as_deref() == Some(INTENT_QUESTION) {
                        spawn_answer_agent(&server_state, &work_db, &session_id, &request_id, &classified).await;
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        comment_id = %comment_id,
                        err = %err,
                        "comment intent classifier: failed to persist classification",
                    );
                }
            },
            Err(err) => {
                tracing::warn!(
                    comment_id = %comment_id,
                    err = %err,
                    "comment intent classifier: classification call failed after exhausting retries; \
                     recording terminal classification failure",
                );
                record_classification_failure(
                    &server_state,
                    &work_db,
                    &session_id,
                    &request_id,
                    &ClassifiedCommentRef {
                        comment_id: &comment_id,
                        artifact_kind: &artifact_kind,
                        artifact_id: &artifact_id,
                    },
                    &err,
                )
                .await;
            }
        }
    });
}

/// Just enough of a comment's identity for [`record_classification_failure`]
/// to persist the failure and publish the right invalidation — bundled so
/// that function stays under the shared arg-count lint rather than taking
/// `comment_id`/`artifact_kind`/`artifact_id` as three separate parameters.
struct ClassifiedCommentRef<'a> {
    comment_id: &'a str,
    artifact_kind: &'a str,
    artifact_id: &'a str,
}

/// Persist a terminal classification failure
/// (`intent_classification_failed_at`/`intent_classification_error`) and
/// publish the invalidation so the sidebar drops the "classifying…" spinner
/// in favor of a failed state. Shared by [`spawn_comment_classifier`]'s
/// no-API-key and retries-exhausted paths. Errors recording the failure are
/// logged, not propagated — the comment simply falls back to the
/// pre-existing behavior of staying in the `classifying` state.
async fn record_classification_failure(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &ClassifiedCommentRef<'_>,
    error: &str,
) {
    if let Err(err) = work_db.record_comment_classification_failed(comment.comment_id, error) {
        tracing::warn!(
            comment_id = %comment.comment_id,
            err = %err,
            "comment intent classifier: failed to record terminal classification failure",
        );
        return;
    }
    publish_comment_invalidation(
        server_state,
        session_id,
        request_id,
        comment.artifact_kind,
        comment.artifact_id,
        "comment_intent_classification_failed",
    )
    .await;
}

/// Spawn a read-only answer-agent run for a freshly `question`-classified
/// comment (P3b of `comment-triggered-document-revisions.md`). Transitions
/// the comment `active → answering`, creates the tracking
/// `answer_agent_runs` row, then creates the `answer_agent` execution and
/// kicks the coordinator so the normal dispatch pipeline (cube lease → spawn)
/// picks it up. The comment's `answering` status + the `running` run row
/// together drive the thinking indicator, pushed here via the same
/// `comment_topic` invalidation every other comment mutation uses — no
/// separate indicator channel.
///
/// Scope guard: only artifacts that `resolve_feedback_target` resolves to
/// are eligible — either a `Design`/`Investigation`-owned `pr_doc`, or a
/// `pr_review_guide` series bound to a PR implementation of any task kind
/// (design §"Scope guard" — the classifier itself runs unconditionally
/// today, so this is the point where bucket 2 enforces the feedback-target
/// gate before spawning anything). A comment that doesn't resolve — e.g.
/// `artifact_kind = 'work_item'`, or a `pr_doc`/`pr_review_guide` whose
/// owning task the resolver can't find — is intentionally left `active` with
/// `intent = question` and no bucket-2 affordance: the same "no
/// comment-driven affordance" outcome the migration section already accepts
/// for `work_item` comments post-magic-wand.
async fn spawn_answer_agent(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &WorkComment,
) {
    // Usually `0` (a comment's first classification into bucket 2), but a
    // manual reclassification back to `question` can re-enter a comment that
    // already has run history (e.g. from an earlier follow-up loop) — compute
    // the next turn the same way `respawn_answer_agent_for_followup` does so
    // `thread_turn` stays a monotonic counter across both entry points.
    let next_turn = match work_db.latest_answer_agent_run_for_comment(&comment.id) {
        Ok(Some(run)) => run.thread_turn + 1,
        Ok(None) => 0,
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent spawn: failed to look up the latest run; defaulting thread_turn to 0",
            );
            0
        }
    };

    let repo_remote_url = match resolve_answer_agent_repo(work_db, comment) {
        AnswerAgentRepoResolution::Resolved(url) => url,
        AnswerAgentRepoResolution::OutOfScope => return,
        AnswerAgentRepoResolution::Failed(error_kind) => {
            record_answer_agent_spawn_failure(
                server_state,
                work_db,
                session_id,
                request_id,
                comment,
                next_turn,
                error_kind,
            )
            .await;
            return;
        }
    };

    // Guarded status flip first (cheapest failure mode, no side effects to
    // unwind): only proceed to create tracking rows once we know this
    // comment is actually eligible to enter the `answering` state.
    if let Err(err) = work_db.transition_comment_to_answering(&comment.id) {
        tracing::warn!(
            comment_id = %comment.id,
            err = %err,
            "answer-agent spawn: comment was not 'active'; skipping spawn",
        );
        return;
    }

    finish_answer_agent_spawn(
        server_state,
        work_db,
        session_id,
        request_id,
        comment,
        &repo_remote_url,
        next_turn,
        "comment_answer_agent_spawned",
        WorkDb::transition_comment_answering_to_active,
    )
    .await;
}

/// Re-entrant answer-agent spawn for a bucket-2 follow-up loop (P3c
/// "Follow-up reclassification loop"): a comment sitting `awaiting_followup`
/// whose reply reclassified as `question` re-enters bucket 2. Mirrors
/// [`spawn_answer_agent`] except for the source status
/// (`awaiting_followup`, not `active`) and the `thread_turn`, which
/// increments from the comment's latest run rather than starting at `0` —
/// design §"Reclassifying follow-ups": "the answer agent runs again with the
/// accumulated thread as context (`thread_turn` increments)."
async fn respawn_answer_agent_for_followup(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &WorkComment,
) {
    let next_turn = match work_db.latest_answer_agent_run_for_comment(&comment.id) {
        Ok(Some(run)) => run.thread_turn + 1,
        Ok(None) => 0,
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent follow-up respawn: failed to look up the latest run; defaulting thread_turn to 0",
            );
            0
        }
    };

    let repo_remote_url = match resolve_answer_agent_repo(work_db, comment) {
        AnswerAgentRepoResolution::Resolved(url) => url,
        AnswerAgentRepoResolution::OutOfScope => return,
        AnswerAgentRepoResolution::Failed(error_kind) => {
            record_answer_agent_spawn_failure(
                server_state,
                work_db,
                session_id,
                request_id,
                comment,
                next_turn,
                error_kind,
            )
            .await;
            return;
        }
    };

    if let Err(err) = work_db.transition_comment_awaiting_followup_to_answering(&comment.id) {
        tracing::warn!(
            comment_id = %comment.id,
            err = %err,
            "answer-agent follow-up respawn: comment was not 'awaiting_followup'; skipping respawn",
        );
        return;
    }

    finish_answer_agent_spawn(
        server_state,
        work_db,
        session_id,
        request_id,
        comment,
        &repo_remote_url,
        next_turn,
        "comment_followup_answer_agent_spawned",
        WorkDb::transition_comment_answering_to_awaiting_followup,
    )
    .await;
}

/// Outcome of [`resolve_answer_agent_repo`]. `OutOfScope` is the
/// intentional "no bucket-2 affordance" case (design §"Scope guard") and
/// leaves the comment untouched with no durable record. `Failed` covers
/// every other case that keeps a `question`-classified, doc-owned comment
/// from spawning — these must not fail silently (a WARN log with no
/// durable trace left the app showing a "thinking" state that never
/// resolves), so callers record a failed `answer_agent_runs` row for it.
enum AnswerAgentRepoResolution {
    Resolved(String),
    OutOfScope,
    Failed(&'static str),
}

/// Resolve the repo an answer-agent execution should be spawned against, for
/// `resolve_feedback_target`'s owner task (`FeedbackTarget::owner_task_id`).
/// Routes through
/// [`WorkDb::resolve_repo_for_task`] — the multi-repo design's single
/// resolution point — rather than reading `tasks.repo_remote_url` directly:
/// an `investigation` (or project-less `design`) task's repo lives on the
/// owning product's `docs_repo` / `design_repo` (or `BOSS_USER_DOCS_REPO`),
/// never stamped on the task row itself, so a direct column read always saw
/// `NULL` for those kinds even though the task has a perfectly resolvable
/// repo. Shared by [`spawn_answer_agent`] (P3b) and
/// [`respawn_answer_agent_for_followup`] (P3c) — the doc-owner/repo lookup
/// is identical for a fresh spawn and a follow-up re-entry.
fn resolve_answer_agent_repo(work_db: &WorkDb, comment: &WorkComment) -> AnswerAgentRepoResolution {
    let owner_task_id = match work_db.resolve_feedback_target(&comment.artifact_kind, &comment.artifact_id) {
        Ok(Some(target)) => target.owner_task_id().to_owned(),
        Ok(None) => {
            tracing::debug!(
                comment_id = %comment.id,
                artifact_kind = %comment.artifact_kind,
                artifact_id = %comment.artifact_id,
                "answer-agent spawn: comment's artifact has no feedback target; skipping (out of bucket-2 scope)",
            );
            return AnswerAgentRepoResolution::OutOfScope;
        }
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent spawn: resolve_feedback_target failed; leaving comment as-is",
            );
            return AnswerAgentRepoResolution::Failed("feedback_target_resolution_failed");
        }
    };

    match work_db.resolve_repo_for_task(&owner_task_id) {
        Ok(Some(url)) => AnswerAgentRepoResolution::Resolved(url),
        Ok(None) => {
            tracing::warn!(
                comment_id = %comment.id,
                task_id = %owner_task_id,
                "answer-agent spawn: doc owner task has no resolvable repo; skipping",
            );
            AnswerAgentRepoResolution::Failed("repo_unresolved")
        }
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                task_id = %owner_task_id,
                err = %err,
                "answer-agent spawn: failed to resolve doc owner task's repo; skipping",
            );
            AnswerAgentRepoResolution::Failed("repo_resolution_error")
        }
    }
}

/// Record that an eligible (doc-owned, `question`-classified) comment's
/// answer-agent spawn could not proceed, so the failure is durable and
/// visible instead of a WARN-and-forget that leaves the app's "thinking"
/// indicator running forever with nothing behind it (see
/// [`AnswerAgentRepoResolution::Failed`]). The comment's status is left
/// untouched — it never entered `answering` — but a terminal `failed`
/// `answer_agent_runs` row now exists for it, and `answer_agent_failed` on
/// the next `CommentsList`/`comment_topic` read reflects that.
async fn record_answer_agent_spawn_failure(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &WorkComment,
    thread_turn: i64,
    error_kind: &str,
) {
    let run = match work_db.create_answer_agent_run(
        &comment.id,
        &comment.artifact_kind,
        &comment.artifact_id,
        &comment.doc_version,
        thread_turn,
    ) {
        Ok(run) => run,
        Err(err) => {
            tracing::error!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent spawn: failed to record the failed-spawn tracking run row",
            );
            return;
        }
    };
    if let Err(err) = work_db.complete_answer_agent_run(&run.id, ANSWER_AGENT_RUN_STATUS_FAILED, None, Some(error_kind))
    {
        tracing::error!(
            comment_id = %comment.id,
            run_id = %run.id,
            err = %err,
            "answer-agent spawn: failed to mark the failed-spawn tracking run row 'failed'",
        );
        return;
    }
    crate::answer_agent_observability::record_failed(&server_state.metrics, error_kind);
    publish_comment_invalidation(
        server_state,
        session_id,
        request_id,
        &comment.artifact_kind,
        &comment.artifact_id,
        "comment_answer_agent_spawn_failed",
    )
    .await;
}

/// Shared tail of an answer-agent spawn, once the caller has already
/// performed the guarded transition into `answering`: create the tracking
/// `answer_agent_runs` row, create the bound `answer_agent` execution, kick
/// the coordinator, and publish the invalidation that drives the sidebar's
/// thinking indicator. `compensate` is the guarded transition back to the
/// comment's pre-`answering` status (`answering → active` for a fresh spawn,
/// `answering → awaiting_followup` for a follow-up re-entry) — invoked if
/// either tracking-row creation step fails, mirroring the original
/// [`spawn_answer_agent`]'s compensation behaviour.
#[allow(clippy::too_many_arguments)]
async fn finish_answer_agent_spawn(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &WorkComment,
    repo_remote_url: &str,
    thread_turn: i64,
    invalidation_reason: &str,
    compensate: fn(&WorkDb, &str) -> anyhow::Result<WorkComment>,
) {
    let run = match work_db.create_answer_agent_run(
        &comment.id,
        &comment.artifact_kind,
        &comment.artifact_id,
        &comment.doc_version,
        thread_turn,
    ) {
        Ok(run) => run,
        Err(err) => {
            tracing::error!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent spawn: comment flipped to 'answering' but failed to create the tracking run row",
            );
            if let Err(err) = compensate(work_db, &comment.id) {
                tracing::warn!(
                    comment_id = %comment.id,
                    err = %err,
                    "answer-agent spawn: failed to compensate 'answering' back after run-row creation failure",
                );
            }
            return;
        }
    };

    let execution = match work_db.create_answer_agent_execution(&comment.id, repo_remote_url) {
        Ok(execution) => execution,
        Err(err) => {
            tracing::error!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent spawn: comment flipped to 'answering' and run row created, but execution creation failed",
            );
            if let Err(err) = work_db.complete_answer_agent_run(
                &run.id,
                ANSWER_AGENT_RUN_STATUS_FAILED,
                None,
                Some("spawn_execution_create_failed"),
            ) {
                tracing::warn!(
                    comment_id = %comment.id,
                    run_id = %run.id,
                    err = %err,
                    "answer-agent spawn: failed to mark the orphaned run row 'failed' after execution creation failure",
                );
            } else {
                crate::answer_agent_observability::record_failed(
                    &server_state.metrics,
                    "spawn_execution_create_failed",
                );
            }
            if let Err(err) = compensate(work_db, &comment.id) {
                tracing::warn!(
                    comment_id = %comment.id,
                    err = %err,
                    "answer-agent spawn: failed to compensate 'answering' back after execution creation failure",
                );
            }
            return;
        }
    };

    if let Err(err) = work_db.bind_answer_agent_run_execution(&run.id, &execution.id) {
        tracing::error!(
            comment_id = %comment.id,
            run_id = %run.id,
            execution_id = %execution.id,
            err = %err,
            "answer-agent spawn: execution was created but could not be bound to its run",
        );
        if let Err(complete_err) = work_db.complete_answer_agent_run(
            &run.id,
            ANSWER_AGENT_RUN_STATUS_FAILED,
            None,
            Some("spawn_execution_bind_failed"),
        ) {
            tracing::warn!(
                comment_id = %comment.id,
                run_id = %run.id,
                err = %complete_err,
                "answer-agent spawn: failed to mark the unbound run 'failed'",
            );
        } else {
            crate::answer_agent_observability::record_failed(&server_state.metrics, "spawn_execution_bind_failed");
        }
        if let Err(cancel_err) = work_db.cancel_execution(&execution.id) {
            tracing::warn!(
                comment_id = %comment.id,
                execution_id = %execution.id,
                err = %cancel_err,
                "answer-agent spawn: failed to cancel the unbound execution",
            );
        }
        if let Err(compensate_err) = compensate(work_db, &comment.id) {
            tracing::warn!(
                comment_id = %comment.id,
                err = %compensate_err,
                "answer-agent spawn: failed to compensate 'answering' after execution-bind failure",
            );
        }
        return;
    }

    crate::answer_agent_observability::record_enqueued(&server_state.metrics);
    let pool = server_state.execution_coordinator.attributed_pool_label(&execution);
    let dispatch_class = work_db
        .classify_work_item_for_dispatch(&execution.work_item_id)
        .unwrap_or(crate::work::DispatchClass::OtherWork);
    tracing::info!(
        comment_id = %comment.id,
        run_id = %run.id,
        execution_id = %execution.id,
        intent_confidence = ?comment.intent_confidence,
        pool,
        dispatch_class = dispatch_class.as_ordinal(),
        dispatch_class_label = dispatch_class.label(),
        "answer-agent enqueued",
    );

    server_state.execution_coordinator.kick();

    // Publish over `comment_topic` so the sidebar sees `status = 'answering'`
    // and renders the thinking indicator — no separate indicator channel.
    publish_comment_invalidation(
        server_state,
        session_id,
        request_id,
        &comment.artifact_kind,
        &comment.artifact_id,
        invalidation_reason,
    )
    .await;
}

/// Operator-authored reply in a bucket-2 comment's thread (P3c "Follow-up
/// reclassification loop" of `comment-triggered-document-revisions.md`).
/// Only valid while the comment is `answered`. Appends the
/// `entry_kind = 'operator_followup'` thread entry, transitions the comment
/// `answered → awaiting_followup`, and — off the request's critical path,
/// mirroring `CommentsCreate`'s classifier dispatch — reclassifies the
/// follow-up with the accumulated thread as context.
pub(super) async fn handle_comments_post_followup(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsPostFollowup {
        comment_id,
        body,
        author,
    } = req
    else {
        unreachable!()
    };
    if body.trim().is_empty() {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: "follow-up body may not be empty".to_owned(),
            },
        );
        return;
    }

    // Guarded status flip first (cheapest failure mode, no side effects to
    // unwind yet): only proceed once we know this comment is actually
    // eligible for a follow-up. In particular this rejects a follow-up
    // arriving while a prior run is still `answering` (design
    // §"Concurrency/idempotency" describes queuing that case rather than
    // rejecting it; not yet implemented — the operator sees a WorkError and
    // can retry once the in-flight run completes).
    let comment = match work_db.transition_comment_to_awaiting_followup(&comment_id) {
        Ok(comment) => comment,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };

    if let Err(err) = work_db.create_comment_thread_entry(
        &comment_id,
        THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP,
        &author,
        &body,
        None,
        None,
    ) {
        // The transition already succeeded; surfacing this as a hard error
        // would tempt the caller into a (now-guarded, hence failing) retry.
        // Log loudly and continue — the thread entry is an audit/UI
        // artifact, not load-bearing for reclassification, which is passed
        // the follow-up body directly rather than re-reading it back.
        tracing::error!(
            comment_id = %comment_id,
            err = %err,
            "CommentsPostFollowup: comment transitioned but failed to create the operator_followup thread entry",
        );
    }

    let revision = publish_comment_invalidation(
        &server_state,
        &session_id,
        &request_id,
        &comment.artifact_kind,
        &comment.artifact_id,
        "comment_followup_posted",
    )
    .await;
    send_response_with_revision(
        &sink,
        &request_id,
        revision,
        FrontendEvent::CommentResult {
            comment: comment.clone(),
        },
    );

    // Reclassification is NOT on the request's critical path, mirroring
    // `CommentsCreate`'s classifier dispatch.
    spawn_followup_classifier(&server_state, &work_db, &session_id, &request_id, &comment, &body);
}

/// Spawn the detached reclassification call for an operator's follow-up
/// reply (P3c). Failures (no API key, transport, malformed reply) are
/// logged and swallowed — the comment simply stays `awaiting_followup` with
/// no retry in this phase, mirroring `spawn_comment_classifier`'s failure
/// handling for the top-level classifier.
fn spawn_followup_classifier(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &WorkComment,
    followup_body: &str,
) {
    let call = match server_state.utility_model.resolve(UtilityTask::CommentIntent) {
        Ok(call) => call,
        Err(err) => {
            tracing::debug!(
                comment_id = %comment.id,
                %err,
                "follow-up classifier: no utility-model credential; comment stays in awaiting_followup state",
            );
            return;
        }
    };
    let server_state = server_state.clone();
    let work_db = work_db.clone();
    let session_id = session_id.to_owned();
    let request_id = request_id.to_owned();
    let comment = comment.clone();
    let followup_body = followup_body.to_owned();

    boss_task_scope::spawn(async move {
        let thread = work_db.list_comment_thread_entries(&comment.id).unwrap_or_default();
        let result = crate::comment_classifier::classify_followup(
            &call,
            &comment.body,
            &comment.anchor,
            &thread,
            &followup_body,
            crate::comment_classifier::ClassifierSubject::from_artifact_kind(&comment.artifact_kind),
        )
        .await;
        let classification = match result {
            Ok(classification) => classification,
            Err(err) => {
                tracing::warn!(
                    comment_id = %comment.id,
                    err = %err,
                    "follow-up classifier: classification call failed; comment stays in awaiting_followup state",
                );
                return;
            }
        };

        if let Err(err) =
            work_db.reclassify_comment_intent(&comment.id, &classification.intent, classification.confidence)
        {
            tracing::warn!(
                comment_id = %comment.id,
                err = %err,
                "follow-up classifier: failed to persist the reclassification",
            );
            return;
        }

        match classification.intent.as_str() {
            INTENT_QUESTION => {
                // Loop back into bucket 2: `awaiting_followup → answering`,
                // the answer agent runs again with the accumulated thread.
                respawn_answer_agent_for_followup(&server_state, &work_db, &session_id, &request_id, &comment).await;
            }
            INTENT_REVISION => {
                // The bucket-1&3 bridge: `awaiting_followup → active`, so
                // the next `[Revise]` batch picks this comment up. The
                // thread's answer-agent reply is carried into that batch's
                // directive by `revise_doc::push_comment_directive_block`
                // (shared by the document and guide composers) reading the
                // comment's thread/latest run directly — no extra column
                // needed here (design §"Bridging a bucket-2 answer into a
                // revision").
                if let Err(err) = work_db.transition_comment_awaiting_followup_to_active(&comment.id) {
                    tracing::warn!(
                        comment_id = %comment.id,
                        err = %err,
                        "follow-up classifier: failed to bridge 'awaiting_followup' into 'active'",
                    );
                    return;
                }
                publish_comment_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    &comment.artifact_kind,
                    &comment.artifact_id,
                    "comment_followup_bridged",
                )
                .await;
            }
            other => tracing::warn!(
                comment_id = %comment.id,
                intent = other,
                "follow-up classifier: returned an intent with no routing case",
            ),
        }
    });
}

pub(super) async fn handle_comments_list(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsList {
        artifact_kind,
        artifact_id,
        include_resolved,
    } = req
    else {
        unreachable!()
    };
    // Fold the bossctl `comments list` pr_doc slug resolution into the
    // engine so CLI and app callers can pass `owner/repo` in the
    // composite key the same way humans write it.
    let artifact_id = if artifact_kind == "pr_doc" {
        match work_db.resolve_pr_doc_artifact_id(&artifact_id) {
            Ok(resolved) => resolved,
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        }
    } else {
        artifact_id
    };
    match work_db.list_comments_with_thread(&artifact_kind, &artifact_id, include_resolved) {
        Ok(comments) => send_response_with_revision(
            &sink,
            &request_id,
            server_state.current_work_revision(),
            FrontendEvent::CommentsList {
                artifact_kind,
                artifact_id,
                comments,
            },
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

/// Read-only: one comment + thread + answer-agent-run history. Backs
/// `boss comment show` (folds the bossctl direct-DB path onto the engine
/// RPC surface so the coordinator never needs raw sqlite3).
pub(super) async fn handle_comments_get(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsGet { comment_id } = req else {
        unreachable!()
    };
    match load_comment_detail(&work_db, &comment_id) {
        Ok(Some((comment, thread_entries, answer_agent_runs))) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::CommentsGetResult {
                    comment,
                    thread_entries,
                    answer_agent_runs,
                },
            );
        }
        Ok(None) => send_work_error(&sink, &request_id, anyhow::anyhow!("unknown comment: {comment_id}")),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

/// Read-only: every `answer_agent_runs` row for a comment. Backs
/// `boss comment runs`.
pub(super) async fn handle_list_answer_agent_runs(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListAnswerAgentRuns { comment_id } = req else {
        unreachable!()
    };
    match work_db.get_comment(&comment_id) {
        Ok(None) => {
            send_work_error(&sink, &request_id, anyhow::anyhow!("unknown comment: {comment_id}"));
            return;
        }
        Ok(Some(_)) => {}
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    }
    match work_db.list_answer_agent_runs_for_comment(&comment_id) {
        Ok(answer_agent_runs) => send_response(
            &sink,
            &request_id,
            FrontendEvent::AnswerAgentRunsList {
                comment_id,
                answer_agent_runs,
            },
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

/// Comment + thread + answer-agent-run history loaded for [`handle_comments_get`].
type CommentDetail = (WorkComment, Vec<CommentThreadEntry>, Vec<AnswerAgentRun>);

/// Shared load path for [`handle_comments_get`]: comment row + thread +
/// answer-agent runs. Returns `Ok(None)` when the comment id is unknown.
fn load_comment_detail(work_db: &WorkDb, comment_id: &str) -> Result<Option<CommentDetail>> {
    let Some(comment) = work_db.get_comment(comment_id)? else {
        return Ok(None);
    };
    let thread_entries = work_db.list_comment_thread_entries(comment_id)?;
    let answer_agent_runs = work_db.list_answer_agent_runs_for_comment(comment_id)?;
    Ok(Some((comment, thread_entries, answer_agent_runs)))
}

/// Reply to `comments_banner_state` — a read-only `[Revise]`-banner summary,
/// so a client can render the banner without loading every comment.
pub(super) async fn handle_comments_banner_state(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsBannerState {
        artifact_kind,
        artifact_id,
    } = req
    else {
        unreachable!()
    };
    match work_db.comments_banner_state(&artifact_kind, &artifact_id) {
        Ok(state) => send_response_with_revision(
            &sink,
            &request_id,
            server_state.current_work_revision(),
            FrontendEvent::CommentsBannerState {
                artifact_kind,
                artifact_id,
                state,
            },
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_comments_resolve(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsResolve {
        guide_version_id,
        artifact_kind,
        artifact_id,
        plain_text,
        plain_text_projection_version,
    } = req
    else {
        unreachable!()
    };
    {
        let config = crate::comments_anchor::CommentFuzzyConfig::from_env();
        let result = if artifact_kind == "pr_review_guide" {
            work_db.resolve_guide_comments(&artifact_id, guide_version_id.as_deref(), &plain_text, &config)
        } else {
            work_db.resolve_comments(
                &artifact_kind,
                &artifact_id,
                &plain_text,
                plain_text_projection_version,
                &config,
            )
        };
        match result {
            Ok(comments) => send_response_with_revision(
                &sink,
                &request_id,
                server_state.current_work_revision(),
                FrontendEvent::CommentsResolved {
                    artifact_kind,
                    artifact_id,
                    comments,
                },
            ),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

/// Batch-address every unaddressed `revision` comment on a
/// design/investigation-owned `pr_doc` artifact or a `pr_review_guide`
/// series: creates a revision (open PR) or, for documents only, a chore
/// (merged/closed/no-PR) — the `[Revise]` / **Revise PR** banner action.
/// App-or-Boss tier — replaces the retired magic-wand dispatch path.
/// Design: `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// §"Buckets 1 & 3" and `automatic-pr-review-guides.md`
/// §"Comments target the implementation".
pub(super) async fn handle_comments_revise_doc(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::CommentsReviseDoc { input } = req else {
        unreachable!()
    };
    {
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                artifact_id = %input.artifact_id,
                "comments_revise_doc rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "comments_revise_doc requires app or Boss authority".to_owned(),
                },
            );
            return;
        }

        let artifact_kind = input.artifact_kind.clone();
        let artifact_id = input.artifact_id.clone();
        match work_db.revise_doc(input, &GhPrStateChecker) {
            Ok(outcome) => {
                let revision = if let ReviseDocOutcome::Created { ref task_id, .. } = outcome {
                    let product_id = work_db
                        .get_work_item(task_id)
                        .ok()
                        .map(|item| item.product_id().to_string());
                    let work_revision = publish_work_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        product_id
                            .as_deref()
                            .map(|id| vec![work_product_topic(id)])
                            .unwrap_or_default(),
                        "doc_comment_revise_created",
                        product_id.clone(),
                        vec![task_id.clone()],
                    )
                    .await;
                    let comment_revision = publish_comment_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        &artifact_kind,
                        &artifact_id,
                        "comment_revise_doc",
                    )
                    .await;
                    comment_revision.max(work_revision)
                } else {
                    server_state.current_work_revision()
                };
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::CommentsReviseDocResult { outcome },
                );
            }
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

/// Shared tail for the comment-mutation handlers that follow the same shape:
/// take the single-comment result of a `work_db` mutation, and on `Ok` publish
/// a comment invalidation with `reason` before sending the `CommentResult`
/// response, or on `Err` send a work error. Keeps `handle_comments_dismiss`,
/// `handle_comments_set_status`, and `handle_comments_set_intent` from
/// duplicating the identical publish + response boilerplate.
async fn respond_comment_invalidation(
    server_state: &ServerState,
    session_id: &str,
    request_id: &str,
    sink: &SessionSink,
    result: anyhow::Result<WorkComment>,
    reason: &str,
) {
    match result {
        Ok(comment) => {
            let revision = publish_comment_invalidation(
                server_state,
                session_id,
                request_id,
                &comment.artifact_kind,
                &comment.artifact_id,
                reason,
            )
            .await;
            send_response_with_revision(sink, request_id, revision, FrontendEvent::CommentResult { comment });
        }
        Err(err) => send_work_error(sink, request_id, &err),
    }
}

pub(super) async fn handle_comments_dismiss(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsDismiss { comment_id, actor } = req else {
        unreachable!()
    };
    let result = work_db.dismiss_comment(&comment_id, actor.as_deref());
    end_answer_agent_on_thread_terminal(&server_state, &work_db, &result).await;
    respond_comment_invalidation(
        &server_state,
        &session_id,
        &request_id,
        &sink,
        result,
        "comment_dismissed",
    )
    .await;
}

pub(super) async fn handle_comments_set_status(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsSetStatus {
        comment_id,
        status,
        actor,
    } = req
    else {
        unreachable!()
    };
    let result = work_db.set_comment_status(&comment_id, &status, actor.as_deref());
    end_answer_agent_on_thread_terminal(&server_state, &work_db, &result).await;
    respond_comment_invalidation(
        &server_state,
        &session_id,
        &request_id,
        &sink,
        result,
        "comment_status_changed",
    )
    .await;
}

/// Manually reclassify a comment's intent (the sidebar badge's override
/// control), and **re-home** it: switching a comment's classification must
/// land it exactly where it would have been had it been classified that way
/// to begin with. `intent` alone is only the label; `status` is the field
/// every consumer actually gates on (`revisable_comment_predicate`), and the
/// intent badge is what the sidebar surfaces the classification with — so
/// both have to move together.
///
/// The directed transitions, by the comment's status *before* the override:
///
/// | before | new intent | what happens |
/// |---|---|---|
/// | `answering` | `revision` | stand the live run down (`superseded`) and cancel + release its execution *before* the override write, then `→ active` |
/// | `answered`/`answer_failed`/`awaiting_followup` | `revision` | `→ active`; the posted answer stays in the thread and rides into the revise directive as context |
/// | `active` | `revision` | no status change needed |
/// | `active` | `question` | spawn the answer agent → `answering` |
/// | any | same kind as before | label only — a live run keeps running, an answered thread stays answered |
/// | `in_revision` / `resolved` / `dismissed` / `orphaned` | anything | **refused** before any write — see [`refuse_comment_reclassification`] |
///
/// A refusal is never a silent no-op: the reply is a `WorkError` carrying
/// the reason, correlated to the request so the picker that sent it can
/// revert and show the reason inline. `in_revision` in particular used to be
/// accepted as a label-only write (see [`WorkDb::override_comment_intent`]
/// for why the claim itself must not move); that read as "nothing happened"
/// because nothing routed, so it is now refused with the claiming task
/// named instead.
///
/// The `answering`-run stand-down runs *before* the `override_comment_intent`
/// write (see `stand_down_answer_agent`'s doc for why); everything else is
/// best-effort relative to that write — a failure to stand a run down is
/// logged and the response still goes out. The answer-agent spawn is awaited
/// rather than detached because `spawn_answer_agent` is itself only
/// bookkeeping plus a coordinator kick — the agent runs out-of-process.
pub(super) async fn handle_comments_set_intent(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsSetIntent { comment_id, intent } = req else {
        unreachable!()
    };

    // The pre-override row decides both admission and routing; read it
    // before the write (which may itself move `status` to `active`).
    let before = match work_db.get_comment(&comment_id) {
        Ok(Some(comment)) => comment,
        Ok(None) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("unknown comment: {comment_id}"),
                },
            );
            return;
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };

    if let Some(reason) = refuse_comment_reclassification(&before, &intent) {
        tracing::info!(
            comment_id,
            status = %before.status,
            requested_intent = %intent,
            %reason,
            "comment reclassification refused",
        );
        send_response(&sink, &request_id, FrontendEvent::WorkError { message: reason });
        return;
    }

    // Stand the live run down *before* the override write: if this happened
    // after, a `CommentsPostAnswer` landing in between would still see a
    // `running` run, complete it `replied`, and feed the retracted question's
    // answer into the revise directive. Standing it down first means the
    // reply can only win this race if it genuinely beat the reclassification.
    if before.status == COMMENT_STATUS_ANSWERING && intent.as_str() == INTENT_REVISION {
        stand_down_answer_agent(&server_state, &work_db, &comment_id).await;
    }

    let overridden = match work_db.override_comment_intent(&comment_id, &intent) {
        Ok(comment) => comment,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };

    rehome_reclassified_comment(&server_state, &work_db, &session_id, &request_id, &overridden).await;

    // Re-read: the routing side effects above may have moved `status` again
    // (a `question` override spawns the answer agent → `answering`), and the
    // response must carry the comment's settled state, not a stale snapshot.
    let comment = work_db.get_comment(&comment_id).ok().flatten().unwrap_or(overridden);
    respond_comment_invalidation(
        &server_state,
        &session_id,
        &request_id,
        &sink,
        Ok(comment),
        "comment_intent_overridden",
    )
    .await;
}

/// Why a manual reclassification cannot be honoured, or `None` when it can.
///
/// Decided before any write, so a refused gesture leaves the row exactly as
/// it was — label included — and the reply can say why instead of echoing an
/// unchanged comment. The reasons are operator-facing: the picker shows them
/// inline next to the kind it reverted to.
///
/// - `in_revision`: the kind of a claimed comment is owned by the revision
///   addressing it. Its directive was assembled once, at claim time, and is
///   immutable, so neither track can take the comment over until that task
///   finishes (the comment resolves) or is abandoned (reconciliation reopens
///   it `active`, at which point the picker works again).
/// - `resolved` / `dismissed`: terminal threads route nowhere; the operator
///   reopens first.
/// - `orphaned`: a question needs an anchor to be answered against and a
///   revision needs one to be addressed; neither can start until the
///   comment re-anchors.
fn refuse_comment_reclassification(comment: &WorkComment, intent: &str) -> Option<String> {
    if intent != INTENT_REVISION && intent != INTENT_QUESTION {
        return Some(format!("invalid comment intent: {intent}"));
    }
    match comment.status.as_str() {
        COMMENT_STATUS_IN_REVISION => Some(match comment.revise_task_id.as_deref() {
            Some(task_id) => format!(
                "Already being addressed by revision {task_id}. Its kind can change once that revision finishes or is abandoned."
            ),
            None => {
                "Already being addressed by a revision. Its kind can change once that revision finishes or is abandoned."
                    .to_owned()
            }
        }),
        COMMENT_STATUS_RESOLVED => Some("This comment is resolved. Reopen it before changing its kind.".to_owned()),
        COMMENT_STATUS_DISMISSED => Some("This comment was dismissed; its kind can no longer be changed.".to_owned()),
        COMMENT_STATUS_ORPHANED => Some(
            "This comment's anchor was lost in the current document. It can't be routed until it re-anchors."
                .to_owned(),
        ),
        _ => None,
    }
}

/// Run the routing side effects for a manual reclassification that don't have
/// to precede the override write. `status_before` is the comment's status
/// prior to [`WorkDb::override_comment_intent`]'s write; `comment` is the row
/// as that write left it. The `answering`-run stand-down runs earlier, in
/// `handle_comments_set_intent`, before the override write itself.
async fn rehome_reclassified_comment(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &WorkComment,
) {
    match comment.intent.as_deref() {
        // The `active` guard is load-bearing: `transition_comment_to_answering`
        // only accepts that source status, and re-affirming `question` on a
        // comment already `answering`/`answered`/`awaiting_followup` (or one
        // claimed `in_revision`) must not start a second run.
        Some(INTENT_QUESTION) if comment.status == COMMENT_STATUS_ACTIVE => {
            spawn_answer_agent(server_state, work_db, session_id, request_id, comment).await;
        }
        _ => {}
    }
}

/// Stand down the answer-agent run the operator just retracted by
/// reclassifying its comment away from `question`.
///
/// Two steps, in this order:
///
/// 1. Supersede the `answer_agent_runs` row. This is the durable half — it is
///    what stops a late `boss comment reply` from resurrecting `answering`,
///    and it holds even if the cancel below fails or the agent's process
///    outlives it.
/// 2. Cancel and release the bound `answer_agent` execution, so the worker
///    slot, pane, and cube workspace lease are reclaimed rather than spent
///    finishing an answer nobody will read. `cancel_execution` refuses an
///    already-terminal row, which is an expected outcome here (the agent may
///    have finished between the read and this call), so it is logged at debug.
///
/// This runs *before* [`WorkDb::override_comment_intent`]'s write, so the run
/// is superseded while the comment is still `answering` — a `CommentsPostAnswer`
/// racing the reclassification can only complete the run `replied` if it
/// genuinely beat this call, not the override write.
async fn stand_down_answer_agent(server_state: &Arc<ServerState>, work_db: &Arc<WorkDb>, comment_id: &str) {
    match work_db.stand_down_answer_agent_run(comment_id) {
        Ok(Some(run)) => {
            crate::answer_agent_observability::record_superseded(&server_state.metrics, "reclassified");
            tracing::info!(
                comment_id,
                run_id = %run.id,
                "comment reclassified away from 'question': answer-agent run superseded",
            );
        }
        Ok(None) => tracing::debug!(
            comment_id,
            "comment reclassified away from 'question': no running answer-agent run to stand down",
        ),
        Err(err) => tracing::warn!(
            comment_id,
            err = %err,
            "comment reclassified away from 'question': failed to supersede the running answer-agent run",
        ),
    }

    let execution = match work_db.live_answer_agent_execution_for_comment(comment_id) {
        Ok(Some(execution)) => execution,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(
                comment_id,
                err = %err,
                "comment reclassified away from 'question': failed to look up the bound answer-agent execution",
            );
            return;
        }
    };

    if let Err(err) = work_db.cancel_execution(&execution.id) {
        tracing::debug!(
            comment_id,
            execution_id = %execution.id,
            err = %err,
            "comment reclassified away from 'question': answer-agent execution was already terminal",
        );
        return;
    }
    tracing::info!(
        comment_id,
        execution_id = %execution.id,
        "comment reclassified away from 'question': answer-agent execution cancelled",
    );

    // Pane releases are keyed by run_id (the slot registry's key), then a
    // final pass by execution_id so the cube lease recorded on the execution
    // row is released even when there was no active run — same shape as
    // `handle_cancel_execution`.
    let active_runs = work_db.active_run_ids_for_execution(&execution.id).unwrap_or_default();
    let handler = server_state.completion_handler.clone();
    let execution_id = execution.id.clone();
    boss_task_scope::spawn(async move {
        for run_id in active_runs {
            handler.force_release(&run_id).await;
        }
        handler.force_release(&execution_id).await;
    });
}

/// Tear the answer agent down when its comment thread reaches a status that
/// means nobody is waiting for the answer any more (`resolved` / `dismissed`).
///
/// Without this, resolving a thread is invisible to the execution bound to it:
/// the agent keeps its pane, worker slot and cube lease with nothing left to
/// do, and (before the events-socket driver-resolution fix in
/// [`WorkDb::get_execution_driver_slug`]) with no Stop it could ever act on.
/// The observed run sat `waiting_human` for 39 minutes *after* its thread was
/// resolved. Resolution is the operator's terminal signal for the thread, so it
/// must be terminal for the execution too.
///
/// `result` is the status write's own outcome: a failed write means the thread
/// did NOT reach a terminal status, so nothing is stood down. Statuses that
/// leave the thread live (`active` — a reopen) are likewise ignored; only the
/// two terminal ones tear anything down.
///
/// Delegates to [`stand_down_answer_agent`], which supersedes a still-`running`
/// run, cancels the bound execution, and force-releases its pane + lease — so
/// the execution row and the pool slot move together. A comment with no live
/// answer-agent execution (the overwhelmingly common case) costs one indexed
/// lookup and does nothing.
async fn end_answer_agent_on_thread_terminal(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    result: &anyhow::Result<WorkComment>,
) {
    let Ok(comment) = result else { return };
    if !matches!(
        comment.status.as_str(),
        COMMENT_STATUS_RESOLVED | COMMENT_STATUS_DISMISSED
    ) {
        return;
    }
    stand_down_answer_agent(server_state, work_db, &comment.id).await;
}

/// Worker-callable: record a grounded per-comment outcome for a review-guide
/// revision (`boss comment guide-outcome`). `run_id` is the caller's own
/// `BOSS_RUN_ID` — resolved to its bound `RevisionImplementation` execution,
/// then to that execution's work item, which must be the revision that
/// claimed `comment_id` (`WorkDb::record_guide_comment_outcome` enforces the
/// claim). The caller cannot target a comment claimed by a different
/// revision. On success: upserts the `guide_comment_outcomes` disposition,
/// writes the revision's single `entry_kind = 'answer'` thread entry
/// (updating it in place on a re-record), and resolves the comment once
/// its owning revision has been delivered successfully (`in_review`, or
/// `done`/`archived`).
/// No `authorize_rpc` gate — worker-callable RPCs (like
/// `CreateAutomationTask`) run without a special tier, matching that
/// precedent.
pub(super) async fn handle_comments_record_guide_outcome(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsRecordGuideOutcome {
        run_id,
        comment_id,
        disposition,
        body,
        request_regeneration,
    } = req
    else {
        unreachable!()
    };
    if body.trim().is_empty() {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: "guide outcome body may not be empty".to_owned(),
            },
        );
        return;
    }
    let execution = match work_db.get_execution(&run_id) {
        Ok(execution) => execution,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };
    if execution.kind != ExecutionKind::RevisionImplementation {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: format!("run '{run_id}' is not a revision execution"),
            },
        );
        return;
    }
    match work_db.record_guide_comment_outcome(
        &execution.work_item_id,
        boss_protocol::GuideCommentOutcome::builder()
            .comment_id(comment_id)
            .disposition(disposition)
            .response(body)
            .request_regeneration(request_regeneration)
            .build(),
    ) {
        Ok(recorded) => {
            match recorded.regeneration {
                Some(Ok(crate::work::RetryReviewGuideOutcome::Created(_))) => {
                    if let Some(root) = work_db
                        .root_task_id_for_review_guide_series(&recorded.comment.artifact_id)
                        .ok()
                        .flatten()
                    {
                        crate::work::notify_review_guide_changed(
                            &work_db,
                            &server_state.publisher,
                            &root,
                            "review_guide_retry_queued",
                        )
                        .await;
                    }
                }
                Some(Ok(crate::work::RetryReviewGuideOutcome::AlreadyRequested(_))) => {
                    tracing::info!(
                        comment_id = %recorded.comment.id,
                        series_id = %recorded.comment.artifact_id,
                        "guide outcome regeneration already queued for this revision"
                    );
                }
                Some(Ok(crate::work::RetryReviewGuideOutcome::NoComparison)) => {
                    tracing::warn!(
                        comment_id = %recorded.comment.id,
                        series_id = %recorded.comment.artifact_id,
                        "guide outcome requested regeneration but no source comparison exists"
                    );
                }
                Some(Err(err)) => {
                    // The outcome itself is already durably recorded (see
                    // `RecordedGuideCommentOutcome::regeneration`'s doc) —
                    // only the regeneration retry failed. Still publish the
                    // comment invalidation and reply with success below;
                    // surfacing this as a `WorkError` would tell the caller
                    // the whole request failed when the disposition it
                    // asked to record is sitting in the DB. Logged at `warn`
                    // so the partial success is visible in engine logs; the
                    // response has no field to carry it.
                    tracing::warn!(
                        comment_id = %recorded.comment.id,
                        series_id = %recorded.comment.artifact_id,
                        error = %err,
                        "guide comment outcome recorded but its regeneration request failed"
                    );
                }
                None => {}
            }
            let comment = recorded.comment;
            let revision = publish_comment_invalidation(
                &server_state,
                &session_id,
                &request_id,
                &comment.artifact_kind,
                &comment.artifact_id,
                "guide_comment_outcome_recorded",
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::CommentResult { comment });
        }
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

/// Worker-callable: post the answer agent's reply (P3b). `run_id` is the
/// caller's own `BOSS_RUN_ID` — resolved to its bound `answer_agent`
/// execution, then to that execution's comment, then to the comment's
/// currently-`running` `answer_agent_runs` row. The caller cannot target any
/// other comment or run: nothing in the request names one directly (see
/// `boss comment reply`'s security note in `crate::answer_agent`).
///
/// On success: completes the run (`replied`), appends an
/// `entry_kind = 'answer'` thread entry, and transitions the comment
/// `answering → answered`. No `authorize_rpc` gate — worker-callable RPCs
/// (like `CreateAutomationTask`) run without a special tier, matching that
/// precedent.
pub(super) async fn handle_comments_post_answer(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsPostAnswer { run_id, body } = req else {
        unreachable!()
    };
    if body.trim().is_empty() {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: "reply body may not be empty".to_owned(),
            },
        );
        return;
    }

    let execution = match work_db.get_execution(&run_id) {
        Ok(execution) => execution,
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("unknown run '{run_id}': {err}"),
                },
            );
            return;
        }
    };
    if execution.kind != ExecutionKind::AnswerAgent {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: format!("run '{run_id}' is not an answer-agent execution"),
            },
        );
        return;
    }
    let comment_id = execution.work_item_id.clone();

    let run = match work_db.running_answer_agent_run_for_comment(&comment_id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: "no running answer-agent run found for this comment (already replied?)".to_owned(),
                },
            );
            return;
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };

    if let Err(err) = work_db.complete_answer_agent_run(&run.id, ANSWER_AGENT_RUN_STATUS_REPLIED, Some(&body), None) {
        send_work_error(&sink, &request_id, &err);
        return;
    }
    crate::answer_agent_observability::record_replied(&server_state.metrics);

    if let Err(err) = work_db.create_comment_thread_entry(
        &comment_id,
        THREAD_ENTRY_KIND_ANSWER,
        "engine",
        &body,
        None,
        Some(&run.id),
    ) {
        // The run is already `replied` — surfacing this as a hard error would
        // tempt the agent into a (now-guarded, hence failing) retry. Log
        // loudly and continue to the status transition; the thread entry is
        // an audit/UI artifact, not load-bearing for `answering → answered`.
        tracing::error!(
            comment_id = %comment_id,
            run_id = %run.id,
            err = %err,
            "CommentsPostAnswer: run completed but failed to create the thread entry",
        );
    }

    match work_db.transition_comment_to_answered(&comment_id) {
        Ok(comment) => {
            let revision = publish_comment_invalidation(
                &server_state,
                &session_id,
                &request_id,
                &comment.artifact_kind,
                &comment.artifact_id,
                "comment_answered",
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::CommentResult { comment });
        }
        Err(err) => {
            tracing::error!(
                comment_id = %comment_id,
                err = %err,
                "CommentsPostAnswer: run completed and thread entry posted, but failed to \
                 transition the comment to 'answered'",
            );
            send_work_error(&sink, &request_id, &err);
        }
    }
}

pub(super) async fn handle_comments_update_anchor(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsUpdateAnchor {
        comment_id,
        anchor,
        new_doc_version,
        plain_text_projection_version,
    } = req
    else {
        unreachable!()
    };
    match work_db.update_comment_anchor(&comment_id, &anchor, &new_doc_version, plain_text_projection_version) {
        Ok(comment) => {
            let revision = publish_comment_invalidation(
                &server_state,
                &session_id,
                &request_id,
                &comment.artifact_kind,
                &comment.artifact_id,
                "comment_anchor_updated",
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::CommentResult { comment });
        }
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

#[cfg(test)]
#[path = "comments_tests.rs"]
mod tests;
