//! `FrontendRequest` handlers — token-cost reporting (`boss cost`).
//!
//! Each handler is a thin fetch-then-build shim: pull the flat
//! `work_runs` rows the request needs via `WorkDb`, hand them to the pure
//! builders in [`boss_engine_runtime::cost_report`], and send the result. See
//! [`super::Dispatch`] for the per-request context every handler
//! receives.

use super::*;

pub(super) async fn handle_stuck_report(ctx: Dispatch, req: FrontendRequest) {
    let FrontendRequest::GetStuckReport { minimum_age_secs } = req else {
        unreachable!()
    };
    match ctx.work_db.stuck_report(
        minimum_age_secs,
        boss_engine_utils::epoch_time::now_epoch_secs(),
        &|pid| boss_engine_runtime::durable_liveness::probe_recorded_pid(pid).is_alive(),
    ) {
        Ok(report) => send_response(&ctx.sink, &ctx.request_id, FrontendEvent::StuckReport { report }),
        Err(error) => send_work_error(&ctx.sink, &ctx.request_id, error),
    }
}

pub(super) async fn handle_get_work_item_cost_report(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetWorkItemCostReport {
        work_item_id,
        estimate_usd,
    } = req
    else {
        unreachable!()
    };
    let work_item_id = match server_state.resolve_work_item_id(&work_item_id).await {
        Ok(id) => id,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };
    {
        let task = match work_db.get_work_item(&work_item_id) {
            Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => task,
            Ok(WorkItem::Product(_) | WorkItem::Project(_)) => {
                return send_work_error(
                    &sink,
                    &request_id,
                    format!(
                        "{work_item_id} is a product/project, not a task — cost reports run/execution rows, which only tasks have"
                    ),
                );
            }
            Err(err) => return send_work_error(&sink, &request_id, &err),
        };
        let records = match work_db.cost_records_for_work_item(&task.id) {
            Ok(records) => records,
            Err(err) => return send_work_error(&sink, &request_id, &err),
        };
        let generated_at_epoch_s = boss_engine_utils::epoch_time::now_epoch_secs();
        let report = boss_engine_runtime::cost_report::build_task_cost_report(
            &task.id,
            task.short_id,
            &task.name,
            &records,
            estimate_usd,
            generated_at_epoch_s,
        );
        send_response(&sink, &request_id, FrontendEvent::WorkItemCostReport { report });
    }
}

pub(super) async fn handle_get_cost_window_report(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetCostWindowReport {
        since_epoch_s,
        until_epoch_s,
        estimate_usd,
    } = req
    else {
        unreachable!()
    };
    {
        let records = match work_db.cost_records_for_window(since_epoch_s, until_epoch_s) {
            Ok(records) => records,
            Err(err) => return send_work_error(&sink, &request_id, &err),
        };
        let generated_at_epoch_s = boss_engine_utils::epoch_time::now_epoch_secs();
        let report = boss_engine_runtime::cost_report::build_window_cost_report(
            since_epoch_s,
            until_epoch_s,
            &records,
            estimate_usd,
            generated_at_epoch_s,
        );
        send_response(&sink, &request_id, FrontendEvent::CostWindowReport { report });
    }
}

pub(super) async fn handle_get_top_cost_consumers(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetTopCostConsumers {
        since_epoch_s,
        until_epoch_s,
        limit,
        estimate_usd,
    } = req
    else {
        unreachable!()
    };
    {
        let records = match work_db.cost_records_for_window(since_epoch_s, until_epoch_s) {
            Ok(records) => records,
            Err(err) => return send_work_error(&sink, &request_id, &err),
        };
        let generated_at_epoch_s = boss_engine_utils::epoch_time::now_epoch_secs();
        let report = boss_engine_runtime::cost_report::build_top_cost_report(
            since_epoch_s,
            until_epoch_s,
            limit,
            &records,
            estimate_usd,
            generated_at_epoch_s,
        );
        send_response(&sink, &request_id, FrontendEvent::TopCostConsumers { report });
    }
}

pub(super) async fn handle_get_run_history(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetRunHistory {
        since_epoch_s,
        until_epoch_s,
        driver,
        kind,
        status,
        work_item_id,
        limit,
    } = req
    else {
        unreachable!()
    };

    let resolved_work_item_id = match work_item_id {
        Some(raw) => match server_state.resolve_work_item_id(&raw).await {
            Ok(id) => Some(id),
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        },
        None => None,
    };

    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    // Bound windowless queries when not scoped to a single work item.
    let (since_epoch_s, until_epoch_s) = if since_epoch_s.is_none() && resolved_work_item_id.is_none() {
        (Some(now - 86_400), until_epoch_s.or(Some(now)))
    } else {
        (since_epoch_s, until_epoch_s)
    };

    let query_limit = limit.map(|n| n.saturating_add(1));
    let mut records = match work_db.run_records(
        &boss_work_db::RunQueryFilter::builder()
            .maybe_since_epoch_s(since_epoch_s)
            .maybe_until_epoch_s(until_epoch_s)
            .maybe_driver(driver.as_deref())
            .maybe_kind(kind.as_deref())
            .maybe_status(status.as_deref())
            .maybe_work_item_id(resolved_work_item_id.as_deref())
            .maybe_limit(query_limit)
            .newest_first(true)
            .build(),
    ) {
        Ok(records) => records,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };

    let truncated = limit.is_some_and(|n| records.len() > n as usize);
    if let Some(n) = limit {
        records.truncate(n as usize);
    }

    let report = boss_engine_runtime::cost_report::build_runs_list_report(
        &records,
        since_epoch_s,
        until_epoch_s,
        driver,
        kind,
        status,
        resolved_work_item_id,
        limit,
        truncated,
        now,
    );
    send_response(&sink, &request_id, FrontendEvent::RunHistory { report });
}
