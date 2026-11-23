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
    let (since_epoch_s, until_epoch_s) =
        resolve_run_history_bounds(since_epoch_s, until_epoch_s, resolved_work_item_id.is_some(), now);

    let mut records = match work_db.run_records(
        &boss_work_db::RunQueryFilter::builder()
            .maybe_since_epoch_s(since_epoch_s)
            .maybe_until_epoch_s(until_epoch_s)
            .maybe_driver(driver.as_deref())
            .maybe_kind(kind.as_deref())
            .maybe_status(status.as_deref())
            .maybe_work_item_id(resolved_work_item_id.as_deref())
            .maybe_limit(run_history_fetch_limit(limit))
            .newest_first(true)
            .build(),
    ) {
        Ok(records) => records,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };

    let truncated = truncate_run_history_rows(&mut records, limit);

    let report = boss_engine_runtime::cost_report::build_runs_list_report(
        &records,
        &boss_engine_runtime::cost_report::RunsListQuery::builder()
            .maybe_since_epoch_s(since_epoch_s)
            .maybe_until_epoch_s(until_epoch_s)
            .maybe_driver_filter(driver)
            .maybe_kind_filter(kind)
            .maybe_status_filter(status)
            .maybe_work_item_filter(resolved_work_item_id)
            .maybe_limit(limit)
            .build(),
        truncated,
        now,
    );
    send_response(&sink, &request_id, FrontendEvent::RunHistory { report });
}

/// Default window when `GetRunHistory` is called with neither `since` nor a
/// work-item owner, so a windowless listing cannot walk the whole table.
pub(crate) const DEFAULT_RUN_HISTORY_WINDOW_SECS: i64 = 86_400;

pub(crate) fn resolve_run_history_bounds(
    since_epoch_s: Option<i64>,
    until_epoch_s: Option<i64>,
    has_work_item: bool,
    now_epoch_s: i64,
) -> (Option<i64>, Option<i64>) {
    if since_epoch_s.is_none() && !has_work_item {
        (
            Some(now_epoch_s.saturating_sub(DEFAULT_RUN_HISTORY_WINDOW_SECS)),
            until_epoch_s.or(Some(now_epoch_s)),
        )
    } else {
        (since_epoch_s, until_epoch_s)
    }
}

/// Fetch one extra row so the handler can set `truncated` without a second query.
pub(crate) fn run_history_fetch_limit(limit: Option<u32>) -> Option<u32> {
    limit.map(|n| n.saturating_add(1))
}

pub(crate) fn truncate_run_history_rows<T>(rows: &mut Vec<T>, limit: Option<u32>) -> bool {
    let truncated = limit.is_some_and(|n| rows.len() > n as usize);
    if let Some(n) = limit {
        rows.truncate(n as usize);
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_RUN_HISTORY_WINDOW_SECS, resolve_run_history_bounds, run_history_fetch_limit, truncate_run_history_rows,
    };

    #[test]
    fn windowless_unscoped_query_is_bounded_to_the_last_24h() {
        let now = 2_000_000_000;
        let (since, until) = resolve_run_history_bounds(None, None, false, now);
        assert_eq!(since, Some(now - DEFAULT_RUN_HISTORY_WINDOW_SECS));
        assert_eq!(until, Some(now));
    }

    #[test]
    fn explicit_since_is_left_untouched() {
        let now = 2_000_000_000;
        let (since, until) = resolve_run_history_bounds(Some(50), Some(80), false, now);
        assert_eq!(since, Some(50));
        assert_eq!(until, Some(80));
    }

    #[test]
    fn work_item_scope_does_not_invent_a_window() {
        let now = 2_000_000_000;
        let (since, until) = resolve_run_history_bounds(None, None, true, now);
        assert_eq!(since, None);
        assert_eq!(until, None);
    }

    #[test]
    fn missing_until_on_a_windowless_unscoped_query_defaults_to_now() {
        let now = 2_000_000_000;
        let (since, until) = resolve_run_history_bounds(None, Some(now - 10), false, now);
        assert_eq!(since, Some(now - DEFAULT_RUN_HISTORY_WINDOW_SECS));
        assert_eq!(until, Some(now - 10));
    }

    #[test]
    fn limit_plus_one_marks_truncation_and_keeps_limit_rows() {
        assert_eq!(run_history_fetch_limit(Some(2)), Some(3));
        let mut rows = vec!["a", "b", "c"];
        assert!(truncate_run_history_rows(&mut rows, Some(2)));
        assert_eq!(rows, vec!["a", "b"]);
    }

    #[test]
    fn limit_none_keeps_every_row() {
        assert_eq!(run_history_fetch_limit(None), None);
        let mut rows = vec!["a", "b"];
        assert!(!truncate_run_history_rows(&mut rows, None));
        assert_eq!(rows, vec!["a", "b"]);
    }

    #[test]
    fn limit_zero_returns_no_rows_and_truncated_when_any_matched() {
        assert_eq!(run_history_fetch_limit(Some(0)), Some(1));
        let mut rows = vec!["a"];
        assert!(truncate_run_history_rows(&mut rows, Some(0)));
        assert!(rows.is_empty());
        let mut empty: Vec<&str> = Vec::new();
        assert!(!truncate_run_history_rows(&mut empty, Some(0)));
    }
}
