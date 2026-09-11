//! Regression: `boss task show` / `task executions` must surface every
//! `work_runs` row for an execution, including when the parent execution is
//! still live and `work_runs.model` is NULL.

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::{CreateExecutionInput, CreateRunInput, ExecutionKind, ExecutionStatus};
use serde_json::Value;

use common::{run_boss, run_boss_human};
use harness::{TestEngine, create_chore, create_product};

fn execution_json<'a>(value: &'a Value, id: &str) -> Result<&'a Value> {
    value["executions"]
        .as_array()
        .ok_or_else(|| anyhow!("executions is not an array: {value}"))?
        .iter()
        .find(|row| row["id"].as_str() == Some(id))
        .ok_or_else(|| anyhow!("missing execution {id} in {value}"))
}

fn assert_run_surfaced(execution: &Value, run_id: &str, transcript_path: &str) -> Result<()> {
    let runs = execution["runs"]
        .as_array()
        .ok_or_else(|| anyhow!("runs is not an array: {execution}"))?;
    assert_eq!(
        runs.len(),
        1,
        "expected one work_runs row for {}, got {runs:?}",
        execution["id"]
    );
    assert_eq!(runs[0]["id"].as_str(), Some(run_id));
    assert_eq!(runs[0]["transcript_path"].as_str(), Some(transcript_path));
    assert!(
        execution.get("runs_unavailable").is_none(),
        "coordinator-tier ListRuns must not mark runs unavailable: {execution}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_show_and_executions_surface_runs_for_running_and_completed_parents() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Investigate run history").await?;
    let db = engine.db()?;

    let running = db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .build(),
    )?;
    let completed = db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Completed)
            .build(),
    )?;

    let running_transcript = "/tmp/running-exec/transcripts/updates.jsonl";
    let completed_transcript = "/tmp/completed-exec/transcripts/updates.jsonl";
    let running_run = db.create_run(
        CreateRunInput::builder()
            .execution_id(running.id.clone())
            .agent_id("worker-2")
            .status("completed")
            .transcript_path(running_transcript)
            .started_at("1000")
            .finished_at("2000")
            .build(),
    )?;
    let completed_run = db.create_run(
        CreateRunInput::builder()
            .execution_id(completed.id.clone())
            .agent_id("worker-1")
            .status("completed")
            .transcript_path(completed_transcript)
            .started_at("900")
            .finished_at("1100")
            .build(),
    )?;

    let show = run_boss(engine.socket_str(), &["task", "show", &chore.id])?;
    assert_run_surfaced(execution_json(&show, &running.id)?, &running_run.id, running_transcript)?;
    assert_run_surfaced(
        execution_json(&show, &completed.id)?,
        &completed_run.id,
        completed_transcript,
    )?;

    let ledger = run_boss(engine.socket_str(), &["task", "executions", &chore.id])?;
    assert_run_surfaced(
        execution_json(&ledger, &running.id)?,
        &running_run.id,
        running_transcript,
    )?;
    assert_run_surfaced(
        execution_json(&ledger, &completed.id)?,
        &completed_run.id,
        completed_transcript,
    )?;

    let human = run_boss_human(engine.socket_str(), &["task", "show", &chore.id])?;
    assert!(
        human.contains(&running_run.id),
        "human task show must print the running execution's run: {human}"
    );
    assert!(
        human.contains(&completed_run.id),
        "human task show must print the completed execution's run: {human}"
    );
    Ok(())
}
