//! Exact app identity is required to confirm: ancestry is insufficient because
//! the coordinator and workers can also be descendants of the app.
use super::*;

fn allowed(agent: bool, coordinator: bool, app_pid: Option<i32>, peer_pid: Option<i32>, request: bool) -> bool {
    !agent
        && if request {
            coordinator
        } else {
            !coordinator && app_pid.is_some() && app_pid == peer_pid
        }
}

pub(super) async fn handle(ctx: Dispatch, req: FrontendRequest) {
    let state = &ctx.server_state;
    let request = matches!(req, FrontendRequest::RequestCriticalPriority { .. });
    if !allowed(
        state.classify_peer(ctx.peer_pid).is_agent(),
        resolve_status_actor(state, ctx.peer_pid) == boss_protocol::LAST_STATUS_ACTOR_BOSS,
        state.current_app_pid(),
        ctx.peer_pid,
        request,
    ) {
        send_work_error(
            &ctx.sink,
            &ctx.request_id,
            anyhow::anyhow!(
                "critical-priority requests require the coordinator; listing and confirmation require the Boss app itself; workers are always refused"
            ),
        );
        return;
    }
    let result = match req {
        FrontendRequest::RequestCriticalPriority { ids, ask } => {
            let mut resolved = Vec::new();
            for id in ids {
                match state.resolve_work_item_id(&id).await {
                    Ok(id) => resolved.push(id),
                    Err(err) => {
                        send_work_error(&ctx.sink, &ctx.request_id, &err);
                        return;
                    }
                }
            }
            ctx.work_db
                .request_critical(&resolved, &ask)
                .map(|id| FrontendEvent::CriticalPriorityRequested { id })
        }
        FrontendRequest::ListCriticalPriorityRequests => ctx
            .work_db
            .list_critical_requests()
            .map(|requests| FrontendEvent::CriticalPriorityRequests { requests }),
        FrontendRequest::ResolveCriticalPriorityRequest { id, confirm } => {
            match ctx.work_db.resolve_critical_request(&id, confirm) {
                Ok(changed) => {
                    for item_id in changed {
                        if let Ok(item) = ctx.work_db.get_work_item(&item_id) {
                            let product = item.product_id().to_owned();
                            publish_work_invalidation(
                                state,
                                &ctx.session_id,
                                &ctx.request_id,
                                vec![work_product_topic(&product)],
                                "critical_priority_confirmed",
                                Some(product),
                                vec![item_id],
                            )
                            .await;
                        }
                    }
                    Ok(FrontendEvent::CriticalPriorityResolved { id })
                }
                Err(err) => Err(err),
            }
        }
        _ => unreachable!(),
    };
    match result {
        Ok(event) => send_response(&ctx.sink, &ctx.request_id, event),
        Err(err) => send_work_error(&ctx.sink, &ctx.request_id, &err),
    }
}

#[cfg(test)]
mod tests {
    use super::allowed;

    #[test]
    fn only_coordinator_can_request_and_only_exact_app_can_confirm() {
        assert!(allowed(false, true, Some(10), Some(20), true));
        assert!(allowed(false, false, Some(10), Some(10), false));
        assert!(!allowed(false, true, Some(10), Some(20), false));
        assert!(!allowed(false, true, Some(10), Some(10), false));
        assert!(!allowed(false, false, Some(10), Some(30), true));
        assert!(!allowed(false, false, None, None, false));
        assert!(!allowed(false, false, Some(10), None, false));
        for request in [true, false] {
            assert!(!allowed(true, true, Some(10), Some(10), request));
        }
    }
}
