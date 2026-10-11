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
//! The GitHub work is `tokio::spawn`ed rather than awaited inline so a slow
//! round trip (up to the 45 s on-demand budget) does not hold bulk-read
//! permits or the per-connection ordering turn while idle on the network. A
//! small dedicated limit bounds the detached fetches instead.

use super::*;

/// Concurrent detached guidance fetches, across all connections.
const MAX_CONCURRENT_FETCHES: usize = 4;
static FETCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_CONCURRENT_FETCHES);

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

    let Ok(permit) = FETCHES.try_acquire() else {
        send_work_error(&sink, &request_id, read_admission::BUSY);
        return;
    };
    let design_docs = server_state.design_docs.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let guidance = crate::coordinator_guidance::load_product_guidance(
            &design_docs,
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
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saturating the detached fetch limit leaves bulk read admission free.
    #[tokio::test]
    async fn saturated_guidance_fetches_do_not_consume_bulk_permits() {
        let held: Vec<_> = (0..MAX_CONCURRENT_FETCHES)
            .map(|_| FETCHES.try_acquire().unwrap())
            .collect();
        assert!(FETCHES.try_acquire().is_err());
        let gate = std::sync::Arc::new(read_admission::ReadAdmission::default());
        let connection = gate.connection();
        assert!(gate.enqueue(&connection).unwrap().acquire().await.is_ok());
        drop(held);
        assert!(FETCHES.try_acquire().is_ok());
    }
}
