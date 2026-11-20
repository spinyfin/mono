//! Regression: `boss task show` / `task executions` must surface every
//! `work_runs` row for an execution, including when the parent execution is
//! still live and `work_runs.model` is NULL.

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::{CreateExecutionInput, CreateRunInput, ExecutionKind, ExecutionStatus};
use serde_json::Value;

use common::{run_boss, run_boss_human};
use harness::{TestEngine, create_chore, create_product};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dismiss_question_cli_records_history_and_rejects_late_answers() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Question history").await?;
    let chore = create_chore(&mut client, &product.id, "Choose an approach").await?;
    let db = engine.db()?;
    let execution = db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(&chore.id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .build(),
    )?;
    let question_payload = serde_json::json!({"outcome": "blocked", "summary": "Needs a decision",
        "question": {"text": "Continue?", "answer_type": {"kind": "yes_no"}, "explanation": "Both choices need a decision."}});
    db.submit_worker_proposal_with_liveness(
        boss_engine::work::SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &chore.id,
            kind: boss_protocol::ProposalKind::RunDone,
            payload_json: &question_payload.to_string(),
            idempotency_key: "question-history",
        },
        &|_| false,
    )?
    .map_err(|err| anyhow!("{err:?}"))?;
    db.record_worker_awaiting_operator_answer(
        &execution.id,
        "Needs a decision",
        &boss_protocol::OperatorQuestion {
            text: "Continue?".into(),
            answer_type: boss_protocol::OperatorAnswerType::YesNo,
            explanation: "Both choices need a decision.".into(),
        },
    )?;
    let question = db.list_operator_questions(&chore.id)?[0].question.id.clone();
    let dismissed = run_boss(
        engine.socket_str(),
        &[
            "task",
            "dismiss-question",
            &chore.id,
            "--question",
            &question,
            "--note",
            "Neither choice fits",
        ],
    )?;
    assert_eq!(dismissed["blocked_reason"], "question_dismissed", "{dismissed}");
    let shown = run_boss(engine.socket_str(), &["task", "show", &chore.id])?;
    let history = &shown["operator_questions"][0];
    assert_eq!(history["status"], "dismissed");
    assert_eq!(history["dismissal"]["note"], "Neither choice fits");
    assert!(history["answer"].is_null());
    let human = run_boss_human(engine.socket_str(), &["task", "show", &chore.id])?;
    assert!(human.contains("Dismissed by:"));
    assert!(human.contains("Neither choice fits"));
    let error = common::run_boss_expect_failure(engine.socket_str(), &["task", "answer", &question, "--yes"])?;
    assert!(error.contains("dismissed"), "{error}");
    assert_eq!(db.list_executions(Some(&chore.id))?.len(), 1);
    Ok(())
}

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

/// A held execution and a failed one must explain themselves in the CLI's
/// own output — JSON fields and text lines — without a trip to the engine
/// database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_executions_surface_wait_reason_and_failure_text() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Explain a held run").await?;
    let db = engine.db()?;

    let failed = db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build(),
    )?;
    db.record_pre_start_failure(
        &failed.id,
        "worker-1",
        None,
        "workspace lease failed: remote unreachable",
        &[],
    )?;
    // `waiting_dependency` keeps the live test engine's dispatcher from
    // rewriting the wait reason the test sets.
    let held = db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::WaitingDependency)
            .build(),
    )?;
    db.set_dispatch_wait_with_blocker(&held.id, "awaiting_predecessor_preservation", Some(&failed.id))?;

    for (verb, args) in [
        ("executions", vec!["task", "executions", chore.id.as_str()]),
        ("show", vec!["task", "show", chore.id.as_str()]),
    ] {
        let value = run_boss(engine.socket_str(), &args)?;
        let held_json = execution_json(&value, &held.id)?;
        assert_eq!(
            held_json["dispatch_wait_reason"], "awaiting_predecessor_preservation",
            "{verb}: {held_json}"
        );
        assert_eq!(held_json["dispatch_wait_blocker_id"], failed.id.as_str(), "{verb}");
        assert!(held_json["dispatch_wait_since"].is_string(), "{verb}: {held_json}");
        assert_eq!(held_json["launch_failed"], false, "{verb}: {held_json}");
        assert_eq!(held_json["force_launched"], false, "{verb}: {held_json}");
        assert_eq!(held_json["operator_forced_admission"], false, "{verb}: {held_json}");

        let failed_json = execution_json(&value, &failed.id)?;
        assert_eq!(failed_json["status"], "failed", "{verb}: {failed_json}");
        assert_eq!(
            failed_json["last_error"], "workspace lease failed: remote unreachable",
            "{verb}: {failed_json}"
        );
    }

    let show = run_boss(engine.socket_str(), &["task", "show", &chore.id])?;
    assert_eq!(show["executions_total"], 2, "{show}");

    for args in [
        ["task", "executions", chore.id.as_str()],
        ["task", "show", chore.id.as_str()],
    ] {
        let human = run_boss_human(engine.socket_str(), &args)?;
        assert!(
            human.contains("waiting: awaiting_predecessor_preservation (blocked by "),
            "{human}"
        );
        assert!(
            human.contains("error: workspace lease failed: remote unreachable"),
            "{human}"
        );
        // The flag-history section below the executions has its own format.
        let executions_text = human.split("Flag and abandonment history").next().unwrap_or(&human);
        let epoch_free = executions_text
            .split_whitespace()
            .all(|word| !(word.len() >= 9 && word.chars().all(|c| c.is_ascii_digit())));
        assert!(
            epoch_free,
            "text output must not print raw epoch seconds: {executions_text}"
        );
    }
    Ok(())
}
