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
use boss_protocol::DesignDocContent;

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
        let emit = |content: DesignDocContent, as_push: bool| {
            let event = FrontendEvent::ProductDesignDocContent {
                repo_remote_url: repo_remote_url.clone(),
                path: path.clone(),
                git_ref: git_ref.clone(),
                content,
            };
            if as_push {
                send_push(&sink, event);
            } else {
                send_response(&sink, &request_id, event);
            }
        };

        // Serve immediately: cache hit does not wait on GitHub. A SHA
        // ref never needs a follow-up; a branch ref is revalidated
        // below and the view updates only if the body changed or the
        // refresh failed (stale banner, cache kept).
        let first = design_docs.open_markdown_doc(&repo_remote_url, &path, &git_ref).await;
        let was_loaded = matches!(first, DesignDocContent::Loaded { .. });
        emit(first, false);

        if !was_loaded || design_docs_ref_is_immutable(&git_ref) {
            return;
        }

        if let Some(update) = design_docs
            .revalidate_markdown_doc(&repo_remote_url, &path, &git_ref)
            .await
        {
            let still_retryable = update.retryable();
            emit(update, true);
            if still_retryable {
                auto_retry_revalidation(&design_docs, &repo_remote_url, &path, &git_ref, &sink).await;
            }
        }
    });
}

fn design_docs_ref_is_immutable(git_ref: &str) -> bool {
    boss_engine_design_docs::is_immutable_git_ref(git_ref)
}

/// Backed-off revalidation after a failed refresh. Does not hammer:
/// three attempts at 2s / 8s / 32s, then stop until the operator
/// retries. Each attempt still serves the cache; a success or a
/// non-retryable outcome ends the loop.
async fn auto_retry_revalidation(
    design_docs: &boss_engine_design_docs::DesignDocsService,
    repo_remote_url: &str,
    path: &str,
    git_ref: &str,
    sink: &std::sync::Arc<super::SessionSink>,
) {
    const DELAYS: [std::time::Duration; 3] = [
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(8),
        std::time::Duration::from_secs(32),
    ];
    for delay in DELAYS {
        tokio::time::sleep(delay).await;
        match design_docs
            .revalidate_markdown_doc(repo_remote_url, path, git_ref)
            .await
        {
            Some(content) => {
                let retryable = content.retryable();
                send_push(
                    sink,
                    FrontendEvent::ProductDesignDocContent {
                        repo_remote_url: repo_remote_url.to_owned(),
                        path: path.to_owned(),
                        git_ref: git_ref.to_owned(),
                        content,
                    },
                );
                if !retryable {
                    return;
                }
            }
            None => return,
        }
    }
}
