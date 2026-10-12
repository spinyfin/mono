//! Frontend handler routing, shared by the ordered live lane and admitted reads.
use super::*;

pub(super) async fn dispatch_request(mut ctx: Dispatch, request: FrontendRequest, bulk_read: bool) {
    if bulk_read {
        let connection = if matches!(request, FrontendRequest::ListProposals { .. }) {
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
