//! `FrontendRequest` handlers — token-cost reporting (`boss cost`).
//!
//! Each handler is a thin fetch-then-build shim: pull the flat
//! `work_runs` rows the request needs via `WorkDb`, hand them to the pure
//! builders in [`boss_engine_runtime::cost_report`] (or, for the usage rows
//! request, [`boss_usage_report`]), and send the result. See
//! [`super::Dispatch`] for the per-request context every handler
//! receives.

use super::*;

/// Minimum spacing between background coordinator usage refreshes.
const COORDINATOR_USAGE_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(2);

use boss_usage_report::{
    PricedTokens, UsageRowsQuery, UsageSeriesQuery, UsdEstimateResult, UsdEstimator, build_usage_rows_report,
    build_usage_series_report,
};

impl ServerState {
    /// Refresh coordinator usage in the background and return immediately, so
    /// a cost request or a supervisor tick never waits on transcript reads.
    /// Overlapping refreshes collapse into the one already running; readers
    /// see the stored slices and pick up new turns on their next request.
    pub(crate) fn spawn_coordinator_usage_refresh(self: &Arc<Self>) {
        let state = self.clone();
        tokio::spawn(async move {
            state
                .sync_coordinator_usage_best_effort(Some(COORDINATOR_USAGE_REFRESH_MIN_INTERVAL))
                .await;
        });
    }

    /// Best-effort sync of coordinator session token usage into `work_db`.
    /// When `min_interval` is `Some`, skips if a sync completed more recently
    /// than that bound. Never fails the caller: capture errors are logged so
    /// cost reads and supervisor ticks remain resilient.
    pub(crate) async fn sync_coordinator_usage_best_effort(&self, min_interval: Option<Duration>) {
        let Ok(working_directory) = self.resolved_coordinator_session_directory() else {
            return;
        };
        let live_record = self.work_db.coordinator_tmux_record().ok().flatten();
        if live_record.is_none() && !working_directory.is_dir() {
            return;
        }
        let context = boss_engine_runtime::coordinator_usage::CoordinatorUsageContext::builder()
            .working_directory(working_directory)
            .maybe_live_record(live_record)
            .now_epoch_s(boss_engine_utils::epoch_time::now_epoch_secs())
            .build();
        let outcome = match min_interval {
            Some(interval) => {
                self.coordinator_usage_recorder
                    .sync_if_stale(self.work_db.as_ref(), &context, interval)
                    .await
            }
            None => {
                self.coordinator_usage_recorder
                    .sync(self.work_db.as_ref(), &context)
                    .await
            }
        };
        if let Err(error) = outcome {
            tracing::warn!(
                error = %format!("{error:#}"),
                "coordinator usage capture failed (best-effort)"
            );
        }
    }
}

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
        server_state,
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
    server_state.spawn_coordinator_usage_refresh();
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
        server_state,
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
    server_state.spawn_coordinator_usage_refresh();
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

/// PR-family outcomes per arm of one experiment (or for every family), plus the
/// registered experiments and where each stands. The flag store is read only for
/// the listing: which arm a family is in comes from what its executions recorded
/// at launch, never from the flags as they are now.
pub(super) async fn handle_get_experiment_outcomes(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetExperimentOutcomes { experiment } = req else {
        unreachable!()
    };
    if let Some(key) = experiment.as_deref()
        && !boss_feature_flags::is_valid_key(key)
    {
        return send_work_error(
            &sink,
            &request_id,
            format!("{key:?} is not an experiment key: keys are lowercase snake case, like `batched_search`"),
        );
    }
    let records = match work_db.family_outcome_records(experiment.as_deref()) {
        Ok(records) => records,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let mut outcomes =
        boss_engine_runtime::cost_report::outcomes::build_experiment_outcomes(experiment.as_deref(), &records);
    let listing = server_state.feature_flags.list_experiments();
    if let Some(key) = experiment.as_deref()
        && !listing.iter().any(|entry| entry.key == key)
    {
        // An experiment that was adopted or dropped leaves its recorded arms
        // behind, so the key still reads; a typo reads as an empty experiment.
        outcomes.notes.insert(
            0,
            format!("`{key}` is not a registered experiment. The figures are what executions recorded under that key, which is nothing if it is misspelled."),
        );
    }
    let report = boss_protocol::ExperimentOutcomesReport {
        master_switch_on: server_state
            .feature_flags
            .is_enabled(boss_feature_flags::TOKEN_EXPERIMENTS_FLAG),
        experiments: listing.into_iter().map(experiment_list_entry).collect(),
        outcomes,
    };
    send_response(&sink, &request_id, FrontendEvent::ExperimentOutcomesReport { report });
}

fn experiment_list_entry(listing: boss_feature_flags::ExperimentListing) -> boss_protocol::ExperimentListEntry {
    use boss_feature_flags::ExperimentListState as State;
    let (state, waiting_on, held_by) = match listing.state {
        State::Off => ("off", Vec::new(), None),
        State::WaitingOn(flags) => ("waiting", flags.into_iter().map(str::to_owned).collect(), None),
        State::Held { holder } => ("held", Vec::new(), Some(holder.to_owned())),
        State::Split => ("split", Vec::new(), None),
        State::All => ("all", Vec::new(), None),
    };
    boss_protocol::ExperimentListEntry::builder()
        .key(listing.key)
        .state(state)
        .maybe_lane(listing.lane.map(|lane| lane.as_str().to_owned()))
        .splits(listing.splits)
        .requires(listing.requires.iter().map(|flag| (*flag).to_owned()).collect())
        .waiting_on(waiting_on)
        .maybe_held_by(held_by)
        .build()
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
        server_state,
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
    server_state.spawn_coordinator_usage_refresh();
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
        server_state,
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
    server_state.spawn_coordinator_usage_refresh();
    let records = match work_db.usage_series_records_for_window(since_epoch_s, until_epoch_s) {
        Ok(records) => records,
        Err(err) => return send_work_error(&sink, &request_id, &err),
    };
    let report = build_usage_series_report(&query, &records, estimate_usd.then_some(&ENGINE_USD_ESTIMATOR));
    send_response(&sink, &request_id, FrontendEvent::TokenUsageSeries { report });
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

    #[tokio::test]
    async fn cost_handlers_include_coordinator_usage_in_window_top_rows_and_series() {
        let (state, _dir) = test_server_state();
        state
            .work_db
            .upsert_coordinator_session_usage(
                &boss_work_db::CoordinatorSessionSnapshot::builder()
                    .session_id("sess-rpc-1")
                    .slices(vec![
                        boss_work_db::CoordinatorUsageSlice::builder()
                            .session_id("sess-rpc-1")
                            .source("claude_transcript")
                            .driver("claude")
                            .launched_model("opus")
                            .bucket_start_epoch_s(3_600)
                            .first_event_at(3_700)
                            .last_event_at(4_100)
                            .model("claude-opus-4-6")
                            .input_tokens(600)
                            .output_tokens(200)
                            .cache_creation_tokens(100)
                            .cache_read_tokens(1_100)
                            .rounds(4)
                            .agent_active_ms(10_000)
                            .build(),
                    ])
                    .build(),
            )
            .unwrap();

        let (_, rows_payload) = ask(&state, request(Vec::new(), Vec::new()), "req-coord-rows").await;
        let FrontendEvent::TokenUsageRows { report: rows_report } = rows_payload else {
            panic!("expected TokenUsageRows, got {rows_payload:?}");
        };
        assert_eq!(rows_report.overall.total_tokens, 2_000);
        assert_eq!(
            rows_report.coordinator_measurement.as_ref().map(|m| m.total_tokens),
            Some(2_000)
        );
        assert_eq!(rows_report.taskless.len(), 1);
        assert_eq!(rows_report.taskless[0].kind, boss_protocol::COORDINATOR_USAGE_KIND);

        let series_req = FrontendRequest::GetTokenUsageSeries(
            boss_protocol::TokenUsageSeriesRequest::builder()
                .since_epoch_s(1_000)
                .until_epoch_s(9_000)
                .bucket(boss_protocol::UsageBucketWidth::Day)
                .group_by(UsageDimension::ExecutionKind)
                .build(),
        );
        let (_, series_payload) = ask_series(&state, series_req, "req-coord-series").await;
        let FrontendEvent::TokenUsageSeries { report: series_report } = series_payload else {
            panic!("expected TokenUsageSeries, got {series_payload:?}");
        };
        assert_eq!(series_report.groups, vec![boss_protocol::COORDINATOR_USAGE_KIND]);
        assert_eq!(series_report.buckets[0].cells[0].measurement.total_tokens, 2_000);

        let window_sink = make_session_sink();
        handle_get_cost_window_report(
            Dispatch::builder()
                .server_state(state.clone())
                .work_db(state.work_db.clone())
                .sink(window_sink.clone())
                .session_id("session-cost-window")
                .request_id("req-coord-window")
                .recv_instant(std::time::Instant::now())
                .decode_ms(0.0)
                .build(),
            FrontendRequest::GetCostWindowReport {
                since_epoch_s: 1_000,
                until_epoch_s: 9_000,
                estimate_usd: false,
            },
        )
        .await;
        window_sink.close();
        let FrontendEvent::CostWindowReport { report: window_report } = window_sink.next().await.unwrap().payload
        else {
            panic!("expected CostWindowReport");
        };
        assert_eq!(window_report.overall.total_tokens, 2_000);
        assert_eq!(window_report.by_kind[0].label, boss_protocol::COORDINATOR_USAGE_KIND);

        let top_sink = make_session_sink();
        handle_get_top_cost_consumers(
            Dispatch::builder()
                .server_state(state.clone())
                .work_db(state.work_db.clone())
                .sink(top_sink.clone())
                .session_id("session-cost-top")
                .request_id("req-coord-top")
                .recv_instant(std::time::Instant::now())
                .decode_ms(0.0)
                .build(),
            FrontendRequest::GetTopCostConsumers {
                since_epoch_s: 1_000,
                until_epoch_s: 9_000,
                limit: 10,
                estimate_usd: false,
            },
        )
        .await;
        top_sink.close();
        let FrontendEvent::TopCostConsumers { report: top_report } = top_sink.next().await.unwrap().payload else {
            panic!("expected TopCostConsumers");
        };
        assert_eq!(top_report.rows.len(), 1);
        assert_eq!(top_report.rows[0].kind, boss_protocol::COORDINATOR_USAGE_KIND);
        assert_eq!(top_report.rows[0].total_tokens, 2_000);
    }

    async fn ask_experiment(
        state: &Arc<ServerState>,
        experiment: Option<&str>,
        request_id: &str,
    ) -> (Option<String>, FrontendEvent) {
        let sink = make_session_sink();
        let dispatch = Dispatch::builder()
            .server_state(state.clone())
            .work_db(state.work_db.clone())
            .sink(sink.clone())
            .session_id("session-experiment")
            .request_id(request_id)
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build();
        let request = FrontendRequest::GetExperimentOutcomes {
            experiment: experiment.map(str::to_owned),
        };
        handle_get_experiment_outcomes(dispatch, request).await;
        sink.close();
        let envelope = sink.next().await.expect("the handler sends a reply");
        assert!(sink.next().await.is_none(), "the handler sends exactly one reply");
        (envelope.request_id, envelope.payload)
    }

    /// A family with a published PR whose first execution launched under `arms`.
    fn seed_family(state: &Arc<ServerState>, product: &str, name: &str, arms: &str, merged: bool) {
        use boss_work_db::test_support::{create_ready_chore_execution, create_test_chore};
        let db = &state.work_db;
        let root = create_test_chore(db, product, name);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = 'https://github.com/org/repo/pull/1' WHERE id = ?1",
                [&root.id],
            )
            .unwrap();
        let execution = create_ready_chore_execution(db, &root.id);
        db.record_execution_launch(&boss_work_db::ExecutionLaunchRecord {
            execution_id: &execution.id,
            driver: "claude",
            model: "model",
            effort_level: None,
            experiment_arms: Some(arms),
        })
        .unwrap();
        if merged {
            // Launch times must sit before the merge stamp, or the merge cutoff
            // would drop the family's own enrolment.
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET created_at = '10', started_at = '10' WHERE id = ?1",
                    [&execution.id],
                )
                .unwrap();
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE pr_family_outcomes SET merged_at = '100' WHERE work_item_id = ?1",
                    [&root.id],
                )
                .unwrap();
        }
    }

    #[tokio::test]
    async fn experiment_outcomes_reads_arms_from_the_launch_record_and_lists_every_experiment() {
        let (state, _dir) = test_server_state();
        let product = boss_work_db::test_support::create_product(&state.work_db);
        seed_family(
            &state,
            &product,
            "treated, merged",
            r#"{"batched_search":"treatment"}"#,
            true,
        );
        seed_family(
            &state,
            &product,
            "treated, open",
            r#"{"batched_search":"treatment"}"#,
            false,
        );
        seed_family(
            &state,
            &product,
            "control, merged",
            r#"{"batched_search":"control"}"#,
            true,
        );
        seed_family(&state, &product, "stopped", r#"{"batched_search":"stopped"}"#, false);
        seed_family(&state, &product, "other experiment", r#"{"other":"treatment"}"#, false);

        let (request_id, payload) = ask_experiment(&state, Some("batched_search"), "req-experiment-1").await;
        assert_eq!(request_id.as_deref(), Some("req-experiment-1"));
        let FrontendEvent::ExperimentOutcomesReport { report } = payload else {
            panic!("expected ExperimentOutcomesReport, got {payload:?}");
        };
        let outcomes = &report.outcomes;
        assert_eq!(outcomes.experiment.as_deref(), Some("batched_search"));
        let families = |arm: &str| {
            let arm = outcomes.arms.iter().find(|a| a.arm == arm).unwrap();
            (arm.outcomes.families, arm.outcomes.merged_families)
        };
        assert_eq!(families("treatment"), (2, 1));
        assert_eq!(families("control"), (1, 1));
        assert_eq!((outcomes.mixed_families, outcomes.stopped_families), (0, 1));
        let by_driver = &outcomes.arms[0].by_driver;
        assert_eq!(by_driver.len(), 1);
        assert_eq!(by_driver[0].driver, "claude");

        // The listing names every registered experiment and its state, from the flags.
        assert!(report.master_switch_on);
        let listed = report
            .experiments
            .iter()
            .find(|entry| entry.key == "batched_search")
            .expect("batched_search is listed");
        assert_eq!(listed.state, "off", "experiments ship inert");
        assert_eq!(listed.lane.as_deref(), Some("worker_prompt"));
        state.feature_flags.set("batched_search", true).unwrap();
        let (_, payload) = ask_experiment(&state, Some("batched_search"), "req-experiment-2").await;
        let FrontendEvent::ExperimentOutcomesReport { report } = payload else {
            panic!("expected ExperimentOutcomesReport, got {payload:?}");
        };
        assert_eq!(report.experiments[0].state, "split");
        // Switching it on changes the listing, not what the families recorded.
        assert_eq!(report.outcomes.arms[0].outcomes.families, 2);
        state.feature_flags.set("batched_search", false).unwrap();
        let (_, payload) = ask_experiment(&state, Some("batched_search"), "req-experiment-3").await;
        let FrontendEvent::ExperimentOutcomesReport { report } = payload else {
            panic!("expected ExperimentOutcomesReport, got {payload:?}");
        };
        assert_eq!(report.experiments[0].state, "off");
        assert_eq!(report.outcomes.arms[0].outcomes.families, 2);
    }

    #[tokio::test]
    async fn experiment_outcomes_without_a_key_reads_every_family_as_one_arm() {
        let (state, _dir) = test_server_state();
        let product = boss_work_db::test_support::create_product(&state.work_db);
        seed_family(&state, &product, "no experiment", "{}", true);
        seed_family(&state, &product, "treated", r#"{"batched_search":"treatment"}"#, false);
        let (_, payload) = ask_experiment(&state, None, "req-experiment-all").await;
        let FrontendEvent::ExperimentOutcomesReport { report } = payload else {
            panic!("expected ExperimentOutcomesReport, got {payload:?}");
        };
        assert_eq!(report.outcomes.experiment, None);
        assert_eq!(report.outcomes.arms.len(), 1);
        assert_eq!(report.outcomes.arms[0].arm, "all");
        assert_eq!(report.outcomes.arms[0].outcomes.families, 2);
        assert_eq!(report.outcomes.arms[0].outcomes.merged_families, 1);
        assert!(!report.experiments.is_empty());
    }

    #[tokio::test]
    async fn experiment_outcomes_says_when_the_key_is_not_registered_and_rejects_a_malformed_one() {
        let (state, _dir) = test_server_state();
        let (_, payload) = ask_experiment(&state, Some("batched_serach"), "req-experiment-typo").await;
        let FrontendEvent::ExperimentOutcomesReport { report } = payload else {
            panic!("expected ExperimentOutcomesReport, got {payload:?}");
        };
        assert_eq!(report.outcomes.arms.iter().map(|a| a.outcomes.families).sum::<u64>(), 0);
        assert!(
            report.outcomes.notes[0].contains("`batched_serach` is not a registered experiment"),
            "{:?}",
            report.outcomes.notes
        );

        let (_, payload) = ask_experiment(&state, Some("Batched Search"), "req-experiment-bad").await;
        let FrontendEvent::WorkError { message } = payload else {
            panic!("expected WorkError, got {payload:?}");
        };
        assert!(message.contains("lowercase snake case"), "{message}");
    }

    #[test]
    fn the_listing_entry_carries_what_a_waiting_or_held_experiment_needs() {
        use boss_feature_flags::{ExperimentLane, ExperimentListState, ExperimentListing};
        let listing = |state| ExperimentListing {
            key: "delta",
            lane: Some(ExperimentLane::PreMergeReview),
            requires: &["review_batch_fanout"],
            splits: true,
            state,
        };
        let waiting = experiment_list_entry(listing(ExperimentListState::WaitingOn(vec!["review_batch_fanout"])));
        assert_eq!(waiting.state, "waiting");
        assert_eq!(waiting.waiting_on, ["review_batch_fanout"]);
        assert_eq!(waiting.requires, ["review_batch_fanout"]);
        assert_eq!(waiting.lane.as_deref(), Some("pre_merge_review"));
        let held = experiment_list_entry(listing(ExperimentListState::Held { holder: "alpha" }));
        assert_eq!((held.state.as_str(), held.held_by.as_deref()), ("held", Some("alpha")));
        assert!(held.waiting_on.is_empty());
        assert_eq!(experiment_list_entry(listing(ExperimentListState::All)).state, "all");
    }
}
