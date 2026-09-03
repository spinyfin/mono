//! `FrontendRequest` handlers — idea CRUD and graduation.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. See [`super::Dispatch`] for the
//! per-request context every handler receives.
//!
//! Every mutating handler publishes on `work.product.<id>` (D7): zero new
//! subscription machinery — ideas ride the same worktree invalidation
//! topic every other product-scoped entity uses.

use super::*;

pub(super) async fn handle_create_idea(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateIdea { input } = req else {
        unreachable!()
    };
    let product_id = input.product_id.clone();
    match work_db.create_idea(input) {
        Ok(idea) => {
            let revision = publish_work_invalidation(
                &server_state,
                &session_id,
                &request_id,
                vec![work_product_topic(&product_id)],
                "idea_created",
                Some(product_id),
                vec![idea.id.clone()],
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::IdeaCreated { idea });
        }
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_list_ideas(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListIdeas { product_id, status } = req else {
        unreachable!()
    };
    match work_db.list_ideas(&product_id, status) {
        Ok(ideas) => send_response(&sink, &request_id, FrontendEvent::IdeasList { product_id, ideas }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_get_idea(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetIdea { id } = req else {
        unreachable!()
    };
    match work_db.get_idea(&id) {
        Ok(Some(idea)) => send_response(&sink, &request_id, FrontendEvent::IdeaResult { idea }),
        Ok(None) => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: format!("unknown idea: {id}"),
            },
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_update_idea(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::UpdateIdea { id, patch } = req else {
        unreachable!()
    };
    match work_db.update_idea(&id, patch) {
        Ok(idea) => {
            let product_id = idea.product_id.clone();
            let revision = publish_work_invalidation(
                &server_state,
                &session_id,
                &request_id,
                vec![work_product_topic(&product_id)],
                "idea_updated",
                Some(product_id),
                vec![idea.id.clone()],
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::IdeaUpdated { idea });
        }
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_delete_idea(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::DeleteIdea { id } = req else {
        unreachable!()
    };
    let product_id = match work_db.get_idea(&id) {
        Ok(Some(idea)) => idea.product_id,
        Ok(None) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("unknown idea: {id}"),
                },
            );
            return;
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };
    match work_db.delete_idea(&id) {
        Ok(()) => {
            let revision = publish_work_invalidation(
                &server_state,
                &session_id,
                &request_id,
                vec![work_product_topic(&product_id)],
                "idea_deleted",
                Some(product_id),
                vec![id.clone()],
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::IdeaDeleted { idea_id: id });
        }
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_graduate_idea(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GraduateIdea {
        id,
        target,
        name,
        effort_level,
        reasoning,
    } = req
    else {
        unreachable!()
    };
    match work_db.graduate_idea(&id, target, name, effort_level, reasoning) {
        Ok((idea, chore, project)) => {
            let product_id = idea.product_id.clone();
            let mut item_ids = vec![idea.id.clone()];
            if let Some(graduated_to_id) = idea.graduated_to_id.clone() {
                item_ids.push(graduated_to_id);
            }
            let revision = publish_work_invalidation(
                &server_state,
                &session_id,
                &request_id,
                vec![work_product_topic(&product_id)],
                "idea_graduated",
                Some(product_id),
                item_ids,
            )
            .await;
            send_response_with_revision(
                &sink,
                &request_id,
                revision,
                FrontendEvent::IdeaGraduated {
                    idea,
                    chore: chore.map(Box::new),
                    project: project.map(Box::new),
                },
            );
        }
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}
