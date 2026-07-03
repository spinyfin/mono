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
                Ok(_) => {
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
