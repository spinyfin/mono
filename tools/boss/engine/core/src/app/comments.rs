//! `FrontendRequest` handlers — markdown-viewer comments and magic-wand dispatch.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

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

    if let Err(err) = work_db.create_answer_agent_run(
        &comment.id,
        &comment.artifact_kind,
        &comment.artifact_id,
        &comment.doc_version,
        0,
    ) {
        tracing::error!(
            comment_id = %comment.id,
            err = %err,
            "answer-agent spawn: comment flipped to 'answering' but failed to create the tracking run row",
        );
        return;
    }

    if let Err(err) = work_db.create_answer_agent_execution(&comment.id, &repo_remote_url) {
        tracing::error!(
            comment_id = %comment.id,
            err = %err,
            "answer-agent spawn: comment flipped to 'answering' and run row created, but execution creation failed",
        );
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

pub(super) async fn handle_comments_dispatch_magic_wand(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        peer_pid,
    } = ctx;
    let FrontendRequest::CommentsDispatchMagicWand { comment_id } = req else {
        unreachable!()
    };
    {
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                comment_id = %comment_id,
                "comments_dispatch_magic_wand rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "comments_dispatch_magic_wand requires app or Boss authority".to_owned(),
                },
            );
            return;
        }

        // Resolve the comment to get the doc text and anchor.
        let comment = match work_db.get_comment(&comment_id) {
            Ok(Some(c)) => c,
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

        match comment.artifact_kind.as_str() {
            "work_item" => {
                // Phase 3: engine-owned doc → specialised, isolated Claude instance.

                // Fetch the work-item description (the doc text).
                let doc_text = match work_db.get_work_item(&comment.artifact_id) {
                    Ok(item) => {
                        use boss_protocol::WorkItem;
                        match item {
                            WorkItem::Task(t) | WorkItem::Chore(t) => t.description,
                            _ => {
                                send_response(
                                    &sink,
                                    &request_id,
                                    FrontendEvent::WorkError {
                                        message: format!(
                                            "magic-wand dispatch: work item '{}' is not a \
                                                     Task/Chore",
                                            comment.artifact_id
                                        ),
                                    },
                                );
                                return;
                            }
                        }
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

                // Create the dispatch row (status = in_flight).
                let dispatch = match work_db.create_magic_wand_dispatch(
                    &comment_id,
                    &comment.artifact_kind,
                    &comment.artifact_id,
                    &comment.doc_version,
                ) {
                    Ok(d) => d,
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

                // Reply immediately so the macOS app can subscribe to the dispatch topic.
                let dispatch_id = dispatch.id.clone();
                let anchor_exact = comment.anchor.exact.clone();
                let comment_body = comment.body.clone();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MagicWandDispatched {
                        dispatch: dispatch.clone(),
                    },
                );

                // Spawn the async Claude call.
                let work_db2 = work_db.clone();
                let server_state2 = server_state.clone();
                tokio::spawn(async move {
                    let topic = magic_wand_dispatch_topic(&dispatch_id);
                    let result = crate::magic_wand::dispatch(&doc_text, &anchor_exact, &comment_body).await;
                    let final_dispatch = match result {
                        Ok(mw) => {
                            match work_db2.complete_magic_wand_dispatch(
                                &dispatch_id,
                                "returned",
                                Some(&mw.result_md),
                                None,
                                Some(mw.input_tokens),
                                Some(mw.output_tokens),
                                mw.anchor_warning,
                            ) {
                                Ok(d) => d,
                                Err(err) => {
                                    tracing::error!(
                                        dispatch_id = %dispatch_id,
                                        err = %err,
                                        "failed to record magic_wand returned status",
                                    );
                                    return;
                                }
                            }
                        }
                        Err(err) => {
                            let (error_msg, error_kind) = err;
                            tracing::warn!(
                                dispatch_id = %dispatch_id,
                                error_kind = %error_kind,
                                error = %error_msg,
                                "magic_wand dispatch failed",
                            );
                            match work_db2.complete_magic_wand_dispatch(
                                &dispatch_id,
                                "failed",
                                None,
                                Some(error_kind),
                                None,
                                None,
                                false,
                            ) {
                                Ok(d) => d,
                                Err(db_err) => {
                                    tracing::error!(
                                        dispatch_id = %dispatch_id,
                                        err = %db_err,
                                        "failed to record magic_wand failed status",
                                    );
                                    return;
                                }
                            }
                        }
                    };
                    let envelope = FrontendEventEnvelope::push(FrontendEvent::MagicWandResult {
                        dispatch: final_dispatch,
                    });
                    server_state2.topic_broker.publish(&topic, envelope).await;
                });
            }

            "pr_doc" => {
                // Phase 4: PR-backed doc → Boss chore worker.
                // Parse the artifact_id: "pr_doc:<repo_remote_url>:<branch>:<path>".
                // repo_remote_url may itself contain ':' (SSH git@ URLs), so we
                // split from the right into exactly 3 parts (path, branch, repo).
                let artifact_id = &comment.artifact_id;
                let suffix = artifact_id.strip_prefix("pr_doc:").unwrap_or("");
                let parts: Vec<&str> = suffix.rsplitn(3, ':').collect();
                if parts.len() != 3 {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "magic-wand: malformed pr_doc artifact_id '{artifact_id}'; \
                                         expected 'pr_doc:<repo>:<branch>:<path>'"
                            ),
                        },
                    );
                    return;
                }
                let (pr_path, pr_branch, pr_repo) = (parts[0], parts[1], parts[2]);

                // Find the product that owns this repo.
                let product_id = match work_db.find_product_id_by_repo_remote_url(pr_repo) {
                    Ok(Some(id)) => id,
                    Ok(None) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!(
                                    "magic-wand: no product found for repo '{pr_repo}'; \
                                             cannot spawn chore"
                                ),
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

                // Build the chore title: truncate the anchor quote to 60 chars.
                let short_quote = if comment.anchor.exact.len() > 60 {
                    format!("{}…", &comment.anchor.exact[..60])
                } else {
                    comment.anchor.exact.clone()
                };
                let chore_name = format!("Address comment on `{pr_path}`: `{short_quote}`");

                // Build the chore description (worker reads this as the directive).
                let chore_description = format!(
                    "A reviewer left a comment on this PR's design doc.\n\n\
                             File: {pr_path}\n\
                             Branch: {pr_branch}\n\n\
                             Quoted section:\n\
                             > {anchor}\n\n\
                             Comment:\n\
                             > {body}\n\n\
                             Please update the file accordingly and push to the existing PR \
                             branch. Do not open a new PR; this branch already has one. \
                             Use `git checkout {pr_branch}` (or `jj edit`) to land on the \
                             branch before editing.",
                    anchor = comment.anchor.exact,
                    body = comment.body,
                );

                // Create the chore via the standard path.
                // `repo_remote_url` is inherited from the product
                // (which was resolved by `find_product_id_by_repo_remote_url`),
                // so we don't need to set it again here.
                let chore = match work_db.create_chore(
                    CreateChoreInput::builder()
                        .product_id(product_id.clone())
                        .name(chore_name)
                        .description(chore_description)
                        .created_via(format!("comment_dispatch:{comment_id}"))
                        .build(),
                ) {
                    Ok(c) => c,
                    Err(err) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!("magic-wand: failed to create chore: {err}"),
                            },
                        );
                        return;
                    }
                };

                // Create the dispatch row (status = chore_created).
                let dispatch = match work_db.create_pr_backed_magic_wand_dispatch(
                    &comment_id,
                    &comment.artifact_kind,
                    &comment.artifact_id,
                    &comment.doc_version,
                    &chore.id,
                ) {
                    Ok(d) => d,
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

                // Transition the comment to `dispatched`.
                let actor = format!("comment_dispatch:{comment_id}");
                if let Err(err) =
                    work_db.set_comment_status(&comment_id, COMMENT_STATUS_DISPATCHED, Some(actor.as_str()))
                {
                    tracing::error!(
                        comment_id = %comment_id,
                        chore_id = %chore.id,
                        err = %err,
                        "magic-wand: failed to transition comment to dispatched",
                    );
                }

                // Publish work invalidation so the kanban sees the new chore.
                publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "comment_dispatch_chore_created",
                    Some(product_id),
                    vec![chore.id.clone()],
                )
                .await;

                tracing::info!(
                    comment_id = %comment_id,
                    chore_id = %chore.id,
                    pr_repo = %pr_repo,
                    pr_branch = %pr_branch,
                    pr_path = %pr_path,
                    "magic-wand: spawned chore for PR-backed doc comment",
                );

                send_response(&sink, &request_id, FrontendEvent::MagicWandDispatched { dispatch });
            }

            other => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("magic-wand dispatch: unsupported artifact_kind '{other}'"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_comments_apply_magic_wand(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsApplyMagicWand {
        dispatch_id,
        current_doc_version,
    } = req
    else {
        unreachable!()
    };
    {
        match work_db.apply_magic_wand_dispatch(&dispatch_id, &current_doc_version, "user") {
            Ok((dispatch, conflict)) => {
                if !conflict {
                    // Publish comment topic invalidation so the sidebar reloads.
                    publish_comment_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        &dispatch.artifact_kind,
                        &dispatch.artifact_id,
                        "magic_wand_applied",
                    )
                    .await;
                }
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MagicWandApplied { dispatch, conflict },
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

pub(super) async fn handle_comments_discard_magic_wand(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsDiscardMagicWand { dispatch_id } = req else {
        unreachable!()
    };
    {
        match work_db.discard_magic_wand_dispatch(&dispatch_id) {
            Ok(dispatch) => send_response(
                &sink,
                &request_id,
                FrontendEvent::MagicWandApplied {
                    dispatch,
                    conflict: false,
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
