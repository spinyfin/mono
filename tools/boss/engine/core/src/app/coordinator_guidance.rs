//! `FrontendRequest::ListCoordinatorGuidance` handler — `boss guidance show`.
//!
//! Re-reads each product's `BOSS_COORDINATOR.md` from GitHub at the
//! default branch's current HEAD (see [`crate::coordinator_guidance`]).
//! This is the on-demand half of the mechanism: the session-start brief
//! injects the same views at launch, and this verb is how the coordinator
//! sees a change mid-session, retries a failed launch-time read, or
//! inspects which sha it is acting on.
//!
//! Coordinator-only at the worker-tier gate
//! (`boss_worker_policy::worker_verb_decision`).
//!
//! Foreground fetches retain bulk admission through the correlated response,
//! with an additional global fetch limit. A fetch past the limit waits for a
//! slot; it is never answered with an error.

use super::*;

/// Concurrent guidance fetches, across all connections.
pub(super) const MAX_CONCURRENT_FETCHES: usize = 4;

pub(super) async fn handle_list_coordinator_guidance(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListCoordinatorGuidance { product_id } = req else {
        unreachable!()
    };

    let products = match product_id {
        Some(product_id) => match work_db.get_product(&product_id) {
            Ok(Some(product)) => vec![product],
            Ok(None) => {
                send_work_error(&sink, &request_id, format!("product `{product_id}` not found"));
                return;
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        },
        None => match crate::coordinator_guidance::guidance_products(&work_db) {
            Ok(products) => products,
            Err(err) => {
                send_work_error(&sink, &request_id, format!("failed to list products: {err:#}"));
                return;
            }
        },
    };

    let permit = match server_state.guidance_fetches.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            tracing::info!(
                %request_id,
                "coordinator guidance fetch waiting: concurrent fetch limit reached"
            );
            server_state
                .guidance_fetches
                .clone()
                .acquire_owned()
                .await
                .expect("guidance fetch semaphore is never closed")
        }
    };
    let design_docs = server_state.design_docs.clone();
    let _permit = permit;
    let guidance = crate::coordinator_guidance::load_product_guidance(
        &design_docs,
        &work_db,
        &products,
        crate::coordinator_guidance::ON_DEMAND_FETCH_BUDGET,
        boss_engine_utils::epoch_time::now_epoch_secs(),
    )
    .await;
    for view in &guidance {
        tracing::info!(
            product_id = %view.product_id,
            owner_repo = view.owner_repo.as_deref().unwrap_or(""),
            state = view.state.tag(),
            git_ref = view.state.git_ref().unwrap_or(""),
            "coordinator guidance read on demand"
        );
    }
    send_response(&sink, &request_id, FrontendEvent::CoordinatorGuidanceList { guidance });
}
