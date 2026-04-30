//! Reusable client for talking to the Boss engine over its frontend socket.
//!
//! `BossClient` opens a Unix-domain connection to the engine and provides a
//! correlated request/response API on top of the framed JSON protocol defined
//! in [`boss_protocol`]. Engine discovery (socket path resolution + optional
//! autostart of `boss-engine`) lives behind [`Discovery`] so the CLI, tests,
//! and future TUI/web frontends share one set of rules.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use boss_protocol::{
    FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::sleep;

pub const DEFAULT_SOCKET_PATH: &str = "/tmp/boss-engine.sock";
pub const DEFAULT_PID_PATH: &str = "/tmp/boss-engine.pid";
pub const DEFAULT_ENGINE_START_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct EngineCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// How a client should locate the engine and (optionally) launch it.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub socket_path: String,
    pub pid_file_path: String,
    pub autostart: bool,
    pub engine: EngineCommand,
    pub launch_directory: PathBuf,
    pub start_timeout: Duration,
}

impl Discovery {
    /// Build a discovery profile from process env + an optional `--socket-path` override.
    pub fn from_env(socket_override: Option<&str>) -> Result<Self> {
        let socket_path = socket_override
            .map(str::to_owned)
            .or_else(|| std::env::var("BOSS_SOCKET_PATH").ok())
            .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_owned());
        let pid_file_path =
            std::env::var("BOSS_ENGINE_PID_PATH").unwrap_or_else(|_| DEFAULT_PID_PATH.to_owned());
        let launch_directory = resolve_launch_directory()?;
        let engine = resolve_engine_command(&socket_path)?;

        Ok(Self {
            socket_path,
            pid_file_path,
            autostart: true,
            engine,
            launch_directory,
            start_timeout: DEFAULT_ENGINE_START_TIMEOUT,
        })
    }

    pub fn with_autostart(mut self, autostart: bool) -> Self {
        self.autostart = autostart;
        self
    }
}

/// Single-connection client over the engine's frontend socket.
pub struct BossClient {
    reader: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_request_id: AtomicU64,
}

impl BossClient {
    /// Connect to the engine, optionally autostarting it per the discovery profile.
    pub async fn connect(discovery: &Discovery) -> Result<Self> {
        if let Ok(client) = Self::connect_socket(&discovery.socket_path).await {
            return Ok(client);
        }

        if !discovery.autostart {
            bail!(
                "boss engine is not reachable at {}",
                discovery.socket_path
            );
        }

        ensure_engine_running(discovery).await?;
        Self::connect_socket(&discovery.socket_path).await
    }

    /// Connect directly to a socket path without autostart logic.
    pub async fn connect_socket(socket_path: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("failed to connect to engine socket {socket_path}"))?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read_half).lines(),
            writer: write_half,
            next_request_id: AtomicU64::new(1),
        })
    }

    /// Send a request and wait for the matching response by `request_id`.
    pub async fn send_request(&mut self, request: &FrontendRequest) -> Result<FrontendEvent> {
        let request_id = format!(
            "client-{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let payload = serde_json::to_string(&FrontendRequestEnvelope {
            request_id: request_id.clone(),
            payload: request.clone(),
        })?;
        self.writer.write_all(payload.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;

        while let Some(line) = self.reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let envelope: FrontendEventEnvelope = serde_json::from_str(&line)
                .with_context(|| format!("failed to decode engine event: {line}"))?;
            if envelope.request_id.as_deref() == Some(request_id.as_str()) {
                return Ok(envelope.payload);
            }
        }

        bail!("engine closed the socket before returning a response")
    }
}

pub async fn engine_socket_reachable(socket_path: &str) -> bool {
    UnixStream::connect(socket_path).await.is_ok()
}

pub async fn wait_for_socket(socket_path: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if engine_socket_reachable(socket_path).await {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

pub fn running_engine_pid(pid_file_path: &str) -> Option<u32> {
    let pid = read_pid_file(pid_file_path)?;
    let status = Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if status.success() {
        Some(pid)
    } else {
        let _ = std::fs::remove_file(pid_file_path);
        None
    }
}

pub fn read_pid_file(pid_file_path: &str) -> Option<u32> {
    let content = std::fs::read_to_string(pid_file_path).ok()?;
    content.trim().parse().ok()
}

pub async fn ensure_engine_running(discovery: &Discovery) -> Result<()> {
    if engine_socket_reachable(&discovery.socket_path).await {
        return Ok(());
    }

    if let Some(pid) = running_engine_pid(&discovery.pid_file_path) {
        if wait_for_socket(&discovery.socket_path, discovery.start_timeout).await {
            return Ok(());
        }
        bail!(
            "boss engine pid file points to pid {pid}, but socket {} never became ready",
            discovery.socket_path
        );
    }

    start_engine_process(discovery)?;
    if wait_for_socket(&discovery.socket_path, discovery.start_timeout).await {
        return Ok(());
    }

    bail!(
        "boss engine did not become ready at {} within {} seconds",
        discovery.socket_path,
        discovery.start_timeout.as_secs()
    )
}

pub fn stop_engine(pid_file_path: &str) -> Result<()> {
    let Some(pid) = running_engine_pid(pid_file_path) else {
        return Ok(());
    };

    let status = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to invoke /bin/kill")?;
    if !status.success() {
        bail!("failed to stop boss engine pid {pid}");
    }

    if let Some(owner) = read_pid_file(pid_file_path) {
        if owner == pid {
            let _ = std::fs::remove_file(pid_file_path);
        }
    }

    Ok(())
}

fn start_engine_process(discovery: &Discovery) -> Result<()> {
    Command::new(&discovery.engine.program)
        .args(&discovery.engine.args)
        .current_dir(&discovery.launch_directory)
        .env("BOSS_ENGINE_PID_PATH", &discovery.pid_file_path)
        .env("BOSS_SOCKET_PATH", &discovery.socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to start engine using `{}`",
                format_engine_command(&discovery.engine.program, &discovery.engine.args)
            )
        })
        .map(|_| ())
}

fn resolve_launch_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("BUILD_WORKSPACE_DIRECTORY") {
        let candidate = PathBuf::from(path);
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    std::env::current_dir().context("failed to resolve current directory")
}

fn resolve_engine_command(socket_path: &str) -> Result<EngineCommand> {
    if let Ok(value) = std::env::var("BOSS_ENGINE_CMD") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            let parts = shlex::split(trimmed)
                .with_context(|| format!("failed to parse BOSS_ENGINE_CMD: {trimmed}"))?;
            let Some((program, args)) = parts.split_first() else {
                bail!("BOSS_ENGINE_CMD resolved to an empty command");
            };
            return Ok(EngineCommand {
                program: program.clone(),
                args: args.to_vec(),
            });
        }
    }

    if let Some(program) = resolve_sibling_engine_binary() {
        return Ok(EngineCommand {
            program,
            args: default_engine_args(socket_path),
        });
    }

    Ok(EngineCommand {
        program: "boss-engine".to_owned(),
        args: default_engine_args(socket_path),
    })
}

fn default_engine_args(socket_path: &str) -> Vec<String> {
    vec![
        "--mode=server".to_owned(),
        "--socket-path".to_owned(),
        socket_path.to_owned(),
    ]
}

fn resolve_sibling_engine_binary() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let mut candidates = Vec::new();
    if let Some(dir) = exe.parent() {
        candidates.push(dir.join("boss-engine"));
        if let Some(boss_dir) = dir.parent() {
            candidates.push(boss_dir.join("engine").join("engine"));
        }
    }

    candidates
        .into_iter()
        .find(|candidate: &PathBuf| candidate.is_file())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

fn format_engine_command(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_owned())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}
