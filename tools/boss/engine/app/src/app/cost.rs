//! `FrontendRequest` handlers — token-cost reporting (`boss cost`).
//!
//! Each handler is a thin fetch-then-build shim: pull the flat
//! `work_runs` rows the request needs via `WorkDb`, hand them to the pure
//! builders in [`boss_engine_runtime::cost_report`] (or, for the usage rows
//! request, [`boss_usage_report`]), and send the result. See
//! [`super::Dispatch`] for the per-request context every handler
//! receives.

use super::*;

use boss_usage_report::{
    PricedTokens, UsageRowsQuery, UsageSeriesQuery, UsdEstimateResult, UsdEstimator, build_usage_rows_report,
    build_usage_series_report,
};

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

struct EngineUsdEstimator;

impl UsdEstimator for EngineUsdEstimator {
    fn estimate_usd(&self, model: &str, tokens: PricedTokens) -> Option<f64> {
        self.estimate_usd_detailed(model, tokens).map(|est| est.usd)
    }

    fn estimate_usd_detailed(&self, model: &str, tokens: PricedTokens) -> Option<UsdEstimateResult> {
        let price = boss_engine_runtime::cost_pricing::price_for_model(model)?;
        let est = boss_engine_runtime::cost_pricing::estimate_usd_detailed(
            price,
            tokens.input,
            tokens.output,
            tokens.cache_creation,
            tokens.cache_creation_5m,
            tokens.cache_creation_1h,
            tokens.cache_read,
        )?;
        Some(UsdEstimateResult {
            usd: est.usd,
            unsplit_cache_write: est.used_unsplit_cache_write_rate,
        })
    }
}

const ENGINE_USD_ESTIMATOR: EngineUsdEstimator = EngineUsdEstimator;

/// Run-grain usage rows for the window. The engine constants the report
/// depends on (capture start, retention bound, rate table) are handed to the
/// pure builder as data, so the builder crate owns none of them.
pub(super) async fn handle_get_token_usage_rows(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetTokenUsageRows(boss_protocol::TokenUsageRowsRequest {
        since_epoch_s,
        until_epoch_s,
        filters,
        group_by,
        roll_up_revisions,
        estimate_usd,
    }) = req
    else {
        unreachable!()
    };
    let query = UsageRowsQuery::builder()
        .since_epoch_s(since_epoch_s)
        .until_epoch_s(until_epoch_s)
        .generated_at_epoch_s(boss_engine_utils::epoch_time::now_epoch_secs())
        .capture_start_epoch_s(boss_engine_runtime::cost_report::TOKEN_CAPTURE_START_EPOCH_S)
        .retention_max_age_secs(boss_work_db::DEFAULT_RETENTION_MAX_AGE_SECS)
        .filters(filters)
        .group_by(group_by)
        .roll_up_revisions(roll_up_revisions)
        .build();
    if let Err(message) = query.validate() {
        return send_work_error(&sink, &request_id, message);
    }
    let records = match work_db.usage_records_for_window(since_epoch_s, until_epoch_s) {
        Ok(records) => records,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let report = build_usage_rows_report(&query, &records, estimate_usd.then_some(&ENGINE_USD_ESTIMATOR));
    send_response(&sink, &request_id, FrontendEvent::TokenUsageRows { report });
}

/// Bucketed token usage series over time with engine-computed normalizers and
/// range coverage notes.
pub(super) async fn handle_get_token_usage_series(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetTokenUsageSeries(boss_protocol::TokenUsageSeriesRequest {
        since_epoch_s,
        until_epoch_s,
        bucket,
        group_by,
        filters,
        estimate_usd,
    }) = req
    else {
        unreachable!()
    };
    let query = UsageSeriesQuery::builder()
        .since_epoch_s(since_epoch_s)
        .until_epoch_s(until_epoch_s)
        .generated_at_epoch_s(boss_engine_utils::epoch_time::now_epoch_secs())
        .capture_start_epoch_s(boss_engine_runtime::cost_report::TOKEN_CAPTURE_START_EPOCH_S)
        .completed_at_start_epoch_s(boss_engine_runtime::cost_report::COMPLETED_AT_MIGRATION_EPOCH_S)
        .retention_max_age_secs(boss_work_db::DEFAULT_RETENTION_MAX_AGE_SECS)
        .maybe_bucket(bucket)
        .maybe_group_by(group_by)
        .filters(filters)
        .build();
    if let Err(message) = query.validate() {
        return send_work_error(&sink, &request_id, message);
    }
    let records = match work_db.usage_series_records_for_window(since_epoch_s, until_epoch_s) {
        Ok(records) => records,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let report = build_usage_series_report(&query, &records, estimate_usd.then_some(&ENGINE_USD_ESTIMATOR));
    send_response(&sink, &request_id, FrontendEvent::TokenUsageSeries { report });
}

/// What automated reviews in the window found and what became of it. A
/// fetch-then-build shim over one `WorkDb` read; the builder is pure and lives
/// with the classifier it shares its definitions with.
pub(super) async fn handle_get_review_yield_report(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetReviewYieldReport(request) = req else {
        unreachable!()
    };
    if request.until_epoch_s <= request.since_epoch_s {
        return send_work_error(&sink, &request_id, "the window is empty: `until` must be after `since`");
    }
    if let Some(key) = &request.experiment
        && !boss_feature_flags::is_valid_key(key)
    {
        return send_work_error(
            &sink,
            &request_id,
            format!("`{key}` is not an experiment key: use lowercase letters, digits and underscores"),
        );
    }
    let generated_at_epoch_s = boss_engine_utils::epoch_time::now_epoch_secs();
    match work_db.review_yield_report(&request, generated_at_epoch_s) {
        Ok(report) => send_response(&sink, &request_id, FrontendEvent::ReviewYieldReport { report }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_session_sink, test_server_state};
    use super::*;
    use boss_protocol::{UsageDimension, UsageFilter};

    async fn ask(
        state: &Arc<ServerState>,
        request: FrontendRequest,
        request_id: &str,
    ) -> (Option<String>, FrontendEvent) {
        ask_handler(state, request, request_id, handle_get_token_usage_rows).await
    }

    async fn ask_handler<Handler, Done>(
        state: &Arc<ServerState>,
        request: FrontendRequest,
        request_id: &str,
        handler: Handler,
    ) -> (Option<String>, FrontendEvent)
    where
        Handler: FnOnce(Dispatch, FrontendRequest) -> Done,
        Done: std::future::Future<Output = ()>,
    {
        let sink = make_session_sink();
        let dispatch = Dispatch::builder()
            .server_state(state.clone())
            .work_db(state.work_db.clone())
            .sink(sink.clone())
            .session_id("session-usage")
            .request_id(request_id)
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build();
        handler(dispatch, request).await;
        sink.close();
        let envelope = sink.next().await.expect("the handler sends a reply");
        assert!(sink.next().await.is_none(), "the handler sends exactly one reply");
        (envelope.request_id, envelope.payload)
    }

    fn request(group_by: Vec<UsageDimension>, filters: Vec<UsageFilter>) -> FrontendRequest {
        FrontendRequest::GetTokenUsageRows(boss_protocol::TokenUsageRowsRequest {
            since_epoch_s: 1_000,
            until_epoch_s: 9_000,
            filters,
            group_by,
            roll_up_revisions: true,
            estimate_usd: false,
        })
    }

    #[tokio::test]
    async fn token_usage_rows_replies_correlated_with_the_windows_runs() {
        let (state, _dir) = test_server_state();
        let conn = state.work_db.connect().unwrap();
        conn.execute_batch(
            "INSERT INTO products (id, name, slug, status, created_at, updated_at) VALUES ('p', 'P', 'p', 'active', '1', '1');
             INSERT INTO tasks (id, product_id, kind, name, status, created_at, updated_at) VALUES ('t', 'p', 'chore', 'T', 'done', '1', '1');
             INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, driver)
               VALUES ('e1', 't', 'chore_implementation', 'completed', 'u', '2000', 'claude'),
                      ('e2', 't', 'chore_implementation', 'completed', 'u', '3000', 'grok');
             INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens)
               VALUES ('r1', 'e1', 'w', 'completed', '2000', 10, 5, 1, 2);
             INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, input_tokens, output_tokens, cache_read_tokens)
               VALUES ('r2', 'e2', 'w', 'completed', '3000', 7, 3, 4);",
        )
        .unwrap();
        drop(conn);

        let (request_id, payload) = ask(&state, request(Vec::new(), Vec::new()), "req-usage-1").await;
        assert_eq!(request_id.as_deref(), Some("req-usage-1"));
        let FrontendEvent::TokenUsageRows { report } = payload else {
            panic!("expected TokenUsageRows, got {payload:?}");
        };
        assert_eq!(report.overall.runs_total, 2);
        assert_eq!(
            report.coverage.runs_partial, 1,
            "the Grok-shaped run lacks cache creation"
        );
        assert_eq!(report.tasks.len(), 1);
        assert_eq!(report.tasks[0].executions.len(), 2);

        // Grouping and filtering reach the builder.
        let (_, payload) = ask(
            &state,
            request(
                vec![UsageDimension::Driver],
                vec![UsageFilter {
                    dimension: UsageDimension::Driver,
                    values: vec!["grok".into()],
                }],
            ),
            "req-usage-2",
        )
        .await;
        let FrontendEvent::TokenUsageRows { report } = payload else {
            panic!("expected TokenUsageRows, got {payload:?}");
        };
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].key[0].value, "grok");
        assert_eq!(report.overall.runs_total, 1);
    }

    #[tokio::test]
    async fn token_usage_rows_rejects_a_grouping_the_reply_cannot_express() {
        let (state, _dir) = test_server_state();
        let (_, payload) = ask(
            &state,
            request(
                vec![UsageDimension::Driver, UsageDimension::Model, UsageDimension::Host],
                Vec::new(),
            ),
            "req-usage-3",
        )
        .await;
        let FrontendEvent::WorkError { message } = payload else {
            panic!("expected WorkError, got {payload:?}");
        };
        assert!(message.contains("at most 2"), "{message}");
    }

    #[tokio::test]
    async fn token_usage_rows_usd_prices_cache_write_split_and_labels_unsplit() {
        let (state, _dir) = test_server_state();
        let conn = state.work_db.connect().unwrap();
        conn.execute_batch(
            "INSERT INTO products (id, name, slug, status, created_at, updated_at) VALUES ('p', 'P', 'p', 'active', '1', '1');
             INSERT INTO tasks (id, product_id, kind, name, status, created_at, updated_at) VALUES ('t', 'p', 'chore', 'T', 'done', '1', '1');
             INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, driver)
               VALUES ('e1', 't', 'chore_implementation', 'completed', 'u', '2000', 'claude'),
                      ('e2', 't', 'chore_implementation', 'completed', 'u', '3000', 'claude'),
                      ('e3', 't', 'chore_implementation', 'completed', 'u', '4000', 'claude');
             INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, model, input_tokens, output_tokens, cache_creation_tokens, cache_creation_5m_tokens, cache_creation_1h_tokens, cache_read_tokens)
               VALUES ('r1', 'e1', 'w', 'completed', '2000', 'claude-fable-5-2', 0, 0, 1000000, 400000, 600000, 0);
             INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens)
               VALUES ('r2', 'e2', 'w', 'completed', '3000', 'claude-fable-5-2', 0, 0, 1000000, 0);
             INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, model, input_tokens, output_tokens, cache_creation_tokens, cache_creation_5m_tokens, cache_creation_1h_tokens, cache_read_tokens)
               VALUES ('r3', 'e3', 'w', 'completed', '4000', 'claude-haiku-4-5-20251001', 0, 0, 1000000, 400000, 600000, 0);",
        )
        .unwrap();
        drop(conn);

        let req = FrontendRequest::GetTokenUsageRows(boss_protocol::TokenUsageRowsRequest {
            since_epoch_s: 1_000,
            until_epoch_s: 9_000,
            filters: Vec::new(),
            group_by: Vec::new(),
            roll_up_revisions: true,
            estimate_usd: true,
        });
        let (_, payload) = ask(&state, req, "req-usage-usd").await;
        let FrontendEvent::TokenUsageRows { report } = payload else {
            panic!("expected TokenUsageRows, got {payload:?}");
        };
        let r1 = &report.tasks[0].executions[0].runs[0];
        let r2 = &report.tasks[0].executions[1].runs[0];
        let r3 = &report.tasks[0].executions[2].runs[0];
        // r1: 0.4M * $10 + 0.6M * $16 = $13.60; no unsplit_cache_write caveat.
        assert!((r1.estimated_usd.unwrap() - 13.60).abs() < 1e-9);
        assert!(
            !r1.caveats
                .iter()
                .any(|c| c.code == boss_usage_report::caveat::UNSPLIT_CACHE_WRITE)
        );
        // r2: 1M * $10 = $10.00; carries unsplit_cache_write caveat.
        assert!((r2.estimated_usd.unwrap() - 10.00).abs() < 1e-9);
        assert!(
            r2.caveats
                .iter()
                .any(|c| c.code == boss_usage_report::caveat::UNSPLIT_CACHE_WRITE)
        );
        // r3: Haiku with split 1h writes falls back to single $1.00 rate and carries unsplit_cache_write caveat.
        assert!((r3.estimated_usd.unwrap() - 1.00).abs() < 1e-9);
        assert!(
            r3.caveats
                .iter()
                .any(|c| c.code == boss_usage_report::caveat::UNSPLIT_CACHE_WRITE)
        );
        assert_eq!(report.overall.unsplit_cache_write_runs, 2);
    }

    async fn ask_series(
        state: &Arc<ServerState>,
        request: FrontendRequest,
        request_id: &str,
    ) -> (Option<String>, FrontendEvent) {
        let sink = make_session_sink();
        let dispatch = Dispatch::builder()
            .server_state(state.clone())
            .work_db(state.work_db.clone())
            .sink(sink.clone())
            .session_id("session-usage-series")
            .request_id(request_id)
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build();
        handle_get_token_usage_series(dispatch, request).await;
        sink.close();
        let envelope = sink.next().await.expect("the handler sends a reply");
        assert!(sink.next().await.is_none(), "the handler sends exactly one reply");
        (envelope.request_id, envelope.payload)
    }

    #[tokio::test]
    async fn token_usage_series_replies_correlated_and_validates_window() {
        let (state, _dir) = test_server_state();
        let conn = state.work_db.connect().unwrap();
        conn.execute_batch(
            "INSERT INTO products (id, name, slug, status, created_at, updated_at) VALUES ('p', 'P', 'p', 'active', '1', '1');
             INSERT INTO tasks (id, product_id, kind, name, status, created_at, updated_at, completed_at)
               VALUES ('t', 'p', 'chore', 'T', 'done', '1', '1', '3500');
             INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, finished_at, driver, pr_url)
               VALUES ('e1', 't', 'chore_implementation', 'completed', 'u', '2000', '2500', 'claude', 'https://github.com/org/repo/pull/1'),
                      ('e2', 't', 'chore_implementation', 'completed', 'u', '3000', '3400', 'grok', 'https://github.com/org/repo/pull/1');
             INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, agent_active_ms)
               VALUES ('r1', 'e1', 'w', 'completed', '2000', 1000, 500, 100, 200, 3600000);
             INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, input_tokens, output_tokens, cache_read_tokens)
               VALUES ('r2', 'e2', 'w', 'completed', '3000', 700, 300, 400);",
        )
        .unwrap();
        drop(conn);

        let req = FrontendRequest::GetTokenUsageSeries(
            boss_protocol::TokenUsageSeriesRequest::builder()
                .since_epoch_s(1_000)
                .until_epoch_s(9_000)
                .bucket(boss_protocol::UsageBucketWidth::Day)
                .build(),
        );
        let (request_id, payload) = ask_series(&state, req, "req-series-1").await;
        assert_eq!(request_id.as_deref(), Some("req-series-1"));
        let FrontendEvent::TokenUsageSeries { report } = payload else {
            panic!("expected TokenUsageSeries, got {payload:?}");
        };
        assert_eq!(report.buckets.len(), 1);
        let cell = &report.buckets[0].cells[0];
        assert_eq!(cell.measurement.runs_total, 2);
        assert!(cell.measurement.tokens_partial);
        assert_eq!(cell.measurement.cache_creation_tokens, None);
        assert_eq!(cell.executions_completed, 2);
        assert_eq!(cell.distinct_prs, 1);
        assert_eq!(cell.tasks_completed, 1);
        assert_eq!(report.coverage.runs_excluded_from_active_hour, 1);
        // Active hour restricts numerator to r1 (1800 tokens / 1h = 1800.0).
        assert!((cell.ratios.tokens_per_active_hour.unwrap() - 1800.0).abs() < 1e-6);

        // Reversed range returns WorkError.
        let bad = FrontendRequest::GetTokenUsageSeries(
            boss_protocol::TokenUsageSeriesRequest::builder()
                .since_epoch_s(9_000)
                .until_epoch_s(1_000)
                .build(),
        );
        let (_, payload) = ask_series(&state, bad, "req-series-bad").await;
        let FrontendEvent::WorkError { message } = payload else {
            panic!("expected WorkError, got {payload:?}");
        };
        assert!(message.contains("until_epoch_s"), "{message}");
    }

    fn review_yield_request(since_epoch_s: i64, until_epoch_s: i64, experiment: Option<&str>) -> FrontendRequest {
        FrontendRequest::GetReviewYieldReport(boss_protocol::ReviewYieldRequest {
            since_epoch_s,
            until_epoch_s,
            experiment: experiment.map(str::to_owned),
        })
    }

    #[tokio::test]
    async fn review_yield_replies_with_the_windows_batches_split_by_the_recorded_arm() {
        let (state, _dir) = test_server_state();
        let conn = state.work_db.connect().unwrap();
        let classification = serde_json::json!({
            "changed_files": ["src/lib.rs"], "complexity_flags": [], "has_production_code": true,
            "metadata_missing": [], "production_languages": ["rust"], "profile": "light",
            "subsystem_buckets": ["src"], "additions": 5, "deletions": 0,
        })
        .to_string();
        conn.execute_batch(&format!(
            "INSERT INTO products (id, name, slug, status, created_at, updated_at) VALUES ('p', 'P', 'p', 'active', '1', '1');
             INSERT INTO tasks (id, product_id, kind, name, status, created_at, updated_at) VALUES ('t', 'p', 'chore', 'T', 'in_review', '1', '1');
             INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, experiment_arms)
               VALUES ('e1', 't', 'pr_review', 'completed', 'u', '1', '{{\"exp\":\"treatment\"}}');
             INSERT INTO pr_review_batches (id, cycle_root_id, base_sha, classification_json, created_at, phase,
                                            pr_number, pr_url, status, target_sha, updated_at)
               VALUES ('b1', 't', 'base', '{classification}', '2000', 'pre_merge', 7, 'https://github.com/o/r/pull/7',
                       'completed', 'head', '2000');
             INSERT INTO pr_review_batch_members (id, batch_id, attempt, created_at, provider_effort, requested_driver,
                                                  resolved_model, role, status, updated_at, execution_id)
               VALUES ('m1', 'b1', 1, '1', 'high', 'claude', 'm', 'supervisor', 'failed', '1', 'e1');"
        ))
        .unwrap();
        drop(conn);

        let (request_id, payload) = ask_handler(
            &state,
            review_yield_request(1_000, 9_000, Some("exp")),
            "req-yield-1",
            handle_get_review_yield_report,
        )
        .await;
        assert_eq!(request_id.as_deref(), Some("req-yield-1"));
        let FrontendEvent::ReviewYieldReport { report } = payload else {
            panic!("expected ReviewYieldReport, got {payload:?}");
        };
        assert_eq!(report.experiment.as_deref(), Some("exp"));
        assert_eq!(report.arms.len(), 1);
        assert_eq!(report.arms[0].arm, Some(boss_protocol::ReviewYieldArm::Treatment));
        assert_eq!(report.arms[0].batches, 1);
        assert_eq!(report.arms[0].phases[0].batches_without_verdict, 1);

        // Outside the window the batch is not reported.
        let (_, payload) = ask_handler(
            &state,
            review_yield_request(3_000, 9_000, None),
            "req-yield-2",
            handle_get_review_yield_report,
        )
        .await;
        let FrontendEvent::ReviewYieldReport { report } = payload else {
            panic!("expected ReviewYieldReport, got {payload:?}");
        };
        assert_eq!(report.arms[0].arm, None);
        assert_eq!(report.arms[0].batches, 0);
    }

    #[tokio::test]
    async fn review_yield_rejects_an_empty_window_and_a_malformed_experiment_key() {
        let (state, _dir) = test_server_state();
        for (request, expected) in [
            (review_yield_request(5, 5, None), "window is empty"),
            (review_yield_request(9, 5, None), "window is empty"),
            (
                review_yield_request(1, 5, Some("Not-A-Key")),
                "is not an experiment key",
            ),
            (review_yield_request(1, 5, Some("")), "is not an experiment key"),
        ] {
            let (_, payload) = ask_handler(&state, request, "req-yield", handle_get_review_yield_report).await;
            let FrontendEvent::WorkError { message } = payload else {
                panic!("expected WorkError, got {payload:?}");
            };
            assert!(message.contains(expected), "{message}");
        }
    }
}
