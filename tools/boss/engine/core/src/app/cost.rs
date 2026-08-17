//! `FrontendRequest` handlers — token-cost reporting (`boss cost`).
//!
//! Each handler is a thin fetch-then-build shim: pull the flat
//! `work_runs` rows the request needs via `WorkDb`, hand them to the pure
//! builders in [`crate::cost_report`], and send the result. See
//! [`super::Dispatch`] for the per-request context every handler
//! receives.

use super::*;

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
        let report = crate::cost_report::build_task_cost_report(
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
        let report = crate::cost_report::build_window_cost_report(
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
        let report = crate::cost_report::build_top_cost_report(
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
