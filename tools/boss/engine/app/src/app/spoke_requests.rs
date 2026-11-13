//! The online worker allowlist, after authenticated durable ingress.
//!
//! No frontend dispatch or peer-pid impersonation: each read reuses the local
//! handler's attributed implementation with the work item derived by the hub.

use super::{ServerState, context, pr_status, proposals::AttributedCaller};
use boss_engine_runtime::hub::projection::{WorkerObservation, WorkerRead, WorkerResponse};
use boss_host_enrollment::HostState;
use boss_protocol::{FrontendEvent, ProposalErrorCode};

pub(super) async fn read(state: &ServerState, observation: &WorkerObservation, method: WorkerRead) -> WorkerResponse {
    let Some(hub) = state.hub_stream() else {
        return WorkerResponse::Denied("worker_host_denied: hub credentials are unavailable".into());
    };
    let credentials = hub.credentials().clone();
    let host_id = observation.origin.host_id.clone();
    // Receipt authenticated the certificate. Projection can happen after a
    // restart or revocation, so recheck host authorization before reading or
    // refreshing anything. Disabled scheduling still permits existing runs.
    match tokio::task::spawn_blocking(move || credentials.status(&host_id)).await {
        Ok(Ok(Some(status))) if status.state == HostState::Enrolled => {}
        Ok(Ok(_)) => return WorkerResponse::Denied("worker_host_denied: host is not enrolled or is revoked".into()),
        Ok(Err(error)) => return WorkerResponse::Retry(error.to_string()),
        Err(error) => return WorkerResponse::Retry(error.to_string()),
    }
    let execution_id = &observation.origin.run.execution_id;
    let work_item_id = match state.work_db.work_item_for_execution(execution_id) {
        Ok(Some(id)) => id,
        Ok(None) => return WorkerResponse::Denied("worker_run_denied: execution is unknown".into()),
        Err(error) => return WorkerResponse::Retry(error.to_string()),
    };
    let caller = AttributedCaller {
        execution_id: execution_id.clone(),
        work_item_id,
    };
    let event = match method {
        WorkerRead::Context => match context::build_bundle(&state.work_db, &caller.work_item_id) {
            Ok(bundle) => FrontendEvent::WorkerContextResult {
                bundle: Box::new(bundle),
            },
            Err(context::AttributedReadError::Transient(error)) => return WorkerResponse::Retry(error.to_string()),
            Err(context::AttributedReadError::Denied(reason)) => return WorkerResponse::Denied(reason),
        },
        WorkerRead::PrStatus { refresh } => {
            pr_status::status_for_caller_timed(
                state,
                &state.work_db,
                &caller,
                refresh,
                Some(pr_status::HUB_PR_STATUS_PROBE_TIMEOUT),
            )
            .await
        }
        WorkerRead::PrBody => pr_status::body_for_caller(&state.work_db, &caller),
    };
    match event {
        FrontendEvent::ProposalRejected { error } if error.code == ProposalErrorCode::Internal => {
            WorkerResponse::Retry(error.message)
        }
        FrontendEvent::ProposalRejected { error } => WorkerResponse::Denied(error.message),
        event => WorkerResponse::Read(Box::new(event)),
    }
}
