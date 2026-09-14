//! `FrontendRequest` handlers — metric catalog and series (`boss metrics`).
//!
//! Thin fetch-then-build shims: pull the requested window of facts via
//! `WorkDb`, hand them to the pure builders in [`crate::metric_series`],
//! and send the result. See [`super::Dispatch`] for the per-request context.

use super::*;

use crate::metric_series::{
    CATALOG_LOOKBACK_SECS, CoverageOverride, SeriesQuery, SeriesSource, build_catalog,
    build_execution_series_report_with_coverage, build_task_series_report_with_coverage, parse_bucket, validate_query,
};
use crate::work::MetricExecutionFactOptions;

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
    let lookback_since = until.saturating_sub(CATALOG_LOOKBACK_SECS).max(0);
    let execution_facts = match work_db.metric_execution_facts(
        lookback_since,
        until,
        None,
        None,
        false,
        MetricExecutionFactOptions {
            first_pr_only: false,
            filters: &[],
        },
    ) {
        Ok(facts) => facts,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let task_facts = match work_db.metric_task_facts(lookback_since, until) {
        Ok(facts) => facts,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let catalog = build_catalog(&execution_facts, &task_facts, generated_at_epoch_s, lookback_since);
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
            require_duration,
            require_pr_url,
            statuses,
            unique_by_pr_url,
        } => {
            let rows = match work_db.metric_execution_facts(
                since_epoch_s,
                until_epoch_s,
                kinds,
                statuses,
                require_pr_url,
                MetricExecutionFactOptions {
                    first_pr_only: unique_by_pr_url,
                    filters: &filters,
                },
            ) {
                Ok(rows) => rows,
                Err(err) => return send_work_error(&sink, &request_id, &err),
            };
            let (data_from_epoch_s, dimension_from_epoch_s) = match work_db.metric_execution_coverage(
                kinds,
                statuses,
                require_pr_url,
                require_duration,
                &filters,
                group_by.as_deref(),
            ) {
                Ok(minima) => minima,
                Err(err) => return send_work_error(&sink, &request_id, &err),
            };
            build_execution_series_report_with_coverage(
                &query,
                &rows,
                generated_at_epoch_s,
                CoverageOverride {
                    data_from_epoch_s,
                    dimension_from_epoch_s,
                },
            )
        }
        SeriesSource::Tasks => {
            let rows = match work_db.metric_task_facts(since_epoch_s, until_epoch_s) {
                Ok(rows) => rows,
                Err(err) => return send_work_error(&sink, &request_id, &err),
            };
            let (data_from_epoch_s, dimension_from_epoch_s) =
                match work_db.metric_task_coverage(&filters, group_by.as_deref()) {
                    Ok(minima) => minima,
                    Err(err) => return send_work_error(&sink, &request_id, &err),
                };
            build_task_series_report_with_coverage(
                &query,
                &rows,
                generated_at_epoch_s,
                CoverageOverride {
                    data_from_epoch_s,
                    dimension_from_epoch_s,
                },
            )
        }
    };
    match report {
        Ok(report) => send_response(&sink, &request_id, FrontendEvent::MetricSeriesResult { report }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}
