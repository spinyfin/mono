//! Frontend handler routing, shared by the ordered live lane and admitted reads.
use super::*;

pub(super) async fn dispatch_request(ctx: Dispatch, request: FrontendRequest, bulk_read: bool) {
    let attribution = crate::work::db_diagnostics::Attribution {
        verb: variant_name(&request),
        request_id: ctx.request_id.clone(),
        session_id: ctx.session_id.clone(),
        background: String::new(),
    };
    attribution.scope(dispatch_attributed(ctx, request, bulk_read)).await;
}

async fn dispatch_attributed(mut ctx: Dispatch, request: FrontendRequest, bulk_read: bool) {
    if read_admission::read_lane(&request) != read_admission::ReadLane::Ordered
        && !matches!(request, FrontendRequest::GetWorkTree { .. })
    {
        ctx.sink
            .view_reads
            .lock()
            .expect("view reads poisoned")
            .begin(&ctx.request_id, read_admission::key(&request));
    }
    if bulk_read {
        let connection = if matches!(
            request,
            FrontendRequest::ListProposals { .. } | FrontendRequest::GetWorkTree { .. }
        ) {
            ctx.work_db.reader()
        } else {
            ctx.work_db.query_connection()
        };
        match connection {
            Ok(db) => ctx.work_db = Arc::new(db),
            Err(err) => {
                send_response(
                    &ctx.sink,
                    &ctx.request_id,
                    FrontendEvent::Error {
                        message: format!("opening read RPC connection: {err}"),
                    },
                );
                return;
            }
        }
    }
    dispatch_request::dispatch_request(ctx, request).await;
}
