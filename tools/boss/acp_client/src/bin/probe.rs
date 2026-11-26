//! `boss-acp-probe`: a manual harness that holds a conversation with an ACP
//! agent run as a child process and records the wire.
//!
//! It is driven by commands, one JSON object per line, from `--script` or from
//! stdin, and reports what happens as events, one JSON object per line, on
//! stdout. Every line that crosses the transport goes to `--journal` in the
//! wire journal's shape (see `boss_acp_client::journal`). The same binary
//! serves a scripted study and an interactive front end that launches it by
//! path: both speak the command and event lines below and nothing else.
//!
//! The probe never executes a tool for the agent. It advertises no `fs` and no
//! `terminal` client capability, so the agent runs every tool itself.
//!
//! It also never routes the agent to a production Boss engine: the agent's
//! environment is rebuilt from scratch (see [`agent_environment`]), and every
//! path and variable a run was given is checked once, before the run creates a
//! file or starts a process (see [`prepare_run`]).

#[path = "probe/events.rs"]
mod events;
#[path = "probe/sdk_attach.rs"]
mod sdk_attach;
#[path = "probe/transcript.rs"]
mod transcript;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use boss_acp_client::common_schema::ProtocolVersion;
use boss_acp_client::connection::{Connection, ConnectionOptions, Incoming, PendingResponse};
use boss_acp_client::journal::Journal;
use boss_acp_client::schema::{
    CancelNotification, ClientCapabilities, ContentBlock, ImageContent, Implementation, InitializeRequest,
    NewSessionRequest, PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse, RequestId,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionConfigId, SessionId, SessionNotification, SetSessionConfigOptionRequest, SetSessionModeRequest, TextContent,
};
use boss_acp_client::{JsonRpcMessage, UntypedMessage};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;
use workspace_procs::{LiveProcess, Signal, SystemWorkspaceProcessTable, WorkspaceProcessTable};

use crate::events::emit;

#[derive(Parser)]
#[command(
    name = "boss-acp-probe",
    about = "Converse with an ACP agent child process and record the wire."
)]
struct Cli {
    #[command(subcommand)]
    command: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Hold a conversation with an ACP agent run as a child process.
    Converse(Box<ConverseArgs>),
    /// Run the official SDK's connection type on a stream that an earlier
    /// client already initialised, against a real agent, and report what it
    /// does with the response to a request that earlier client sent.
    SdkAttach(Box<sdk_attach::SdkAttachArgs>),
    /// Check that Claude Code wrote a transcript for a session where Boss
    /// expects it, in a dialect Boss's transcript parser reads.
    TranscriptCheck(transcript::TranscriptArgs),
}

#[derive(Args)]
struct ConverseArgs {
    #[command(flatten)]
    session: SessionArgs,
    /// File of commands, one JSON object per line. Without it commands are read from stdin.
    #[arg(long)]
    script: Option<PathBuf>,
    /// What happens to a permission request until a `permission` command answers it.
    #[arg(long, value_enum, default_value_t = PermissionPolicy::Ask)]
    permission: PermissionPolicy,
}

/// The agent to run and the session to open with it.
#[derive(Args)]
struct SessionArgs {
    /// Session directory: sent as `cwd` and used as the agent's working directory.
    #[arg(long)]
    cwd: PathBuf,
    /// File the wire journal is written to.
    #[arg(long)]
    journal: PathBuf,
    /// File the agent's stderr goes to. Without it the probe's stderr is used.
    #[arg(long)]
    agent_stderr: Option<PathBuf>,
    /// Claude Code binary the agent must run (`CLAUDE_CODE_EXECUTABLE`).
    /// Without it the agent picks its own.
    #[arg(long)]
    claude_executable: Option<PathBuf>,
    /// Socket of the isolated engine that `boss` commands run by the agent must reach.
    #[arg(long)]
    boss_socket: Option<PathBuf>,
    /// Directory put first on the agent's `PATH`. Repeatable.
    #[arg(long = "path-prefix")]
    path_prefixes: Vec<PathBuf>,
    /// Extra environment for the agent, as `KEY=VALUE`. Repeatable.
    #[arg(long = "env")]
    extra_env: Vec<String>,
    /// Model for the session (`_meta.claudeCode.options.model` on `session/new`).
    #[arg(long)]
    model: Option<String>,
    /// Setting source for the session (`_meta.claudeCode.options.settingSources`). Repeatable.
    #[arg(long = "setting-source")]
    setting_sources: Vec<String>,
    /// Inline settings, as a JSON object (`_meta.claudeCode.options.settings`).
    #[arg(long)]
    settings_json: Option<String>,
    /// Session option to set once the session exists, as `ID=VALUE`
    /// (`session/set_config_option`). Repeatable.
    #[arg(long = "config")]
    config: Vec<String>,
    /// Session mode to select once the session exists.
    #[arg(long)]
    mode: Option<String>,
    /// Extension capability to advertise under `clientCapabilities._meta.jetbrains.air`. Repeatable.
    #[arg(long = "air-capability")]
    air_capabilities: Vec<String>,
    /// The agent command and its arguments, after `--`.
    #[arg(last = true, required = true)]
    agent: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PermissionPolicy {
    /// Leave the request pending for a `permission` command.
    Ask,
    /// Select the agent's allow-once option.
    Allow,
    /// Select the agent's reject-once option.
    Reject,
    /// Answer `cancelled`.
    Cancel,
}

/// One command line. `op` selects the variant.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    /// Sends `session/prompt` and returns at once; the turn's end is an event.
    Prompt {
        #[serde(default)]
        text: String,
        /// Image files sent as `image` blocks ahead of the text.
        #[serde(default)]
        images: Vec<PathBuf>,
    },
    /// Answers every pending permission request `cancelled`, then sends `session/cancel`.
    Cancel,
    /// Answers a pending permission request: the named one, or the oldest.
    /// Picks the option by id, else by kind; with neither, answers `cancelled`.
    Permission {
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        option_id: Option<String>,
        #[serde(default)]
        kind: Option<PermissionOptionKind>,
    },
    /// Changes what happens to later permission requests.
    PermissionPolicy {
        policy: PermissionPolicy,
    },
    SetMode {
        mode: String,
    },
    /// Sends any request, typed or not, and reports its response as an event.
    /// In `params`, the string `$session` stands for the session id and
    /// `$cwd` for the session directory.
    Request {
        method: String,
        #[serde(default)]
        params: Value,
    },
    /// Reports the agent's process tree.
    Processes,
    /// Kills the agent's descendant processes whose command line contains `name`.
    KillDescendant {
        name: String,
    },
    /// Reports whether each of the agent's descendants registered a Claude
    /// Code cross-session inbox.
    InboxRegistrations,
    /// Does nothing for `ms`. Events keep flowing.
    Sleep {
        ms: u64,
    },
    /// Waits until no prompt is in flight.
    AwaitIdle {
        timeout_ms: u64,
    },
    /// Waits for the next event with this name.
    AwaitEvent {
        event: String,
        timeout_ms: u64,
    },
    /// Waits until nothing has arrived from the agent for `quiet_ms`.
    AwaitQuiet {
        quiet_ms: u64,
        timeout_ms: u64,
    },
    /// Closes the agent's input and ends the probe.
    Shutdown,
}

/// What holds the next command back.
enum Gate {
    Open,
    Sleep { until: Instant },
    Idle { deadline: Instant },
    Event { name: String, deadline: Instant },
    Quiet { quiet: Duration, deadline: Instant },
}

struct PendingPermission {
    id: RequestId,
    options: Vec<PermissionOption>,
}

/// A request sent to the agent whose response has not been reported yet.
///
/// A request completes when the connection reports its response on the same
/// ordered stream as the agent's notifications ([`Incoming::ResponseArrived`]),
/// never from a task that merely awaits it: a turn's end is then reported
/// after every update of that turn and before every update of the next.
enum InFlight {
    Prompt {
        turn: u64,
        pending: PendingResponse<PromptResponse>,
    },
    Request {
        method: String,
        pending: PendingResponse<Value>,
    },
}

impl InFlight {
    fn id(&self) -> &RequestId {
        match self {
            Self::Prompt { pending, .. } => pending.id(),
            Self::Request { pending, .. } => pending.id(),
        }
    }
}

/// Whether the probe goes on after an incoming message.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    /// The agent's stream ended; nothing more will arrive.
    Closed,
}

/// How long the agent has to exit after its input was closed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct Probe {
    connection: Connection,
    session_id: SessionId,
    /// The session directory, as sent in `session/new`.
    cwd: String,
    agent_pid: u32,
    policy: PermissionPolicy,
    #[builder(default)]
    pending_permissions: Vec<PendingPermission>,
    #[builder(default)]
    in_flight: Vec<InFlight>,
    #[builder(default)]
    prompts_in_flight: usize,
    #[builder(default)]
    turns: u64,
    #[builder(default = Gate::Open)]
    gate: Gate,
    #[builder(default = Instant::now())]
    last_arrival: Instant,
    /// Where events go: stdout, except in tests.
    #[builder(default = Box::new(emit))]
    sink: Box<dyn Fn(Value) + Send>,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Mode::Converse(args) => converse(*args).await,
        Mode::SdkAttach(args) => sdk_attach::run(*args).await,
        Mode::TranscriptCheck(args) => transcript::check(&args),
    }
}

async fn converse(args: ConverseArgs) -> Result<()> {
    // Everything that can be refused is refused before a file is created or a
    // process is started.
    let environment = prepare_run(&args.session)?;
    let journal = Arc::new(
        Journal::create(&args.session.journal).with_context(|| format!("creating {:?}", args.session.journal))?,
    );
    let SpawnedAgent {
        mut child,
        pid: agent_pid,
        stdin,
        stdout,
        environment,
    } = spawn_agent(&args.session, environment)?;
    let (connection, mut incoming) = Connection::open(stdout, stdin, Some(journal), ConnectionOptions::default());

    let initialized = connection
        .call(&initialize_request(&args.session))
        .await
        .context("initialize")?;
    let session = connection
        .call(&new_session_request(&args.session)?)
        .await
        .context("session/new")?;
    let session_id = session.session_id.clone();

    let mut config = events::config_summary(session.config_options.iter().flatten());
    for (option, value) in config_assignments(&args.session)? {
        let request = SetSessionConfigOptionRequest::new(session_id.clone(), SessionConfigId::new(option), value);
        let changed = connection
            .call(&request)
            .await
            .with_context(|| format!("session/set_config_option {option}={value}"))?;
        config = events::config_summary(&changed.config_options);
    }

    let mut selected_mode = None;
    if let Some(mode) = &args.session.mode {
        connection
            .call(&SetSessionModeRequest::new(session_id.clone(), mode.clone()))
            .await
            .with_context(|| format!("session/set_mode {mode}"))?;
        selected_mode = Some(mode.clone());
    }

    emit(json!({
        "event": "ready",
        "session_id": session_id,
        "agent_pid": agent_pid,
        "protocol_version": initialized.protocol_version,
        "agent": initialized.agent_info,
        "agent_capabilities": initialized.agent_capabilities,
        "initialize_meta": initialized.meta,
        "auth_methods": initialized.auth_methods.len(),
        "mode_at_creation": session.modes.as_ref().map(|modes| modes.current_mode_id.clone()),
        "mode_selected": selected_mode,
        "config": config,
        "environment": environment.report,
    }));

    let mut probe = Probe::builder()
        .connection(connection)
        .session_id(session_id)
        .cwd(args.session.cwd.to_string_lossy())
        .agent_pid(agent_pid)
        .policy(args.permission)
        .build();

    let mut commands = spawn_command_reader(args.script.as_deref()).await?;
    drive(&mut probe, &mut incoming, &mut commands, SHUTDOWN_GRACE).await?;

    let _ = child.start_kill();
    let status = child.wait().await.context("waiting for the agent")?;
    emit(json!({"event": "exit", "code": status.code(), "status": status.to_string()}));
    Ok(())
}

/// Runs commands and reports what the agent sends until the agent's stream
/// ends, or until `shutdown_grace` after a `shutdown` command has closed the
/// agent's input and the agent still has not closed its output.
async fn drive(
    probe: &mut Probe,
    incoming: &mut mpsc::UnboundedReceiver<Incoming>,
    commands: &mut mpsc::UnboundedReceiver<Result<Command, String>>,
    shutdown_grace: Duration,
) -> Result<()> {
    let mut commands_open = true;
    // Kept apart from the command gate: whatever the gate is doing, and
    // whatever the agent keeps sending, the agent is not waited for past this.
    let mut shutdown_deadline: Option<Instant> = None;
    loop {
        probe.settle_gate();
        let gate_open = matches!(probe.gate, Gate::Open);
        let wake = probe.gate_wake();
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(shutdown_deadline.unwrap_or(wake)), if shutdown_deadline.is_some() => {
                // The agent did not exit on end of input.
                probe.emit_event(json!({"event": "shutdown_timed_out", "grace_ms": shutdown_grace.as_millis() as u64}));
                break;
            }
            arrived = incoming.recv() => match arrived {
                Some(arrived) => {
                    if probe.handle_incoming(arrived).await? == Flow::Closed {
                        break;
                    }
                }
                None => break,
            },
            command = commands.recv(), if gate_open && commands_open => match command {
                Some(Ok(Command::Shutdown)) | None => {
                    commands_open = false;
                    // End of input for the agent. The loop keeps running until
                    // the agent closes its side, so nothing it still sends is lost.
                    let _ = probe.connection.shutdown_writer().await;
                    shutdown_deadline = Some(Instant::now() + shutdown_grace);
                }
                Some(Ok(command)) => probe.run(command).await?,
                Some(Err(error)) => probe.emit_event(json!({"event": "command_error", "error": error})),
            },
            () = tokio::time::sleep_until(wake), if !gate_open => {}
        }
    }
    Ok(())
}

/// The agent child process with its stdio taken.
struct SpawnedAgent {
    child: tokio::process::Child,
    pid: u32,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    environment: AgentEnvironment,
}

/// Starts the agent in the session directory with the environment that
/// [`prepare_run`] built.
fn spawn_agent(args: &SessionArgs, environment: AgentEnvironment) -> Result<SpawnedAgent> {
    let mut command = tokio::process::Command::new(&args.agent[0]);
    command
        .args(&args.agent[1..])
        .current_dir(&args.cwd)
        .env_clear()
        .envs(environment.variables.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    match &args.agent_stderr {
        Some(path) => {
            let file = std::fs::File::create(path).with_context(|| format!("creating {path:?}"))?;
            command.stderr(Stdio::from(file));
        }
        None => {
            command.stderr(Stdio::inherit());
        }
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting the agent `{}`", args.agent[0]))?;
    let pid = child.id().context("the agent exited before it could be driven")?;
    let stdin = child.stdin.take().context("agent stdin")?;
    let stdout = child.stdout.take().context("agent stdout")?;
    Ok(SpawnedAgent {
        child,
        pid,
        stdin,
        stdout,
        environment,
    })
}

/// `initialize` for protocol version 1. No `fs` and no `terminal` capability
/// is ever advertised: the agent runs every tool itself.
fn initialize_request(args: &SessionArgs) -> InitializeRequest {
    let mut capabilities = ClientCapabilities::new();
    if !args.air_capabilities.is_empty() {
        let mut meta = Map::new();
        meta.insert(
            "jetbrains".into(),
            json!({"air": {"version": 1, "capabilities": args.air_capabilities}}),
        );
        capabilities.meta = Some(meta);
    }
    InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(capabilities)
        .client_info(Implementation::new("boss-acp-probe", "0.1.0"))
}

fn new_session_request(args: &SessionArgs) -> Result<NewSessionRequest> {
    let mut new_session = NewSessionRequest::new(&args.cwd);
    if let Some(options) = claude_code_options(args)? {
        let mut meta = Map::new();
        meta.insert("claudeCode".into(), json!({"options": options}));
        new_session = new_session.meta(meta);
    }
    Ok(new_session)
}

/// The `--config ID=VALUE` assignments, in the order given.
fn config_assignments(args: &SessionArgs) -> Result<Vec<(&str, &str)>> {
    args.config
        .iter()
        .map(|assignment| {
            assignment
                .split_once('=')
                .filter(|(option, value)| !option.is_empty() && !value.is_empty())
                .ok_or_else(|| anyhow!("--config {assignment:?} is not ID=VALUE"))
        })
        .collect()
}

/// Checks everything a run was given and builds the agent's environment,
/// before the run creates a file or starts a process.
///
/// Both modes call this first. A refusal that came after the journal was
/// created would already have truncated a file it was meant to protect, so
/// nothing that writes may run before this returns. Options that are only
/// read later (`--config`, `--settings-json`) are parsed here as well, so a
/// typo fails before any output exists rather than half-way through a run.
fn prepare_run(args: &SessionArgs) -> Result<AgentEnvironment> {
    refuse_production_paths(args)?;
    config_assignments(args)?;
    claude_code_options(args)?;
    agent_environment(args, std::env::vars())
}

/// The Boss session directory and engine socket of the installed app must
/// never be handed to a probe run, as a place to read, write or route to.
fn refuse_production_paths(args: &SessionArgs) -> Result<()> {
    let mut checks = vec![
        ("--cwd", Some(args.cwd.as_path())),
        ("--boss-socket", args.boss_socket.as_deref()),
        // The journal and the agent's stderr are opened for writing and
        // truncated.
        ("--journal", Some(args.journal.as_path())),
        ("--agent-stderr", args.agent_stderr.as_deref()),
    ];
    checks.extend(
        args.path_prefixes
            .iter()
            .map(|prefix| ("--path-prefix", Some(prefix.as_path()))),
    );
    for (flag, path) in checks {
        let Some(path) = path else { continue };
        let text = path.to_string_lossy();
        if is_production_path(&text) {
            bail!("{flag} {text} is a production Boss path; the probe only runs against an isolated instance");
        }
    }
    if !args.cwd.is_dir() {
        bail!("--cwd {:?} is not a directory", args.cwd);
    }
    Ok(())
}

fn is_production_path(text: &str) -> bool {
    text.contains(boss_log_files::STATE_ROOT_SUFFIX) || text == "/tmp/boss-engine.sock"
}

struct AgentEnvironment {
    variables: Vec<(String, String)>,
    /// What was done to the inherited environment, with no values.
    report: Value,
}

/// Builds the agent's environment from the probe's own.
///
/// The probe is usually started from a shell that already belongs to a Boss
/// worker or to a Claude Code session, and both export variables that would
/// misroute a nested session: the production engine socket, the events
/// socket, the parent session's identity. So nothing Boss- or Claude-Code-
/// specific is inherited, and what the session needs is set explicitly.
///
/// The parent's tmux pane is not inherited either. Claude Code records the
/// pane it believes it runs in when it registers a session, so a nested
/// session would otherwise register itself under the pane of the worker that
/// started the probe.
///
/// `ANTHROPIC_API_KEY` is removed the way the coordinator's own launch
/// removes it, so the session authenticates with the Claude login. Other
/// provider overrides are left alone and reported by name.
///
/// `--env` cannot undo any of that: it may not set a variable that names the
/// parent session, routes to an engine, or carries the API key (only
/// [`ALLOWED_BOSS_ENV`] is let through), nor carry a production path. The
/// report is read back from the variables the agent will actually get.
fn agent_environment(
    args: &SessionArgs,
    inherited: impl Iterator<Item = (String, String)>,
) -> Result<AgentEnvironment> {
    let mut variables = Vec::new();
    let mut removed = BTreeSet::new();
    let mut provider_overrides = BTreeSet::new();
    let mut api_key_was_set = false;
    let mut path = String::new();
    for (name, value) in inherited {
        if name == "ANTHROPIC_API_KEY" {
            api_key_was_set = true;
            continue;
        }
        if names_the_parent_session(&name) {
            removed.insert(name);
            continue;
        }
        if name == "PATH" {
            path = value;
            continue;
        }
        if name.starts_with("ANTHROPIC_") || name.starts_with("AWS_BEARER_TOKEN_BEDROCK") {
            provider_overrides.insert(name.clone());
        }
        variables.push((name, value));
    }
    let prefixes: Vec<String> = args
        .path_prefixes
        .iter()
        .map(|prefix| prefix.to_string_lossy().into_owned())
        .collect();
    let path = prefixes
        .iter()
        .map(String::as_str)
        .chain((!path.is_empty()).then_some(path.as_str()))
        .collect::<Vec<_>>()
        .join(":");
    variables.push(("PATH".into(), path));

    // Without an isolated engine, `boss` must fail rather than fall back to
    // the installed app's socket.
    let boss_socket = match &args.boss_socket {
        Some(socket) => socket.to_string_lossy().into_owned(),
        None => format!("{}.no-engine.sock", args.journal.to_string_lossy()),
    };
    variables.push(("BOSS_SOCKET_PATH".into(), boss_socket));
    if let Some(executable) = &args.claude_executable {
        variables.push((
            "CLAUDE_CODE_EXECUTABLE".into(),
            executable.to_string_lossy().into_owned(),
        ));
    }
    let mut extra = Vec::new();
    for assignment in &args.extra_env {
        let (name, value) = assignment
            .split_once('=')
            .ok_or_else(|| anyhow!("--env {assignment:?} is not KEY=VALUE"))?;
        if let Some(why) = reserved_env_reason(name) {
            bail!("--env {name}=...: {why}");
        }
        if is_production_path(value) {
            bail!("--env {name}={value} is a production Boss path; the probe only runs against an isolated instance");
        }
        extra.push(name.to_owned());
        variables.retain(|(existing, _)| existing != name);
        variables.push((name.to_owned(), value.to_owned()));
    }
    let final_value = |name: &str| {
        variables
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, value)| value.as_str())
    };
    let report = json!({
        "anthropic_api_key_was_set": api_key_was_set,
        "anthropic_api_key_passed_on": final_value("ANTHROPIC_API_KEY").is_some(),
        "provider_override_variables": provider_overrides,
        "removed_variables": removed,
        "claude_executable": final_value("CLAUDE_CODE_EXECUTABLE"),
        "boss_socket_is_isolated_engine": args
            .boss_socket
            .as_ref()
            .is_some_and(|socket| final_value("BOSS_SOCKET_PATH") == Some(&*socket.to_string_lossy())),
        "path_prefixes": prefixes,
        "extra_variables": extra,
    });
    Ok(AgentEnvironment { variables, report })
}

/// `BOSS_` variables that `--env` may set: they configure the agent's tools
/// without identifying a session or routing to an engine.
const ALLOWED_BOSS_ENV: &[&str] = &["BOSS_BIN"];

/// Variables that identify the Boss worker or Claude Code session that started
/// the probe, or route to its engine. None is inherited.
fn names_the_parent_session(name: &str) -> bool {
    name.starts_with("BOSS_")
        || name.starts_with("CLAUDE_CODE_")
        || matches!(name, "CLAUDECODE" | "CLAUDE_PID" | "TMUX" | "TMUX_PANE")
}

/// Why `--env` may not set `name`, if it may not.
fn reserved_env_reason(name: &str) -> Option<&'static str> {
    match name {
        "ANTHROPIC_API_KEY" => {
            Some("the probe never passes an API key on; the session authenticates with the Claude login")
        }
        "BOSS_SOCKET_PATH" => Some("the engine socket is set by --boss-socket"),
        "CLAUDE_CODE_EXECUTABLE" => Some("the Claude Code binary is set by --claude-executable"),
        _ if names_the_parent_session(name) && !ALLOWED_BOSS_ENV.contains(&name) => {
            Some("it names the parent session or routes to an engine, so the agent may not be given it")
        }
        _ => None,
    }
}

/// The Claude Code options that the coordinator's terminal launch passes as
/// flags, in the shape the adapter takes them on `session/new`.
fn claude_code_options(args: &SessionArgs) -> Result<Option<Value>> {
    let mut options = Map::new();
    if let Some(model) = &args.model {
        options.insert("model".into(), json!(model));
    }
    if !args.setting_sources.is_empty() {
        options.insert("settingSources".into(), json!(args.setting_sources));
    }
    if let Some(settings) = &args.settings_json {
        let settings: Value = serde_json::from_str(settings).context("--settings-json is not JSON")?;
        if !settings.is_object() {
            bail!("--settings-json must be a JSON object");
        }
        options.insert("settings".into(), settings);
    }
    Ok((!options.is_empty()).then_some(Value::Object(options)))
}

/// Reads command lines on a task so a slow or idle source never blocks events.
async fn spawn_command_reader(script: Option<&Path>) -> Result<mpsc::UnboundedReceiver<Result<Command, String>>> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let source: Box<dyn tokio::io::AsyncRead + Send + Unpin> = match script {
        Some(path) => Box::new(
            tokio::fs::File::open(path)
                .await
                .with_context(|| format!("opening {path:?}"))?,
        ),
        None => Box::new(tokio::io::stdin()),
    };
    tokio::spawn(async move {
        let mut lines = BufReader::new(source).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parsed = serde_json::from_str::<Command>(line).map_err(|error| format!("{error}: {line}"));
            if sender.send(parsed).is_err() {
                break;
            }
        }
    });
    Ok(receiver)
}

impl Probe {
    /// Opens the gate when its condition holds or its bound has passed.
    fn settle_gate(&mut self) {
        let now = Instant::now();
        let (op, met, timed_out) = match &self.gate {
            Gate::Open => return,
            Gate::Sleep { until } => ("sleep", now >= *until, false),
            Gate::Idle { deadline } => ("await_idle", self.prompts_in_flight == 0, now >= *deadline),
            // An event gate is opened by `note_event`.
            Gate::Event { deadline, .. } => ("await_event", false, now >= *deadline),
            Gate::Quiet { quiet, deadline } => (
                "await_quiet",
                now.duration_since(self.last_arrival) >= *quiet,
                now >= *deadline,
            ),
        };
        if met || timed_out {
            if op != "sleep" {
                self.emit_event(json!({"event": "gate", "op": op, "met": met, "timed_out": !met && timed_out}));
            }
            self.gate = Gate::Open;
        }
    }

    fn emit_event(&self, event: Value) {
        (self.sink)(event);
    }

    /// When the main loop must look at the gate again.
    fn gate_wake(&self) -> Instant {
        match &self.gate {
            Gate::Open => Instant::now() + Duration::from_secs(3600),
            Gate::Sleep { until } => *until,
            Gate::Idle { deadline } | Gate::Event { deadline, .. } => *deadline,
            Gate::Quiet { quiet, deadline } => (self.last_arrival + *quiet).min(*deadline),
        }
    }

    /// Reports an event and opens an `await_event` gate that waits for it.
    fn report(&mut self, event: Value) {
        if let Gate::Event { name, .. } = &self.gate
            && event.get("event").and_then(Value::as_str) == Some(name.as_str())
        {
            self.emit_event(event);
            self.emit_event(json!({"event": "gate", "op": "await_event", "met": true, "timed_out": false}));
            self.gate = Gate::Open;
            return;
        }
        self.emit_event(event);
    }

    async fn run(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Prompt { text, images } => self.prompt(text, &images).await?,
            Command::Cancel => {
                // The protocol requires every pending permission request to
                // be answered `cancelled` when the turn is cancelled.
                let cancelled = self.pending_permissions.len();
                for pending in std::mem::take(&mut self.pending_permissions) {
                    self.answer_permission(pending, None).await?;
                }
                self.connection
                    .send_notification(&CancelNotification::new(self.session_id.clone()))
                    .await?;
                self.report(json!({"event": "cancel_sent", "permissions_cancelled": cancelled}));
            }
            Command::Permission {
                request_id,
                option_id,
                kind,
            } => {
                let position = match &request_id {
                    Some(wanted) => self
                        .pending_permissions
                        .iter()
                        .position(|pending| &pending.id.to_string() == wanted),
                    None => (!self.pending_permissions.is_empty()).then_some(0),
                };
                let Some(position) = position else {
                    self.report(json!({"event": "command_error", "error": "no such pending permission request"}));
                    return Ok(());
                };
                let pending = self.pending_permissions.remove(position);
                let choice = pending
                    .options
                    .iter()
                    .find(|option| match (&option_id, kind) {
                        (Some(id), _) => option.option_id.to_string() == *id,
                        (None, Some(kind)) => option.kind == kind,
                        (None, None) => false,
                    })
                    .cloned();
                if choice.is_none() && (option_id.is_some() || kind.is_some()) {
                    self.report(json!({
                        "event": "command_error",
                        "error": "the agent did not offer that option; the request stays pending",
                    }));
                    self.pending_permissions.insert(position, pending);
                    return Ok(());
                }
                self.answer_permission(pending, choice).await?;
            }
            Command::PermissionPolicy { policy } => self.policy = policy,
            Command::SetMode { mode } => {
                let request = SetSessionModeRequest::new(self.session_id.clone(), mode.clone());
                let outcome = self.connection.call(&request).await;
                self.report(json!({
                    "event": "mode_set",
                    "mode": mode,
                    "error": outcome.err().map(|error| error.to_string()),
                }));
            }
            Command::Request { method, mut params } => {
                substitute_placeholders(&mut params, self.session_id.0.as_ref(), &self.cwd);
                let request = UntypedMessage::new(&method, params).map_err(|error| anyhow!("{error}"))?;
                let pending = self.connection.send_request(&request).await?;
                self.report(json!({"event": "request_sent", "method": method, "request_id": pending.id()}));
                self.in_flight.push(InFlight::Request { method, pending });
            }
            Command::Processes => {
                let Some(processes) = self.descendants_or_report() else {
                    return Ok(());
                };
                self.report(json!({
                    "event": "processes",
                    "agent_pid": self.agent_pid,
                    "descendants": processes.iter().map(process_summary).collect::<Vec<_>>(),
                }));
            }
            Command::KillDescendant { name } => {
                let table = SystemWorkspaceProcessTable;
                let mut killed = Vec::new();
                let Some(descendants) = self.descendants_or_report() else {
                    return Ok(());
                };
                for process in descendants {
                    if process.argv_summary().contains(&name) {
                        let outcome = table.signal(process.pid, Signal::Kill);
                        let mut summary = process_summary(&process);
                        summary["error"] = json!(outcome.err().map(|error| error.to_string()));
                        killed.push(summary);
                    }
                }
                self.report(json!({"event": "killed", "signal": "SIGKILL", "name": name, "processes": killed}));
            }
            Command::InboxRegistrations => {
                let Some(descendants) = self.descendants_or_report() else {
                    return Ok(());
                };
                let registrations: Vec<Value> = descendants
                    .iter()
                    .map(|process| transcript::inbox_registration(process.pid, &process.argv_summary()))
                    .collect();
                self.report(json!({"event": "inbox_registrations", "descendants": registrations}));
            }
            Command::Sleep { ms } => {
                self.gate = Gate::Sleep {
                    until: Instant::now() + Duration::from_millis(ms),
                };
            }
            Command::AwaitIdle { timeout_ms } => {
                self.gate = Gate::Idle {
                    deadline: Instant::now() + Duration::from_millis(timeout_ms),
                };
            }
            Command::AwaitEvent { event, timeout_ms } => {
                self.gate = Gate::Event {
                    name: event,
                    deadline: Instant::now() + Duration::from_millis(timeout_ms),
                };
            }
            Command::AwaitQuiet { quiet_ms, timeout_ms } => {
                self.last_arrival = Instant::now();
                self.gate = Gate::Quiet {
                    quiet: Duration::from_millis(quiet_ms),
                    deadline: Instant::now() + Duration::from_millis(timeout_ms),
                };
            }
            Command::Shutdown => unreachable!("handled by the main loop"),
        }
        Ok(())
    }

    /// The agent's descendants, or `None` after reporting that the process
    /// listing failed.
    fn descendants_or_report(&mut self) -> Option<Vec<LiveProcess>> {
        match self.descendants() {
            Ok(processes) => Some(processes),
            Err(error) => {
                self.report(json!({"event": "command_error", "error": error.to_string()}));
                None
            }
        }
    }

    async fn prompt(&mut self, text: String, images: &[PathBuf]) -> Result<()> {
        let mut blocks = Vec::new();
        for image in images {
            // A bad attachment is this command's problem, not the
            // conversation's: report it and carry on.
            let bytes = match tokio::fs::read(image).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.report(json!({"event": "command_error", "error": format!("reading {image:?}: {error}")}));
                    return Ok(());
                }
            };
            let Some(mime) = image_mime_type(&bytes) else {
                self.report(json!({
                    "event": "command_error",
                    "error": format!("{image:?} is not a PNG, JPEG, GIF or WebP image"),
                }));
                return Ok(());
            };
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            blocks.push(ContentBlock::Image(ImageContent::new(encoded, mime)));
        }
        if !text.is_empty() {
            blocks.push(ContentBlock::Text(TextContent::new(text.clone())));
        }
        if blocks.is_empty() {
            self.report(json!({"event": "command_error", "error": "a prompt needs text or an image"}));
            return Ok(());
        }
        let pending = self
            .connection
            .send_request(&PromptRequest::new(self.session_id.clone(), blocks))
            .await?;
        self.turns += 1;
        self.prompts_in_flight += 1;
        let turn = self.turns;
        self.report(json!({
            "event": "turn_started",
            "turn": turn,
            "request_id": pending.id(),
            "text": text,
            "images": images.len(),
            "prompts_in_flight": self.prompts_in_flight,
        }));
        self.in_flight.push(InFlight::Prompt { turn, pending });
        Ok(())
    }

    /// Reports the end of a request. Its result is already in hand when the
    /// connection has reported the response, or has closed, so this never waits.
    async fn complete(&mut self, finished: InFlight) {
        match finished {
            InFlight::Prompt { turn, pending } => {
                let id = pending.id().clone();
                let result = pending.response().await;
                self.prompts_in_flight -= 1;
                let event = match result {
                    Ok(response) => json!({
                        "event": "turn_ended",
                        "turn": turn,
                        "request_id": id,
                        "stop_reason": response.stop_reason,
                        "prompts_in_flight": self.prompts_in_flight,
                    }),
                    Err(error) => json!({
                        "event": "turn_failed",
                        "turn": turn,
                        "request_id": id,
                        "error": error.to_string(),
                        "prompts_in_flight": self.prompts_in_flight,
                    }),
                };
                self.report(event);
            }
            InFlight::Request { method, pending } => {
                let id = pending.id().clone();
                let event = match pending.response().await {
                    Ok(result) => json!({"event": "response", "method": method, "request_id": id, "result": result}),
                    Err(error) => {
                        json!({"event": "response", "method": method, "request_id": id, "error": error.to_string()})
                    }
                };
                self.report(event);
            }
        }
    }

    async fn handle_incoming(&mut self, incoming: Incoming) -> Result<Flow> {
        self.last_arrival = Instant::now();
        match incoming {
            Incoming::Notification { method, params, seq } if method == "session/update" => {
                let event = match SessionNotification::parse_message(&method, &params) {
                    Ok(notification) => events::session_update(&notification.update, &params, seq),
                    // The agent sent an update the typed schema cannot read.
                    Err(_) => events::untyped_session_update(&params, seq),
                };
                self.report(with_in_flight(event, self.prompts_in_flight));
            }
            Incoming::Notification { method, params, seq } if method == "$/cancel_request" => {
                // The agent withdraws a request of its own, for example a
                // permission request whose tool call can no longer run. It
                // expects no answer, so the request stops being pending.
                let withdrawn = params
                    .get("requestId")
                    .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok());
                let was_pending = withdrawn.as_ref().is_some_and(|id| {
                    let before = self.pending_permissions.len();
                    self.pending_permissions.retain(|pending| &pending.id != id);
                    self.pending_permissions.len() != before
                });
                self.report(json!({
                    "event": "permission_withdrawn",
                    "request_id": withdrawn.map(|id| id.to_string()),
                    "was_pending": was_pending,
                    "seq": seq,
                }));
            }
            Incoming::Notification { method, params, seq } => {
                self.report(events::extension_notification(&method, &params, seq));
            }
            Incoming::Request {
                id,
                method,
                params,
                seq,
            } if method == "session/request_permission" => {
                let request = match RequestPermissionRequest::parse_message(&method, &params) {
                    Ok(request) => request,
                    Err(error) => {
                        self.connection
                            .respond::<Value>(id, Err(boss_acp_client::schema::Error::invalid_params()))
                            .await?;
                        self.report(
                            json!({"event": "agent_request", "method": method, "error": error.to_string(), "seq": seq}),
                        );
                        return Ok(Flow::Continue);
                    }
                };
                let event = events::permission_request(&id, &request, &params, seq);
                self.report(with_in_flight(event, self.prompts_in_flight));
                let pending = PendingPermission {
                    id,
                    options: request.options,
                };
                let wanted = match self.policy {
                    PermissionPolicy::Ask => {
                        self.pending_permissions.push(pending);
                        return Ok(Flow::Continue);
                    }
                    PermissionPolicy::Allow => Some(PermissionOptionKind::AllowOnce),
                    PermissionPolicy::Reject => Some(PermissionOptionKind::RejectOnce),
                    PermissionPolicy::Cancel => None,
                };
                let choice = wanted.and_then(|kind| pending.options.iter().find(|option| option.kind == kind).cloned());
                self.answer_permission(pending, choice).await?;
            }
            Incoming::Request { id, method, seq, .. } => {
                // No `fs`, no `terminal`: the probe executes nothing for the agent.
                self.connection
                    .respond::<Value>(id.clone(), Err(boss_acp_client::schema::Error::method_not_found()))
                    .await?;
                self.report(json!({
                    "event": "agent_request",
                    "method": method,
                    "request_id": id,
                    "answered": "method_not_found",
                    "seq": seq,
                }));
            }
            Incoming::ResponseArrived { id, .. } => {
                if let Some(position) = self.in_flight.iter().position(|request| request.id() == &id) {
                    let finished = self.in_flight.remove(position);
                    self.complete(finished).await;
                }
            }
            Incoming::UnmatchedResponse { id, result, seq } => {
                self.report(json!({
                    "event": "unmatched_response",
                    "request_id": id,
                    "is_error": result.is_err(),
                    "seq": seq,
                }));
            }
            Incoming::Malformed { preview, reason, seq } => {
                self.report(json!({"event": "malformed", "reason": reason, "preview": preview, "seq": seq}));
            }
            Incoming::Closed(reason) => {
                // The connection failed every request still waiting before it
                // reported the close, so each one is reported as failed here,
                // after everything the agent sent before it went away. The
                // close itself comes last: a front end treats it as the end of
                // the conversation.
                for finished in std::mem::take(&mut self.in_flight) {
                    self.complete(finished).await;
                }
                self.emit_event(json!({"event": "closed", "reason": reason.to_string()}));
                return Ok(Flow::Closed);
            }
        }
        Ok(Flow::Continue)
    }

    /// Sends the permission response: the chosen option, or `cancelled`.
    async fn answer_permission(&mut self, pending: PendingPermission, choice: Option<PermissionOption>) -> Result<()> {
        let outcome = match &choice {
            Some(option) => {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
            }
            None => RequestPermissionOutcome::Cancelled,
        };
        self.connection
            .respond(pending.id.clone(), Ok(RequestPermissionResponse::new(outcome)))
            .await?;
        self.report(json!({
            "event": "permission_resolved",
            "request_id": pending.id.to_string(),
            "outcome": if choice.is_some() { "selected" } else { "cancelled" },
            "option_id": choice.as_ref().map(|option| option.option_id.clone()),
            "option_kind": choice.as_ref().map(|option| option.kind),
            "option_name": choice.as_ref().map(|option| option.name.clone()),
        }));
        Ok(())
    }

    /// Every live process below the agent, parents before children.
    fn descendants(&self) -> Result<Vec<LiveProcess>> {
        let all = SystemWorkspaceProcessTable.list().context("listing processes")?;
        let mut found: Vec<LiveProcess> = Vec::new();
        let mut frontier = vec![self.agent_pid as i32];
        while let Some(parent) = frontier.pop() {
            for process in all.iter().filter(|process| process.ppid == parent) {
                frontier.push(process.pid);
                found.push(process.clone());
            }
        }
        Ok(found)
    }
}

/// Replaces the `$session` and `$cwd` strings anywhere in `value`.
fn substitute_placeholders(value: &mut Value, session_id: &str, cwd: &str) {
    match value {
        Value::String(text) if text == "$session" => *text = session_id.to_owned(),
        Value::String(text) if text == "$cwd" => *text = cwd.to_owned(),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| substitute_placeholders(item, session_id, cwd)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| substitute_placeholders(field, session_id, cwd)),
        _ => {}
    }
}

fn with_in_flight(mut event: Value, prompts_in_flight: usize) -> Value {
    event["prompts_in_flight"] = json!(prompts_in_flight);
    event
}

fn process_summary(process: &LiveProcess) -> Value {
    json!({"pid": process.pid, "ppid": process.ppid, "command": process.argv_summary()})
}

/// Recognises the image formats Claude accepts from their leading bytes.
fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn converse_args(extra: &[&str]) -> SessionArgs {
        let mut argv = vec![
            "boss-acp-probe",
            "converse",
            "--cwd",
            "/tmp",
            "--journal",
            "/tmp/probe/j.jsonl",
        ];
        argv.extend_from_slice(extra);
        argv.extend_from_slice(&["--", "agent"]);
        match Cli::try_parse_from(argv).unwrap().command {
            Mode::Converse(args) => args.session,
            _ => panic!("parsed the wrong subcommand"),
        }
    }

    fn inherited(pairs: &[(&str, &str)]) -> impl Iterator<Item = (String, String)> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn value_of<'a>(environment: &'a AgentEnvironment, name: &str) -> Option<&'a str> {
        environment
            .variables
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn the_agent_inherits_nothing_that_routes_to_a_production_engine_or_a_parent_session() {
        let args = converse_args(&[
            "--boss-socket",
            "/tmp/isolated/engine.sock",
            "--path-prefix",
            "/built/bin",
        ]);
        let environment = agent_environment(
            &args,
            inherited(&[
                ("HOME", "/Users/someone"),
                ("PATH", "/usr/bin"),
                (
                    "BOSS_SOCKET_PATH",
                    "/Users/someone/Library/Application Support/Boss/engine.sock",
                ),
                (
                    "BOSS_EVENTS_SOCKET",
                    "/Users/someone/Library/Application Support/Boss/events.sock",
                ),
                ("BOSS_SPAWN_TOKEN", "secret"),
                ("CLAUDECODE", "1"),
                ("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/cc-socks/1.sock"),
                ("CLAUDE_PID", "1"),
                ("TMUX", "/private/tmp/tmux-501/default,1,0"),
                ("TMUX_PANE", "%7"),
                ("ANTHROPIC_API_KEY", "sk-secret"),
                ("ANTHROPIC_BASE_URL", "https://gateway.example"),
            ]),
        )
        .unwrap();

        assert_eq!(
            value_of(&environment, "BOSS_SOCKET_PATH"),
            Some("/tmp/isolated/engine.sock")
        );
        assert_eq!(value_of(&environment, "PATH"), Some("/built/bin:/usr/bin"));
        assert_eq!(value_of(&environment, "HOME"), Some("/Users/someone"));
        for gone in [
            "BOSS_EVENTS_SOCKET",
            "BOSS_SPAWN_TOKEN",
            "CLAUDECODE",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_PID",
            "TMUX",
            "TMUX_PANE",
            "ANTHROPIC_API_KEY",
        ] {
            assert_eq!(value_of(&environment, gone), None, "{gone} must not reach the agent");
        }
        // A provider override is not ours to remove, but it is named.
        assert_eq!(
            value_of(&environment, "ANTHROPIC_BASE_URL"),
            Some("https://gateway.example")
        );
        assert_eq!(
            environment.report["provider_override_variables"],
            json!(["ANTHROPIC_BASE_URL"])
        );
        assert_eq!(environment.report["anthropic_api_key_was_set"], json!(true));
        let report = environment.report.to_string();
        assert!(
            !report.contains("sk-secret") && !report.contains("secret"),
            "the report holds no values: {report}"
        );
    }

    #[test]
    fn without_an_isolated_engine_boss_is_pointed_at_a_socket_that_does_not_exist() {
        let environment = agent_environment(&converse_args(&[]), inherited(&[("PATH", "/usr/bin")])).unwrap();
        assert_eq!(
            value_of(&environment, "BOSS_SOCKET_PATH"),
            Some("/tmp/probe/j.jsonl.no-engine.sock")
        );
        assert_eq!(environment.report["boss_socket_is_isolated_engine"], json!(false));
    }

    #[test]
    fn explicit_environment_wins_and_must_be_an_assignment() {
        let args = converse_args(&[
            "--env",
            "BOSS_BIN=/built/bin/boss",
            "--claude-executable",
            "/usr/local/bin/claude",
        ]);
        let environment = agent_environment(&args, inherited(&[("BOSS_BIN", "/Applications/Boss.app/boss")])).unwrap();
        assert_eq!(value_of(&environment, "BOSS_BIN"), Some("/built/bin/boss"));
        assert_eq!(
            value_of(&environment, "CLAUDE_CODE_EXECUTABLE"),
            Some("/usr/local/bin/claude")
        );
        assert!(agent_environment(&converse_args(&["--env", "NOT_AN_ASSIGNMENT"]), inherited(&[])).is_err());
    }

    const PRODUCTION_DIRECTORY: &str = "/Users/someone/Library/Application Support/Boss/boss-session";

    #[test]
    fn production_paths_are_refused_wherever_the_run_would_read_write_or_route_to_them() {
        // One flag is given `value`; `--cwd` and `--journal` default to a safe place.
        let refused = |flag: &str, value: &str| {
            let defaults = [("--cwd", "/tmp"), ("--journal", "/tmp/j.jsonl")];
            let mut all = vec!["boss-acp-probe", "converse"];
            for (name, default) in defaults {
                all.extend([name, if name == flag { value } else { default }]);
            }
            if !defaults.iter().any(|(name, _)| *name == flag) {
                all.extend([flag, value]);
            }
            all.extend(["--", "agent"]);
            let Mode::Converse(args) = Cli::try_parse_from(all).unwrap().command else {
                panic!("parsed the wrong subcommand");
            };
            refuse_production_paths(&args.session).map_err(|error| error.to_string())
        };
        for flag in ["--boss-socket", "--cwd", "--journal", "--agent-stderr", "--path-prefix"] {
            let error = refused(flag, PRODUCTION_DIRECTORY).unwrap_err();
            assert!(
                error.contains("production Boss path") && error.contains(flag),
                "{flag}: {error}"
            );
        }
        assert!(refused("--boss-socket", "/tmp/boss-engine.sock").is_err());
        assert!(refused("--boss-socket", "/tmp/x/engine.sock").is_ok());
        assert!(refused("--agent-stderr", "/tmp/x/agent.log").is_ok());
    }

    #[test]
    fn env_cannot_undo_the_isolation_or_carry_a_production_path() {
        let isolated = ["--boss-socket", "/tmp/isolated/engine.sock"];
        for assignment in [
            "BOSS_SOCKET_PATH=/tmp/another/engine.sock",
            "BOSS_EVENTS_SOCKET=/tmp/another/events.sock",
            "BOSS_SPAWN_TOKEN=secret",
            "ANTHROPIC_API_KEY=sk-secret",
            "CLAUDE_CODE_EXECUTABLE=/usr/local/bin/claude",
            "CLAUDE_CODE_MESSAGING_SOCKET=/tmp/cc-socks/1.sock",
            "CLAUDECODE=1",
            "CLAUDE_PID=1",
            "TMUX=/private/tmp/tmux-501/default,1,0",
            "TMUX_PANE=%7",
        ] {
            let mut flags = isolated.to_vec();
            flags.extend(["--env", assignment]);
            let error = agent_environment(&converse_args(&flags), inherited(&[]))
                .err()
                .unwrap_or_else(|| panic!("--env {assignment} was accepted"))
                .to_string();
            assert!(error.contains("--env"), "{assignment}: {error}");
            assert!(
                !error.contains("secret"),
                "the refusal does not echo the value: {error}"
            );
        }
        // Not a variable that identifies a session or routes to an engine.
        let mut flags = isolated.to_vec();
        flags.extend(["--env", "BOSS_BIN=/built/bin/boss", "--env", "RUST_LOG=debug"]);
        assert!(agent_environment(&converse_args(&flags), inherited(&[])).is_ok());

        // A value that points into the installed app's state is refused under any name.
        let mut flags = isolated.to_vec();
        let production = format!("STATE_DIRECTORY={PRODUCTION_DIRECTORY}");
        flags.extend(["--env", &production]);
        let error = agent_environment(&converse_args(&flags), inherited(&[]))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("production Boss path"), "{error}");
    }

    #[test]
    fn the_report_is_read_from_the_variables_the_agent_gets() {
        let isolated = converse_args(&[
            "--boss-socket",
            "/tmp/isolated/engine.sock",
            "--env",
            "BOSS_BIN=/b/boss",
        ]);
        let environment = agent_environment(
            &isolated,
            inherited(&[("ANTHROPIC_API_KEY", "sk-secret"), ("BOSS_SOCKET_PATH", "/prod.sock")]),
        )
        .unwrap();
        assert_eq!(environment.report["anthropic_api_key_was_set"], json!(true));
        assert_eq!(environment.report["anthropic_api_key_passed_on"], json!(false));
        assert_eq!(environment.report["boss_socket_is_isolated_engine"], json!(true));
        assert_eq!(environment.report["extra_variables"], json!(["BOSS_BIN"]));
        assert_eq!(value_of(&environment, "ANTHROPIC_API_KEY"), None);

        let without =
            agent_environment(&converse_args(&["--claude-executable", "/bin/claude"]), inherited(&[])).unwrap();
        assert_eq!(without.report["boss_socket_is_isolated_engine"], json!(false));
        assert_eq!(without.report["claude_executable"], json!("/bin/claude"));
    }

    // ── A refused run creates and truncates nothing ──────────────────────────

    /// A fresh scratch directory and, inside it, one whose path is the
    /// installed app's state directory. Both exist on disk.
    pub(crate) fn scratch_with_production_directory(name: &str) -> (PathBuf, PathBuf) {
        let scratch = std::env::temp_dir().join(format!("boss-acp-probe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let production = scratch.join("Library/Application Support/Boss/session");
        std::fs::create_dir_all(&production).unwrap();
        (scratch, production)
    }

    /// A file that must survive a refused run, with the text it holds.
    pub(crate) fn sentinel(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, "sentinel").unwrap();
        path
    }

    #[tokio::test]
    async fn a_refused_converse_run_leaves_every_file_it_was_pointed_at_untouched() {
        let (scratch, production) = scratch_with_production_directory("converse");
        let journal = sentinel(&production, "journal.jsonl");
        let stderr = sentinel(&production, "agent.log");
        let elsewhere_journal = scratch.join("journal.jsonl");
        let elsewhere_stderr = scratch.join("agent.log");
        let scratch_text = scratch.to_string_lossy().into_owned();
        for (journal_flag, stderr_flag) in [
            (&journal, &elsewhere_stderr),
            (&elsewhere_journal, &stderr),
            (&journal, &stderr),
        ] {
            let argv = [
                "boss-acp-probe",
                "converse",
                "--cwd",
                &scratch_text,
                "--journal",
                &journal_flag.to_string_lossy(),
                "--agent-stderr",
                &stderr_flag.to_string_lossy(),
                "--",
                "/nonexistent/agent",
            ];
            let Mode::Converse(args) = Cli::try_parse_from(argv).unwrap().command else {
                panic!("parsed the wrong subcommand");
            };
            let error = converse(*args).await.unwrap_err().to_string();
            assert!(error.contains("production Boss path"), "{error}");
        }
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), "sentinel");
        assert_eq!(std::fs::read_to_string(&stderr).unwrap(), "sentinel");
        assert!(!elsewhere_journal.exists(), "a refused run created no journal");
        assert!(!elsewhere_stderr.exists(), "a refused run created no stderr file");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test]
    async fn a_run_with_a_malformed_option_creates_no_journal() {
        let (scratch, _) = scratch_with_production_directory("malformed");
        let journal = scratch.join("journal.jsonl");
        let argv = [
            "boss-acp-probe",
            "converse",
            "--cwd",
            &scratch.to_string_lossy(),
            "--journal",
            &journal.to_string_lossy(),
            "--config",
            "not-an-assignment",
            "--",
            "/nonexistent/agent",
        ];
        let Mode::Converse(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("parsed the wrong subcommand");
        };
        assert!(converse(*args).await.is_err());
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    // ── Order of what the probe reports ──────────────────────────────────────

    /// A probe on an in-memory stream, with the agent's side in the test's hands.
    struct Harness {
        probe: Probe,
        incoming: mpsc::UnboundedReceiver<Incoming>,
        reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        events: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    async fn soon<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(20), future)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    impl Harness {
        fn new() -> Self {
            let (client_io, peer_io) = tokio::io::duplex(1 << 20);
            let (client_reader, client_writer) = tokio::io::split(client_io);
            let (peer_reader, writer) = tokio::io::split(peer_io);
            let (connection, incoming) =
                Connection::open(client_reader, client_writer, None, ConnectionOptions::default());
            let events: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
            let recorder = events.clone();
            let probe = Probe::builder()
                .connection(connection)
                .session_id(SessionId::new("session-1"))
                .cwd("/tmp")
                .agent_pid(0)
                .policy(PermissionPolicy::Ask)
                .sink(Box::new(move |event| recorder.lock().unwrap().push(event)))
                .build();
            Self {
                probe,
                incoming,
                reader: BufReader::new(peer_reader),
                writer,
                events,
            }
        }

        /// Sends a prompt and returns the id the agent saw it with.
        async fn prompt(&mut self, text: &str) -> Value {
            self.probe
                .run(Command::Prompt {
                    text: text.into(),
                    images: Vec::new(),
                })
                .await
                .unwrap();
            let mut line = String::new();
            soon("the prompt on the wire", self.reader.read_line(&mut line))
                .await
                .unwrap();
            serde_json::from_str::<Value>(&line).unwrap()["id"].clone()
        }

        /// The agent writes all of `lines` in one go, so one read carries them all.
        async fn agent_says(&mut self, lines: &[Value]) {
            let bytes: String = lines.iter().map(|line| format!("{line}\n")).collect();
            tokio::io::AsyncWriteExt::write_all(&mut self.writer, bytes.as_bytes())
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::flush(&mut self.writer).await.unwrap();
        }

        /// Handles the next `count` things the connection reports.
        async fn pump(&mut self, count: usize) {
            for _ in 0..count {
                let arrived = soon("an incoming message", self.incoming.recv()).await.unwrap();
                assert_eq!(self.probe.handle_incoming(arrived).await.unwrap(), Flow::Continue);
            }
        }

        async fn pump_until_closed(&mut self) {
            loop {
                let arrived = soon("an incoming message", self.incoming.recv()).await.unwrap();
                if self.probe.handle_incoming(arrived).await.unwrap() == Flow::Closed {
                    return;
                }
            }
        }

        /// What was reported, one short string per event.
        fn reported(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|event| {
                    let name = event["event"].as_str().unwrap();
                    let detail = event
                        .get("turn")
                        .map(Value::to_string)
                        .or_else(|| event.get("text").and_then(Value::as_str).map(str::to_owned))
                        .unwrap_or_default();
                    format!("{name} {detail}").trim_end().to_owned()
                })
                .collect()
        }

        fn event(&self, name: &str) -> Vec<Value> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event["event"] == name)
                .cloned()
                .collect()
        }
    }

    fn agent_text(text: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "session-1",
                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}},
            },
        })
    }

    fn end_of_turn(id: &Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}})
    }

    #[tokio::test]
    async fn a_prompt_naming_a_missing_image_reports_command_error_and_leaves_the_probe_running() {
        let mut harness = Harness::new();
        harness
            .probe
            .run(Command::Prompt {
                text: "look".into(),
                images: vec![PathBuf::from("/nonexistent/boss-acp-probe/missing.png")],
            })
            .await
            .unwrap();
        let errors = harness.event("command_error");
        assert_eq!(errors.len(), 1, "{:?}", harness.reported());
        assert!(errors[0]["error"].as_str().unwrap().contains("missing.png"));
        assert!(harness.event("turn_started").is_empty());

        // The conversation carries on.
        let id = harness.prompt("again").await;
        harness.agent_says(&[end_of_turn(&id)]).await;
        harness.pump(1).await;
        assert_eq!(harness.event("turn_ended").len(), 1);
    }

    #[tokio::test]
    async fn a_turn_ends_between_the_updates_that_surround_it_even_when_one_read_carries_them_all() {
        let mut harness = Harness::new();
        let first = harness.prompt("A").await;
        let second = harness.prompt("B").await;
        harness
            .agent_says(&[
                agent_text("A-last"),
                end_of_turn(&first),
                agent_text("B-first"),
                end_of_turn(&second),
            ])
            .await;
        harness.pump(4).await;

        assert_eq!(
            harness.reported(),
            [
                "turn_started 1",
                "turn_started 2",
                "message_chunk A-last",
                "turn_ended 1",
                "message_chunk B-first",
                "turn_ended 2",
            ]
        );
        let ended = harness.event("turn_ended");
        assert_eq!(
            ended
                .iter()
                .map(|event| event["prompts_in_flight"].clone())
                .collect::<Vec<_>>(),
            [json!(1), json!(0)],
            "the count of turns in flight is right as each one ends"
        );
        assert_eq!(harness.event("message_chunk")[1]["prompts_in_flight"], 1);
    }

    #[tokio::test]
    async fn a_turn_that_ended_just_before_the_agent_closed_is_reported_as_ended() {
        let mut harness = Harness::new();
        let first = harness.prompt("A").await;
        harness.agent_says(&[agent_text("A-last"), end_of_turn(&first)]).await;
        tokio::io::AsyncWriteExt::shutdown(&mut harness.writer).await.unwrap();
        harness.pump_until_closed().await;

        assert_eq!(
            harness.reported(),
            ["turn_started 1", "message_chunk A-last", "turn_ended 1", "closed"]
        );
        assert!(harness.event("turn_failed").is_empty());
    }

    #[tokio::test]
    async fn a_turn_the_agent_never_answered_is_reported_as_failed_before_the_close() {
        let mut harness = Harness::new();
        harness.prompt("A").await;
        harness.agent_says(&[agent_text("A-partial")]).await;
        tokio::io::AsyncWriteExt::shutdown(&mut harness.writer).await.unwrap();
        harness.pump_until_closed().await;

        assert_eq!(
            harness.reported(),
            ["turn_started 1", "message_chunk A-partial", "turn_failed 1", "closed"]
        );
        let failed = &harness.event("turn_failed")[0];
        assert!(
            failed["error"].as_str().unwrap().contains("closed before the response"),
            "{failed}"
        );
        assert_eq!(failed["prompts_in_flight"], 0);
    }

    // ── Shutdown ─────────────────────────────────────────────────────────────

    /// The agent never closes its output and keeps talking: with the
    /// deadline passing while a line is being handled, the run still ends.
    #[tokio::test]
    async fn shutdown_ends_the_run_at_its_deadline_however_busy_the_agent_is() {
        let Harness {
            mut probe,
            mut incoming,
            reader,
            mut writer,
            events,
        } = Harness::new();
        let (commands_tx, mut commands) = mpsc::unbounded_channel();
        commands_tx.send(Ok(Command::Shutdown)).unwrap();
        let talker = tokio::spawn(async move {
            let _input = reader;
            while tokio::io::AsyncWriteExt::write_all(&mut writer, b"{\"jsonrpc\":\"2.0\",\"method\":\"probe/tick\"}\n")
                .await
                .is_ok()
            {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });

        soon(
            "the run to end",
            drive(&mut probe, &mut incoming, &mut commands, Duration::from_millis(150)),
        )
        .await
        .unwrap();

        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| event["event"] == "shutdown_timed_out"));
        assert!(
            events.len() > 1,
            "the agent was still being heard when the deadline passed"
        );
        talker.abort();
    }

    #[tokio::test]
    async fn shutdown_ends_the_run_as_soon_as_the_agent_closes_its_output() {
        let Harness {
            mut probe,
            mut incoming,
            mut reader,
            writer,
            events,
        } = Harness::new();
        let (commands_tx, mut commands) = mpsc::unbounded_channel();
        commands_tx.send(Ok(Command::Shutdown)).unwrap();
        // An agent that exits when its input ends.
        let agent = tokio::spawn(async move {
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;
            drop(writer);
        });

        soon(
            "the run to end",
            drive(&mut probe, &mut incoming, &mut commands, Duration::from_secs(300)),
        )
        .await
        .unwrap();

        agent.await.unwrap();
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| event["event"] == "closed"));
        assert!(!events.iter().any(|event| event["event"] == "shutdown_timed_out"));
    }

    #[test]
    fn session_options_use_the_adapter_option_names_and_are_omitted_when_empty() {
        assert_eq!(claude_code_options(&converse_args(&[])).unwrap(), None);
        let args = converse_args(&[
            "--model",
            "opus",
            "--setting-source",
            "user",
            "--setting-source",
            "project",
            "--setting-source",
            "local",
            "--settings-json",
            r#"{"crossSessionInbound":"accept"}"#,
        ]);
        assert_eq!(
            claude_code_options(&args).unwrap(),
            Some(json!({
                "model": "opus",
                "settingSources": ["user", "project", "local"],
                "settings": {"crossSessionInbound": "accept"},
            }))
        );
        assert!(claude_code_options(&converse_args(&["--settings-json", "[1]"])).is_err());
    }

    #[test]
    fn config_assignments_keep_their_order_and_must_name_an_option_and_a_value() {
        let args = converse_args(&["--config", "model=opus", "--config", "effort=high"]);
        assert_eq!(
            config_assignments(&args).unwrap(),
            [("model", "opus"), ("effort", "high")]
        );
        for malformed in ["model", "=opus", "model="] {
            assert!(
                config_assignments(&converse_args(&["--config", malformed])).is_err(),
                "{malformed}"
            );
        }
    }

    #[test]
    fn the_handshake_advertises_no_tool_execution_and_only_the_extensions_asked_for() {
        let plain = serde_json::to_value(initialize_request(&converse_args(&[]))).unwrap();
        assert_eq!(plain["protocolVersion"], 1);
        assert_eq!(plain["clientCapabilities"]["terminal"], false);
        assert_eq!(
            plain["clientCapabilities"]["fs"],
            json!({"readTextFile": false, "writeTextFile": false})
        );
        assert_eq!(plain["clientCapabilities"].get("_meta"), None);

        let extended = converse_args(&[
            "--air-capability",
            "backgroundSubagents",
            "--air-capability",
            "asyncTasks",
        ]);
        let extended = serde_json::to_value(initialize_request(&extended)).unwrap();
        assert_eq!(
            extended["clientCapabilities"]["_meta"],
            json!({"jetbrains": {"air": {"version": 1, "capabilities": ["backgroundSubagents", "asyncTasks"]}}})
        );
        assert_eq!(extended["clientCapabilities"]["terminal"], false);
    }

    #[test]
    fn commands_parse_from_the_documented_lines_and_reject_unknown_fields() {
        let parse = |line: &str| serde_json::from_str::<Command>(line);
        assert_eq!(
            parse(r#"{"op":"prompt","text":"hi","images":["/tmp/a.png"]}"#).unwrap(),
            Command::Prompt {
                text: "hi".into(),
                images: vec!["/tmp/a.png".into()],
            }
        );
        assert_eq!(
            parse(r#"{"op":"permission","kind":"reject_once"}"#).unwrap(),
            Command::Permission {
                request_id: None,
                option_id: None,
                kind: Some(PermissionOptionKind::RejectOnce),
            }
        );
        assert_eq!(parse(r#"{"op":"cancel"}"#).unwrap(), Command::Cancel);
        assert_eq!(
            parse(r#"{"op":"await_quiet","quiet_ms":5000,"timeout_ms":60000}"#).unwrap(),
            Command::AwaitQuiet {
                quiet_ms: 5000,
                timeout_ms: 60000,
            }
        );
        assert!(parse(r#"{"op":"prompt","txt":"typo"}"#).is_err());
        assert!(parse(r#"{"op":"no_such_command"}"#).is_err());
    }

    #[test]
    fn request_placeholders_are_replaced_wherever_they_appear_and_nothing_else_is() {
        let mut params = json!({
            "sessionId": "$session",
            "cwd": "$cwd",
            "nested": [{"again": "$session"}, "$sessions", 7],
        });
        substitute_placeholders(&mut params, "session-1", "/tmp/dir");
        assert_eq!(
            params,
            json!({
                "sessionId": "session-1",
                "cwd": "/tmp/dir",
                "nested": [{"again": "session-1"}, "$sessions", 7],
            })
        );
    }

    #[test]
    fn image_formats_are_recognised_from_their_leading_bytes() {
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\n rest"), Some("image/png"));
        assert_eq!(image_mime_type(b"\xff\xd8\xff\xe0 rest"), Some("image/jpeg"));
        assert_eq!(image_mime_type(b"GIF89a rest"), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_mime_type(b"<svg xmlns"), None);
        assert_eq!(image_mime_type(b""), None);
    }
}
