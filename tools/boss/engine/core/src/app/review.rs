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
        let (pr_url, task_status) = match &item {
            WorkItem::Task(task) | WorkItem::Chore(task) => (task.pr_url.clone(), task.status.clone()),
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
