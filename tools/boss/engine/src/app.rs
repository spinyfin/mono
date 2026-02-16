use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use crate::acp::{AcpClient, AcpEvent};
use crate::cli::{Cli, Mode};
use crate::config::RuntimeConfig;

const DEFAULT_SOCKET_PATH: &str = "/tmp/boss-engine.sock";
const DEFAULT_PID_PATH: &str = "/tmp/boss-engine.pid";

struct PidFileGuard {
    path: String,
    pid: u32,
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(_) => return,
        };

        let parsed = content.trim().parse::<u32>().ok();
        if parsed == Some(self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FrontendRequest {
    Prompt { text: String },
    PermissionResponse { id: String, granted: bool },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FrontendEvent {
    Chunk { text: String },
    Done { stop_reason: String },
    ToolCall { name: String, status: String },
    PermissionRequest { id: String, title: String },
    Error { message: String },
}

pub async fn run(cli: Cli) -> Result<()> {
    let cfg = RuntimeConfig::load_from_env()?;
    cfg.preflight()?;
    tracing::info!(
        acp_command = %cfg.acp_command,
        acp_args = ?cfg.acp_args,
        cwd = %cfg.cwd.display(),
        "starting boss-engine runtime",
    );

    match cli.mode {
        Mode::Cli => run_cli(cli, &cfg).await,
        Mode::Server => run_server(cli, &cfg).await,
    }
}

async fn run_cli(cli: Cli, cfg: &RuntimeConfig) -> Result<()> {
    let acp = AcpClient::connect(cfg).await?;
    acp.initialize().await?;
    let session_id = acp.new_session(&cfg.cwd).await?;

    println!("Connected to ACP adapter. Session: {session_id}");

    if let Some(prompt) = cli.prompt {
        run_prompt(&acp, &session_id, &prompt).await?;
        return Ok(());
    }

    println!("Enter a prompt (Ctrl-D to exit):");
    print!("> ");
    std::io::stdout().flush()?;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let prompt = line.trim();
        if prompt.is_empty() {
            print!("> ");
            std::io::stdout().flush()?;
            continue;
        }

        run_prompt(&acp, &session_id, prompt).await?;
        println!();
        print!("> ");
        std::io::stdout().flush()?;
    }

    Ok(())
}

async fn run_server(cli: Cli, cfg: &RuntimeConfig) -> Result<()> {
    let socket_path = cli
        .socket_path
        .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_owned());

    if Path::new(&socket_path).exists() {
        tokio::fs::remove_file(&socket_path)
            .await
            .with_context(|| format!("failed to remove existing socket {socket_path}"))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind unix socket {socket_path}"))?;

    let pid_path =
        std::env::var("BOSS_ENGINE_PID_PATH").unwrap_or_else(|_| DEFAULT_PID_PATH.to_owned());
    let pid = std::process::id();
    std::fs::write(&pid_path, format!("{pid}\n"))
        .with_context(|| format!("failed to write pid file {pid_path}"))?;
    let _pid_guard = PidFileGuard {
        path: pid_path.clone(),
        pid,
    };

    tracing::info!(socket_path = %socket_path, "frontend socket is ready");
    tracing::info!(pid, pid_file = %pid_path, "engine pid file is ready");
    println!("boss-engine listening on {socket_path}");

    loop {
        let (stream, _) = listener.accept().await.context("socket accept failed")?;
        if let Err(err) = handle_frontend_connection(stream, cfg).await {
            tracing::error!(?err, "frontend connection failed");
        }
    }
}

async fn handle_frontend_connection(stream: UnixStream, cfg: &RuntimeConfig) -> Result<()> {
    tracing::info!("frontend connected");
    let acp = Arc::new(AcpClient::connect_with_external_permissions(cfg).await?);
    acp.initialize().await?;
    let session_id = acp.new_session(&cfg.cwd).await?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).lines();

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<FrontendEvent>();
    let writer_task = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let line = match serde_json::to_string(&event) {
                Ok(line) => line,
                Err(err) => {
                    tracing::error!(?err, "failed to serialize frontend event");
                    continue;
                }
            };

            if let Err(err) = write_half.write_all(line.as_bytes()).await {
                tracing::error!(?err, "failed to write event to frontend socket");
                break;
            }
            if let Err(err) = write_half.write_all(b"\n").await {
                tracing::error!(?err, "failed to delimit frontend event line");
                break;
            }
            if let Err(err) = write_half.flush().await {
                tracing::error!(?err, "failed to flush frontend socket");
                break;
            }
        }
    });

    let _ = event_tx.send(FrontendEvent::ToolCall {
        name: format!("session_started:{session_id}"),
        status: "ready".to_owned(),
    });

    let prompt_lock = Arc::new(Mutex::new(()));

    while let Some(line) = reader.next_line().await.context("socket read failed")? {
        if line.trim().is_empty() {
            continue;
        }

        let request: FrontendRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                let _ = event_tx.send(FrontendEvent::Error {
                    message: format!("invalid request payload: {err}"),
                });
                continue;
            }
        };

        match request {
            FrontendRequest::Prompt { text } => {
                tracing::info!(prompt_chars = text.chars().count(), "received prompt from frontend");
                let acp = acp.clone();
                let session_id = session_id.clone();
                let event_tx = event_tx.clone();
                let prompt_lock = prompt_lock.clone();

                tokio::spawn(async move {
                    let _guard = prompt_lock.lock().await;

                    let result = acp
                        .prompt_streaming(&session_id, &text, |event| match event {
                            AcpEvent::AgentMessageChunk { text, .. } => {
                                let _ = event_tx.send(FrontendEvent::Chunk { text });
                            }
                            AcpEvent::ToolCall { title, status, .. } => {
                                let _ = event_tx.send(FrontendEvent::ToolCall {
                                    name: title,
                                    status: status.unwrap_or_else(|| "started".to_owned()),
                                });
                            }
                            AcpEvent::ToolCallUpdate {
                                tool_call_id,
                                title,
                                status,
                                ..
                            } => {
                                let label = title.unwrap_or_else(|| {
                                    tool_call_id.unwrap_or_else(|| "tool".to_owned())
                                });
                                let _ = event_tx.send(FrontendEvent::ToolCall {
                                    name: label,
                                    status: status.unwrap_or_else(|| "update".to_owned()),
                                });
                            }
                            AcpEvent::PermissionRequest {
                                permission_id,
                                title,
                                ..
                            } => {
                                let _ = event_tx.send(FrontendEvent::PermissionRequest {
                                    id: permission_id,
                                    title,
                                });
                            }
                        })
                        .await;

                    match result {
                        Ok(response) => {
                            tracing::info!(stop_reason = %response.stop_reason, "prompt completed");
                            let _ = event_tx.send(FrontendEvent::Done {
                                stop_reason: response.stop_reason,
                            });
                        }
                        Err(err) => {
                            tracing::error!(?err, "prompt failed");
                            let _ = event_tx.send(FrontendEvent::Error {
                                message: err.to_string(),
                            });
                        }
                    }
                });
            }
            FrontendRequest::PermissionResponse { id, granted } => {
                tracing::info!(permission_id = %id, granted, "received permission response");
                if let Err(err) = acp.respond_permission(&id, granted).await {
                    tracing::error!(?err, permission_id = %id, "failed to apply permission response");
                    let _ = event_tx.send(FrontendEvent::Error {
                        message: err.to_string(),
                    });
                }
            }
        }
    }

    drop(event_tx);
    let _ = writer_task.await;
    Ok(())
}

async fn run_prompt(acp: &AcpClient, session_id: &str, prompt: &str) -> Result<()> {
    let response = acp
        .prompt_streaming(session_id, prompt, |event| match event {
            AcpEvent::AgentMessageChunk { text, .. } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            AcpEvent::ToolCall { title, status, .. } => {
                eprintln!(
                    "\n[tool] {title} ({})",
                    status.unwrap_or_else(|| "started".to_owned())
                );
            }
            AcpEvent::ToolCallUpdate {
                tool_call_id,
                title,
                status,
                ..
            } => {
                let label =
                    title.unwrap_or_else(|| tool_call_id.unwrap_or_else(|| "tool".to_owned()));
                eprintln!(
                    "\n[tool-update] {label} ({})",
                    status.unwrap_or_else(|| "update".to_owned())
                );
            }
            AcpEvent::PermissionRequest { title, .. } => {
                eprintln!("\n[permission] auto-approving: {title}");
            }
        })
        .await?;

    eprintln!("\n[done] {}", response.stop_reason);
    Ok(())
}
