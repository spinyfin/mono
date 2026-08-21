//! Integration tests for the four control verbs added by chore
//! `Implement stubbed bossctl verbs and fix agents stop BossOnly
//! rejection`. Each verb gets a thin end-to-end test through the
//! engine's frontend socket so that re-stubbing them shows up as a
//! red test instead of silently degrading the coordinator.
//!
//! - `cancel_execution` (work cancel / executions cancel): mark a
//!   non-terminal execution `cancelled`; refuse already-terminal rows.
//!   `queued_only` (executions cancel) additionally refuses rows that
//!   have already started and points at `agents stop`.
//! - `tail_run_transcript` (agents transcript): return the last N
//!   lines of a run's transcript, or surface a structured error when
//!   no transcript path is recorded yet.
//! - `workspace_pool_summary` (workspace summary): proxy whatever the
//!   cube layer returns, plus engine-side annotations. The engine's
//!   in-process cube client is a no-op stub here, so we mainly check
//!   the wire shape and that the response decodes.
//! - `stop_run` (agents stop): regression test for the auth tier on
//!   the StopRun verb. `bossctl agents stop` is the coordinator's
//!   imperative kill switch; humans run it from the Boss pane, the
//!   app shell, *and* from inside worker (slot) panes. The earlier
//!   BossOnly tier rejected the worker-pane case; the verb now uses
//!   AppOrBoss, which accepts worker descendants too.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use boss_client::BossClient;
use boss_engine::merge_poller::{
    MergeProbe, OpenPrCiStatus, OpenPrMergeability, OpenPrStatus, PrLifecycleProbe, PrLifecycleState, PrReviewState,
};
use boss_engine::work::WorkDb;
use boss_protocol::{
    BoardColumn, BoardDropTarget, BoardGroup, CreateChoreInput, CreateProductInput, CreateRunInput, ExecutionStatus,
    FrontendEvent, FrontendRequest, RequestExecutionInput, Task, TaskStatus, WorkItem, WorkItemPatch,
};

mod common;
use common::{TestEngine, TestEngineOptions};

/// The `boss engine ci …` / `boss engine conflicts …` remediation-verb half
/// of this suite, split into its own file so neither half trips the repo's
/// 3000-line file-size check. It reads this crate root's fixtures via
/// `use super::*`.
///
/// `#[path]`-attached because this file is the crate root: a bare
/// `mod remediation_verbs;` would resolve to `tests/remediation_verbs.rs`,
/// a sibling that `cargo`/`bazel` would treat as its own integration-test
/// crate root. Same idiom as `engine_lib`'s `#[path = "ci_watch_tests/mod.rs"]`.
#[path = "control_verbs/remediation_verbs.rs"]
mod remediation_verbs;

/// Spawn an engine backed by an on-disk DB so `state_root()` and out-of-band
/// `WorkDb::open(engine.db_path)` reopen a real SQLite file.
async fn spawn_engine() -> Result<TestEngine> {
    TestEngine::spawn_with(TestEngineOptions {
        on_disk_db: true,
        ..Default::default()
    })
    .await
}

/// Returned by `seed_execution` so the test can verify both
/// execution-row state (status flip) and work-item state (kanban
/// column) in the same round-trip.
struct SeededExecution {
    work_item_id: String,
    execution_id: String,
}

/// Create a product + chore + ready execution and return both the
/// chore id and the execution id. Workers don't run in these tests;
/// we just want a row in `work_executions` we can then cancel /
/// inspect, plus the backing work item for kanban-status assertions.
async fn seed_execution(client: &mut BossClient) -> Result<SeededExecution> {
    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: boss_protocol::WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("test chore")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: boss_protocol::WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: boss_protocol::WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };

    let execution = match client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder().work_item_id(chore.id.clone()).build(),
        })
        .await?
    {
        FrontendEvent::ExecutionRequested { execution }
        | FrontendEvent::ExecutionResult { execution }
        | FrontendEvent::ExecutionCreated { execution } => execution,
        other => return Err(anyhow!("unexpected response to RequestExecution: {other:?}")),
    };
    Ok(SeededExecution {
        work_item_id: chore.id,
        execution_id: execution.id,
    })
}

async fn fetch_task_status(client: &mut BossClient, work_item_id: &str) -> Result<TaskStatus> {
    Ok(fetch_task(client, work_item_id).await?.status)
}

/// Re-read a task/chore row. Use over [`fetch_task_status`] when the
/// assertion is about a field other than `status` — or about `status`
/// *staying put* while another field moves.
async fn fetch_task(client: &mut BossClient, work_item_id: &str) -> Result<Task> {
    let response = client
        .send_request(&FrontendRequest::GetWorkItem {
            id: work_item_id.to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkItemResult {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemResult {
            item: WorkItem::Task(t),
        } => Ok(t),
        other => Err(anyhow!("unexpected GetWorkItem response: {other:?}")),
    }
}

#[tokio::test]
async fn work_cancel_marks_execution_cancelled() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution {
        work_item_id,
        execution_id,
    } = seed_execution(&mut client).await?;

    // Drive the chore into Doing and start the execution so cancel is on
    // a *live* row. Demote policy only returns active → todo for live
    // cancel of the work item's latest execution (never-started ready/
    // queued cancel leaves kanban alone — see work/tests).
    client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: work_item_id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    assert_eq!(fetch_task_status(&mut client, &work_item_id).await?, TaskStatus::Active);
    let work_db = WorkDb::open(engine.db_path.clone())?;
    work_db.start_execution_run(
        &execution_id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )?;

    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: execution_id.clone(),
            reason: None,
            queued_only: false,
        })
        .await?;
    let cancelled = match response {
        FrontendEvent::ExecutionCancelled { execution } => execution,
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    assert_eq!(cancelled.id, execution_id);
    assert_eq!(cancelled.status, ExecutionStatus::Cancelled);
    assert!(cancelled.finished_at.is_some(), "cancel must stamp finished_at");

    // Live cancel of the current execution: active → todo so the kanban
    // card returns to Backlog (stop/abandon semantics).
    assert_eq!(fetch_task_status(&mut client, &work_item_id).await?, TaskStatus::Todo);

    // Cancelling a row that's already terminal should error rather than
    // silently no-op — this is what guards the engine against double
    // cancels racing the reconciler.
    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id,
            reason: None,
            queued_only: false,
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("cancelled") || message.contains("terminal"),
                "expected terminal-status error, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn work_cancel_unknown_execution_returns_clear_error() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: "exec_does_not_exist".to_owned(),
            reason: None,
            queued_only: false,
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("unknown execution"),
                "expected unknown-execution message, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

/// `queued_only` (the `bossctl executions cancel` gate) accepts a ready
/// row and stamps it `cancelled`, and refuses a running row with a
/// message that points at `agents stop`.
#[tokio::test]
async fn executions_cancel_queued_only_accepts_ready_refuses_running() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution {
        work_item_id: _,
        execution_id: ready_id,
    } = seed_execution(&mut client).await?;

    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: ready_id.clone(),
            reason: Some("moot after work completed elsewhere".to_owned()),
            queued_only: true,
        })
        .await?;
    let cancelled = match response {
        FrontendEvent::ExecutionCancelled { execution } => execution,
        other => return Err(anyhow!("unexpected response cancelling ready: {other:?}")),
    };
    assert_eq!(cancelled.id, ready_id);
    assert_eq!(cancelled.status, ExecutionStatus::Cancelled);

    // Seed a second ready execution and start it via WorkDb so the
    // status is `running` without needing a live worker pane.
    let SeededExecution {
        work_item_id: _,
        execution_id: running_id,
    } = seed_execution(&mut client).await?;
    let work_db = WorkDb::open(engine.db_path.clone())?;
    work_db.start_execution_run(
        &running_id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )?;

    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: running_id.clone(),
            reason: Some("should refuse".to_owned()),
            queued_only: true,
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("agents stop") || message.contains("already started"),
                "expected refuse-with-agents-stop message, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError for running+queued_only, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_transcript_returns_tail_lines() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution { execution_id, .. } = seed_execution(&mut client).await?;

    // The Run record is the carrier for `transcript_path`; create one
    // directly via WorkDb (no real worker available in this test) and
    // write a small transcript file to disk. Production wires this up
    // through the spawn flow; the engine-side tail behaviour is what
    // we're checking here.
    let transcript_dir = tempfile::tempdir()?;
    let transcript_path = transcript_dir.path().join("transcript.jsonl");
    std::fs::write(
        &transcript_path,
        "{\"event\":\"first\"}\n{\"event\":\"second\"}\n{\"event\":\"third\"}\n",
    )?;
    let work_db = WorkDb::open(engine.db_path.clone())?;
    let run = work_db.create_run(CreateRunInput {
        execution_id,
        agent_id: "test-agent".to_owned(),
        status: Some("active".to_owned()),
        transcript_path: Some(transcript_path.display().to_string()),
        artifacts_path: None,
        result_summary: None,
        error_text: None,
        started_at: None,
        finished_at: None,
    })?;

    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run.id.clone(),
            lines: 2,
        })
        .await?;
    match response {
        FrontendEvent::RunTranscriptTail {
            run_id,
            transcript_path: returned_path,
            lines,
            truncated,
            ..
        } => {
            assert_eq!(run_id, run.id);
            assert_eq!(returned_path, transcript_path.display().to_string());
            assert_eq!(
                lines,
                vec!["{\"event\":\"second\"}".to_owned(), "{\"event\":\"third\"}".to_owned()]
            );
            assert!(truncated, "asking for 2 of 3 lines must mark truncated");
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }

    // Asking for more lines than the file holds returns the whole
    // file and clears the truncated flag.
    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run.id,
            lines: 10,
        })
        .await?;
    match response {
        FrontendEvent::RunTranscriptTail { lines, truncated, .. } => {
            assert_eq!(lines.len(), 3);
            assert!(!truncated);
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_transcript_errors_when_run_has_no_transcript_path() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution { execution_id, .. } = seed_execution(&mut client).await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    let run = work_db.create_run(CreateRunInput {
        execution_id,
        agent_id: "test-agent".to_owned(),
        status: Some("active".to_owned()),
        transcript_path: None,
        artifacts_path: None,
        result_summary: None,
        error_text: None,
        started_at: None,
        finished_at: None,
    })?;

    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run.id,
            lines: 5,
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("transcript"),
                "expected transcript-error message, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_transcript_via_execution_id_returns_tail_lines() -> Result<()> {
    // Regression test for AI #1: `bossctl agents transcript <exec_id>`
    // must work for completed/terminal executions. The engine resolves
    // the transcript path via `work_runs.transcript_path` using the
    // execution_id foreign key, not the run's own id. This test drives
    // `TailRunTranscript` with an exec_* id to confirm the engine's
    // `transcript_path_for_execution` fallback inside
    // `resolve_transcript_for_tail` is reachable.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution { execution_id, .. } = seed_execution(&mut client).await?;

    let transcript_dir = tempfile::tempdir()?;
    let transcript_path = transcript_dir.path().join("transcript.jsonl");
    std::fs::write(
        &transcript_path,
        "{\"event\":\"alpha\"}\n{\"event\":\"beta\"}\n{\"event\":\"gamma\"}\n",
    )?;
    let work_db = WorkDb::open(engine.db_path.clone())?;
    work_db.create_run(CreateRunInput {
        execution_id: execution_id.clone(),
        agent_id: "test-agent".to_owned(),
        status: Some("done".to_owned()),
        transcript_path: Some(transcript_path.display().to_string()),
        artifacts_path: None,
        result_summary: None,
        error_text: None,
        started_at: None,
        finished_at: None,
    })?;

    // Pass the execution id (exec_*) rather than the run id (run_*).
    // This is the path that was broken before AI #1: the engine
    // returned "unknown run: exec_..." because the hot-path cache was
    // gone for a terminal execution and the DB was only queried by run id.
    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: execution_id.clone(),
            lines: 2,
        })
        .await?;
    match response {
        FrontendEvent::RunTranscriptTail {
            transcript_path: returned_path,
            lines,
            truncated,
            ..
        } => {
            assert_eq!(returned_path, transcript_path.display().to_string());
            assert_eq!(
                lines,
                vec!["{\"event\":\"beta\"}".to_owned(), "{\"event\":\"gamma\"}".to_owned()]
            );
            assert!(truncated, "asking for 2 of 3 lines must set truncated");
        }
        other => return Err(anyhow!("expected RunTranscriptTail, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_transcript_reports_the_execution_driver_slug() -> Result<()> {
    // `bossctl agents transcript` needs the run's driver alongside the raw
    // tail to normalize non-Claude/Codex dialects (e.g. Grok's ACP
    // `session/update` envelope, which carries no schema-detectable
    // top-level `type` field) before rendering. Pins that `RunTranscriptTail`
    // actually carries the resolved slug for both the `run_*` and `exec_*`
    // id namespaces `bossctl` may pass.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution {
        execution_id,
        work_item_id,
    } = seed_execution(&mut client).await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    work_db.update_work_item(
        &work_item_id,
        WorkItemPatch {
            driver: Some("grok".to_owned()),
            ..WorkItemPatch::default()
        },
    )?;

    let transcript_dir = tempfile::tempdir()?;
    let transcript_path = transcript_dir.path().join("transcript.jsonl");
    std::fs::write(&transcript_path, "{\"event\":\"alpha\"}\n")?;
    let run = work_db.create_run(CreateRunInput {
        execution_id: execution_id.clone(),
        agent_id: "test-agent".to_owned(),
        status: Some("done".to_owned()),
        transcript_path: Some(transcript_path.display().to_string()),
        artifacts_path: None,
        result_summary: None,
        error_text: None,
        started_at: None,
        finished_at: None,
    })?;

    // exec_* namespace.
    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: execution_id.clone(),
            lines: 0,
        })
        .await?;
    match response {
        FrontendEvent::RunTranscriptTail { driver, .. } => {
            assert_eq!(
                driver.as_deref(),
                Some("grok"),
                "exec_* lookup must resolve the driver slug"
            );
        }
        other => return Err(anyhow!("expected RunTranscriptTail, got: {other:?}")),
    }

    // run_* namespace.
    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run.id,
            lines: 0,
        })
        .await?;
    match response {
        FrontendEvent::RunTranscriptTail { driver, .. } => {
            assert_eq!(
                driver.as_deref(),
                Some("grok"),
                "run_* lookup must resolve the driver slug"
            );
        }
        other => return Err(anyhow!("expected RunTranscriptTail, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn get_run_via_execution_id_returns_run_record() -> Result<()> {
    // Regression test for AI #1: `bossctl agents status <exec_id>`
    // must return the run record for a completed execution. Before this
    // fix, `GetRun { id: exec_id }` returned "unknown run: exec_..."
    // because the handler only queried `work_runs.id` (run_* ns).
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution { execution_id, .. } = seed_execution(&mut client).await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    let run = work_db.create_run(CreateRunInput {
        execution_id: execution_id.clone(),
        agent_id: "test-agent-history".to_owned(),
        status: Some("done".to_owned()),
        transcript_path: None,
        artifacts_path: None,
        result_summary: None,
        error_text: None,
        started_at: None,
        finished_at: None,
    })?;

    // Pass execution id; the engine must resolve it to the run row.
    let response = client
        .send_request(&FrontendRequest::GetRun {
            id: execution_id.clone(),
        })
        .await?;
    match response {
        FrontendEvent::RunResult { run: returned } => {
            assert_eq!(returned.id, run.id);
            assert_eq!(returned.execution_id, execution_id);
        }
        other => return Err(anyhow!("expected RunResult, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn workspace_summary_returns_pool_snapshot() -> Result<()> {
    // The in-process engine builds a `CommandCubeClient` which would
    // shell out to a real `cube` binary. That isn't available in
    // sandboxed test environments, so this test asserts the verb
    // round-trips at the protocol level: it either returns a
    // (possibly empty) workspace list, or surfaces a structured
    // WorkError from the cube CLI failure. Both prove the verb is
    // wired through the engine — what we're really guarding against
    // is the verb regressing back to the literal `not_implemented`
    // stub it used to return.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let response = client.send_request(&FrontendRequest::WorkspacePoolSummary).await?;
    match response {
        FrontendEvent::WorkspacePoolSummaryResult { .. } => {}
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("cube") || message.contains("workspace"),
                "expected cube-related error, got: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_stop_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // Reproduces the bug from the work item: even after the earlier
    // BossOnly fallback fix, `bossctl agents stop` still hit
    // "stop_run is BossOnly" when invoked from inside a worker
    // (slot) pane — the BossOnly gate explicitly excludes callers
    // that descend from a registered worker shell pid. The verb is
    // now AppOrBoss, which accepts worker descendants too. In the
    // in-process test harness app_pid and boss_pid are both unset
    // (treated as permissive), so any local caller must succeed
    // here; the worker-descendant case is locked in by the macOS
    // unit test `app_or_boss_admits_worker_descendant`.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::StopRun {
            run_id: "run-does-not-exist".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::RunStopped { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly") && !message.contains("requires app or Boss authority"),
                "stop_run must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn probe_run_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // Same regression class as `agents_stop` (PR #218): the BossOnly
    // gate rejected `bossctl probe` calls from worker-pane shells
    // because the gate explicitly excludes descendants of any
    // registered worker pid. The verb is now AppOrBoss — worker
    // descendants are admitted (workers are siblings under the app).
    // The macOS unit test `app_or_boss_admits_worker_descendant`
    // locks in the worker-descendant admission; this test is the
    // wire-shape smoke that asserts probe is reachable at all.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::ProbeRun {
            run_id: "run-does-not-exist".to_owned(),
            text: "ping".to_owned(),
            urgent: false,
            // Boundary delivery: this test is about the queue-and-wait path.
            interrupt: false,
        })
        .await?;
    match response {
        // `ProbeRefused` is the expected answer for a run with no live pane
        // and is itself proof that authorization passed — the auth gate
        // returns `Error` and short-circuits before the deliverability check
        // this response comes from.
        FrontendEvent::ProbeRefused { reason, .. } => {
            assert!(
                !reason.contains("authority"),
                "a deliverability refusal must not be an auth refusal in disguise: {reason}"
            );
        }
        FrontendEvent::ProbeQueued { .. } => {}
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            assert!(
                !message.contains("BossOnly") && !message.contains("requires app or Boss authority"),
                "probe_run must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_send_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // `bossctl agents send` writes user-typed input into a sibling
    // worker pane. Same auth class as `agents focus` / `probe` /
    // `agents stop` (AppOrBoss). With no run seeded, the verb should
    // pass auth and then fail the run-id lookup with a `WorkError`.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::SendInputToWorker {
            run_id: "run-does-not-exist".to_owned(),
            text: "hi\n".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly") && !message.contains("requires app or Boss authority"),
                "send_input_to_worker must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn probe_run_refuses_a_run_with_no_live_pane_rather_than_queueing_it() -> Result<()> {
    // Wire-shape smoke for the refusal contract. A probe aimed at a run the
    // engine has no pane for can never be delivered, and reporting it as
    // queued makes it indistinguishable from one that is "arriving shortly".
    // Both attempts must be refused, and the refusal must name the blocking
    // condition rather than being generic.
    //
    // The accepted-probe shapes this test used to cover (`probe_id` minting,
    // the echoed `urgent` flag, the committed delivery boundary) need a live
    // worker slot, which this out-of-process harness cannot register; they are
    // covered in-process in `app::tests::probe_delivery`, alongside
    // `queue_probe_mints_unique_probe_ids` in `worker_probe_dispatch`.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    for text in ["first", "second"] {
        let response = client
            .send_request(&FrontendRequest::ProbeRun {
                run_id: "run-xyz".to_owned(),
                text: text.to_owned(),
                urgent: false,
                // Boundary delivery: this test is about the queue-and-wait path.
                interrupt: false,
            })
            .await?;
        match response {
            FrontendEvent::ProbeRefused { run_id, reason } => {
                assert_eq!(run_id, "run-xyz");
                assert!(
                    reason.contains("no live worker pane"),
                    "refusal must name the blocking condition: {reason}"
                );
            }
            other => return Err(anyhow!("unexpected response to probe {text:?}: {other:?}")),
        }
    }
    Ok(())
}

#[tokio::test]
async fn urgent_probe_against_a_run_with_no_pane_is_refused_too() -> Result<()> {
    // The `--urgent` flag must not buy a probe past the deliverability check.
    // An urgent probe promises tool-boundary delivery; with no pane at all
    // there is no boundary that could honour it, so the answer is the same
    // refusal as the non-urgent case.
    //
    // The positive urgent shapes — `urgent: true` echoed back and an
    // `expected_delivery` of `next_tool_boundary` — require a registered
    // worker slot and live in `app::tests::probe_delivery`.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let urgent_resp = client
        .send_request(&FrontendRequest::ProbeRun {
            run_id: "run-urgent".to_owned(),
            text: "course-correct now".to_owned(),
            urgent: true,
            // Boundary delivery: this test is about the queue-and-wait path.
            interrupt: false,
        })
        .await?;
    match urgent_resp {
        FrontendEvent::ProbeRefused { run_id, reason } => {
            assert_eq!(run_id, "run-urgent");
            assert!(
                reason.contains("no live worker pane"),
                "refusal must name the blocking condition: {reason}"
            );
        }
        other => return Err(anyhow!("unexpected response to urgent probe: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn probe_status_reports_an_unknown_probe_id_as_an_error() -> Result<()> {
    // Wire-shape smoke for the queryable delivery state: the verb is
    // reachable, and an id this engine process never minted comes back as an
    // error rather than a fabricated state. (Probe ids are per-process and
    // are not persisted, so "unknown" is a real answer, not a bug.)
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::ProbeStatus {
            probe_id: "probe-never-minted".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("unknown probe id"),
                "unknown probe id must be reported as such: {message}"
            );
            assert!(
                !message.contains("requires app or Boss authority"),
                "probe_status must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_transcript_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // `bossctl agents transcript` shares the BossOnly→AppOrBoss
    // downgrade with `bossctl probe` and `bossctl agents stop`. This
    // smoke test guards against the verb regressing back to BossOnly
    // and silently locking the coordinator out of worker transcripts.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: "run-does-not-exist".to_owned(),
            lines: 5,
        })
        .await?;
    match response {
        // Auth passed; the verb went on to fail the run lookup
        // (expected — we did not seed a run).
        FrontendEvent::WorkError { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly") && !message.contains("requires app or Boss authority"),
                "tail_run_transcript must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_interrupt_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // `bossctl agents interrupt` ships at the same AppOrBoss tier as
    // `agents focus` / `agents stop` — humans run it from the Boss
    // pane, the app shell, *and* from inside worker (slot) panes.
    // This smoke guards against the verb regressing to BossOnly and
    // silently locking the coordinator out of in-flight Esc.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::InterruptWorkerPane {
            run_id: "run-does-not-exist".to_owned(),
        })
        .await?;
    match response {
        // Auth passed; the verb went on to fail the run lookup
        // (expected — we did not seed a run).
        FrontendEvent::WorkError { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly") && !message.contains("requires app or Boss authority"),
                "interrupt_worker_pane must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_reap_marks_running_execution_orphaned() -> Result<()> {
    // Drive a seeded chore from `ready` → `running` (so it has the
    // workspace columns the orphan path needs to preserve), then send
    // a `ReapRun` and verify:
    //   - the engine returns `RunReaped` with status='orphaned',
    //   - cube workspace columns are preserved on the row,
    //   - a second reap on the now-terminal row errors cleanly.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution { execution_id, .. } = seed_execution(&mut client).await?;

    // Start an actual run on the execution so the workspace columns
    // are populated. `start_execution_run` requires the row to be
    // `ready` first, which `seed_execution` guarantees.
    let work_db = WorkDb::open(engine.db_path.clone())?;
    work_db.start_execution_run(
        &execution_id,
        "test-agent",
        "mono",
        "lease-REAP",
        "mono-agent-007",
        "/tmp/mono-agent-007",
    )?;

    let response = client
        .send_request(&FrontendRequest::ReapRun {
            run_id: execution_id.clone(),
        })
        .await?;
    let reaped = match response {
        FrontendEvent::RunReaped { run_id, execution } => {
            assert_eq!(run_id, execution_id);
            execution
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    assert_eq!(reaped.id, execution_id);
    assert_eq!(reaped.status, ExecutionStatus::Orphaned);
    assert!(reaped.finished_at.is_some(), "reap must stamp finished_at");
    // Workspace columns preserved — that's the whole contract.
    assert_eq!(reaped.cube_lease_id.as_deref(), Some("lease-REAP"));
    assert_eq!(reaped.cube_workspace_id.as_deref(), Some("mono-agent-007"));
    assert_eq!(reaped.workspace_path.as_deref(), Some("/tmp/mono-agent-007"));

    // Second reap on the now-terminal row must error rather than
    // silently no-op — same contract as `cancel_execution`.
    let response = client
        .send_request(&FrontendRequest::ReapRun { run_id: execution_id })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("terminal"),
                "expected terminal-status error, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_reap_unknown_run_returns_clear_error() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::ReapRun {
            run_id: "exec_does_not_exist".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("unknown execution"),
                "expected unknown-execution message, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

/// Regression: dragging an `autostart=false` chore from Todo to
/// Doing in the macOS kanban must dispatch a worker. The earlier
/// failure shape was that `UpdateWorkItem` flipped status to `active`
/// but no execution row appeared — `tasks.autostart=0` made reconcile
/// silently skip the chore at create time and there was no
/// server-side hook on the human transition to seed one. The
/// kanban-drag fix now fires `RequestExecution` from the engine
/// itself when a task/chore moves into `active` via UpdateWorkItem,
/// so the invariant holds regardless of whether the client also fires
/// the RPC.
///
/// Acceptance:
/// - chore created with `autostart=false` has no execution row,
/// - after `UpdateWorkItem` flips status to `active`, the chore has a
///   non-terminal execution backing it.
#[tokio::test]
async fn kanban_drag_to_doing_dispatches_autostart_false_chore() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Parked chore")
                // The bug scenario: --no-autostart leaves the chore in
                // `todo` with no execution, waiting for an explicit
                // schedule trigger (drag-to-Doing or `bossctl work
                // start`). Without the fix, the drag does not trigger
                // dispatch and the card is "active" with no worker.
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };
    assert_eq!(chore.status, TaskStatus::Todo);
    assert!(!chore.autostart);

    // No execution at create time — autostart=false means the
    // reconcile gate (`task_accepts_execution`) skips creation while
    // the chore sits in `todo`.
    let before = list_executions_for(&mut client, &chore.id).await?;
    assert!(
        before.is_empty(),
        "autostart=false chore must not have a creation-time execution; got {before:?}"
    );

    // Drive the kanban drag-to-Doing: `UpdateWorkItem` with `status =
    // active`. The fix makes this fire `RequestExecution` server-side
    // — without it, the chore sat `active` with no execution.
    let updated = match client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?
    {
        FrontendEvent::WorkItemUpdated { item } => item,
        other => return Err(anyhow!("unexpected response to UpdateWorkItem: {other:?}")),
    };
    match updated {
        WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
        other => return Err(anyhow!("unexpected item kind: {other:?}")),
    }

    // After the drag, the chore must have a non-terminal execution.
    let after = list_executions_for(&mut client, &chore.id).await?;
    assert_eq!(
        after.len(),
        1,
        "drag-to-Doing must create exactly one ready execution; got {after:?}"
    );
    let exec = &after[0];
    assert!(
        matches!(
            exec.status.as_str(),
            "ready" | "queued" | "running" | "waiting_human" | "waiting_dependency"
        ),
        "drag-to-Doing execution should be non-terminal; got status={status:?}",
        status = exec.status
    );
    assert_eq!(exec.work_item_id, chore.id);

    Ok(())
}

/// A second drag from `active` → `active` (idempotent client retry,
/// or status patch from a different field landing alongside an
/// already-active card) must not multiply executions. The fix only
/// fires dispatch on a *transition* into `active`, and even then only
/// when there is no existing non-terminal execution.
#[tokio::test]
async fn kanban_drag_to_doing_is_idempotent_on_repeat() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Parked chore")
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };

    // First drag: creates exec #1.
    let _ = client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    let after_first = list_executions_for(&mut client, &chore.id).await?;
    assert_eq!(after_first.len(), 1, "first drag should create exec");
    let first_exec_id = after_first[0].id.clone();

    // Second UpdateWorkItem that re-asserts `active` (e.g., a stray
    // patch from a sibling field). Must not abandon the existing
    // ready exec or insert a duplicate.
    let _ = client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    let after_second = list_executions_for(&mut client, &chore.id).await?;
    assert_eq!(
        after_second.len(),
        1,
        "no-op active→active must not create a new execution; got {after_second:?}"
    );
    assert_eq!(
        after_second[0].id, first_exec_id,
        "original execution must be preserved",
    );

    Ok(())
}

/// A kanban drag-to-Doing fires the `status_transition` dispatch
/// event so an operator running `bossctl dispatch tail` can see
/// exactly when (and whether) the engine decided to auto-dispatch
/// after the human flipped the card.
#[tokio::test]
async fn kanban_drag_emits_status_transition_event() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Parked chore")
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };

    let _ = client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;

    // Drain a beat so the async emit lands on disk before we read.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let events = boss_engine::dispatch_reader::read_current(&engine.state_root())?.events;
    let transition: Vec<_> = events.iter().filter(|e| e.stage == "status_transition").collect();
    assert_eq!(
        transition.len(),
        1,
        "expected exactly one status_transition event; got {transition:?}"
    );
    assert_eq!(transition[0].outcome, "ok");
    assert_eq!(transition[0].work_item_id.as_deref(), Some(chore.id.as_str()));
    assert_eq!(
        transition[0].details.get("did_dispatch"),
        Some(&serde_json::Value::Bool(true)),
        "first drag should have did_dispatch=true; got {:?}",
        transition[0].details
    );

    // Second drag is a no-op (already active) — must NOT emit a
    // duplicate status_transition because `task_transitioned_to_active`
    // requires an actual transition.
    let _ = client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let events_after = boss_engine::dispatch_reader::read_current(&engine.state_root())?.events;
    let transitions_after: Vec<_> = events_after.iter().filter(|e| e.stage == "status_transition").collect();
    assert_eq!(
        transitions_after.len(),
        1,
        "no-op active→active must not emit a second status_transition event",
    );

    Ok(())
}

/// Bug #679: dragging a card into Doing when the work item has no
/// resolvable repo used to flip `tasks.status='active'` server-side
/// and then swallow the dispatch failure in a `WARN`. The card sat
/// in Doing with no worker. The fix pre-validates the repo
/// precondition: if the engine can prove dispatch will fail, the
/// `UpdateWorkItem` is rejected as `WorkError`, the card stays in
/// its previous column, and the user sees the actionable message
/// naming the missing repo.
#[tokio::test]
async fn kanban_drag_to_doing_rejects_chore_with_no_resolvable_repo() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let chore = seed_unresolvable_chore(&mut client).await?;
    assert_eq!(chore.status, TaskStatus::Todo);

    // The drag. Without the fix this returns WorkItemUpdated with
    // status=active and only a tracing WARN records the failure.
    let response = client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("has no repo resolution"),
                "expected repo-resolution message, got: {message}"
            );
            assert!(
                message.contains("boss chore update --repo <url>"),
                "error should name the CLI fix, got: {message}"
            );
        }
        other => {
            return Err(anyhow!(
                "drag-to-Doing with no repo must return WorkError, got: {other:?}"
            ));
        }
    }

    // Status stayed in `todo` — the rejection blocked the patch.
    assert_eq!(fetch_task_status(&mut client, &chore.id).await?, TaskStatus::Todo);

    // No execution row was created.
    let execs = list_executions_for(&mut client, &chore.id).await?;
    assert!(
        execs.is_empty(),
        "rejected drag must not leave an execution row; got {execs:?}"
    );

    // The kanban Attention lane mirrors the CLI: a sticky
    // `repo_unresolved` item names the offender.
    let attn_response = client
        .send_request(&FrontendRequest::ListAttentionItemsForWorkItem {
            work_item_id: chore.id.clone(),
        })
        .await?;
    let attention = match attn_response {
        FrontendEvent::AttentionItemsForWorkItemList { items, .. } => items,
        other => {
            return Err(anyhow!(
                "unexpected response to ListAttentionItemsForWorkItem: {other:?}"
            ));
        }
    };
    assert_eq!(
        attention.len(),
        1,
        "exactly one repo_unresolved attention item should be open; got {attention:?}"
    );
    assert_eq!(attention[0].kind, "repo_unresolved");
    assert_eq!(attention[0].status, "open");

    Ok(())
}

/// The rejected drag still records a `status_transition` event with
/// `outcome=error` so `bossctl dispatch tail` reflects the
/// deterministic gate firing — same observability surface as the
/// successful and skipped paths.
#[tokio::test]
async fn kanban_drag_emits_status_transition_error_when_repo_unresolvable() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let chore = seed_unresolvable_chore(&mut client).await?;

    let _ = client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;

    // Drain a beat so the async emit lands on disk before we read.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let events = boss_engine::dispatch_reader::read_current(&engine.state_root())?.events;
    let transition: Vec<_> = events.iter().filter(|e| e.stage == "status_transition").collect();
    assert_eq!(
        transition.len(),
        1,
        "expected exactly one status_transition event; got {transition:?}"
    );
    assert_eq!(transition[0].outcome, "error");
    assert_eq!(transition[0].work_item_id.as_deref(), Some(chore.id.as_str()));
    assert_eq!(
        transition[0].details.get("did_dispatch"),
        Some(&serde_json::Value::Bool(false)),
        "rejection should have did_dispatch=false; got {:?}",
        transition[0].details
    );
    assert_eq!(
        transition[0].details.get("rejected"),
        Some(&serde_json::Value::Bool(true)),
        "rejection should be tagged; got {:?}",
        transition[0].details
    );
    let error_message = transition[0].error_message.as_deref().unwrap_or_default();
    assert!(
        error_message.contains("has no repo resolution"),
        "event error_message should name the missing repo; got {error_message:?}"
    );

    Ok(())
}

/// Drive the engine into the state the bug report describes: a
/// task/chore whose product has no default repo and whose row has no
/// override, so `resolve_repo_for_work_item` returns `None`. The
/// chore-creation precheck blocks the direct path, so we round-trip
/// through the engine: create with a repo, then clear the product's
/// repo via `UpdateWorkItem`. The chore row's own `repo_remote_url`
/// stays NULL (inherited from product at insert), and the cleared
/// product default makes resolution fail — the exact failure shape
/// the bug describes for a `kind=design` task auto-created under a
/// no-repo product.
async fn seed_unresolvable_chore(client: &mut BossClient) -> Result<boss_protocol::Task> {
    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Unresolvable chore")
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };

    // Clear the product's repo so the chore now resolves to nothing.
    // `apply_repo_remote_url_patch` canonicalises `Some("")` → `None`,
    // matching the bossctl `product update --repo ""` clear path.
    match client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: product.id.clone(),
            patch: WorkItemPatch {
                repo_remote_url: Some(String::new()),
                ..WorkItemPatch::default()
            },
        })
        .await?
    {
        FrontendEvent::WorkItemUpdated { .. } => {}
        other => {
            return Err(anyhow!("unexpected response clearing product repo: {other:?}"));
        }
    };

    Ok(chore)
}

/// `MoveWorkItemOnBoard` with no pause-only override requested — the shape
/// every drag in this file's fixtures needs. Centralised so the two
/// override-only fields (`bypass_dispatch_pause`, `observed_pause_since_epoch_s`)
/// have one call site to update instead of one per test.
fn move_on_board_request(id: &str, column: BoardColumn, group: Option<BoardGroup>) -> FrontendRequest {
    FrontendRequest::MoveWorkItemOnBoard {
        id: id.to_owned(),
        target: BoardDropTarget::new(column, group),
        bypass_dispatch_pause: false,
        observed_pause_since_epoch_s: None,
    }
}

async fn list_executions_for(client: &mut BossClient, work_item_id: &str) -> Result<Vec<boss_protocol::WorkExecution>> {
    match client
        .send_request(&FrontendRequest::ListExecutions {
            work_item_id: Some(work_item_id.to_owned()),
            include_revision_chain: false,
        })
        .await?
    {
        FrontendEvent::ExecutionsList { executions, .. } => Ok(executions),
        other => Err(anyhow!("unexpected response to ListExecutions: {other:?}")),
    }
}

#[tokio::test]
async fn workspace_summary_does_not_reject_caller_on_auth_grounds() -> Result<()> {
    // Live-coordinator-session repro: `bossctl workspace summary` was
    // failing AppOrBoss when invoked from a shell that descended from
    // neither the app nor the registered Boss session (e.g., a plain
    // terminal). The verb is read-only and proxies a view that any
    // local user can already get from `cube workspace list`, so it's
    // now User-tier. This smoke asserts no auth rejection fires.
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client.send_request(&FrontendRequest::WorkspacePoolSummary).await?;
    match response {
        FrontendEvent::WorkspacePoolSummaryResult { .. } => {}
        // The in-process engine builds a CommandCubeClient which
        // shells out; the cube binary may not be on PATH in the
        // sandbox, so a `WorkError` from the cube layer is acceptable.
        // What we're guarding against is an `Error` carrying an auth
        // rejection.
        FrontendEvent::WorkError { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly")
                    && !message.contains("requires app or Boss authority")
                    && !message.contains("user-tier check"),
                "workspace_pool_summary must not reject callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

/// `RequestExecution` should accept friendly-id forms (T1, T2, …) in
/// addition to the primary `task_*` ids. This covers the
/// `bossctl work start T3` failure from 2026-05-14 where the engine
/// rejected the bareword before even checking the work-item table.
#[tokio::test]
async fn request_execution_accepts_tnnn_friendly_id() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // Seed a product + chore. The first chore in a fresh DB gets short_id=1
    // (or the next available sequence slot), making it addressable as T1.
    let SeededExecution { work_item_id, .. } = seed_execution(&mut client).await?;

    // Fetch the chore to learn its short_id.
    let short_id = match client
        .send_request(&FrontendRequest::GetWorkItem {
            id: work_item_id.clone(),
        })
        .await?
    {
        FrontendEvent::WorkItemResult {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemResult {
            item: WorkItem::Task(t),
        } => t.short_id.expect("chore must have a short_id after creation"),
        other => return Err(anyhow!("unexpected GetWorkItem response: {other:?}")),
    };

    let friendly_id = format!("T{short_id}");

    // Re-request execution using the friendly id — the engine must
    // resolve it and return the same execution row (idempotent path).
    let response = client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(friendly_id.clone())
                .build(),
        })
        .await?;

    match response {
        FrontendEvent::ExecutionRequested { execution }
        | FrontendEvent::ExecutionResult { execution }
        | FrontendEvent::ExecutionCreated { execution } => {
            assert_eq!(
                execution.work_item_id, work_item_id,
                "resolved work_item_id must match the primary id"
            );
        }
        FrontendEvent::WorkError { message } => {
            return Err(anyhow!(
                "engine rejected RequestExecution with friendly id {friendly_id}: {message}"
            ));
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

/// Regression: reordering a card *inside* the Done column's "Merging"
/// group must not mark it `done`.
///
/// The Done column is not a flat list of completed work. Its "Merging"
/// group holds `in_review` rows whose PR is in a merge queue or has Merge
/// When Ready armed — in flight, deliberately rendered where the merge will
/// land. The macOS client used to translate a drop into that column
/// straight into `UpdateWorkItem { status: "done" }`, and its only guard
/// was `status != target_status`; `in_review != done`, so the patch landed
/// and an ordinary reorder gesture silently completed a live merge. The
/// consequence was not cosmetic: the merge poller only watches
/// `status = 'in_review'` rows, so a wrongly-`done` row stops being tracked
/// and a later queue CI failure is never observed.
///
/// The drag now reports its drop target and the engine resolves the
/// meaning. Acceptance, all against one queued row:
/// - dropped on Done ▸ Merging (the group it is already in): still
///   `in_review`, merge-queue state intact,
/// - dropped on the Done column with no group named (a drop that missed
///   every section): still `in_review`,
/// - dropped on Done ▸ a completion group: `done`, because crossing out of
///   Merging into completed work is the one gesture there that does mean
///   "this is finished".
#[tokio::test]
async fn board_drop_inside_merging_group_does_not_complete_the_merge() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Migrate stranded runbooks")
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };

    // Put the row in the exact state that renders in Done ▸ Merging: PR
    // open (`in_review`) and sitting in the merge queue.
    client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/spinyfin/mono/pull/1".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    let work_db = WorkDb::open(engine.db_path.clone())?;
    assert!(
        work_db.set_task_merge_queue_state(&chore.id, Some("queued"), None)?,
        "queue-state seed must land or the row would not render in Merging"
    );
    assert_eq!(fetch_task_status(&mut client, &chore.id).await?, TaskStatus::InReview);

    // The gesture from the bug report: a reorder inside the group.
    match client
        .send_request(&move_on_board_request(
            &chore.id,
            BoardColumn::Done,
            Some(BoardGroup::Merging),
        ))
        .await?
    {
        FrontendEvent::WorkItemUpdated { item } => match item {
            WorkItem::Chore(t) | WorkItem::Task(t) => {
                assert_eq!(
                    t.status,
                    TaskStatus::InReview,
                    "a reorder inside Merging must not change status"
                );
                assert_eq!(
                    t.merge_queue_state.as_deref(),
                    Some("queued"),
                    "the row must still be tracked as an in-flight merge"
                );
            }
            other => return Err(anyhow!("unexpected item kind: {other:?}")),
        },
        other => return Err(anyhow!("unexpected response to MoveWorkItemOnBoard: {other:?}")),
    }

    // A drop that missed every section names the column only. Less intent,
    // not more — it must not be read as a completion either.
    client
        .send_request(&move_on_board_request(&chore.id, BoardColumn::Done, None))
        .await?;
    assert_eq!(
        fetch_task_status(&mut client, &chore.id).await?,
        TaskStatus::InReview,
        "an unqualified drop on the card's own column must not change status"
    );

    // Crossing out of Merging into a completion group is a real completion
    // and must keep working — the fix must not blanket-disable Done.
    client
        .send_request(&move_on_board_request(
            &chore.id,
            BoardColumn::Done,
            Some(BoardGroup::Completed),
        ))
        .await?;
    assert_eq!(
        fetch_task_status(&mut client, &chore.id).await?,
        TaskStatus::Done,
        "Merging → a completion group must still complete the row"
    );
    Ok(())
}

/// Companion to the test above, for a column with no groups: dropping a
/// card back onto the column it already occupies is a reorder, so it must
/// not fire the transition that column would otherwise assert. Before the
/// engine owned this decision, a `blocked` row reordered inside its own
/// column flipped to that column's status — clearing the block as a side
/// effect of a gesture that only meant "move this up a bit".
#[tokio::test]
async fn board_drop_on_own_column_is_a_reorder_for_a_blocked_row() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Parked and blocked")
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };

    // `blocked` for a non-review reason renders in Backlog.
    client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("blocked".to_owned()),
                blocked_reason: Some("waiting_on_human".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    assert_eq!(fetch_task_status(&mut client, &chore.id).await?, TaskStatus::Blocked);

    client
        .send_request(&move_on_board_request(&chore.id, BoardColumn::Backlog, None))
        .await?;
    assert_eq!(
        fetch_task_status(&mut client, &chore.id).await?,
        TaskStatus::Blocked,
        "reordering inside Backlog must not clear the block"
    );

    // Leaving the column is still a real transition.
    client
        .send_request(&move_on_board_request(&chore.id, BoardColumn::Review, None))
        .await?;
    assert_eq!(
        fetch_task_status(&mut client, &chore.id).await?,
        TaskStatus::InReview,
        "Backlog → Review must still transition"
    );
    Ok(())
}

/// Nothing can be dragged *into* the Merging group: a row is there because
/// the engine observed its PR in a merge queue, so the gesture asserts a
/// fact the client cannot know. The engine refuses rather than picking
/// the nearest plausible transition, and mutates nothing.
#[tokio::test]
async fn board_drop_into_merging_group_is_refused() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Not in any queue")
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };

    match client
        .send_request(&move_on_board_request(
            &chore.id,
            BoardColumn::Done,
            Some(BoardGroup::Merging),
        ))
        .await?
    {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("Merging"),
                "refusal must name the group it is about, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    assert_eq!(
        fetch_task_status(&mut client, &chore.id).await?,
        TaskStatus::Todo,
        "a refused drop must mutate nothing"
    );
    Ok(())
}

/// Doing → Backlog for a row that is queued to dispatch but has not started.
/// This is the one drag whose *status* does not change: `todo` + `autostart`
/// renders in Doing, plain `todo` renders in Backlog, so both columns map to
/// `todo` and a status patch would be a no-op that snapped the card back.
/// Clearing `autostart` is what parks it.
///
/// Covered here and not only at the resolver level because this branch
/// changed layers: the drag used to send `update_work_item { autostart:
/// false }` from the app, and now the engine derives it. The end-to-end
/// assertion is that the patch actually lands on the row — resolver unit
/// tests and the client's optimistic-column test can both pass while the
/// verb writes nothing.
#[tokio::test]
async fn board_drop_from_doing_to_backlog_clears_autostart_on_a_dispatch_pending_row() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    // Created parked so no creation-time dispatch races the assertion, then
    // armed — leaving exactly the `todo` + `autostart` shape that renders in
    // Doing with no execution row.
    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Queued to dispatch")
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };
    client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                autostart: Some(true),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    let armed = fetch_task(&mut client, &chore.id).await?;
    assert_eq!(armed.status, TaskStatus::Todo);
    assert!(armed.autostart, "the row must be dispatch-pending to render in Doing");

    client
        .send_request(&move_on_board_request(&chore.id, BoardColumn::Backlog, None))
        .await?;

    let parked = fetch_task(&mut client, &chore.id).await?;
    assert_eq!(
        parked.status,
        TaskStatus::Todo,
        "both columns are `todo`; the drag must not invent a status change"
    );
    assert!(
        !parked.autostart,
        "Doing → Backlog must clear autostart, or the card snaps back to Doing"
    );
    Ok(())
}
