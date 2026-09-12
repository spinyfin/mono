//! The boss CLI resolves `engine.sock` relative to `$HOME` unless
//! `BOSS_SOCKET_PATH` is set. Grok scopes worker `$HOME` to a per-run
//! process-home that never contains `engine.sock`, so these tests drive the
//! compiled `boss` binary the way a Grok worker does and assert both
//! outcomes: success with `BOSS_SOCKET_PATH` set, and exit 5 without it.
//! `--socket-path` at the production data dir is blocked by the path-guard
//! hook; the bound frontend socket is how a worker reaches the engine.

use std::process::Command;

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::{
    CreateExecutionInput, ExecutionKind, ExecutionStatus, ProposalKind, ProposalState, RunDoneOutcome,
};
use serde_json::Value;
use tempfile::TempDir;

use common::boss_binary;
use harness::{TestEngine, create_chore, create_product};

fn grok_process_home() -> Result<TempDir> {
    let home = tempfile::tempdir()?;
    // The CLI's default discovery is `$HOME/Library/Application Support/Boss/engine.sock`.
    // A Grok process-home never has that file; creating the parent without the
    // socket matches the production sandbox layout.
    std::fs::create_dir_all(home.path().join("Library/Application Support/Boss"))?;
    Ok(home)
}

fn boss_under_home(
    home: &std::path::Path,
    socket: Option<&str>,
    run_id: Option<&str>,
    args: &[&str],
) -> std::process::Output {
    let mut cmd = Command::new(boss_binary());
    cmd.args(["--json", "--no-input", "--no-autostart", "--no-engine-autostart"])
        .args(args)
        .env("HOME", home)
        .env_remove("BOSS_ENGINE_PID_PATH")
        .env_remove("BOSS_ENGINE_CONTROL_TOKEN_PATH")
        // The `boss` binary lives under bazel-out in this test's runfiles.
        // Inherit `TEST_TMPDIR`/`BAZEL_TEST` and Discovery treats it as a
        // test process and panics instead of resolving from `$HOME` — that
        // is not how a Grok worker's `$BOSS_BIN` launcher behaves.
        .env_remove("TEST_TMPDIR")
        .env_remove("TEST_SRCDIR")
        .env_remove("BAZEL_TEST")
        .env_remove("TESTBRIDGE_TEST_ONLY");
    match socket {
        Some(path) => {
            cmd.env("BOSS_SOCKET_PATH", path);
        }
        None => {
            cmd.env_remove("BOSS_SOCKET_PATH");
        }
    }
    match run_id {
        Some(id) => {
            cmd.env("BOSS_RUN_ID", id);
        }
        None => {
            cmd.env_remove("BOSS_RUN_ID");
        }
    }
    cmd.output().expect("spawn boss")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boss_reaches_engine_from_grok_process_home_via_boss_socket_path() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let home = grok_process_home()?;

    let output = boss_under_home(home.path(), Some(engine.socket_str()), None, &["product", "list"]);
    if !output.status.success() {
        return Err(anyhow!(
            "boss product list under sandboxed HOME with BOSS_SOCKET_PATH failed (status={:?}):\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        value.get("products").map(|v| v.is_array()).unwrap_or(false),
        "product list must return a products array, got {value}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boss_misses_engine_from_grok_process_home_without_boss_socket_path() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let home = grok_process_home()?;
    // Engine is running, but the worker must not find it via the sandboxed HOME.
    let _ = engine.socket_str();

    let output = boss_under_home(home.path(), None, None, &["product", "list"]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "without BOSS_SOCKET_PATH the CLI must exit 5 (engine unavailable) from a Grok process-home; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("not reachable")
            || combined.contains("did not become ready")
            || combined.contains("failed to connect")
            || combined.contains("engine.sock"),
        "failure must name the missing engine socket, got: {combined}",
    );
    Ok(())
}

/// `boss propose done` from a Grok-scoped HOME: the CLI must reach the
/// engine via `BOSS_SOCKET_PATH`, attribute the call through the worker
/// registry's peer-pid walk (`BOSS_RUN_ID` is the cross-check), write a
/// `worker_proposals` row, and stamp the terminal declaration on the
/// execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boss_propose_done_from_grok_process_home_via_boss_socket_path() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Sandbox propose done").await?;
    let execution = engine.db()?.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build(),
    )?;
    // The compiled `boss` child is a descendant of this test process. The
    // engine walks the socket peer's ancestors to a registered worker pid,
    // the same path a pane-spawned session uses.
    engine.register_worker(std::process::id(), execution.id.clone());

    let home = grok_process_home()?;
    let output = boss_under_home(
        home.path(),
        Some(engine.socket_str()),
        Some(&execution.id),
        &[
            "propose",
            "done",
            "--outcome",
            "delivered",
            "--summary",
            "sandbox socket path reached the attributed propose-done path",
        ],
    );
    if !output.status.success() {
        return Err(anyhow!(
            "boss propose done under sandboxed HOME with BOSS_SOCKET_PATH failed (status={:?}):\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        value["already_submitted"], false,
        "first propose done must insert, not replay, got {value}"
    );
    assert_eq!(
        value["proposal"]["kind"], "run_done",
        "submitted proposal must be run_done, got {value}"
    );

    let db = engine.db()?;
    let proposals = db.list_worker_proposals_for_execution(&execution.id, ProposalKind::RunDone)?;
    assert_eq!(
        proposals.len(),
        1,
        "exactly one run_done worker_proposals row, got {proposals:?}"
    );
    assert_eq!(proposals[0].execution_id, execution.id);
    assert_eq!(proposals[0].work_item_id, Some(chore.id.clone()));
    assert_eq!(proposals[0].state, ProposalState::Applied);
    assert_eq!(
        db.execution_run_done_outcome(&execution.id)?,
        Some(RunDoneOutcome::Delivered),
        "execution row must carry the terminal run_done declaration"
    );
    Ok(())
}
