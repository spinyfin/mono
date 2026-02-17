use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
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
    CreateAgent {
        name: Option<String>,
    },
    ListAgents,
    RemoveAgent {
        agent_id: String,
    },
    Prompt {
        agent_id: String,
        text: String,
    },
    PermissionResponse {
        agent_id: String,
        id: String,
        granted: bool,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FrontendEvent {
    AgentCreated {
        agent_id: String,
        name: String,
    },
    AgentReady {
        agent_id: String,
    },
    AgentList {
        agents: Vec<AgentInfo>,
    },
    AgentRemoved {
        agent_id: String,
    },
    Chunk {
        agent_id: String,
        text: String,
    },
    Done {
        agent_id: String,
        stop_reason: String,
    },
    ToolCall {
        agent_id: String,
        name: String,
        status: String,
    },
    TerminalStarted {
        agent_id: String,
        id: String,
        title: String,
        command: String,
        cwd: Option<String>,
    },
    TerminalOutput {
        agent_id: String,
        id: String,
        text: String,
    },
    TerminalDone {
        agent_id: String,
        id: String,
        exit_code: Option<i64>,
        signal: Option<String>,
    },
    PermissionRequest {
        agent_id: String,
        id: String,
        title: String,
    },
    Error {
        agent_id: Option<String>,
        message: String,
    },
}

#[derive(Debug, Serialize)]
struct AgentInfo {
    agent_id: String,
    name: String,
}

struct Agent {
    id: String,
    name: String,
    acp_client: Arc<AcpClient>,
    session_id: String,
    prompt_lock: Arc<Mutex<()>>,
}

struct AgentRegistry {
    agents: Mutex<HashMap<String, Agent>>,
    next_id: AtomicU64,
    cfg: RuntimeConfig,
}

impl AgentRegistry {
    fn new(cfg: RuntimeConfig) -> Self {
        Self {
            agents: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            cfg,
        }
    }

    fn allocate_agent(&self, name: Option<String>) -> (String, String) {
        let id = format!(
            "agent-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let name = name.unwrap_or_else(|| format!("Agent {}", id.strip_prefix("agent-").unwrap_or(&id)));
        (id, name)
    }

    async fn initialize_agent(&self, id: &str, name: &str) -> Result<()> {
        let acp_client = Arc::new(AcpClient::connect_with_external_permissions(&self.cfg).await?);
        acp_client.initialize().await?;
        let session_id = acp_client.new_session(&self.cfg.cwd).await?;

        tracing::info!(agent_id = %id, name = %name, session_id = %session_id, "agent ready");

        let agent = Agent {
            id: id.to_owned(),
            name: name.to_owned(),
            acp_client,
            session_id,
            prompt_lock: Arc::new(Mutex::new(())),
        };

        self.agents.lock().await.insert(id.to_owned(), agent);
        Ok(())
    }

    async fn remove_agent(&self, agent_id: &str) -> Result<()> {
        let removed = self.agents.lock().await.remove(agent_id);
        if removed.is_none() {
            bail!("unknown agent: {agent_id}");
        }
        tracing::info!(agent_id = %agent_id, "agent removed");
        Ok(())
    }

    async fn list_agents(&self) -> Vec<AgentInfo> {
        self.agents
            .lock()
            .await
            .values()
            .map(|agent| AgentInfo {
                agent_id: agent.id.clone(),
                name: agent.name.clone(),
            })
            .collect()
    }

    async fn get_acp_and_session(&self, agent_id: &str) -> Result<(Arc<AcpClient>, String, Arc<Mutex<()>>)> {
        let agents = self.agents.lock().await;
        let agent = agents
            .get(agent_id)
            .with_context(|| format!("unknown agent: {agent_id}"))?;
        Ok((
            agent.acp_client.clone(),
            agent.session_id.clone(),
            agent.prompt_lock.clone(),
        ))
    }
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

    let registry = Arc::new(AgentRegistry::new(cfg.clone()));

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

    while let Some(line) = reader.next_line().await.context("socket read failed")? {
        if line.trim().is_empty() {
            continue;
        }

        let request: FrontendRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                let _ = event_tx.send(FrontendEvent::Error {
                    agent_id: None,
                    message: format!("invalid request payload: {err}"),
                });
                continue;
            }
        };

        match request {
            FrontendRequest::CreateAgent { name } => {
                let (agent_id, agent_name) = registry.allocate_agent(name);
                let _ = event_tx.send(FrontendEvent::AgentCreated {
                    agent_id: agent_id.clone(),
                    name: agent_name.clone(),
                });

                let event_tx = event_tx.clone();
                let registry = registry.clone();
                tokio::spawn(async move {
                    match registry.initialize_agent(&agent_id, &agent_name).await {
                        Ok(()) => {
                            let _ = event_tx.send(FrontendEvent::AgentReady {
                                agent_id,
                            });
                        }
                        Err(err) => {
                            tracing::error!(?err, agent_id = %agent_id, "failed to initialize agent");
                            let _ = event_tx.send(FrontendEvent::Error {
                                agent_id: Some(agent_id),
                                message: format!("failed to initialize agent: {err}"),
                            });
                        }
                    }
                });
            }
            FrontendRequest::ListAgents => {
                let agents = registry.list_agents().await;
                let _ = event_tx.send(FrontendEvent::AgentList { agents });
            }
            FrontendRequest::RemoveAgent { agent_id } => {
                match registry.remove_agent(&agent_id).await {
                    Ok(()) => {
                        let _ = event_tx.send(FrontendEvent::AgentRemoved { agent_id });
                    }
                    Err(err) => {
                        tracing::error!(?err, agent_id = %agent_id, "failed to remove agent");
                        let _ = event_tx.send(FrontendEvent::Error {
                            agent_id: Some(agent_id),
                            message: err.to_string(),
                        });
                    }
                }
            }
            FrontendRequest::Prompt { agent_id, text } => {
                tracing::info!(
                    agent_id = %agent_id,
                    prompt_chars = text.chars().count(),
                    "received prompt from frontend"
                );

                let (acp, session_id, prompt_lock) = match registry.get_acp_and_session(&agent_id).await {
                    Ok(tuple) => tuple,
                    Err(err) => {
                        let _ = event_tx.send(FrontendEvent::Error {
                            agent_id: Some(agent_id),
                            message: err.to_string(),
                        });
                        continue;
                    }
                };

                let event_tx = event_tx.clone();
                let agent_id_owned = agent_id.clone();

                tokio::spawn(async move {
                    let _guard = prompt_lock.lock().await;
                    let aid = agent_id_owned.clone();

                    let result = acp
                        .prompt_streaming(&session_id, &text, |event| match event {
                            AcpEvent::AgentMessageChunk { text, .. } => {
                                let _ = event_tx.send(FrontendEvent::Chunk {
                                    agent_id: aid.clone(),
                                    text,
                                });
                            }
                            AcpEvent::ToolCall { title, status, .. } => {
                                let _ = event_tx.send(FrontendEvent::ToolCall {
                                    agent_id: aid.clone(),
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
                                    agent_id: aid.clone(),
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
                                    agent_id: aid.clone(),
                                    id: permission_id,
                                    title,
                                });
                            }
                            AcpEvent::TerminalStarted {
                                id,
                                title,
                                command,
                                cwd,
                                ..
                            } => {
                                let _ = event_tx.send(FrontendEvent::TerminalStarted {
                                    agent_id: aid.clone(),
                                    id,
                                    title,
                                    command,
                                    cwd,
                                });
                            }
                            AcpEvent::TerminalOutput { id, text, .. } => {
                                let _ = event_tx.send(FrontendEvent::TerminalOutput {
                                    agent_id: aid.clone(),
                                    id,
                                    text,
                                });
                            }
                            AcpEvent::TerminalDone {
                                id,
                                exit_code,
                                signal,
                                ..
                            } => {
                                let _ = event_tx.send(FrontendEvent::TerminalDone {
                                    agent_id: aid.clone(),
                                    id,
                                    exit_code,
                                    signal,
                                });
                            }
                        })
                        .await;

                    match result {
                        Ok(response) => {
                            tracing::info!(
                                agent_id = %agent_id_owned,
                                stop_reason = %response.stop_reason,
                                "prompt completed"
                            );
                            let _ = event_tx.send(FrontendEvent::Done {
                                agent_id: agent_id_owned,
                                stop_reason: response.stop_reason,
                            });
                        }
                        Err(err) => {
                            tracing::error!(?err, agent_id = %agent_id_owned, "prompt failed");
                            let _ = event_tx.send(FrontendEvent::Error {
                                agent_id: Some(agent_id_owned),
                                message: err.to_string(),
                            });
                        }
                    }
                });
            }
            FrontendRequest::PermissionResponse { agent_id, id, granted } => {
                tracing::info!(
                    agent_id = %agent_id,
                    permission_id = %id,
                    granted,
                    "received permission response"
                );

                let acp = match registry.get_acp_and_session(&agent_id).await {
                    Ok((acp, _, _)) => acp,
                    Err(err) => {
                        let _ = event_tx.send(FrontendEvent::Error {
                            agent_id: Some(agent_id),
                            message: err.to_string(),
                        });
                        continue;
                    }
                };

                if let Err(err) = acp.respond_permission(&id, granted).await {
                    tracing::error!(?err, permission_id = %id, "failed to apply permission response");
                    let _ = event_tx.send(FrontendEvent::Error {
                        agent_id: Some(agent_id),
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
            AcpEvent::TerminalStarted {
                title,
                command,
                cwd,
                ..
            } => {
                if let Some(cwd) = cwd {
                    eprintln!("\n[terminal] {title} (cwd={cwd})");
                } else {
                    eprintln!("\n[terminal] {title}");
                }
                eprintln!("{command}");
            }
            AcpEvent::TerminalOutput { text, .. } => {
                eprint!("{text}");
            }
            AcpEvent::TerminalDone {
                exit_code, signal, ..
            } => {
                if let Some(code) = exit_code {
                    eprintln!("\n[terminal done] exit={code}");
                } else if let Some(signal) = signal {
                    eprintln!("\n[terminal done] signal={signal}");
                } else {
                    eprintln!("\n[terminal done]");
                }
            }
        })
        .await?;

    eprintln!("\n[done] {}", response.stop_reason);
    Ok(())
}
