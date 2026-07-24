//! `FrontendRequest` handlers — the Designs tab's GitHub-backed
//! markdown browser.
//!
//! Both handlers are thin: they resolve the product's configured repo
//! from the work DB and hand off to [`boss_engine_design_docs`], which
//! owns every GitHub query, the auth path, the markdown filtering, the
//! listing cache, and the classification of failures into the states
//! the UI renders. Nothing here consults the local filesystem — the tab
//! works whether or not a clone of the repo exists on this machine.
//!
//! Both handlers `tokio::spawn` their GitHub work rather than awaiting
//! it inline. `handle_frontend_connection` awaits each handler in the
//! connection's read loop, so a slow or hanging network call awaited
//! here would stall every subsequent request on that connection — the
//! whole app, not just the Designs tab. Spawning detaches the round
//! trip and the reply still lands on the same `request_id`. (Same
//! pattern as the org-state re-probe in [`super::github_auth`].)

use super::*;

pub(super) async fn handle_list_product_design_docs(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListProductDesignDocs { product_id, refresh } = req else {
        unreachable!()
    };

    // The repo comes from the product row's `repo_remote_url`, never
    // from the product's name.
    let product = match work_db.get_product(&product_id) {
        Ok(Some(product)) => product,
        Ok(None) => {
            send_work_error(&sink, &request_id, format!("product `{product_id}` not found"));
            return;
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };

    let design_docs = server_state.design_docs.clone();
    tokio::spawn(async move {
        let state = design_docs
            .list_markdown_docs(product.repo_remote_url.as_deref(), refresh)
            .await;
        send_response(
            &sink,
            &request_id,
            FrontendEvent::ProductDesignDocsList { product_id, state },
        );
    });
}

pub(super) async fn handle_get_product_design_doc(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetProductDesignDoc {
        repo_remote_url,
        path,
        git_ref,
    } = req
    else {
        unreachable!()
    };

    let design_docs = server_state.design_docs.clone();
    tokio::spawn(async move {
        let content = design_docs.fetch_markdown_doc(&repo_remote_url, &path, &git_ref).await;
        send_response(
            &sink,
            &request_id,
            FrontendEvent::ProductDesignDocContent {
                repo_remote_url,
                path,
                git_ref,
                content,
            },
        );
    });
}
