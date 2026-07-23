//! `FrontendRequest` handlers — markdown-viewer comments.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

use boss_protocol::{INTENT_DIRECTIVE, INTENT_LARGER_CHANGE, THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP};

/// Design § "Buckets 1 & 3 — unified" example nudge text.
const NUDGE_BODY: &str = "This looks like it wants a doc change — click [Revise] to start one.";

pub(super) async fn handle_comments_create(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsCreate { input } = req else {
        unreachable!()
    };
    {
        let artifact_kind = input.artifact_kind.clone();
        let artifact_id = input.artifact_id.clone();
        match work_db.create_comment(input) {
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
                // (comment-triggered-document-revisions.md § "The classifier
                // (P1 — foundation)"): spawn it detached, mirroring the
                // magic-wand dispatch pattern. The comment starts with
                // `intent` NULL — the transient `classifying` state — until
                // this completes and publishes on `comment_topic`.
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

    tokio::spawn(async move {
        let Some(api_key) = crate::comment_classifier::resolve_api_key() else {
            tracing::debug!(
                comment_id = %comment_id,
                "comment intent classifier: no API key configured; recording terminal classification failure",
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
                "no API key configured",
            )
            .await;
            return;
        };

        match crate::comment_classifier::classify(&api_key, &body, &anchor).await {
            Ok(result) => match work_db.set_comment_intent(&comment_id, &result.intent, result.confidence) {
                Ok(classified) => {
                    // Buckets 1&3 (P2b): nudge the operator toward a revision
                    // immediately on classification, before `[Revise]` is
                    // even clicked (design § "Buckets 1 & 3 — unified").
                    if matches!(result.intent.as_str(), INTENT_DIRECTIVE | INTENT_LARGER_CHANGE)
                        && let Err(err) = work_db.create_nudge_thread_entry(&comment_id, NUDGE_BODY)
                    {
                        tracing::warn!(
                            comment_id = %comment_id,
                            err = %err,
                            "comment intent classifier: failed to post nudge thread entry",
                        );
                    }

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
                    // read-only answer agent. Buckets 1&3 (directive/larger_change)
                    // are a later phase — the comment just stays `active` for now.
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
/// Scope guard: only artifacts that `resolve_doc_owner` resolves to a
/// `Design`/`Investigation` task are eligible (design §"Scope guard" — the
/// classifier itself runs unconditionally today, so this is the point where
/// bucket 2 enforces the doc-owner gate before spawning anything). A comment
/// that doesn't resolve — e.g. `artifact_kind = 'work_item'`, or a `pr_doc`
/// whose owning task the resolver can't find — is intentionally left
/// `active` with `intent = question` and no bucket-2 affordance: the same
/// "no comment-driven affordance" outcome the migration section already
/// accepts for `work_item` comments post-magic-wand.
async fn spawn_answer_agent(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &WorkComment,
) {
    let repo_remote_url = match resolve_answer_agent_repo(work_db, comment) {
        AnswerAgentRepoResolution::Resolved(url) => url,
        AnswerAgentRepoResolution::OutOfScope => return,
        AnswerAgentRepoResolution::Failed(error_kind) => {
            record_answer_agent_spawn_failure(server_state, work_db, session_id, request_id, comment, 0, error_kind)
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
        0,
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
/// `resolve_doc_owner`'s owning task. Routes through
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
    let doc_owner = match work_db.resolve_doc_owner(&comment.artifact_kind, &comment.artifact_id) {
        Ok(Some(owner)) => owner,
        Ok(None) => {
            tracing::debug!(
                comment_id = %comment.id,
                artifact_kind = %comment.artifact_kind,
                artifact_id = %comment.artifact_id,
                "answer-agent spawn: comment's artifact has no design/investigation doc owner; skipping (out of bucket-2 scope)",
            );
            return AnswerAgentRepoResolution::OutOfScope;
        }
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent spawn: resolve_doc_owner failed; leaving comment as-is",
            );
            return AnswerAgentRepoResolution::Failed("doc_owner_resolution_failed");
        }
    };

    match work_db.resolve_repo_for_task(&doc_owner.task_id) {
        Ok(Some(url)) => AnswerAgentRepoResolution::Resolved(url),
        Ok(None) => {
            tracing::warn!(
                comment_id = %comment.id,
                task_id = %doc_owner.task_id,
                "answer-agent spawn: doc owner task has no resolvable repo; skipping",
            );
            AnswerAgentRepoResolution::Failed("repo_unresolved")
        }
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                task_id = %doc_owner.task_id,
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

    if let Err(err) = work_db.create_answer_agent_execution(&comment.id, repo_remote_url) {
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
    let Some(api_key) = crate::comment_classifier::resolve_api_key() else {
        tracing::debug!(
            comment_id = %comment.id,
            "follow-up classifier: no API key configured; comment stays in awaiting_followup state",
        );
        return;
    };
    let server_state = server_state.clone();
    let work_db = work_db.clone();
    let session_id = session_id.to_owned();
    let request_id = request_id.to_owned();
    let comment = comment.clone();
    let followup_body = followup_body.to_owned();

    tokio::spawn(async move {
        let thread = work_db.list_comment_thread_entries(&comment.id).unwrap_or_default();
        let result = crate::comment_classifier::classify_followup(
            &api_key,
            &comment.body,
            &comment.anchor,
            &thread,
            &followup_body,
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
            INTENT_DIRECTIVE | INTENT_LARGER_CHANGE => {
                // The bucket-1&3 bridge: `awaiting_followup → active`, so
                // the next `[Revise]` batch picks this comment up. The
                // thread's answer-agent reply is carried into that batch's
                // directive by `compose_doc_comment_directive` reading the
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
                if let Err(err) = work_db.create_nudge_thread_entry(&comment.id, NUDGE_BODY) {
                    tracing::warn!(
                        comment_id = %comment.id,
                        err = %err,
                        "follow-up classifier: failed to post the bridged nudge thread entry",
                    );
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
        match work_db.resolve_comments(
            &artifact_kind,
            &artifact_id,
            &plain_text,
            plain_text_projection_version,
            &config,
        ) {
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

/// Batch-address every unaddressed `directive`/`larger_change` comment on a
/// design/investigation-owned `pr_doc` artifact — the `[Revise]`-banner
/// action. App-or-Boss tier — replaces the retired magic-wand dispatch path.
/// Design: `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// §"Buckets 1 & 3".
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
/// control). `intent_overridden_by = 'user'` is stamped by
/// `override_comment_intent` itself; publishing the comment invalidation is
/// what "re-runs routing from the new intent's entry point" means today —
/// there is no bucket-1&3/bucket-2 routing yet (later phases of
/// comment-triggered-document-revisions.md), so a fresh load of the comment
/// (with its new `intent`) is the whole of "routing" until those land.
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
    let result = work_db.override_comment_intent(&comment_id, &intent);
    respond_comment_invalidation(
        &server_state,
        &session_id,
        &request_id,
        &sink,
        result,
        "comment_intent_overridden",
    )
    .await;
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
mod tests {
    use super::*;

    fn test_server_state() -> (Arc<ServerState>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(temp.path().to_path_buf())
                .db_path(temp.path().join("state.db"))
                .build(),
            None,
        ));
        let state = ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, None, None, None).unwrap();
        (state, temp)
    }

    fn make_session_sink() -> Arc<SessionSink> {
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
        Arc::new(SessionSink::new(shutdown_tx))
    }

    /// Stand up a `question`-classified comment already `answering`, with
    /// its tracking `answer_agent_runs` row (`running`) and its bound
    /// `answer_agent` execution — the state `handle_comments_post_answer`
    /// expects to resolve a reply against. Returns `(comment_id,
    /// execution_id)`.
    fn seed_answering_comment(work_db: &Arc<WorkDb>) -> (String, String) {
        let product = work_db
            .create_product(
                crate::work::CreateProductInput::builder()
                    .name("Boss")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        let comment = work_db
            .create_comment(boss_protocol::CreateCommentInput {
                artifact_id: "pr_doc:git@github.com:spinyfin/mono.git:main:docs/design.md".into(),
                anchor: boss_protocol::CommentAnchor {
                    exact: "the quoted text".into(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                artifact_kind: "pr_doc".into(),
                author: "human".into(),
                body: "What does this mean?".into(),
                doc_version: "v1".into(),
                plain_text_projection_version: 0,
            })
            .unwrap();
        work_db.set_comment_intent(&comment.id, INTENT_QUESTION, 0.9).unwrap();
        work_db.transition_comment_to_answering(&comment.id).unwrap();
        work_db
            .create_answer_agent_run(
                &comment.id,
                &comment.artifact_kind,
                &comment.artifact_id,
                &comment.doc_version,
                0,
            )
            .unwrap();
        let execution = work_db
            .create_answer_agent_execution(&comment.id, &product.repo_remote_url.unwrap())
            .unwrap();
        (comment.id, execution.id)
    }

    fn dispatch_ctx(server_state: &Arc<ServerState>, work_db: &Arc<WorkDb>, sink: &Arc<SessionSink>) -> Dispatch {
        Dispatch::builder()
            .server_state(server_state.clone())
            .work_db(work_db.clone())
            .sink(sink.clone())
            .session_id("session-1")
            .request_id("req-1")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build()
    }

    #[tokio::test]
    async fn post_answer_replies_and_transitions_comment_to_answered() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        let sink = make_session_sink();
        let (comment_id, execution_id) = seed_answering_comment(&work_db);

        let ctx = dispatch_ctx(&server_state, &work_db, &sink);
        handle_comments_post_answer(
            ctx,
            FrontendRequest::CommentsPostAnswer {
                run_id: execution_id.clone(),
                body: "Here's the answer.".to_owned(),
            },
        )
        .await;

        let comment = work_db
            .get_comment(&comment_id)
            .unwrap()
            .expect("comment should still exist");
        assert_eq!(comment.status, "answered");
        let entries = work_db.list_comment_thread_entries(&comment_id).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "Here's the answer.");
        assert!(
            work_db
                .running_answer_agent_run_for_comment(&comment_id)
                .unwrap()
                .is_none(),
            "the run must no longer be 'running' after a reply is posted",
        );

        // The success response should have been enqueued, not a WorkError.
        let envelope = sink
            .next()
            .await
            .expect("a response envelope should have been enqueued");
        assert!(
            matches!(envelope.payload, FrontendEvent::CommentResult { .. }),
            "expected CommentResult, got {:?}",
            envelope.payload
        );
    }

    #[tokio::test]
    async fn post_answer_rejects_duplicate_reply_on_the_same_run() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        let sink = make_session_sink();
        let (comment_id, execution_id) = seed_answering_comment(&work_db);

        // First reply succeeds and completes the run.
        handle_comments_post_answer(
            dispatch_ctx(&server_state, &work_db, &sink),
            FrontendRequest::CommentsPostAnswer {
                run_id: execution_id.clone(),
                body: "First answer.".to_owned(),
            },
        )
        .await;
        let _ = sink.next().await;

        // A second reply against the same (now-completed) run must be
        // rejected rather than silently double-transitioning the comment.
        handle_comments_post_answer(
            dispatch_ctx(&server_state, &work_db, &sink),
            FrontendRequest::CommentsPostAnswer {
                run_id: execution_id,
                body: "Second answer.".to_owned(),
            },
        )
        .await;

        let envelope = sink
            .next()
            .await
            .expect("a response envelope should have been enqueued");
        assert!(
            matches!(envelope.payload, FrontendEvent::WorkError { .. }),
            "expected WorkError for a duplicate reply, got {:?}",
            envelope.payload
        );
        let entries = work_db.list_comment_thread_entries(&comment_id).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "the duplicate call must not post a second thread entry"
        );
    }

    #[tokio::test]
    async fn post_answer_rejects_a_non_answer_agent_execution() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        let sink = make_session_sink();
        let (_comment_id, _execution_id) = seed_answering_comment(&work_db);

        // A `run_id` pointing at some other kind of execution (e.g. a
        // regular chore run) must be rejected up front — this RPC is only
        // ever valid for an `answer_agent` execution.
        let product = work_db
            .create_product(
                crate::work::CreateProductInput::builder()
                    .name("Other")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        let chore = work_db
            .create_chore(
                crate::work::CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Unrelated chore")
                    .build(),
            )
            .unwrap();
        let other_execution = work_db
            .create_execution(
                crate::work::CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(crate::work::ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        handle_comments_post_answer(
            dispatch_ctx(&server_state, &work_db, &sink),
            FrontendRequest::CommentsPostAnswer {
                run_id: other_execution.id,
                body: "irrelevant".to_owned(),
            },
        )
        .await;

        let envelope = sink
            .next()
            .await
            .expect("a response envelope should have been enqueued");
        assert!(
            matches!(envelope.payload, FrontendEvent::WorkError { .. }),
            "expected WorkError for a non-answer-agent run_id, got {:?}",
            envelope.payload
        );
    }

    // --- Follow-up reclassification loop (P3c) ---

    /// Stand up a `question`-classified comment already `answered` — the
    /// state `handle_comments_post_followup` expects an operator reply
    /// against. No `answer_agent_runs`/execution rows are needed: the
    /// `answering → answered` transition only guards on `work_comments`
    /// status.
    fn seed_answered_comment(work_db: &Arc<WorkDb>) -> String {
        let comment = work_db
            .create_comment(boss_protocol::CreateCommentInput {
                artifact_id: "t1".into(),
                anchor: boss_protocol::CommentAnchor {
                    exact: "the quoted text".into(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                artifact_kind: "work_item".into(),
                author: "human".into(),
                body: "What does this mean?".into(),
                doc_version: "v1".into(),
                plain_text_projection_version: 0,
            })
            .unwrap();
        work_db.set_comment_intent(&comment.id, INTENT_QUESTION, 0.9).unwrap();
        work_db.transition_comment_to_answering(&comment.id).unwrap();
        work_db.transition_comment_to_answered(&comment.id).unwrap();
        comment.id
    }

    #[tokio::test]
    async fn post_followup_appends_thread_entry_and_transitions_to_awaiting_followup() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        let sink = make_session_sink();
        let comment_id = seed_answered_comment(&work_db);

        handle_comments_post_followup(
            dispatch_ctx(&server_state, &work_db, &sink),
            FrontendRequest::CommentsPostFollowup {
                comment_id: comment_id.clone(),
                body: "ok, please make that change".to_owned(),
                author: "user:test@example.com".to_owned(),
            },
        )
        .await;

        let comment = work_db
            .get_comment(&comment_id)
            .unwrap()
            .expect("comment should still exist");
        assert_eq!(comment.status, "awaiting_followup");

        let entries = work_db.list_comment_thread_entries(&comment_id).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_kind, THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP);
        assert_eq!(entries[0].body, "ok, please make that change");
        assert_eq!(entries[0].author, "user:test@example.com");

        let envelope = sink
            .next()
            .await
            .expect("a response envelope should have been enqueued");
        assert!(
            matches!(envelope.payload, FrontendEvent::CommentResult { .. }),
            "expected CommentResult, got {:?}",
            envelope.payload
        );
    }

    #[tokio::test]
    async fn post_followup_rejects_empty_body() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        let sink = make_session_sink();
        let comment_id = seed_answered_comment(&work_db);

        handle_comments_post_followup(
            dispatch_ctx(&server_state, &work_db, &sink),
            FrontendRequest::CommentsPostFollowup {
                comment_id: comment_id.clone(),
                body: "   ".to_owned(),
                author: "user:test@example.com".to_owned(),
            },
        )
        .await;

        let comment = work_db.get_comment(&comment_id).unwrap().unwrap();
        assert_eq!(
            comment.status, "answered",
            "an empty reply must not transition the comment"
        );

        let envelope = sink
            .next()
            .await
            .expect("a response envelope should have been enqueued");
        assert!(matches!(envelope.payload, FrontendEvent::WorkError { .. }));
    }

    #[tokio::test]
    async fn post_followup_rejects_a_comment_that_is_not_answered() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        let sink = make_session_sink();
        let comment = work_db
            .create_comment(boss_protocol::CreateCommentInput {
                artifact_id: "t1".into(),
                anchor: boss_protocol::CommentAnchor {
                    exact: "alpha".into(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                artifact_kind: "work_item".into(),
                author: "human".into(),
                body: "a directive comment".into(),
                doc_version: "v1".into(),
                plain_text_projection_version: 0,
            })
            .unwrap();
        // Still 'active' — never went through bucket 2 at all.

        handle_comments_post_followup(
            dispatch_ctx(&server_state, &work_db, &sink),
            FrontendRequest::CommentsPostFollowup {
                comment_id: comment.id.clone(),
                body: "a reply".to_owned(),
                author: "user:test@example.com".to_owned(),
            },
        )
        .await;

        let reloaded = work_db.get_comment(&comment.id).unwrap().unwrap();
        assert_eq!(reloaded.status, "active");
        assert!(work_db.list_comment_thread_entries(&comment.id).unwrap().is_empty());

        let envelope = sink
            .next()
            .await
            .expect("a response envelope should have been enqueued");
        assert!(matches!(envelope.payload, FrontendEvent::WorkError { .. }));
    }

    // --- Answer-agent repo resolution (multi-repo R1: investigation tasks
    // never carry their own `repo_remote_url`; it's inherited from the
    // owning product) ---

    const DOC_REPO: &str = "git@github.com:spinyfin/mono.git";
    const DOC_PATH: &str = "tools/boss/docs/investigations/foo.md";

    /// Stand up a project-less `question`-classified comment owned by a
    /// fresh investigation task, with the doc pointer wired so
    /// `resolve_doc_owner` matches it. `docs_repo` / `repo_remote_url` are
    /// the product-level fields under test — the investigation task itself
    /// is created with no `repo_remote_url` override, mirroring the real
    /// bug report (an investigation task reaching `in_review` with that
    /// column `NULL`).
    fn seed_investigation_question_comment(
        work_db: &Arc<WorkDb>,
        docs_repo: Option<&str>,
        repo_remote_url: Option<&str>,
    ) -> WorkComment {
        let product = work_db
            .create_product(
                crate::work::CreateProductInput::builder()
                    .name("Boss")
                    .maybe_repo_remote_url(repo_remote_url.map(str::to_owned))
                    .maybe_docs_repo(docs_repo.map(str::to_owned))
                    .build(),
            )
            .unwrap();
        let investigation = work_db
            .create_investigation(
                boss_protocol::CreateInvestigationInput::builder()
                    .product_id(product.id.clone())
                    .name("Investigate the thing")
                    .build(),
            )
            .unwrap();
        assert!(
            investigation.repo_remote_url.is_none(),
            "investigation tasks must not carry their own repo_remote_url override",
        );
        work_db
            .set_task_doc_pointer(&investigation.id, Some(DOC_REPO), Some("main"), Some(DOC_PATH))
            .unwrap();

        let artifact_id = format!("pr_doc:{DOC_REPO}:main:{DOC_PATH}");
        let comment = work_db
            .create_comment(boss_protocol::CreateCommentInput {
                artifact_id,
                anchor: boss_protocol::CommentAnchor {
                    exact: "the quoted text".into(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                artifact_kind: "pr_doc".into(),
                author: "human".into(),
                body: "Is comment classification working properly now?".into(),
                doc_version: "v1".into(),
                plain_text_projection_version: 0,
            })
            .unwrap();
        work_db.set_comment_intent(&comment.id, INTENT_QUESTION, 0.95).unwrap();
        work_db.get_comment(&comment.id).unwrap().unwrap()
    }

    #[test]
    fn resolve_answer_agent_repo_falls_back_to_product_docs_repo_for_investigation_task() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        let comment = seed_investigation_question_comment(&work_db, Some(DOC_REPO), None);

        match resolve_answer_agent_repo(&work_db, &comment) {
            AnswerAgentRepoResolution::Resolved(url) => assert_eq!(url, DOC_REPO),
            _ => panic!("expected Resolved({DOC_REPO})"),
        }
    }

    #[test]
    fn resolve_answer_agent_repo_falls_back_to_product_repo_remote_url_when_no_docs_repo() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        // No `docs_repo` configured — the resolver's next fallback is the
        // product's default code repo, exactly like real dispatch does for
        // an investigation-kind task.
        let comment = seed_investigation_question_comment(&work_db, None, Some(DOC_REPO));

        match resolve_answer_agent_repo(&work_db, &comment) {
            AnswerAgentRepoResolution::Resolved(url) => assert_eq!(url, DOC_REPO),
            _ => panic!("expected the product's repo_remote_url fallback to resolve"),
        }
    }

    #[test]
    fn resolve_answer_agent_repo_reports_failed_when_product_has_no_repo_at_all() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        let comment = seed_investigation_question_comment(&work_db, None, None);

        match resolve_answer_agent_repo(&work_db, &comment) {
            AnswerAgentRepoResolution::Failed(error_kind) => assert_eq!(error_kind, "repo_unresolved"),
            _ => panic!("expected Failed(\"repo_unresolved\") when the product has no repo at all"),
        }
    }

    #[tokio::test]
    async fn spawn_answer_agent_records_a_failed_run_and_leaves_the_comment_active_when_repo_unresolved() {
        let (server_state, _dir) = test_server_state();
        let work_db = server_state.work_db.clone();
        let sink = make_session_sink();
        let comment = seed_investigation_question_comment(&work_db, None, None);

        spawn_answer_agent(&server_state, &work_db, "session-1", "req-1", &comment).await;

        // The comment never entered `answering` — the repo never resolved —
        // but the failure left a durable, terminal trace instead of a
        // WARN-and-forget with nothing behind it.
        let reloaded = work_db.get_comment(&comment.id).unwrap().unwrap();
        assert_eq!(reloaded.status, "active");

        let run = work_db
            .latest_answer_agent_run_for_comment(&comment.id)
            .unwrap()
            .expect("a failed run row should have been recorded");
        assert_eq!(run.status, ANSWER_AGENT_RUN_STATUS_FAILED);
        assert_eq!(run.error_kind.as_deref(), Some("repo_unresolved"));

        let with_thread = work_db
            .list_comments_with_thread("pr_doc", &comment.artifact_id, false)
            .unwrap();
        let listed = with_thread
            .iter()
            .find(|c| c.comment.id == comment.id)
            .expect("comment should be listed");
        assert!(
            listed.answer_agent_failed,
            "CommentsList must surface the failed spawn so the app doesn't show an indefinite thinking indicator",
        );
        assert!(!listed.answer_agent_running);

        // The invalidation the failure recorder published should have
        // bumped the work revision (no separate assertion possible without
        // a live subscriber here, but the call must not panic).
        let _ = sink;
    }
}
