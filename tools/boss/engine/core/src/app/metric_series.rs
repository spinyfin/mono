//! `FrontendRequest` handlers — metric catalog and series (`boss metrics`).
//!
//! Thin fetch-then-build shims: pull the windowed facts via `WorkDb`,
//! hand them to the pure builders in [`crate::metric_series`], and send
//! the result. See [`super::Dispatch`] for the per-request context.

use super::*;

use crate::metric_series::{
    SeriesQuery, SeriesSource, build_catalog, build_execution_series_report, build_task_series_report, parse_bucket,
    validate_query,
};

pub(super) async fn handle_get_metric_catalog(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetMetricCatalog = req else {
        unreachable!()
    };
    let generated_at_epoch_s = boss_engine_utils::epoch_time::now_epoch_secs();
    let until = generated_at_epoch_s.saturating_add(1);
    let execution_facts = match work_db.metric_execution_facts(0, until, None, None, false) {
        Ok(facts) => facts,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let task_facts = match work_db.metric_task_facts(0, until) {
        Ok(facts) => facts,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let catalog = build_catalog(&execution_facts, &task_facts, generated_at_epoch_s);
    send_response(&sink, &request_id, FrontendEvent::MetricCatalogResult { catalog });
}

pub(super) async fn handle_get_metric_series(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetMetricSeries {
        series,
        since_epoch_s,
        until_epoch_s,
        bucket,
        filters,
        group_by,
    } = req
    else {
        unreachable!()
    };
    let bucket = match parse_bucket(bucket.as_deref()) {
        Ok(bucket) => bucket,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let query = SeriesQuery {
        series: &series,
        since_epoch_s,
        until_epoch_s,
        bucket,
        group_by: group_by.as_deref(),
        filters: &filters,
    };
    let spec = match validate_query(&query) {
        Ok(spec) => spec,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let generated_at_epoch_s = boss_engine_utils::epoch_time::now_epoch_secs();
    let report = match spec.source {
        SeriesSource::Executions {
            kinds,
            require_pr_url,
            statuses,
            ..
        } => match work_db.metric_execution_facts(since_epoch_s, until_epoch_s, kinds, statuses, require_pr_url) {
            Ok(rows) => build_execution_series_report(&query, &rows, generated_at_epoch_s),
            Err(err) => return send_work_error(&sink, &request_id, &err),
        },
        SeriesSource::Tasks => match work_db.metric_task_facts(since_epoch_s, until_epoch_s) {
            Ok(rows) => build_task_series_report(&query, &rows, generated_at_epoch_s),
            Err(err) => return send_work_error(&sink, &request_id, &err),
        },
    };
    match report {
        Ok(report) => send_response(&sink, &request_id, FrontendEvent::MetricSeriesResult { report }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}
