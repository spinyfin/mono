//! `FrontendRequest` handlers — token-cost reporting (`boss cost`).
//!
//! Each handler is a thin fetch-then-build shim: pull the flat
//! `work_runs` rows the request needs via `WorkDb`, hand them to the pure
//! builders in [`boss_engine_runtime::cost_report`] (or, for the usage rows
//! request, [`boss_usage_report`]), and send the result. See
//! [`super::Dispatch`] for the per-request context every handler
//! receives.

use super::*;

use boss_usage_report::{PricedTokens, UsageRowsQuery, build_usage_rows_report};

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
    let price = |model: &str, tokens: PricedTokens| {
        let price = boss_engine_runtime::cost_pricing::price_for_model(model)?;
        boss_engine_runtime::cost_pricing::estimate_usd(
            price,
            tokens.input,
            tokens.output,
            tokens.cache_creation,
            tokens.cache_read,
        )
    };
    let report = build_usage_rows_report(&query, &records, estimate_usd.then_some(&price));
    send_response(&sink, &request_id, FrontendEvent::TokenUsageRows { report });
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_session_sink, test_server_state};
    use super::*;
    use boss_engine_runtime::cost_report::{CostRunRecord, build_window_cost_report};
    use boss_protocol::{UsageDimension, UsageFilter};
    use boss_usage_report::{UsageRunRecord, UsageWindowRecords};

    async fn ask(
        state: &Arc<ServerState>,
        request: FrontendRequest,
        request_id: &str,
    ) -> (Option<String>, FrontendEvent) {
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
        handle_get_token_usage_rows(dispatch, request).await;
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

    /// `boss cost window` and the usage rows request read the same runs, so
    /// their totals must agree. The two accumulate independently (the builder
    /// crate cannot depend on the runtime crate that owns the first), so this
    /// is the test that stops them drifting.
    #[test]
    fn usage_overall_matches_the_cost_window_total_for_the_same_runs() {
        // (driver, input, output, cache_creation, cache_read, active_ms, usage_unavailable)
        type Row = (
            &'static str,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<&'static str>,
        );
        let rows: [Row; 6] = [
            ("claude", Some(100), Some(50), Some(10), Some(5), Some(1_000), None),
            (
                "claude",
                None,
                None,
                None,
                None,
                None,
                Some("No provider token usage observed"),
            ),
            ("grok", Some(20), Some(4), None, Some(9), None, None),
            ("codex", Some(500), Some(60), Some(0), Some(400), Some(2_000), None),
            ("claude", Some(0), Some(0), Some(0), Some(0), Some(0), None),
            (
                "agy",
                Some(30),
                Some(2),
                None,
                Some(8),
                None,
                Some("One or more provider responses omit valid input, output, or cache-read usage"),
            ),
        ];
        let mut cost_records = Vec::new();
        let mut usage_runs = Vec::new();
        for (index, (driver, input, output, cache_creation, cache_read, active_ms, unavailable)) in
            rows.into_iter().enumerate()
        {
            let id = format!("r{index}");
            cost_records.push(
                CostRunRecord::builder()
                    .run_id(&id)
                    .execution_id(format!("e{index}"))
                    .work_item_id("t")
                    .work_item_name("t")
                    .execution_kind("chore_implementation")
                    .execution_status("completed")
                    .run_status("completed")
                    .created_at_epoch_s(5_000 + index as i64)
                    .driver(driver)
                    .maybe_input_tokens(input)
                    .maybe_output_tokens(output)
                    .maybe_cache_creation_tokens(cache_creation)
                    .maybe_cache_read_tokens(cache_read)
                    .maybe_agent_active_ms(active_ms)
                    .maybe_usage_unavailable(unavailable.map(str::to_owned))
                    .build(),
            );
            let mut run = UsageRunRecord::builder()
                .run_id(&id)
                .run_status("completed")
                .run_created_at_epoch_s(5_000 + index as i64)
                .run_host_id("local")
                .execution_id(format!("e{index}"))
                .work_item_id("t")
                .execution_kind("chore_implementation")
                .execution_status("completed")
                .execution_created_at_epoch_s(5_000 + index as i64)
                .driver(driver)
                .build();
            run.input_tokens = input;
            run.output_tokens = output;
            run.cache_creation_tokens = cache_creation;
            run.cache_read_tokens = cache_read;
            run.agent_active_ms = active_ms;
            run.usage_unavailable = unavailable.map(str::to_owned);
            usage_runs.push(run);
        }
        let window = build_window_cost_report(1_000, 9_000, &cost_records, false, 9_000);
        let query = UsageRowsQuery::builder()
            .since_epoch_s(1_000)
            .until_epoch_s(9_000)
            .generated_at_epoch_s(9_000)
            .capture_start_epoch_s(0)
            .retention_max_age_secs(boss_work_db::DEFAULT_RETENTION_MAX_AGE_SECS)
            .build();
        let usage = build_usage_rows_report(
            &query,
            &UsageWindowRecords {
                runs: usage_runs,
                tasks: Vec::new(),
            },
            None,
        );
        assert_eq!(usage.overall, window.overall);
    }
}
