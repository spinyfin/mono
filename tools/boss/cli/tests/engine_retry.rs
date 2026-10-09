//! `boss` rides out a briefly unavailable engine: it retries with backoff,
//! reports progress on stderr only (so `--json` stdout stays machine
//! readable), and fails loudly with exit 5 once the budget is spent.
//!
//! The "engine" is a minimal fake on a temp Unix socket that answers every
//! request with an empty product list; the point is the CLI's transport
//! behaviour, not engine semantics.

use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::Result;
use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequestEnvelope};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use common::boss_binary;

fn run_boss(socket: &str, extra: &[&str], max_wait_secs: &str) -> Output {
    Command::new(boss_binary())
        .args(["--json", "--no-input", "--no-engine-autostart", "--socket-path", socket])
        .args(extra)
        .args(["product", "list"])
        .env("BOSS_ENGINE_MAX_WAIT_SECS", max_wait_secs)
        .env_remove("BOSS_RUN_ID")
        .output()
        .expect("spawn boss")
}

/// Bind `socket` after `delay` and serve empty product lists forever.
fn serve_after(socket: String, delay: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let listener = UnixListener::bind(&socket).unwrap();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                while stream.read_line(&mut line).await.is_ok_and(|n| n > 0) {
                    let request: FrontendRequestEnvelope = serde_json::from_str(&line).unwrap();
                    let reply = FrontendEventEnvelope::response(
                        request.request_id,
                        FrontendEvent::ProductsList { products: vec![] },
                    );
                    let out = format!("{}\n", serde_json::to_string(&reply).unwrap());
                    stream.get_mut().write_all(out.as_bytes()).await.unwrap();
                    line.clear();
                }
            });
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_succeeds_when_the_engine_appears_after_a_few_failed_connects() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket = dir.path().join("engine.sock").to_string_lossy().into_owned();
    let engine = serve_after(socket.clone(), Duration::from_millis(1500));

    let output = tokio::task::spawn_blocking({
        let socket = socket.clone();
        move || run_boss(&socket, &[], "60")
    })
    .await?;

    assert!(
        output.status.success(),
        "boss should have waited for the engine: status={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    // stdout is exactly one JSON document — no retry chatter mixed in.
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| anyhow::anyhow!("--json stdout is not clean JSON ({err}): {:?}", output.stdout))?;
    assert!(value["products"].is_array(), "{value}");
    // The progress notice went to stderr, naming the socket.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("engine not reachable"),
        "expected a retry notice, got: {stderr}"
    );
    assert!(stderr.contains(&socket), "notice names the socket: {stderr}");
    engine.abort();
    Ok(())
}

#[test]
fn exhausted_budget_exits_5_naming_socket_and_time_waited_with_clean_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("missing.sock").to_string_lossy().into_owned();

    let started = Instant::now();
    let output = run_boss(&socket, &["--engine-max-wait", "1"], "600");
    let elapsed = started.elapsed();

    assert_eq!(output.status.code(), Some(5), "engine unavailable exit code");
    assert!(
        elapsed >= Duration::from_millis(900),
        "waited out the 1s budget: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "bounded by the override, not the 600s env: {elapsed:?}"
    );
    assert!(output.stdout.is_empty(), "nothing on stdout: {:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&socket), "{stderr}");
    assert!(stderr.contains("waiting"), "message states the time waited: {stderr}");
}

#[test]
fn no_retry_fails_fast_without_a_progress_notice() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("missing.sock").to_string_lossy().into_owned();

    let started = Instant::now();
    let output = run_boss(&socket, &["--no-retry"], "600");

    assert_eq!(output.status.code(), Some(5));
    assert!(started.elapsed() < Duration::from_secs(5), "fast failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("retry disabled"), "{stderr}");
    assert!(!stderr.contains("retrying"), "no retry notice when disabled: {stderr}");
}
