//! Every cost request reports a review guide's runs: under its own execution
//! kind, with the tokens the run recorded, and against the task that owns the
//! PR the guide explains. Runs of an execution with no task are reported too.

use super::super::tests::{make_session_sink, test_server_state};
use super::*;
use boss_protocol::{RunCoverage, UsageDimension};

const SINCE: i64 = 1_000;
const UNTIL: i64 = 9_000;

/// One task with an implementation run (18 tokens), a completed review guide
/// run (650 tokens), a failed guide attempt whose run recorded no usage, and
/// an answer agent run that belongs to no task (10 tokens).
const SEED: &str = "
    INSERT INTO products (id, name, slug, status, created_at, updated_at)
      VALUES ('p', 'P', 'p', 'active', '1', '1');
    INSERT INTO tasks (id, product_id, kind, name, status, created_at, updated_at, completed_at, pr_url)
      VALUES ('task_owner', 'p', 'chore', 'Fix cursor flicker', 'done', '1', '1', '3500',
              'https://github.com/org/repo/pull/1');
    INSERT INTO pr_review_guide_source_series
        (id, root_task_id, canonical_pr_url, latest_observation_sequence, created_at, updated_at)
      VALUES ('series_1', 'task_owner', 'https://github.com/org/repo/pull/1', 1, '2000', '2000');
    INSERT INTO pr_review_guide_source_comparisons
        (id, series_id, observation_sequence, observed_base_sha, merge_base_sha, head_sha, trigger,
         packet_hash, complete, captured_at)
      VALUES ('prgc_1', 'series_1', 1, 'base', 'base', 'head', 'creation', 'hash', 1, '2000');
    INSERT INTO work_executions
        (id, work_item_id, kind, status, repo_remote_url, created_at, finished_at, driver, model, effort_level)
      VALUES ('e_impl', 'task_owner', 'chore_implementation', 'completed', 'u', '2000', '2400', 'claude', 'opus', 'medium'),
             ('e_guide', 'prgc_1', 'pr_review_guide', 'completed', 'u', '2500', '2600', 'codex', 'gpt-6-astra', 'high'),
             ('e_guide_failed', 'prgc_1', 'pr_review_guide', 'failed', 'u', '2700', '2800', 'codex', 'gpt-6-astra', 'high'),
             ('e_answer', 'comment_1', 'answer_agent', 'completed', 'u', '2900', '3000', 'claude', 'sonnet', 'medium');
    INSERT INTO work_runs
        (id, execution_id, agent_id, status, created_at, model, input_tokens, output_tokens,
         cache_creation_tokens, cache_read_tokens)
      VALUES ('r_impl', 'e_impl', 'w', 'completed', '2000', 'claude-opus-5-5', 10, 5, 1, 2),
             ('r_guide', 'e_guide', 'w', 'completed', '2500', 'gpt-6-astra', 400, 50, 0, 200),
             ('r_guide_failed', 'e_guide_failed', 'w', 'failed', '2700', NULL, NULL, NULL, NULL, NULL),
             ('r_answer', 'e_answer', 'w', 'completed', '2900', 'claude-sonnet-5-5', 7, 3, 0, 0);
";

fn seeded_state() -> (Arc<ServerState>, impl Sized) {
    let (state, dir) = test_server_state();
    state.work_db.connect().unwrap().execute_batch(SEED).unwrap();
    (state, dir)
}

/// Runs one request through `handler` and returns its single reply.
async fn reply<F, Fut>(state: &Arc<ServerState>, handler: F, request: FrontendRequest) -> FrontendEvent
where
    F: FnOnce(Dispatch, FrontendRequest) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let sink = make_session_sink();
    let dispatch = Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-cost-guide")
        .request_id("req-cost-guide")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build();
    handler(dispatch, request).await;
    sink.close();
    let envelope = sink.next().await.expect("the handler sends a reply");
    assert!(sink.next().await.is_none(), "the handler sends exactly one reply");
    envelope.payload
}

#[tokio::test]
async fn the_window_report_counts_review_guide_runs_under_their_own_kind() {
    let (state, _dir) = seeded_state();
    let payload = reply(
        &state,
        handle_get_cost_window_report,
        FrontendRequest::GetCostWindowReport {
            since_epoch_s: SINCE,
            until_epoch_s: UNTIL,
            estimate_usd: false,
        },
    )
    .await;
    let FrontendEvent::CostWindowReport { report } = payload else {
        panic!("expected CostWindowReport, got {payload:?}");
    };

    let kind = |label: &str| {
        &report
            .by_kind
            .iter()
            .find(|bucket| bucket.label == label)
            .unwrap_or_else(|| panic!("no {label} bucket in {:?}", report.by_kind))
            .measurement
    };
    let guide = kind("pr_review_guide");
    assert_eq!(guide.total_tokens, 650);
    assert_eq!(
        (
            guide.runs_total,
            guide.runs_measured,
            guide.runs_unmeasured,
            guide.runs_zero
        ),
        (2, 1, 1, 0),
        "the guide run with no recorded usage is counted as unknown, not as zero"
    );
    assert_eq!(kind("answer_agent").total_tokens, 10);
    assert_eq!(kind("chore_implementation").total_tokens, 18);

    assert_eq!(report.overall.runs_total, 4);
    assert_eq!(report.overall.total_tokens, 18 + 650 + 10);
    let driver = |label: &str| report.by_driver.iter().find(|bucket| bucket.label == label).unwrap();
    assert_eq!(driver("codex").measurement.total_tokens, 650);
    assert_eq!(
        report.by_model.iter().map(|b| b.measurement.runs_total).sum::<u32>(),
        4,
        "every breakdown covers every run"
    );
}

#[tokio::test]
async fn the_top_report_ranks_review_guide_runs_against_their_owning_task() {
    let (state, _dir) = seeded_state();
    let payload = reply(
        &state,
        handle_get_top_cost_consumers,
        FrontendRequest::GetTopCostConsumers {
            since_epoch_s: SINCE,
            until_epoch_s: UNTIL,
            limit: 10,
            estimate_usd: false,
        },
    )
    .await;
    let FrontendEvent::TopCostConsumers { report } = payload else {
        panic!("expected TopCostConsumers, got {payload:?}");
    };
    let rows: Vec<(&str, &str, &str, &str, i64)> = report
        .rows
        .iter()
        .map(|row| {
            (
                row.run_id.as_str(),
                row.kind.as_str(),
                row.work_item_id.as_str(),
                row.name.as_str(),
                row.total_tokens,
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("r_guide", "pr_review_guide", "task_owner", "Fix cursor flicker", 650),
            ("r_impl", "chore_implementation", "task_owner", "Fix cursor flicker", 18),
            ("r_answer", "answer_agent", "comment_1", "(no task)", 10),
        ]
    );
    assert_eq!(
        report.excluded_unmeasured_count, 1,
        "the unmeasured guide run cannot be ranked and is counted as excluded"
    );
}

#[tokio::test]
async fn a_tasks_cost_report_includes_the_review_guides_generated_for_its_pr() {
    let (state, _dir) = seeded_state();
    let payload = reply(
        &state,
        handle_get_work_item_cost_report,
        FrontendRequest::GetWorkItemCostReport {
            work_item_id: "task_owner".into(),
            estimate_usd: false,
        },
    )
    .await;
    let FrontendEvent::WorkItemCostReport { report } = payload else {
        panic!("expected WorkItemCostReport, got {payload:?}");
    };
    let executions: Vec<(&str, &str, i64, u32)> = report
        .by_execution
        .iter()
        .map(|row| {
            (
                row.execution_id.as_str(),
                row.kind.as_str(),
                row.measurement.total_tokens,
                row.measurement.runs_unmeasured,
            )
        })
        .collect();
    assert_eq!(
        executions,
        [
            ("e_impl", "chore_implementation", 18, 0),
            ("e_guide", "pr_review_guide", 650, 0),
            ("e_guide_failed", "pr_review_guide", 0, 1),
        ]
    );
    assert_eq!(report.overall.total_tokens, 18 + 650);
    assert_eq!((report.overall.runs_total, report.overall.runs_unmeasured), (3, 1));
}

#[tokio::test]
async fn usage_rows_list_review_guide_executions_under_their_owning_task() {
    let (state, _dir) = seeded_state();
    let request = |group_by: Vec<UsageDimension>| {
        FrontendRequest::GetTokenUsageRows(boss_protocol::TokenUsageRowsRequest {
            since_epoch_s: SINCE,
            until_epoch_s: UNTIL,
            filters: Vec::new(),
            group_by,
            roll_up_revisions: true,
            estimate_usd: false,
        })
    };
    let payload = reply(&state, handle_get_token_usage_rows, request(Vec::new())).await;
    let FrontendEvent::TokenUsageRows { report } = payload else {
        panic!("expected TokenUsageRows, got {payload:?}");
    };
    assert_eq!(report.overall.runs_total, 4);
    assert_eq!(report.overall.total_tokens, 18 + 650 + 10);
    assert_eq!(report.tasks.len(), 1);
    let task = &report.tasks[0];
    assert_eq!(task.task_id, "task_owner");
    let kinds: Vec<&str> = task.executions.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["chore_implementation", "pr_review_guide", "pr_review_guide"]);
    assert_eq!(task.direct.total_tokens, 18 + 650);
    let failed = &task.executions[2].runs[0];
    assert_eq!(failed.coverage, RunCoverage::Unmeasured);
    assert_eq!(failed.total_tokens, None, "unknown, never zero");
    let taskless: Vec<&str> = report.taskless.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(taskless, ["answer_agent"]);

    let payload = reply(
        &state,
        handle_get_token_usage_rows,
        request(vec![UsageDimension::ExecutionKind]),
    )
    .await;
    let FrontendEvent::TokenUsageRows { report } = payload else {
        panic!("expected TokenUsageRows, got {payload:?}");
    };
    let guide = report
        .groups
        .iter()
        .find(|group| group.key[0].value == "pr_review_guide")
        .expect("review guides form their own execution-kind group");
    assert_eq!((guide.measurement.total_tokens, guide.measurement.runs_total), (650, 2));
}

#[tokio::test]
async fn the_usage_series_counts_review_guide_runs_and_charges_them_to_the_completed_task() {
    let (state, _dir) = seeded_state();
    let payload = reply(
        &state,
        handle_get_token_usage_series,
        FrontendRequest::GetTokenUsageSeries(
            boss_protocol::TokenUsageSeriesRequest::builder()
                .since_epoch_s(SINCE)
                .until_epoch_s(UNTIL)
                .bucket(boss_protocol::UsageBucketWidth::Day)
                .group_by(UsageDimension::ExecutionKind)
                .build(),
        ),
    )
    .await;
    let FrontendEvent::TokenUsageSeries { report } = payload else {
        panic!("expected TokenUsageSeries, got {payload:?}");
    };
    assert_eq!(report.buckets.len(), 1);
    let index = report
        .groups
        .iter()
        .position(|group| group == "pr_review_guide")
        .unwrap_or_else(|| panic!("no pr_review_guide group in {:?}", report.groups));
    let guide = &report.buckets[0].cells[index];
    assert_eq!(guide.measurement.total_tokens, 650);
    assert_eq!(
        (guide.measurement.runs_total, guide.measurement.runs_unmeasured),
        (2, 1)
    );
    assert_eq!(guide.executions_completed, 1, "only the completed guide execution");

    let cells = &report.buckets[0].cells;
    assert_eq!(
        cells.iter().map(|c| c.measurement.total_tokens).sum::<i64>(),
        18 + 650 + 10
    );
    assert_eq!(
        cells.iter().map(|c| c.tasks_completed_cost.total_tokens).sum::<i64>(),
        18 + 650,
        "the completed task's cost includes its guides and not the answer agent"
    );
}
