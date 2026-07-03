//! `FrontendRequest` handlers — markdown-viewer comments.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

use boss_protocol::{INTENT_DIRECTIVE, INTENT_LARGER_CHANGE};

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
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
        }
    }
}

/// Spawn the detached classifier call for a freshly created top-level
/// comment. Failures (no API key, transport, malformed reply) are logged
/// and swallowed — the comment simply stays in the `classifying` state
/// (`intent IS NULL`) with no retry in this phase.
fn spawn_comment_classifier(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    session_id: &str,
    request_id: &str,
    comment: &WorkComment,
) {
    let Some(api_key) = crate::comment_classifier::resolve_api_key() else {
        tracing::debug!(
            comment_id = %comment.id,
            "comment intent classifier: no API key configured; comment stays in classifying state",
        );
        return;
    };
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
                    "comment intent classifier: classification call failed; comment stays in classifying state",
                );
            }
        }
    });
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
    let doc_owner = match work_db.resolve_doc_owner(&comment.artifact_kind, &comment.artifact_id) {
        Ok(Some(owner)) => owner,
        Ok(None) => {
            tracing::debug!(
                comment_id = %comment.id,
                artifact_kind = %comment.artifact_kind,
                artifact_id = %comment.artifact_id,
                "answer-agent spawn: comment's artifact has no design/investigation doc owner; skipping (out of bucket-2 scope)",
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent spawn: resolve_doc_owner failed; leaving comment active",
            );
            return;
        }
    };

    let repo_remote_url = match work_db.get_work_item(&doc_owner.task_id) {
        Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => match task.repo_remote_url {
            Some(url) if !url.is_empty() => url,
            _ => {
                tracing::warn!(
                    comment_id = %comment.id,
                    task_id = %doc_owner.task_id,
                    "answer-agent spawn: doc owner task has no repo_remote_url; skipping",
                );
                return;
            }
        },
        Ok(_) => {
            tracing::warn!(
                comment_id = %comment.id,
                task_id = %doc_owner.task_id,
                "answer-agent spawn: doc owner did not resolve to a task; skipping",
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                task_id = %doc_owner.task_id,
                err = %err,
                "answer-agent spawn: failed to load doc owner task; skipping",
            );
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

    let run = match work_db.create_answer_agent_run(
        &comment.id,
        &comment.artifact_kind,
        &comment.artifact_id,
        &comment.doc_version,
        0,
    ) {
        Ok(run) => run,
        Err(err) => {
            tracing::error!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent spawn: comment flipped to 'answering' but failed to create the tracking run row",
            );
            if let Err(err) = work_db.transition_comment_answering_to_active(&comment.id) {
                tracing::warn!(
                    comment_id = %comment.id,
                    err = %err,
                    "answer-agent spawn: failed to compensate 'answering' back to 'active' after run-row creation failure",
                );
            }
            return;
        }
    };

    if let Err(err) = work_db.create_answer_agent_execution(&comment.id, &repo_remote_url) {
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
        if let Err(err) = work_db.transition_comment_answering_to_active(&comment.id) {
            tracing::warn!(
                comment_id = %comment.id,
                err = %err,
                "answer-agent spawn: failed to compensate 'answering' back to 'active' after execution creation failure",
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
        "comment_answer_agent_spawned",
    )
    .await;
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
    match work_db.list_comments(&artifact_kind, &artifact_id, include_resolved) {
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
        Err(err) => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: err.to_string(),
            },
        ),
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
        Err(err) => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: err.to_string(),
            },
        ),
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
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
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
                        .map(|item| work_item_product_id(&item));
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
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
        }
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
    {
        match work_db.dismiss_comment(&comment_id, actor.as_deref()) {
            Ok(comment) => {
                let revision = publish_comment_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    &comment.artifact_kind,
                    &comment.artifact_id,
                    "comment_dismissed",
                )
                .await;
                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::CommentResult { comment });
            }
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
        }
    }
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
    match work_db.set_comment_status(&comment_id, &status, actor.as_deref()) {
        Ok(comment) => {
            let revision = publish_comment_invalidation(
                &server_state,
                &session_id,
                &request_id,
                &comment.artifact_kind,
                &comment.artifact_id,
                "comment_status_changed",
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::CommentResult { comment });
        }
        Err(err) => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: err.to_string(),
            },
        ),
    }
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
    match work_db.override_comment_intent(&comment_id, &intent) {
        Ok(comment) => {
            let revision = publish_comment_invalidation(
                &server_state,
                &session_id,
                &request_id,
                &comment.artifact_kind,
                &comment.artifact_id,
                "comment_intent_overridden",
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::CommentResult { comment });
        }
        Err(err) => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: err.to_string(),
            },
        ),
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

    if let Err(err) = work_db.complete_answer_agent_run(&run.id, ANSWER_AGENT_RUN_STATUS_REPLIED, Some(&body), None) {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: err.to_string(),
            },
        );
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
        Err(err) => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: err.to_string(),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server_state() -> Arc<ServerState> {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(temp.path().to_path_buf())
                .db_path(temp.path().join("state.db"))
                .build(),
            None,
        ));
        // Leak the temp dir for the lifetime of the test process; the
        // ServerState's WorkDb keeps a handle to a path inside it.
        std::mem::forget(temp);
        ServerState::new_arc_with_app_pid(cfg, None, None).unwrap()
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
            .create_product(crate::work::CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
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
            .build()
    }

    #[tokio::test]
    async fn post_answer_replies_and_transitions_comment_to_answered() {
        let server_state = test_server_state();
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
        let server_state = test_server_state();
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
        let server_state = test_server_state();
        let work_db = server_state.work_db.clone();
        let sink = make_session_sink();
        let (_comment_id, _execution_id) = seed_answering_comment(&work_db);

        // A `run_id` pointing at some other kind of execution (e.g. a
        // regular chore run) must be rejected up front — this RPC is only
        // ever valid for an `answer_agent` execution.
        let product = work_db
            .create_product(crate::work::CreateProductInput {
                name: "Other".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
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
}
