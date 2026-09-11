//! Grok scopes worker `$HOME` to a per-run process-home. The `boss` CLI
//! used to resolve `engine.sock` from that fake home, miss the real engine,
//! and wait 5s for a socket that will never appear. `--socket-path` at the
//! production data dir is blocked by the path-guard hook.
//!
//! The fix is spawn-flow exporting the bound frontend socket as
//! `BOSS_SOCKET_PATH`. These tests drive the compiled `boss` binary the way
//! a Grok worker does: no `--socket-path`, `HOME` pointed at a process-home
//! that has never contained `engine.sock`.

use std::process::Command;

use anyhow::{Result, anyhow};
use serde_json::Value;
use tempfile::TempDir;

use common::boss_binary;
use harness::TestEngine;

fn grok_process_home() -> Result<TempDir> {
    let home = tempfile::tempdir()?;
    // The CLI's default discovery is `$HOME/Library/Application Support/Boss/engine.sock`.
    // A Grok process-home never has that file; creating the parent without the
    // socket matches the production sandbox layout.
    std::fs::create_dir_all(home.path().join("Library/Application Support/Boss"))?;
    Ok(home)
}

fn boss_under_home(home: &std::path::Path, socket: Option<&str>, args: &[&str]) -> std::process::Output {
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
    cmd.output().expect("spawn boss")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boss_reaches_engine_from_grok_process_home_via_boss_socket_path() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let home = grok_process_home()?;

    let output = boss_under_home(home.path(), Some(engine.socket_str()), &["product", "list"]);
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

    let output = boss_under_home(home.path(), None, &["product", "list"]);
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
