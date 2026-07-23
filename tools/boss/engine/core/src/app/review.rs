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
                tokio::spawn(async move {
                    match merge_when_ready::gh_merge_when_ready(&pr_url2).await {
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
                            send_response(
                                &sink2,
                                &request_id2,
                                FrontendEvent::WorkError {
                                    message: format!("merge_when_ready failed: {err:#}"),
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
                    work_item_id,
                    pr_url,
                    target_branch,
                )
                .await;
            }
        }
    }
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
    work_item_id: String,
    pr_url: String,
    target_branch: String,
) {
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
            // item. No-op — re-report success without re-submitting to
            // Trunk (design: "a second merge click on an already-active
            // intent is a no-op that re-reports current queue state").
            server_state.pr_reconciler_kick.notify_one();
            send_response(
                &sink,
                &request_id,
                FrontendEvent::MergeWhenReadyAccepted {
                    work_item_id,
                    pr_url,
                    action: merge_when_ready::MergeAction::TrunkEnqueued.as_str().to_owned(),
                },
            );
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
    let kick = server_state.pr_reconciler_kick.clone();
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
                // the queue poller's first sweep.
                let detail = serde_json::json!({"source": "trunk", "state": "pending"}).to_string();
                if let Err(err) = work_db.set_task_merge_queue_state(&work_item_id, Some("queued"), Some(&detail)) {
                    tracing::error!(
                        %work_item_id,
                        ?err,
                        "merge_when_ready: trunk submit succeeded but optimistic merge_queue_state write failed",
                    );
                }
                kick.notify_one();
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
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(temp.path().to_path_buf())
                .db_path(temp.path().join("state.db"))
                .build(),
            None,
        ));
        let state =
            ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, None, None, Some(trunk_client)).unwrap();
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
        let (_product_id, work_item_id) = seed_trunk_queue_chore(
            &state.work_db,
            "trunk-chore",
            "https://github.com/brianduff/flunge/pull/978",
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
                assert!(
                    message.contains("trunk"),
                    "expected a trunk-specific error, got: {message}"
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
}
