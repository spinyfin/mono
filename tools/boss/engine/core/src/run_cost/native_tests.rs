use super::*;
use crate::cost_report::{self, CostRunRecord};
use crate::test_support::{create_product, create_ready_chore_execution, create_test_chore, open_db};
use serde_json::json;

const GROK: &str = include_str!("testdata/grok_turns.jsonl");
const AGY: &str = include_str!("testdata/agy_planner.jsonl");

fn parse(text: &str) -> RunCostSnapshot {
    let mut accumulator = CostAccumulator::default();
    for line in text.lines().chain(text.lines()) {
        accumulator.ingest(
            Path::new("/transcript.jsonl"),
            &serde_json::from_str(line).unwrap(),
            false,
        );
    }
    accumulator.snapshot().unwrap()
}

#[test]
fn captured_grok_turns_include_cancelled_usage_without_double_counting() {
    let snapshot = parse(GROK);
    assert_eq!(snapshot.input_tokens, Some(2486));
    assert_eq!(snapshot.cache_read_tokens, Some(24576));
    assert_eq!(snapshot.output_tokens, Some(115));
    assert_eq!(snapshot.cache_creation_tokens, None);
    assert_eq!(snapshot.cache_creation_5m_tokens, None);
    assert_eq!(snapshot.cache_creation_1h_tokens, None);
    assert_eq!(snapshot.agent_active_ms, None);
    assert_eq!(snapshot.rounds, Some(2));
    assert_eq!(snapshot.model.as_deref(), Some("grok-4.5-build"));
    assert_eq!(snapshot.usage_unavailable, None);
}

#[test]
fn captured_agy_steps_report_fresh_input_and_unknown_cache_creation() {
    let snapshot = parse(AGY);
    assert_eq!(snapshot.input_tokens, Some(20570));
    assert_eq!(snapshot.cache_read_tokens, Some(8101));
    assert_eq!(snapshot.output_tokens, Some(109));
    assert_eq!(snapshot.cache_creation_tokens, None);
    assert_eq!(snapshot.agent_active_ms, None);
    assert_eq!(snapshot.rounds, Some(3));
    assert_eq!(snapshot.model.as_deref(), Some("gemini-3.8-flash-low"));
    assert_eq!(snapshot.usage_unavailable, None);
}

#[test]
fn absent_invalid_and_partial_usage_remain_unknown() {
    for text in [
        json!({"type":"PLANNER_RESPONSE", "step_index":1}).to_string(),
        json!({"type":"PLANNER_RESPONSE", "step_index":1, "input_tokens":-1,
            "output_tokens":"20", "cache_read_tokens":null})
        .to_string(),
        json!({"method":"_x.ai/session/update","params":{"update":{
            "sessionUpdate":"turn_completed","prompt_id":"missing"}}})
        .to_string(),
        json!({"method":"_x.ai/session/update","params":{"update":{
            "sessionUpdate":"turn_completed","prompt_id":"invalid",
            "usage":{"inputTokens":2,"cachedReadTokens":3}}}})
        .to_string(),
    ] {
        let snapshot = parse(&text);
        assert_eq!(snapshot.input_tokens, None);
        assert_eq!(snapshot.output_tokens, None);
        assert!(snapshot.usage_unavailable.is_some());
    }
    let snapshot = parse(&format!(
        "{AGY}{}\n",
        json!({"type":"PLANNER_RESPONSE", "step_index":99})
    ));
    assert_eq!(
        snapshot.input_tokens, None,
        "a partial sum is not a complete run measurement"
    );
    assert_eq!(snapshot.rounds, Some(4));
}

#[test]
fn step_revisions_replace_usage_and_separate_transcripts_do_not_collide() {
    let mut accumulator = CostAccumulator::default();
    let mut step = json!({"type":"PLANNER_RESPONSE", "source":"MODEL", "step_index":1,
        "input_tokens":10, "output_tokens":0, "cache_read_tokens":0, "model":"first"});
    accumulator.ingest(Path::new("/one"), &step, false);
    step["output_tokens"] = json!(3);
    accumulator.ingest(Path::new("/one"), &step, false);
    step["model"] = json!("second");
    accumulator.ingest(Path::new("/two"), &step, false);
    let snapshot = accumulator.snapshot().unwrap();
    assert_eq!(snapshot.input_tokens, Some(20));
    assert_eq!(snapshot.output_tokens, Some(6));
    assert_eq!(snapshot.cache_read_tokens, Some(0));
    assert_eq!(snapshot.model, None);
    assert_eq!(snapshot.rounds, Some(2));
}

fn grok_turn(prompt_id: &str, usage: serde_json::Value) -> String {
    json!({"method":"_x.ai/session/update","params":{"update":{
        "sessionUpdate":"turn_completed","prompt_id":prompt_id,
        "usage":usage}}})
    .to_string()
}

fn record_from_snapshot(run_id: &str, snapshot: RunCostSnapshot) -> CostRunRecord {
    CostRunRecord::builder()
        .run_id(run_id)
        .execution_id("exec_1")
        .work_item_id("task_1")
        .work_item_name("t")
        .execution_kind("chore_implementation")
        .execution_status("completed")
        .run_status("completed")
        .created_at_epoch_s(1)
        .maybe_input_tokens(snapshot.input_tokens)
        .maybe_output_tokens(snapshot.output_tokens)
        .maybe_cache_creation_tokens(snapshot.cache_creation_tokens)
        .maybe_cache_read_tokens(snapshot.cache_read_tokens)
        .maybe_usage_unavailable(snapshot.usage_unavailable)
        .build()
}

fn fully_measured_claude_run() -> CostRunRecord {
    CostRunRecord::builder()
        .run_id("run_claude")
        .execution_id("exec_1")
        .work_item_id("task_1")
        .work_item_name("t")
        .execution_kind("chore_implementation")
        .execution_status("completed")
        .run_status("completed")
        .created_at_epoch_s(1)
        .input_tokens(100)
        .output_tokens(40)
        .cache_creation_tokens(10)
        .cache_read_tokens(5)
        .build()
}

#[test]
fn grok_turn_without_input_still_reports_known_output_and_cache_read() {
    let snapshot = parse(&grok_turn("partial", json!({"outputTokens":50,"cachedReadTokens":20})));
    assert_eq!(snapshot.input_tokens, None);
    assert_eq!(snapshot.output_tokens, Some(50));
    assert_eq!(snapshot.cache_read_tokens, Some(20));
    assert_eq!(snapshot.cache_creation_tokens, None);
    assert_eq!(snapshot.agent_active_ms, None);
    let records = [record_from_snapshot("run_partial", snapshot)];
    let report = cost_report::build_window_cost_report(0, i64::MAX, &records, false, 0);
    assert_eq!(report.overall.runs_measured, 1);
    assert_eq!(report.overall.runs_unmeasured, 0);
    assert_eq!(report.overall.input_tokens, None);
    assert_eq!(report.overall.output_tokens, Some(50));
    assert_eq!(report.overall.cache_creation_tokens, None);
    assert_eq!(report.overall.cache_read_tokens, Some(20));
    assert!(report.overall.tokens_partial);
    assert_eq!(report.overall.total_tokens, 70);
    let top = cost_report::build_top_cost_report(0, i64::MAX, 10, &records, false, 0);
    assert_eq!(top.rows.len(), 1);
    assert_eq!(top.rows[0].total_tokens, 70);
    assert!(top.rows[0].tokens_partial);
    assert_eq!(top.excluded_unmeasured_count, 0);
}

#[test]
fn grok_partial_turn_mixed_with_complete_run_keeps_known_as_partial_sums() {
    let partial = record_from_snapshot(
        "run_partial",
        parse(&grok_turn("partial", json!({"outputTokens":50,"cachedReadTokens":20}))),
    );
    let records = vec![fully_measured_claude_run(), partial];
    let report = cost_report::build_window_cost_report(0, i64::MAX, &records, false, 0);
    assert_eq!(report.overall.runs_measured, 2);
    assert_eq!(report.overall.input_tokens, None);
    assert_eq!(report.overall.output_tokens, Some(90));
    assert_eq!(report.overall.cache_creation_tokens, None);
    assert_eq!(report.overall.cache_read_tokens, Some(25));
    assert!(report.overall.tokens_partial);
    assert_eq!(report.overall.total_tokens, 225);
    let top = cost_report::build_top_cost_report(0, i64::MAX, 10, &records, false, 0);
    assert_eq!(top.rows.len(), 2);
    assert_eq!(top.rows[0].run_id, "run_claude");
    assert_eq!(top.rows[0].total_tokens, 155);
    assert_eq!(top.rows[1].run_id, "run_partial");
    assert_eq!(top.rows[1].total_tokens, 70);
}

#[tokio::test]
async fn completed_native_runs_are_durable_and_reported_by_driver() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let capture = RunCostCapture::new();
    for (driver, fixture, model) in [("grok", GROK, "grok-4.5-build"), ("agy", AGY, "gemini-3.8-flash-low")] {
        let task = create_test_chore(&db, &product, driver);
        let execution = create_ready_chore_execution(&db, &task.id);
        db.record_execution_launch_config(&execution.id, driver, model, None)
            .unwrap();
        db.create_run(
            boss_protocol::CreateRunInput::builder()
                .execution_id(&execution.id)
                .agent_id("worker")
                .status("completed")
                .build(),
        )
        .unwrap();
        let path = dir.path().join(format!("{driver}.jsonl"));
        std::fs::write(&path, fixture).unwrap();
        for _ in 0..2 {
            capture
                .capture_and_persist(&db, &execution.id, &path, None)
                .await
                .unwrap();
        }
        // Replaying after an engine restart is also assignment, not addition.
        capture.forget(&execution.id);
        capture
            .capture_and_persist(&db, &execution.id, &path, None)
            .await
            .unwrap();
    }
    let records = db.cost_records_for_window(0, i64::MAX).unwrap();
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|row| row.run_status == "completed" && row.usage_unavailable.is_none())
    );
    let report = crate::cost_report::build_window_cost_report(0, i64::MAX, &records, true, 0);
    assert_eq!(report.by_driver.len(), 2);
    for (driver, total) in [("grok", 27177), ("agy", 28780)] {
        let bucket = report.by_driver.iter().find(|bucket| bucket.label == driver).unwrap();
        assert_eq!(bucket.measurement.total_tokens, total);
        assert_eq!(bucket.measurement.runs_measured, 1);
        assert_eq!(bucket.measurement.cache_creation_tokens, None);
        assert!(bucket.measurement.tokens_partial);
        assert_eq!(bucket.measurement.estimated_usd, None);
    }
    assert_eq!(report.pricing_gaps.len(), 2);
    assert_eq!(report.overall.total_tokens, 55957);
    assert_eq!(
        serde_json::to_value(&report).unwrap()["overall"]["cache_creation_tokens"],
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn missing_usage_is_visible_and_a_late_measurement_clears_the_reason() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let task = create_test_chore(&db, &product, "missing usage");
    let execution = create_ready_chore_execution(&db, &task.id);
    db.record_execution_launch_config(&execution.id, "agy", "gemini-3.8-flash-low", None)
        .unwrap();
    db.create_run(
        boss_protocol::CreateRunInput::builder()
            .execution_id(&execution.id)
            .agent_id("worker")
            .status("completed")
            .build(),
    )
    .unwrap();
    let records = db.cost_records_for_window(0, i64::MAX).unwrap();
    assert!(
        records[0].usage_unavailable.is_some(),
        "even a missing transcript is visible"
    );
    let path = dir.path().join("usage.jsonl");
    std::fs::write(&path, "{\"type\":\"PLANNER_RESPONSE\",\"step_index\":1}\n").unwrap();
    let capture = RunCostCapture::new();
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    let records = db.cost_records_for_window(0, i64::MAX).unwrap();
    let report = crate::cost_report::build_window_cost_report(0, i64::MAX, &records, true, 0);
    assert_eq!(report.overall.runs_unmeasured, 1);
    assert_eq!(report.overall.runs_zero, 0);
    assert_eq!(report.overall.input_tokens, None);
    assert_eq!(report.overall.usage_unavailable.len(), 1);
    std::fs::write(&path, AGY).unwrap();
    capture.forget(&execution.id);
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    let records = db.cost_records_for_window(0, i64::MAX).unwrap();
    assert_eq!(records[0].input_tokens, Some(20570));
    assert_eq!(records[0].usage_unavailable, None);
    std::fs::write(&path, "").unwrap();
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    let records = db.cost_records_for_window(0, i64::MAX).unwrap();
    assert_eq!(records[0].input_tokens, None);
    assert!(
        records[0].usage_unavailable.is_some(),
        "truncation must not erase the missing-usage reason"
    );
}

#[tokio::test]
async fn model_less_agy_snapshots_keep_the_launch_model_until_observed_or_mixed() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let task = create_test_chore(&db, &product, "launch model");
    let execution = create_ready_chore_execution(&db, &task.id);
    db.record_execution_launch_config(&execution.id, "agy", "gemini-launch", None)
        .unwrap();
    db.create_run(
        boss_protocol::CreateRunInput::builder()
            .execution_id(&execution.id)
            .agent_id("worker")
            .status("completed")
            .build(),
    )
    .unwrap();
    let path = dir.path().join("agy.jsonl");
    let capture = RunCostCapture::new();
    let step = |index: i64, model: Option<&str>| {
        let mut value = json!({"type":"PLANNER_RESPONSE","source":"MODEL","step_index":index,
            "input_tokens":10,"output_tokens":5,"cache_read_tokens":0});
        if let Some(model) = model {
            value["model"] = json!(model);
        }
        format!("{value}\n")
    };
    let model_of_report = |db: &crate::work::WorkDb| {
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        let report = crate::cost_report::build_window_cost_report(0, i64::MAX, &records, true, 0);
        (
            records[0].model.clone(),
            report
                .by_model
                .iter()
                .map(|bucket| bucket.label.clone())
                .collect::<Vec<_>>(),
        )
    };

    // Model-less response: the transcript replacement keeps the launch model.
    std::fs::write(&path, step(1, None)).unwrap();
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    let (model, labels) = model_of_report(&db);
    assert_eq!(model.as_deref(), Some("gemini-launch"));
    assert_eq!(labels, vec!["gemini-launch".to_owned()]);

    // An observed model takes precedence over the launch model.
    std::fs::write(&path, step(1, Some("gemini-observed"))).unwrap();
    capture.forget(&execution.id);
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    assert_eq!(model_of_report(&db).0.as_deref(), Some("gemini-observed"));

    // Mixed observed models clear the model rather than reverting to the launch model.
    std::fs::write(
        &path,
        format!("{}{}", step(1, Some("gemini-observed")), step(2, Some("gemini-other"))),
    )
    .unwrap();
    capture.forget(&execution.id);
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    assert_eq!(model_of_report(&db).0, None);

    // A model-less row alongside one observed model keeps the observed model.
    std::fs::write(&path, format!("{}{}", step(1, None), step(2, Some("gemini-observed")))).unwrap();
    capture.forget(&execution.id);
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    assert_eq!(model_of_report(&db).0.as_deref(), Some("gemini-observed"));
    let mut appended = std::fs::read_to_string(&path).unwrap();
    appended.push_str(&step(3, None));
    std::fs::write(&path, appended).unwrap();
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    assert_eq!(model_of_report(&db).0.as_deref(), Some("gemini-observed"));

    // A Grok turn spread across several models clears the launch model.
    let grok = json!({"method":"session/update","params":{"update":{
        "sessionUpdate":"turn_completed","prompt_id":"p1",
        "usage":{"inputTokens":10,"cachedReadTokens":0,"outputTokens":5,"modelCalls":2,
            "modelUsage":{"grok-a":{},"grok-b":{}}}}}});
    std::fs::write(&path, format!("{grok}\n")).unwrap();
    capture.forget(&execution.id);
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    assert_eq!(model_of_report(&db).0, None);
}
