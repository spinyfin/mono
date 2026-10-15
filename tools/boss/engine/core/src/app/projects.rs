//! `FrontendRequest` handlers — project CRUD, ordering, and design docs.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;
use crate::app::handler_helpers::{BoardRefusal, send_refusal};
use boss_protocol::BoardRefusalCode;

pub(super) async fn handle_list_projects(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListProjects { product_id, dep_filter } = req else {
        unreachable!()
    };
    {
        match work_db.list_projects(&product_id, dep_filter.as_ref()) {
            Ok(projects) => {
                send_response_with_revision(
                    &sink,
                    &request_id,
                    server_state.current_work_revision(),
                    FrontendEvent::ProjectsList { product_id, projects },
                );
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

pub(super) async fn handle_create_project(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateProject { input } = req else {
        unreachable!()
    };
    match work_db.create_project(input) {
        Ok(project) => {
            let item = WorkItem::Project(project);
            let product_id = item.product_id().to_string();
            let revision = publish_work_invalidation(
                &server_state,
                &session_id,
                &request_id,
                vec![work_product_topic(&product_id)],
                "project_created",
                Some(product_id),
                vec![work_item_id(&item)],
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemCreated { item });
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
        }
    }
}

pub(super) async fn handle_reorder_project_tasks(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ReorderProjectTasks {
        project_id,
        task_ids,
        moved_task_id,
    } = req
    else {
        unreachable!()
    };
    match work_db.get_work_item(&project_id) {
        Ok(project_item) => match work_db.reorder_project_tasks(&project_id, &task_ids) {
            Ok(()) => {
                let product_id = project_item.product_id().to_string();
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "project_tasks_reordered",
                    Some(product_id),
                    task_ids.clone(),
                )
                .await;
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::ProjectTasksReordered { project_id, task_ids },
                );
            }
            Err(err) => send_reorder_error(&sink, &request_id, moved_task_id.as_deref(), &err),
        },
        Err(err) => send_reorder_error(&sink, &request_id, moved_task_id.as_deref(), &err),
    }
}

/// Answer a failed reorder. A board Move Up/Down names the card it moved, so
/// its refusal is the structured `WorkItemRefused` shown on that card; any
/// other caller (the CLI) keeps the plain `WorkError` it matches on.
fn send_reorder_error(sink: &SessionSink, request_id: &str, moved_task_id: Option<&str>, err: &anyhow::Error) {
    let Some(moved_task_id) = moved_task_id else {
        send_work_error(sink, request_id, err);
        return;
    };
    let mut refusal = BoardRefusal::new(
        BoardRefusalCode::ReorderRejected,
        "Couldn't move that task: the project's task list has changed since the board last loaded. Try the move again.",
    );
    refusal.raw = Some(format!("{err:#}"));
    send_refusal(sink, request_id, true, moved_task_id, refusal);
}

pub(super) async fn handle_set_project_design_doc(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetProjectDesignDoc { input } = req else {
        unreachable!()
    };
    {
        match work_db.set_project_design_doc(input) {
            Ok(project) => {
                let item = WorkItem::Project(project);
                let product_id = item.product_id().to_string();
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "project_design_doc_set",
                    Some(product_id),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemUpdated { item });
            }
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_resolve_project_design_doc(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ResolveProjectDesignDoc { project_id } = req else {
        unreachable!()
    };
    {
        // Build a (repo_remote_url -> workspace_path) lookup so the
        // resolver can hand the open dispatcher an absolute
        // workspace path for `$EDITOR` / renderer fast-path.
        // First-match wins when multiple workspaces lease the
        // same repo — any of them resolves the file equally well.
        let leased_repo_paths: HashMap<String, String> = work_db
            .list_in_flight_executions()
            .map(|execs| {
                let mut map = HashMap::new();
                for exec in execs {
                    if let Some(path) = exec.workspace_path
                        && !map.contains_key(&exec.repo_remote_url)
                    {
                        map.insert(exec.repo_remote_url, path);
                    }
                }
                map
            })
            .unwrap_or_default();
        match work_db.resolve_project_design_doc(&project_id, |repo| leased_repo_paths.get(repo).cloned()) {
            Ok(output) => send_response(&sink, &request_id, FrontendEvent::ProjectDesignDocResolved { output }),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}
