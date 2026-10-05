use super::*;

pub(super) async fn handle_start(ctx: Dispatch, req: FrontendRequest) {
    let FrontendRequest::StartProjectPostmortem { project_id } = req else {
        unreachable!()
    };
    match ctx.work_db.start_project_postmortem(&project_id) {
        Ok((task, created)) => {
            if created {
                if let Err(err) = ctx.work_db.reconcile_product_executions(&task.product_id) {
                    tracing::warn!(%err, "failed to reconcile project postmortem execution");
                }
                ctx.server_state.execution_coordinator.kick();
            }
            let revision = publish_work_invalidation(
                &ctx.server_state,
                &ctx.session_id,
                &ctx.request_id,
                vec![work_product_topic(&task.product_id)],
                "project_postmortem",
                Some(task.product_id.clone()),
                vec![project_id, task.id.clone()],
            )
            .await;
            send_response_with_revision(
                &ctx.sink,
                &ctx.request_id,
                revision,
                FrontendEvent::ProjectPostmortemResult { task, created },
            );
        }
        Err(err) => send_work_error(&ctx.sink, &ctx.request_id, &err),
    }
}
