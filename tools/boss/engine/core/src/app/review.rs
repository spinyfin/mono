//! `FrontendRequest` handlers — merge-when-ready and review terminals.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_merge_when_ready(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MergeWhenReady { work_item_id } = req else {
        unreachable!()
    };
    {
        // Pre-flight: task must exist and be a Task/Chore.
        let item = match work_db.get_work_item(&work_item_id) {
            Ok(item) => item,
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("merge_when_ready: unknown work item: {err}"),
                    },
                );
                return;
            }
        };
        let (pr_url, task_status, product_id) = match &item {
            WorkItem::Task(task) | WorkItem::Chore(task) => {
                (task.pr_url.clone(), task.status.clone(), task.product_id.clone())
            }
            _ => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "merge_when_ready: only supported for tasks/chores".to_owned(),
                    },
                );
                return;
            }
        };
        if task_status != TaskStatus::InReview {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("merge_when_ready: task is not in review (status: {task_status})"),
                },
            );
            return;
        }
        let pr_url = match pr_url.filter(|s| !s.is_empty()) {
            Some(u) => u,
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "merge_when_ready: task has no PR URL".to_owned(),
                    },
                );
                return;
            }
        };
        let product = match work_db.get_product(&product_id) {
            Ok(Some(product)) => product,
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("merge_when_ready: unknown product: {product_id}"),
                    },
                );
                return;
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("merge_when_ready: failed to load product: {err}"),
                    },
                );
                return;
            }
        };
        let mechanism = match crate::merge_mechanism::MergeMechanism::parse(product.merge_mechanism.as_deref()) {
            Ok(mechanism) => mechanism,
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("merge_when_ready: {err}"),
                    },
                );
                return;
            }
        };
        match mechanism {
            crate::merge_mechanism::MergeMechanism::Direct => {
                // Spawn the GitHub interaction so the main loop isn't blocked.
                let sink2 = sink.clone();
                let request_id2 = request_id.clone();
                let work_item_id2 = work_item_id.clone();
                let pr_url2 = pr_url.clone();
                let kick = server_state.pr_reconciler_kick.clone();
                let direct_merge_executor = server_state.direct_merge_executor.clone();
                let work_db2 = work_db.clone();
                let product_name = product.name.clone();
                let product_slug = product.slug.clone();
                tokio::spawn(async move {
                    match direct_merge_executor.execute(&pr_url2).await {
                        Ok(action) => {
                            // Kick the PR reconciler so the kanban state
                            // reflects the new merge-queue / auto-merge
                            // state promptly without waiting for the next
                            // periodic sweep.
                            kick.notify_one();
                            send_response(
                                &sink2,
                                &request_id2,
                                FrontendEvent::MergeWhenReadyAccepted {
                                    work_item_id: work_item_id2,
                                    pr_url: pr_url2,
                                    action: action.as_str().to_owned(),
                                },
                            );
                        }
                        Err(err) => {
                            let message = format!("{err:#}");
                            if crate::merge_mechanism::is_push_restriction_error(&message) {
                                let _ = work_db2.create_attention_item(crate::work::CreateAttentionItemInput {
                                    work_item_id: Some(work_item_id2.clone()),
                                    kind: crate::merge_mechanism::PUSH_RESTRICTION_ATTENTION_KIND.to_owned(),
                                    title: crate::merge_mechanism::PUSH_RESTRICTION_ATTENTION_TITLE.to_owned(),
                                    body_markdown: crate::merge_mechanism::render_push_restriction_attention_body(
                                        &product_name,
                                        &product_slug,
                                        &message,
                                    ),
                                    execution_id: None,
                                    status: None,
                                    resolved_at: None,
                                });
                            }
                            send_response(
                                &sink2,
                                &request_id2,
                                FrontendEvent::WorkError {
                                    message: format!("merge_when_ready failed: {message}"),
                                },
                            );
                        }
                    }
                });
            }
            crate::merge_mechanism::MergeMechanism::TrunkQueue { target_branch } => {
                handle_trunk_queue_merge(
                    server_state,
                    work_db,
                    sink,
                    request_id,
                    TrunkQueueMergeRequest {
                        product_id,
                        work_item_id,
                        pr_url,
                        target_branch,
                    },
                )
                .await;
            }
        }
    }
}

/// Bundles the per-call arguments [`handle_trunk_queue_merge`] needs beyond
/// its shared server/session handles — kept as a single struct so adding a
/// field there doesn't grow the function's argument list.
struct TrunkQueueMergeRequest {
    product_id: String,
    work_item_id: String,
    pr_url: String,
    target_branch: String,
}

/// The `trunk_queue` branch of [`handle_merge_when_ready`]: submit the PR
/// to the product's Trunk merge queue and record a standing merge intent.
///
/// See `trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`
/// §"The merge verb: submit + standing merge intent". No step here ever
/// falls back to `gh pr merge` — a submission failure (including a
/// missing/rejected Trunk API token) is a loud [`FrontendEvent::WorkError`]
/// and the task stays exactly as it was.
async fn handle_trunk_queue_merge(
    server_state: Arc<ServerState>,
    work_db: Arc<WorkDb>,
    sink: Arc<SessionSink>,
    request_id: String,
    req: TrunkQueueMergeRequest,
) {
    let TrunkQueueMergeRequest {
        product_id,
        work_item_id,
        pr_url,
        target_branch,
    } = req;
    let coords = match crate::trunk_merge::parse_trunk_pr_coordinates(&pr_url) {
        Ok(coords) => coords,
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("merge_when_ready: {err}"),
                },
            );
            return;
        }
    };
    let insert_input = crate::work::TrunkMergeIntentInsertInput::builder()
        .work_item_id(work_item_id.clone())
        .pr_url(pr_url.clone())
        .pr_number(coords.number as i64)
        .repo(format!("{}/{}", coords.owner, coords.repo))
        .target_branch(target_branch.clone())
        .build();
    let intent = match work_db.insert_trunk_merge_intent(insert_input) {
        Ok(Some(intent)) => intent,
        Ok(None) => {
            // Duplicate click: an intent is already active for this work
            // item. Only a genuine no-op when that intent is still for the
            // *current* PR — re-report success without re-submitting to
            // Trunk (design: "a second merge click on an already-active
            // intent is a no-op that re-reports current queue state"). If
            // the active intent is for a different PR (closed/reopened,
            // replaced), reporting success here would tell the user the
            // current PR was enqueued when nothing was submitted for it.
            let active = match work_db.get_active_trunk_merge_intent(&work_item_id) {
                Ok(active) => active,
                Err(err) => {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!("merge_when_ready: failed to load active trunk merge intent: {err}"),
                        },
                    );
                    return;
                }
            };
            match active {
                Some(active) if active.pr_url == pr_url => {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::MergeWhenReadyAccepted {
                            work_item_id,
                            pr_url,
                            action: merge_when_ready::MergeAction::TrunkEnqueued.as_str().to_owned(),
                        },
                    );
                }
                Some(active) => {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "merge_when_ready: a trunk merge intent is already active for {} (this work item's current PR is {pr_url}); resolve the stale intent before retrying",
                                active.pr_url
                            ),
                        },
                    );
                }
                None => {
                    // The insert reported a duplicate but no active row is
                    // visible now (raced with the poller retiring it, or
                    // `INSERT OR IGNORE` swallowed a genuine constraint
                    // violation unrelated to the dedup index) — do not claim
                    // success for a submission that never happened.
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: "merge_when_ready: trunk merge intent insert was ignored but no active intent \
                                      was found; retry the merge"
                                .to_owned(),
                        },
                    );
                }
            }
            return;
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("merge_when_ready: failed to record trunk merge intent: {err}"),
                },
            );
            return;
        }
    };

    let trunk_client = server_state.trunk_client.clone();
    let publisher = server_state.publisher.clone();
    tokio::spawn(async move {
        let request = boss_trunk_client::SubmitPullRequestRequest::builder()
            .repo(boss_trunk_client::TrunkRepoRef::new(
                "github.com",
                coords.owner,
                coords.repo,
            ))
            .pr(boss_trunk_client::TrunkPrRef::new(coords.number))
            .target_branch(target_branch)
            .build();
        match trunk_client.submit_pull_request(&request).await {
            Ok(()) => {
                // Optimistically move the card into the Merging UI ahead of
                // the queue poller's first sweep. Deliberately does NOT kick
                // `pr_reconciler_kick`: that wakes the GitHub merge poller to
                // refresh GitHub-native auto-merge / merge-queue state,
                // which never applies to a trunk_queue product, so it would
                // do nothing useful here (and
                // `merge_poller::update_pr_poll_state`'s
                // `preserve_merge_queue_state` gate is what keeps the
                // poller's own scheduled sweeps off these two columns).
                // The app is push-only, so the optimistic write needs its
                // own `work_item_changed` publish to reach the Merging UI —
                // nothing else emits one for this write.
                let detail = serde_json::json!({"source": "trunk", "state": "pending"}).to_string();
                if let Err(err) = work_db.set_task_merge_queue_state(&work_item_id, Some("queued"), Some(&detail)) {
                    tracing::error!(
                        %work_item_id,
                        ?err,
                        "merge_when_ready: trunk submit succeeded but optimistic merge_queue_state write failed",
                    );
                } else {
                    publisher
                        .publish_work_item_changed(&product_id, &work_item_id, "trunk_merge_submitted")
                        .await;
                }
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MergeWhenReadyAccepted {
                        work_item_id,
                        pr_url,
                        action: merge_when_ready::MergeAction::TrunkEnqueued.as_str().to_owned(),
                    },
                );
            }
            Err(err) => {
                // Loud, no fallback to `gh pr merge`: no intent row
                // survives a failed submission.
                if let Err(del_err) = work_db.delete_trunk_merge_intent(&intent.id) {
                    tracing::error!(
                        %work_item_id,
                        intent_id = %intent.id,
                        ?del_err,
                        "merge_when_ready: failed to roll back trunk merge intent after a failed submit",
                    );
                }
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("merge_when_ready: trunk queue submission failed for {pr_url}: {err}"),
                    },
                );
            }
        }
    });
}

pub(super) async fn handle_open_review_terminal(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::OpenReviewTerminal { work_item_id } = req else {
        unreachable!()
    };
    {
        let item = match work_db.get_work_item(&work_item_id) {
            Ok(item) => item,
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("open_review_terminal: unknown work item: {err}"),
                    },
                );
                return;
            }
        };
        let (pr_url, product_id, task_repo_url) = match &item {
            WorkItem::Task(task) | WorkItem::Chore(task) => (
                task.pr_url.clone(),
                task.product_id.clone(),
                task.repo_remote_url.clone(),
            ),
            _ => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "open_review_terminal only supports tasks/chores".to_owned(),
                    },
                );
                return;
            }
        };
        let pr_url = match pr_url.filter(|s| !s.is_empty()) {
            Some(u) => u,
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "open_review_terminal: task has no PR URL".to_owned(),
                    },
                );
                return;
            }
        };
        let product = match work_db.get_product(&product_id).ok().flatten() {
            Some(p) => p,
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("open_review_terminal: unknown product: {product_id}"),
                    },
                );
                return;
            }
        };
        let repo_remote_url = match task_repo_url
            .filter(|s| !s.is_empty())
            .or_else(|| product.repo_remote_url.filter(|s| !s.is_empty()))
        {
            Some(url) => url,
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "open_review_terminal: task has no repo URL".to_owned(),
                    },
                );
                return;
            }
        };
        let cube_client = server_state.cube_client.clone();
        let sink2 = sink.clone();
        let request_id2 = request_id.clone();
        let work_item_id2 = work_item_id.clone();
        tokio::spawn(async move {
            match open_review_terminal_async(&cube_client, &repo_remote_url, &pr_url, &work_item_id2).await {
                Ok((workspace_path, lease_id)) => {
                    send_response(
                        &sink2,
                        &request_id2,
                        FrontendEvent::ReviewTerminalReady {
                            work_item_id: work_item_id2,
                            workspace_path,
                            lease_id,
                        },
                    );
                }
                Err(err) => {
                    send_response(
                        &sink2,
                        &request_id2,
                        FrontendEvent::WorkError {
                            message: format!("open_review_terminal failed: {err:#}"),
                        },
                    );
                }
            }
        });
    }
}

/// Open a terminal into a work item's already-live execution workspace
/// (Doing-column debugging affordance). Unlike `handle_open_review_terminal`,
/// this never leases a new workspace — it reads `workspace_path` off the
/// existing `running`/`waiting_human` execution row, which the worker's
/// own pane is already using. Sending back the path is enough; there is
/// no matching release handler because the lease belongs to the worker,
/// not to this terminal window.
pub(super) async fn handle_open_live_workspace_terminal(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::OpenLiveWorkspaceTerminal { work_item_id } = req else {
        unreachable!()
    };
    let execution = match work_db.get_live_execution_for_work_item(&work_item_id, "") {
        Ok(Some(execution)) => execution,
        Ok(None) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: "open_live_workspace_terminal: work item has no live execution".to_owned(),
                },
            );
            return;
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("open_live_workspace_terminal: {err}"),
                },
            );
            return;
        }
    };
    let workspace_path = match execution.workspace_path.filter(|s| !s.is_empty()) {
        Some(path) => path,
        None => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: "open_live_workspace_terminal: live execution has no leased workspace".to_owned(),
                },
            );
            return;
        }
    };
    send_response(
        &sink,
        &request_id,
        FrontendEvent::LiveWorkspaceTerminalReady {
            work_item_id,
            workspace_path,
        },
    );
}

pub(super) async fn handle_release_review_terminal(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch { server_state, .. } = ctx;
    let FrontendRequest::ReleaseReviewTerminal { lease_id } = req else {
        unreachable!()
    };
    {
        let cube_client = server_state.cube_client.clone();
        tokio::spawn(async move {
            if let Err(err) = cube_client.release_workspace(&lease_id).await {
                tracing::warn!(
                    %lease_id,
                    ?err,
                    "release_review_terminal: workspace release failed"
                );
            }
        });
        // fire-and-forget: no reply sent
    }
}

/// `bossctl review start --pr <n>`: re-enqueue the automated review
/// pipeline for an open PR on demand. Same dispatch path
/// (`WorkDb::request_pr_review`) the dead-review auto-recovery sweep uses
/// — useful for post-hoc review after an incident (a prior reviewer died
/// without producing findings) or a deliberate re-review after
/// significant new commits.
pub(super) async fn handle_trigger_pr_review(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::TriggerPrReview { pr_number, repo } = req else {
        unreachable!()
    };
    {
        let matches = match work_db.find_work_items_by_pr(pr_number) {
            Ok(matches) => matches,
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
        // Repo filter (when given) matches by substring against the PR
        // URL — same disambiguation shape `boss task by-pr --repo` uses:
        // the same PR number can exist in more than one repo, and the PR
        // URL (not any per-task repo override) is authoritative for which
        // repo a PR lives in.
        let matches: Vec<_> = match repo.as_deref().filter(|r| !r.is_empty()) {
            Some(repo_filter) => matches
                .into_iter()
                .filter(|m| m.owner.pr_url.as_deref().is_some_and(|url| url.contains(repo_filter)))
                .collect(),
            None => matches,
        };
        let owner = match matches.len() {
            0 => {
                let scope = repo
                    .as_deref()
                    .map(|r| format!(" in a repo matching {r:?}"))
                    .unwrap_or_default();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("no work item bound to PR #{pr_number}{scope}"),
                    },
                );
                return;
            }
            1 => matches.into_iter().next().expect("len checked == 1").owner,
            n => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("PR #{pr_number} is ambiguous across {n} repos — pass --repo to disambiguate"),
                    },
                );
                return;
            }
        };
        match work_db.request_pr_review(&owner.id, &GhPrStateChecker) {
            Ok(execution) => {
                tracing::info!(
                    work_item_id = %owner.id,
                    execution_id = %execution.id,
                    pr_number,
                    "review start: re-enqueued pr_review execution",
                );
                server_state.execution_coordinator.kick();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::PrReviewTriggered {
                        execution,
                        work_item_id: owner.id,
                        pr_url: owner.pr_url.unwrap_or_default(),
                    },
                );
            }
            Err(err) => {
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
}

#[cfg(test)]
mod trunk_queue_tests {
    use std::time::Duration;

    use boss_trunk_client::{CallConfig, StaticTokenProvider, TrunkClient};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product_with_repo};
    use crate::work::WorkItemPatch;

    fn test_server_state(trunk_client: TrunkClient) -> (Arc<ServerState>, tempfile::TempDir) {
        test_server_state_with_direct_executor(trunk_client, None)
    }

    /// Like [`test_server_state`], but also lets the caller inject a fake
    /// [`crate::merge_when_ready::DirectMergeExecutor`] — used by the
    /// Direct-branch routing tests so they never shell out to a real `gh`
    /// process.
    fn test_server_state_with_direct_executor(
        trunk_client: TrunkClient,
        direct_merge_executor: Option<Arc<dyn crate::merge_when_ready::DirectMergeExecutor>>,
    ) -> (Arc<ServerState>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(temp.path().to_path_buf())
                .db_path(temp.path().join("state.db"))
                .build(),
            None,
        ));
        let state = ServerState::new_arc_with_app_pid_and_merge_probe(
            cfg,
            None,
            None,
            None,
            None,
            Some(trunk_client),
            direct_merge_executor,
        )
        .unwrap();
        (state, temp)
    }

    fn dispatch_ctx(server_state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
        Dispatch::builder()
            .server_state(server_state.clone())
            .work_db(server_state.work_db.clone())
            .sink(sink.clone())
            .session_id("s1")
            .request_id("req-1")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build()
    }

    fn make_session_sink() -> Arc<SessionSink> {
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
        Arc::new(SessionSink::new(shutdown_tx))
    }

    fn trunk_client_for(base_url: String) -> TrunkClient {
        TrunkClient::new(
            Arc::new(StaticTokenProvider::new("test-token")),
            CallConfig::new(Duration::from_secs(5)).with_base_url(base_url),
        )
    }

    /// Seed a `trunk_queue`-mechanism product and an `in_review` chore with
    /// `pr_url` bound to it — the state `handle_merge_when_ready` requires
    /// before it will route through the Trunk submission branch. Returns
    /// `(product_id, work_item_id)`.
    fn seed_trunk_queue_chore(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
        let product = create_test_product_with_repo(db, name, Some("git@github.com:brianduff/flunge.git"));
        db.set_product_merge_mechanism(&product.id, Some("trunk_queue"))
            .unwrap();
        let chore = create_test_chore_manual(db, product.id.clone(), name);
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        (product.id, chore.id)
    }

    #[tokio::test]
    async fn trunk_queue_merge_submits_and_marks_task_queued() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let (state, _temp) = test_server_state(trunk_client_for(server.uri()));
        let (product_id, work_item_id) = seed_trunk_queue_chore(
            &state.work_db,
            "trunk-chore",
            "https://github.com/brianduff/flunge/pull/978",
        );
        let sink = make_session_sink();

        // Register + subscribe this session on the work item's product
        // topic so we can assert the optimistic `merge_queue_state="queued"`
        // write actually reaches the (push-only) app as a
        // `work_item_changed`/`WorkInvalidated` event, not just a DB row —
        // see the doc comment on the `publisher.publish_work_item_changed`
        // call in `handle_trunk_queue_merge`.
        state.topic_broker.register_session("s1", sink.clone()).await;
        state
            .topic_broker
            .subscribe("s1", &[crate::protocol::work_product_topic(&product_id)])
            .await;

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: work_item_id.clone(),
            },
        )
        .await;

        // The topic push is enqueued (and awaited) before the RPC response
        // in `handle_trunk_queue_merge`, so it arrives first.
        let invalidation = sink.next().await.expect("a work-item-changed push should be enqueued");
        match invalidation.payload {
            FrontendEvent::TopicEvent {
                event: crate::protocol::TopicEventPayload::WorkInvalidated { reason, item_ids, .. },
                ..
            } => {
                assert_eq!(reason, "trunk_merge_submitted");
                assert_eq!(item_ids, vec![work_item_id.clone()]);
            }
            other => panic!("expected a WorkInvalidated topic push naming the work item, got {other:?}"),
        }

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::MergeWhenReadyAccepted { action, pr_url, .. } => {
                assert_eq!(action, "trunk_enqueued");
                assert_eq!(pr_url, "https://github.com/brianduff/flunge/pull/978");
            }
            other => panic!("expected MergeWhenReadyAccepted, got {other:?}"),
        }

        let intent = state
            .work_db
            .get_active_trunk_merge_intent(&work_item_id)
            .unwrap()
            .expect("an active trunk merge intent should exist");
        assert_eq!(intent.repo, "brianduff/flunge");
        assert_eq!(intent.pr_number, 978);
        assert_eq!(intent.submit_count, 1);
        assert_eq!(intent.status, "active");

        let item = state.work_db.get_work_item(&work_item_id).unwrap();
        let WorkItem::Chore(task) = item else {
            panic!("expected a chore");
        };
        assert_eq!(task.merge_queue_state.as_deref(), Some("queued"));
        assert!(
            task.merge_queue_detail.as_deref().unwrap_or("").contains("trunk"),
            "expected merge_queue_detail to carry source=trunk, got: {:?}",
            task.merge_queue_detail
        );

        server.verify().await;
    }

    #[tokio::test]
    async fn trunk_queue_merge_duplicate_click_is_a_no_op() {
        let server = MockServer::start().await;
        // `.expect(1)`: a second merge click must NOT re-submit — verified by
        // `server.verify()` failing if `submitPullRequest` is called twice.
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let (state, _temp) = test_server_state(trunk_client_for(server.uri()));
        let (_product_id, work_item_id) = seed_trunk_queue_chore(
            &state.work_db,
            "trunk-chore-dup",
            "https://github.com/brianduff/flunge/pull/979",
        );
        let sink = make_session_sink();

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: work_item_id.clone(),
            },
        )
        .await;
        let first = sink.next().await.expect("first response");
        assert!(matches!(first.payload, FrontendEvent::MergeWhenReadyAccepted { .. }));

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: work_item_id.clone(),
            },
        )
        .await;
        let second = sink.next().await.expect("second response");
        match second.payload {
            FrontendEvent::MergeWhenReadyAccepted { action, .. } => assert_eq!(action, "trunk_enqueued"),
            other => panic!("expected a no-op MergeWhenReadyAccepted, got {other:?}"),
        }

        // Still exactly one intent row, unincremented — the duplicate click
        // did not insert a second row or re-submit.
        let intent = state
            .work_db
            .get_active_trunk_merge_intent(&work_item_id)
            .unwrap()
            .unwrap();
        assert_eq!(intent.submit_count, 1);

        server.verify().await;
    }

    #[tokio::test]
    async fn trunk_queue_merge_auth_failure_is_loud_with_no_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad token"))
            .mount(&server)
            .await;

        let (state, _temp) = test_server_state(trunk_client_for(server.uri()));
        let (_product_id, work_item_id) = seed_trunk_queue_chore(
            &state.work_db,
            "trunk-chore-auth",
            "https://github.com/brianduff/flunge/pull/980",
        );
        let sink = make_session_sink();

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: work_item_id.clone(),
            },
        )
        .await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::WorkError { message } => {
                // Matches `TrunkError::Auth`'s `Display` text rather than
                // the handler's `"trunk queue submission failed for ..."`
                // wrapper, which every `TrunkError` variant shares — this
                // pins the 401 -> `TrunkError::Auth` mapping specifically.
                assert!(
                    message.contains("trunk authentication failed"),
                    "expected the 401 to map to TrunkError::Auth, got: {message}"
                );
            }
            other => panic!("expected WorkError, got {other:?}"),
        }

        // No intent row survives a failed submission — the loud-failure path
        // never falls back to `gh pr merge` and leaves no dangling state.
        assert!(
            state
                .work_db
                .get_active_trunk_merge_intent(&work_item_id)
                .unwrap()
                .is_none()
        );
        let item = state.work_db.get_work_item(&work_item_id).unwrap();
        let WorkItem::Chore(task) = item else {
            panic!("expected a chore");
        };
        assert_eq!(task.merge_queue_state, None);
    }

    /// A `TrunkTokenProvider` that always reports "no token configured" —
    /// exercises the missing-token path (distinct from a rejected-token
    /// 401 response from the server) through the same loud-no-fallback
    /// contract as [`trunk_queue_merge_auth_failure_is_loud_with_no_fallback`].
    #[derive(Clone)]
    struct NoTokenProvider;

    impl boss_trunk_client::TrunkTokenProvider for NoTokenProvider {
        fn token(&self) -> Result<boss_trunk_client::SecretString, boss_trunk_client::TrunkError> {
            Err(boss_trunk_client::TrunkError::Auth("no token configured".to_owned()))
        }
    }

    #[tokio::test]
    async fn trunk_queue_merge_missing_token_is_loud_with_no_fallback() {
        let server = MockServer::start().await;
        // The client must fail before ever issuing the HTTP call.
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&server)
            .await;

        let trunk_client = TrunkClient::new(
            Arc::new(NoTokenProvider),
            CallConfig::new(Duration::from_secs(5)).with_base_url(server.uri()),
        );
        let (state, _temp) = test_server_state(trunk_client);
        let (_product_id, work_item_id) = seed_trunk_queue_chore(
            &state.work_db,
            "trunk-chore-no-token",
            "https://github.com/brianduff/flunge/pull/981",
        );
        let sink = make_session_sink();

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: work_item_id.clone(),
            },
        )
        .await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::WorkError { message } => {
                assert!(
                    message.contains("trunk authentication failed"),
                    "expected a missing-token auth error, got: {message}"
                );
            }
            other => panic!("expected WorkError, got {other:?}"),
        }

        assert!(
            state
                .work_db
                .get_active_trunk_merge_intent(&work_item_id)
                .unwrap()
                .is_none()
        );
        server.verify().await;
    }

    /// [`crate::merge_when_ready::DirectMergeExecutor`] fake for the
    /// Direct-branch routing tests below — records the PR URLs it was
    /// invoked with instead of shelling out to a real `gh` process, so
    /// these tests can pin the routing decision without any risk of
    /// issuing a live, mutating `gh pr merge` call.
    #[derive(Default)]
    struct FakeDirectMergeExecutor {
        calls: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl crate::merge_when_ready::DirectMergeExecutor for FakeDirectMergeExecutor {
        async fn execute(&self, pr_url: &str) -> anyhow::Result<crate::merge_when_ready::MergeAction> {
            self.calls.lock().unwrap().push(pr_url.to_owned());
            Ok(crate::merge_when_ready::MergeAction::Merged)
        }
    }

    /// A product with `merge_mechanism` `NULL` still routes through the
    /// `gh pr merge` (Direct) path: no trunk merge intent is created and
    /// the Trunk server is never contacted, even though the new
    /// product/mechanism preflight runs ahead of the Direct branch. The
    /// Direct executor is a fake (see [`FakeDirectMergeExecutor`]) so this
    /// never shells out to a real `gh` process.
    #[tokio::test]
    async fn merge_mechanism_null_still_routes_direct() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&server)
            .await;

        let direct_executor = Arc::new(FakeDirectMergeExecutor::default());
        let (state, _temp) =
            test_server_state_with_direct_executor(trunk_client_for(server.uri()), Some(direct_executor.clone()));
        let product = create_test_product_with_repo(
            &state.work_db,
            "direct-null-product",
            Some("git@github.com:brianduff/flunge.git"),
        );
        let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "direct-null-chore");
        state
            .work_db
            .update_work_item(
                &chore.id,
                WorkItemPatch {
                    status: Some("in_review".into()),
                    pr_url: Some("https://github.com/brianduff/flunge/pull/990".into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        let sink = make_session_sink();

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: chore.id.clone(),
            },
        )
        .await;

        // Wait for the spawned Direct-branch task to complete before
        // asserting on the fake — `handle_merge_when_ready` fires it via
        // `tokio::spawn` and replies once it resolves.
        let envelope = sink.next().await.expect("a response should be enqueued");
        assert!(matches!(envelope.payload, FrontendEvent::MergeWhenReadyAccepted { .. }));
        assert_eq!(
            direct_executor.calls.lock().unwrap().as_slice(),
            ["https://github.com/brianduff/flunge/pull/990"],
            "a NULL-mechanism product must route through the Direct executor"
        );

        assert!(
            state
                .work_db
                .get_active_trunk_merge_intent(&chore.id)
                .unwrap()
                .is_none(),
            "a NULL-mechanism product must never create a trunk merge intent"
        );
        server.verify().await;
    }

    /// A product with `merge_mechanism` explicitly `"direct"` also routes
    /// through the Direct path — mirrors the NULL case above, pinning that
    /// the explicit value behaves identically. Also uses the fake Direct
    /// executor, never a real `gh` process.
    #[tokio::test]
    async fn merge_mechanism_explicit_direct_still_routes_direct() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&server)
            .await;

        let direct_executor = Arc::new(FakeDirectMergeExecutor::default());
        let (state, _temp) =
            test_server_state_with_direct_executor(trunk_client_for(server.uri()), Some(direct_executor.clone()));
        let product = create_test_product_with_repo(
            &state.work_db,
            "direct-explicit-product",
            Some("git@github.com:brianduff/flunge.git"),
        );
        state
            .work_db
            .set_product_merge_mechanism(&product.id, Some("direct"))
            .unwrap();
        let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "direct-explicit-chore");
        state
            .work_db
            .update_work_item(
                &chore.id,
                WorkItemPatch {
                    status: Some("in_review".into()),
                    pr_url: Some("https://github.com/brianduff/flunge/pull/991".into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        let sink = make_session_sink();

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: chore.id.clone(),
            },
        )
        .await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        assert!(matches!(envelope.payload, FrontendEvent::MergeWhenReadyAccepted { .. }));
        assert_eq!(
            direct_executor.calls.lock().unwrap().as_slice(),
            ["https://github.com/brianduff/flunge/pull/991"],
            "an explicit direct-mechanism product must route through the Direct executor"
        );

        assert!(
            state
                .work_db
                .get_active_trunk_merge_intent(&chore.id)
                .unwrap()
                .is_none(),
            "an explicit direct-mechanism product must never create a trunk merge intent"
        );
        server.verify().await;
    }

    /// An unknown `product_id` on the task fails the new product preflight
    /// loudly rather than silently falling through to either merge path.
    #[tokio::test]
    async fn unknown_product_is_a_loud_work_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&server)
            .await;

        let (state, _temp) = test_server_state(trunk_client_for(server.uri()));
        // Create a product, bind a chore to it, then delete the product row
        // out from under the chore — `create_chore` validates the product
        // exists at insert time, so this is the simplest way to leave a
        // chore pointing at a `product_id` `get_product` no longer resolves.
        let product = create_test_product_with_repo(
            &state.work_db,
            "vanishing-product",
            Some("git@github.com:brianduff/flunge.git"),
        );
        let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "orphaned-chore");
        state
            .work_db
            .update_work_item(
                &chore.id,
                WorkItemPatch {
                    status: Some("in_review".into()),
                    pr_url: Some("https://github.com/brianduff/flunge/pull/992".into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        {
            let conn = state.work_db.connect().unwrap();
            // The chore still references this product, so the delete must
            // run with the FK check off — deliberately reproducing an
            // orphaned `product_id` rather than a state schema constraints
            // would allow.
            conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
            conn.execute("DELETE FROM products WHERE id = ?1", [&product.id])
                .unwrap();
        }

        let sink = make_session_sink();
        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: chore.id.clone(),
            },
        )
        .await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::WorkError { message } => {
                assert!(
                    message.contains("unknown product"),
                    "expected an unknown-product error, got: {message}"
                );
            }
            other => panic!("expected WorkError, got {other:?}"),
        }
        server.verify().await;
    }

    /// A corrupt `merge_mechanism` value (anything other than `NULL`,
    /// `"direct"`, `"trunk_queue"`) fails loudly rather than silently
    /// defaulting to either merge path.
    #[tokio::test]
    async fn corrupt_merge_mechanism_is_a_loud_work_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&server)
            .await;

        let (state, _temp) = test_server_state(trunk_client_for(server.uri()));
        let product = create_test_product_with_repo(
            &state.work_db,
            "corrupt-mechanism-product",
            Some("git@github.com:brianduff/flunge.git"),
        );
        // `set_product_merge_mechanism` validates before writing, so a
        // corrupt value can only arise as data corruption — write it
        // directly, bypassing that validation, to simulate that.
        state
            .work_db
            .connect()
            .unwrap()
            .execute(
                "UPDATE products SET merge_mechanism = 'bogus' WHERE id = ?1",
                [&product.id],
            )
            .unwrap();
        let chore = create_test_chore_manual(&state.work_db, product.id.clone(), "corrupt-mechanism-chore");
        state
            .work_db
            .update_work_item(
                &chore.id,
                WorkItemPatch {
                    status: Some("in_review".into()),
                    pr_url: Some("https://github.com/brianduff/flunge/pull/993".into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();

        let sink = make_session_sink();
        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: chore.id.clone(),
            },
        )
        .await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::WorkError { message } => {
                assert!(
                    message.contains("bogus"),
                    "expected the unknown-mechanism error to name the offending value, got: {message}"
                );
            }
            other => panic!("expected WorkError, got {other:?}"),
        }
        assert!(
            state
                .work_db
                .get_active_trunk_merge_intent(&chore.id)
                .unwrap()
                .is_none(),
            "a corrupt mechanism must never create a trunk merge intent"
        );
        server.verify().await;
    }
}
