//! `FrontendRequest` handlers — product decision records (T-B2-decision).
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. See [`super::Dispatch`] for the
//! per-request context every handler receives.

use super::*;

pub(super) async fn handle_create_decision(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateDecision { input } = req else {
        unreachable!()
    };
    match work_db.create_decision(input) {
        Ok(decision) => send_response(&sink, &request_id, FrontendEvent::DecisionCreated { decision }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_get_decision(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetDecision { id } = req else {
        unreachable!()
    };
    match work_db.get_decision(&id) {
        Ok(Some(decision)) => send_response(&sink, &request_id, FrontendEvent::DecisionResult { decision }),
        Ok(None) => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: format!("unknown decision: {id}"),
            },
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_list_decisions(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListDecisions {
        product_id,
        include_inactive,
    } = req
    else {
        unreachable!()
    };
    match work_db.list_decisions(&product_id, include_inactive) {
        Ok(decisions) => send_response(
            &sink,
            &request_id,
            FrontendEvent::DecisionsList { product_id, decisions },
        ),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_revoke_decision(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RevokeDecision { id } = req else {
        unreachable!()
    };
    match work_db.revoke_decision(&id) {
        Ok(decision) => send_response(&sink, &request_id, FrontendEvent::DecisionUpdated { decision }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_supersede_decision(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SupersedeDecision { id, successor_id } = req else {
        unreachable!()
    };
    match work_db.supersede_decision(&id, &successor_id) {
        Ok(decision) => send_response(&sink, &request_id, FrontendEvent::DecisionUpdated { decision }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}
