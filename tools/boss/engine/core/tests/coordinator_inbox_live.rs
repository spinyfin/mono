//! Exercises the installed Claude inbox protocol against a loopback model
//! fixture. No production session, configuration, or model credential is used.
use std::path::{Path, PathBuf};

#[path = "coordinator_inbox_tui.rs"]
mod tui;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

fn declared_claude() -> Result<PathBuf> {
    let root = PathBuf::from(std::env::var("TEST_SRCDIR")?);
    std::fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("host_claude"))
        })
        .map(|path| path.join("claude"))
        .context("Bazel must declare the host Claude executable for this test")
}

async fn model_fixture() -> Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut header = String::new();
                let mut content_length = 0;
                let mut request_line = String::new();
                reader.read_line(&mut request_line).await.unwrap();
                loop {
                    header.clear();
                    reader.read_line(&mut header).await.unwrap();
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                    if let Some(value) = header.to_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).await.unwrap();
                let (content_type, response) = if request_line.contains("count_tokens") {
                    ("application/json", "{\"input_tokens\":1}".to_owned())
                } else {
                    let events = [
                        json!({"type":"message_start", "message":{"id":"msg_fixture", "type":"message", "role":"assistant", "model":"claude-sonnet-4-5", "content":[], "stop_reason":null, "stop_sequence":null, "usage":{"input_tokens":1, "output_tokens":0}}}),
                        json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
                        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"fixture ready"}}),
                        json!({"type":"content_block_stop", "index":0}),
                        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn", "stop_sequence":null}, "usage":{"output_tokens":2}}),
                        json!({"type":"message_stop"}),
                    ];
                    let response = events
                        .iter()
                        .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()))
                        .collect::<String>();
                    ("text/event-stream", response)
                };
                let mut stream = reader.into_inner();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            });
        }
    });
    Ok(address)
}

#[tokio::test]
async fn installed_claude_acknowledges_a_separate_inbox_message() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = root.path().join("config");
    std::fs::create_dir(&config)?;
    let output = root.path().join("claude-output.jsonl");
    let errors = root.path().join("claude-errors.log");
    let api = model_fixture().await?;
    let mut child = tokio::process::Command::new(declared_claude()?)
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "",
            "--setting-sources",
            "",
            "--settings",
            boss_engine::COORDINATOR_INBOUND_SETTINGS,
        ])
        .current_dir(root.path())
        .env("CLAUDE_CONFIG_DIR", &config)
        .env("CLAUDE_CODE_TMPDIR", root.path())
        .env("ANTHROPIC_BASE_URL", format!("http://{api}"))
        .env("ANTHROPIC_API_KEY", "sk-ant-test-fixture-not-a-real-key")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("BOSS_EVENTS_SOCKET")
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create(&output)?)
        .stderr(std::fs::File::create(&errors)?)
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().context("Claude has no pid")?;
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"fixture startup\"}}\n")
        .await?;
    let registration_path = config.join("sessions").join(format!("{pid}.json"));
    let started = Instant::now();
    let registration: Value = loop {
        if let Ok(bytes) = std::fs::read(&registration_path) {
            let value: Value = serde_json::from_slice(&bytes)?;
            if value["messagingSocketPath"].is_string()
                && value["status"] == "idle"
                && std::fs::read_to_string(&output)?.contains("\"type\":\"result\"")
            {
                break value;
            }
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "fixture Claude exited {status}: {} {}",
                std::fs::read_to_string(&errors)?,
                std::fs::read_to_string(&output)?
            );
        }
        if started.elapsed() > Duration::from_secs(30) {
            bail!(
                "fixture Claude did not become idle: {} {}",
                std::fs::read_to_string(&errors)?,
                std::fs::read_to_string(&output)?
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let socket = PathBuf::from(registration["messagingSocketPath"].as_str().unwrap());
    let transcript = fixture_transcript(&config, &registration)?;
    let delivered_at = Instant::now();
    tokio::time::timeout(
        boss_engine::COORDINATOR_DELIVERY_TIMEOUT,
        boss_engine::deliver_coordinator_handoff_to_inbox(
            &socket,
            pid as libc::pid_t,
            registration["sessionId"].as_str().unwrap(),
            "fixture handoff request",
            &transcript,
        ),
    )
    .await
    .with_context(|| format!("native Claude did not confirm delivery within the production delivery deadline; registration={registration}; errors={}; inputs={}",
        std::fs::read_to_string(&errors).unwrap_or_default(),
        std::fs::read_to_string(&transcript).unwrap_or_default().lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).filter(|record| record["type"] == "user").map(|record| record.to_string()).collect::<Vec<_>>().join("\n")))??;
    eprintln!(
        "native Claude inbox delivered in {}ms",
        delivered_at.elapsed().as_millis()
    );
    child.kill().await?;
    child.wait().await?;
    Ok(())
}

fn fixture_transcript(config: &Path, registration: &Value) -> Result<PathBuf> {
    let projects = config.join("projects");
    std::fs::read_dir(&projects)?
        .filter_map(Result::ok)
        .map(|entry| {
            entry
                .path()
                .join(format!("{}.jsonl", registration["sessionId"].as_str().unwrap()))
        })
        .find(|path| path.exists())
        .context("fixture transcript is absent")
}
