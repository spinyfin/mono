//! `FrontendRequest` handlers — work-item dependency edges.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_add_dependency(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AddDependency { input } = req else {
        unreachable!()
    };
    {
        match work_db.add_dependency_with_worker_reconcile(input) {
            Ok((edge, reaped)) => {
                // Defect #2: adding this edge may have pushed an
                // actively-running dependent into `blocked`. The DB
                // transition already cancelled its execution row
                // atomically; here we release the physical worker —
                // tear down its libghostty pane and free its cube
                // workspace lease — so a `blocked` task never leaves a
                // live worker behind ("in backlog but executing").
                // Mirrors the cancel-execution reaping path: pane slots
                // are keyed by run_id, the lease by execution_id, so we
                // release both. force_release is idempotent.
                if let Some(execution) = reaped {
                    tracing::warn!(
                        dependent_id = %edge.dependent_id,
                        prerequisite_id = %edge.prerequisite_id,
                        execution_id = %execution.id,
                        "add_dependency: dependent became blocked while a worker was running it — \
                         cancelled the execution and releasing its pane + cube lease",
                    );
                    let active_runs = work_db.active_run_ids_for_execution(&execution.id).unwrap_or_default();
                    let handler = server_state.completion_handler.clone();
                    let exec_for_release = execution.id.clone();
                    tokio::spawn(async move {
                        for run_id in active_runs {
                            handler.force_release(&run_id).await;
                        }
                        handler.force_release(&exec_for_release).await;
                    });
                }
                // Publish a work-invalidation so subscribers re-render
                // the dependency surfaces (kanban badge, show view) and
                // the dependent's possibly-changed status.
                let product_id = match work_db.get_work_item(&edge.dependent_id) {
                    Ok(item) => Some(work_item_product_id(&item)),
                    Err(_) => None,
                };
                let revision = if let Some(pid) = product_id.as_deref() {
                    publish_work_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        vec![work_product_topic(pid)],
                        "dependency_added",
                        Some(pid.to_owned()),
                        vec![edge.dependent_id.clone(), edge.prerequisite_id.clone()],
                    )
                    .await
                } else {
                    server_state.current_work_revision()
                };
                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::DependencyAdded { edge });
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

pub(super) async fn handle_remove_dependency(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RemoveDependency { input } = req else {
        unreachable!()
    };
    {
        let dependent_id = input.dependent.clone();
        let prerequisite_id = input.prerequisite.clone();
        let relation = input.relation.clone().unwrap_or_else(|| "blocks".to_owned());
        match work_db.remove_dependency(input) {
            Ok(removed) => {
                let product_id = match work_db.get_work_item(&dependent_id) {
                    Ok(item) => Some(work_item_product_id(&item)),
                    Err(_) => None,
                };
                let revision = if let Some(pid) = product_id.as_deref() {
                    publish_work_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        vec![work_product_topic(pid)],
                        "dependency_removed",
                        Some(pid.to_owned()),
                        vec![dependent_id.clone(), prerequisite_id.clone()],
                    )
                    .await
                } else {
                    server_state.current_work_revision()
                };
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::DependencyRemoved {
                        dependent_id,
                        prerequisite_id,
                        relation,
                        removed,
                    },
                );
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

pub(super) async fn handle_list_dependencies(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListDependencies { input } = req else {
        unreachable!()
    };
    match work_db.list_dependencies(input) {
        Ok(view) => send_response(&sink, &request_id, FrontendEvent::DependencyList { view }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_list_dependencies_detailed(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListDependenciesDetailed { input } = req else {
        unreachable!()
    };
    {
        match work_db.list_dependencies_detailed(input) {
            Ok(detail) => send_response(&sink, &request_id, FrontendEvent::DependencyDetail { detail }),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}
