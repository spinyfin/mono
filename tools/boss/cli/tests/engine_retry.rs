//! `boss` rides out a briefly unavailable engine: it retries with backoff,
//! reports progress on stderr only (so `--json` stdout stays machine
//! readable), and fails loudly with exit 5 once the budget is spent.
//!
//! The "engine" is a minimal fake on a temp Unix socket that answers every
//! request with an empty product list; the point is the CLI's transport
//! behaviour, not engine semantics.

use std::io::{BufRead, BufReader as StdBufReader, Read};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequestEnvelope};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use common::boss_binary;

fn boss_command(socket: &str, extra: &[&str], max_wait_secs: &str) -> Command {
    let mut command = Command::new(boss_binary());
    command
        .args(["--json", "--no-input", "--no-engine-autostart", "--socket-path", socket])
        .args(extra)
        .args(["product", "list"])
        .env("BOSS_ENGINE_MAX_WAIT_SECS", max_wait_secs)
        .env_remove("BOSS_RUN_ID");
    command
}

fn run_boss(socket: &str, extra: &[&str], max_wait_secs: &str) -> Output {
    boss_command(socket, extra, max_wait_secs).output().expect("spawn boss")
}

/// Run `boss`, calling `on_first_notice` as soon as it prints its first
/// "engine not reachable" progress line. That ties the engine's appearance to
/// the CLI's own progress (at least one connect has failed) instead of to a
/// wall-clock delay that a slow process start could overtake.
fn run_boss_until_first_notice(socket: &str, max_wait_secs: &str, on_first_notice: impl FnOnce()) -> Output {
    let mut child = boss_command(socket, &[], max_wait_secs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn boss");
    let mut stderr = StdBufReader::new(child.stderr.take().expect("piped stderr"));
    let mut seen = String::new();
    let mut on_first_notice = Some(on_first_notice);
    let mut line = String::new();
    while stderr.read_line(&mut line).expect("read boss stderr") > 0 {
        seen.push_str(&line);
        if line.contains("engine not reachable")
            && let Some(callback) = on_first_notice.take()
        {
            callback();
        }
        line.clear();
    }
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).ok();
    let mut output = child.wait_with_output().expect("wait for boss");
    output.stderr = seen.into_bytes();
    output
}

/// Bind `socket` and serve empty product lists forever.
fn serve(socket: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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
    let handle = tokio::runtime::Handle::current();

    let (output, engine) = tokio::task::spawn_blocking({
        let socket = socket.clone();
        move || {
            let mut engine = None;
            let output = run_boss_until_first_notice(&socket, "60", || {
                let _guard = handle.enter();
                engine = Some(serve(socket.clone()));
            });
            (output, engine.expect("boss never reported the engine as unreachable"))
        }
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

    // A budget far larger than the bound below: finishing well inside it
    // proves `--no-retry` overrides the budget, however slow process start is.
    let started = Instant::now();
    let output = run_boss(&socket, &["--no-retry"], "3600");

    assert_eq!(output.status.code(), Some(5));
    assert!(started.elapsed() < Duration::from_secs(60), "fast failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("retry disabled"), "{stderr}");
    assert!(!stderr.contains("retrying"), "no retry notice when disabled: {stderr}");
}

#[test]
fn invalid_wait_environment_warns_on_stderr_with_clean_json_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("missing.sock").to_string_lossy().into_owned();
    let output = run_boss(&socket, &["--no-retry"], "5s");

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty(), "nothing on stdout: {:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("BOSS_ENGINE_MAX_WAIT_SECS"), "{stderr}");
    assert!(stderr.contains("not a whole number of seconds"), "{stderr}");
    assert!(stderr.contains("600"), "warning names the default fallback: {stderr}");
}
