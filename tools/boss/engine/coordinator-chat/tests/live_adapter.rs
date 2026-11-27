//! Manual harness: the chat service against the real Claude ACP adapter, the
//! real `claude` binary and the real login. It is not a test and is not run by
//! `bazel test`: the hermetic test sandbox cannot reach a login, and a real
//! session spends the operator's plan usage. Build it and launch it by path.
//!
//! ```text
//! live_adapter_probe --host <boss-agent-host> --adapter <claude-agent-acp> \
//!     --claude <claude> [--model haiku]
//! ```
//!
//! It answers one question the coordinator ACP spike could not: after the
//! engine process that sent a prompt is gone, does the new one recover that
//! request, take the adapter's response to it, and carry on? The spike only
//! showed that the official SDK's connection drops such a response, and that
//! Boss's own id table can be told to expect it, against a scripted peer.
//!
//! Two scenarios run against one session, each restarting the service (a new
//! `ChatService` over the same generation directory, as a restarted engine
//! has) while the agent is mid-turn:
//!
//! 1. A turn is in flight and a second message is queued behind it. The new
//!    service must see the first turn's response arrive live, then send the
//!    queued message, then see its response.
//! 2. A turn is waiting on a permission request. The agent numbers its own
//!    requests from 0, so its request is `0` while the client has used ids up
//!    to 7. The new service must answer it, by direction as well as id, and
//!    the agent must carry out what was allowed.
//!
//! The adapter's version and the Claude Code version are whatever is
//! installed; the spike recorded 0.110.0 and 2.1.332.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use boss_agent_host::Handshake;
use boss_agent_host::journal::{Direction, WireRecord, read_records};
use boss_engine_coordinator_chat::{
    ChatService, ConnectionStatus, CredentialRedactor, Generation, GenerationStore, HostEndpoint, ServiceConfig,
    ServiceTuning,
};
use boss_protocol::{
    CoordinatorChatConnectionPhase, CoordinatorChatFetchOutcome, CoordinatorChatItemKind, CoordinatorChatMessageRole,
    CoordinatorChatPermissionResolution,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

/// A real model answers in seconds, not milliseconds.
const STEP: Duration = Duration::from_secs(120);

struct Args {
    host: PathBuf,
    adapter: PathBuf,
    claude: PathBuf,
    model: String,
}

fn parse_args() -> Result<Args, String> {
    let mut values = std::collections::HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        values.insert(flag, value);
    }
    let path = |flag: &str| {
        values
            .get(flag)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{flag} is required"))
    };
    Ok(Args {
        host: path("--host")?,
        adapter: path("--adapter")?,
        claude: path("--claude")?,
        model: values.get("--model").cloned().unwrap_or_else(|| "haiku".to_owned()),
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("live_adapter_probe: {message}");
            return ExitCode::from(2);
        }
    };
    match run(&args).await {
        Ok(()) => {
            println!("LIVE_PROBE: all scenarios passed");
            ExitCode::SUCCESS
        }
        Err(message) => {
            println!("LIVE_PROBE: FAILED: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &Args) -> Result<(), String> {
    // Short, so the host's socket path fits the 104-byte limit, and outside
    // Claude Code's own temporary root, where it would not load the session
    // directory's settings.
    let root = PathBuf::from(format!("/tmp/bcl-{}", std::process::id()));
    let session_dir = root.join("session");
    std::fs::create_dir_all(&session_dir).map_err(|err| err.to_string())?;
    set_private(&root)?;
    let generation = GenerationStore::new(root.join("chat"))
        .create()
        .map_err(|err| err.to_string())?;
    let socket = root.join("h.sock");

    let mut host = start_host(args, &socket, &generation).await?;
    let host_pid = i32::try_from(host.id().ok_or("the host exited")?).map_err(|err| err.to_string())?;
    let result = scenarios(args, &socket, host_pid, &generation, &session_dir).await;
    report_journal(&generation);
    let _ = host.kill().await;
    let _ = std::fs::remove_dir_all(&root);
    result
}

async fn scenarios(
    args: &Args,
    socket: &Path,
    host_pid: i32,
    generation: &Generation,
    session_dir: &Path,
) -> Result<(), String> {
    // What the coordinator lifecycle will do: set the session up on a
    // connection of its own, with the extension capabilities, the model as a
    // session option, and `default` mode so that a tool asks for permission.
    let mut setup = RawClient::connect(socket).await?;
    setup
        .request(
            1,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                    "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["backgroundSubagents", "asyncTasks"]}}},
                },
            }),
        )
        .await?;
    let created = setup
        .request(2, "session/new", json!({"cwd": session_dir, "mcpServers": []}))
        .await?;
    let session_id = created["sessionId"]
        .as_str()
        .ok_or("session/new returned no session id")?
        .to_owned();
    setup
        .request(
            3,
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "model", "value": args.model}),
        )
        .await?;
    setup
        .request(
            4,
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": "default"}),
        )
        .await?;
    drop(setup);
    println!("LIVE_PROBE: session {session_id} ready on model {}", args.model);

    let config = || ServiceConfig {
        generation: generation.clone(),
        endpoint: HostEndpoint {
            socket_path: socket.to_path_buf(),
            peer_pid: host_pid,
        },
        redactor: CredentialRedactor::none(),
        tuning: ServiceTuning::default(),
    };

    // Scenario 1: restart mid-turn with a message queued behind it.
    let first = ChatService::start(config()).await.map_err(|err| err.to_string())?;
    wait_live(&first).await?;
    first
        .send_operator_message("Reply with exactly the word: first".to_owned(), "cm-1".to_owned())
        .await
        .map_err(|err| err.to_string())?;
    wait(&first, "the first prompt to reach the agent", |f| {
        !texts(f, CoordinatorChatMessageRole::Operator).is_empty()
    })
    .await?;
    first
        .send_operator_message("Reply with exactly the word: second".to_owned(), "cm-2".to_owned())
        .await
        .map_err(|err| err.to_string())?;
    let before = first.fetch(0).await.map_err(|err| err.to_string())?;
    if turns(&before) != 0 {
        return Err("the first turn ended before the restart; the scenario did not test recovery".to_owned());
    }
    first.shutdown().await;
    println!("LIVE_PROBE: scenario 1: engine restarted with turn 1 in flight and message 2 queued");

    let second = ChatService::start(config()).await.map_err(|err| err.to_string())?;
    let done = wait(&second, "both turns to end", |f| turns(f) == 2).await?;
    let operator = texts(&done, CoordinatorChatMessageRole::Operator);
    let coordinator = texts(&done, CoordinatorChatMessageRole::Coordinator);
    println!("LIVE_PROBE: scenario 1: operator {operator:?} coordinator {coordinator:?}");
    if operator
        != [
            "Reply with exactly the word: first",
            "Reply with exactly the word: second",
        ]
    {
        return Err(format!("unexpected operator messages {operator:?}"));
    }
    if coordinator.len() != 2
        || !coordinator[0].to_lowercase().contains("first")
        || !coordinator[1].to_lowercase().contains("second")
    {
        return Err(format!("the answers do not match the prompts: {coordinator:?}"));
    }
    if done.state.phase != CoordinatorChatConnectionPhase::Idle {
        return Err(format!("not idle after both turns: {:?}", done.state.phase));
    }

    // Scenario 2: restart while the agent waits on a permission request.
    let marker = session_dir.join("live-probe-marker.txt");
    second
        .send_operator_message(
            "Run exactly this shell command and nothing else: touch live-probe-marker.txt".to_owned(),
            "cm-3".to_owned(),
        )
        .await
        .map_err(|err| err.to_string())?;
    let waiting = wait(&second, "a permission request", |f| pending_permission(f).is_some()).await?;
    let request_id = pending_permission(&waiting).ok_or("no permission request")?;
    println!("LIVE_PROBE: scenario 2: the agent's permission request id is {request_id:?}");
    second.shutdown().await;
    let third = ChatService::start(config()).await.map_err(|err| err.to_string())?;
    let recovered = third.fetch(0).await.map_err(|err| err.to_string())?;
    if pending_permission(&recovered).as_deref() != Some(request_id.as_str()) {
        return Err("the restarted service did not recover the pending permission request".to_owned());
    }
    wait_live(&third).await?;
    third
        .answer_permission(&request_id, "allow-once")
        .await
        .map_err(|err| format!("answering the recovered permission request: {err}"))?;
    let finished = wait(&third, "the third turn to end", |f| turns(f) == 3).await?;
    let selected = finished.items.iter().any(|item| {
        matches!(
            &item.kind,
            CoordinatorChatItemKind::PermissionRequest(request)
                if matches!(&request.resolution, CoordinatorChatPermissionResolution::Selected { option_id, .. } if option_id == "allow-once")
        )
    });
    if !selected {
        return Err("the permission request did not resolve as selected".to_owned());
    }
    if !marker.exists() {
        return Err("the agent did not run the command that was allowed".to_owned());
    }
    println!("LIVE_PROBE: scenario 2: the command ran after the recovered request was answered");
    third.shutdown().await;
    Ok(())
}

// ---- helpers ---------------------------------------------------------------

fn set_private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|err| err.to_string())
}

async fn start_host(args: &Args, socket: &Path, generation: &Generation) -> Result<Child, String> {
    let mut command = Command::new(&args.host);
    command
        .arg("--socket")
        .arg(socket)
        .arg("--journal")
        .arg(generation.wire_path())
        .arg("--")
        .arg(&args.adapter)
        // The login is the plan's, as the coordinator's is.
        .env_remove("ANTHROPIC_API_KEY")
        .env("CLAUDE_CODE_EXECUTABLE", &args.claude)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    let host = command.spawn().map_err(|err| format!("starting the host: {err}"))?;
    let deadline = Instant::now() + STEP;
    while UnixStream::connect(socket).await.is_err() {
        if Instant::now() > deadline {
            return Err("the host did not start listening".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(host)
}

/// A client that speaks the host's wire contract directly.
struct RawClient {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl RawClient {
    async fn connect(socket: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(socket).await.map_err(|err| err.to_string())?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{}\n", Handshake { replay_from: 1 }.line()).as_bytes())
            .await
            .map_err(|err| err.to_string())?;
        Ok(Self {
            lines: BufReader::new(read).lines(),
            write,
        })
    }

    /// Sends a request and returns its result.
    async fn request(&mut self, id: i64, method: &str, params: Value) -> Result<Value, String> {
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        self.write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|err| err.to_string())?;
        tokio::time::timeout(STEP, async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .map_err(|err| err.to_string())?
                    .ok_or("the host closed the connection")?;
                let record: WireRecord = serde_json::from_str(&line).map_err(|err| err.to_string())?;
                let Ok(message) = serde_json::from_str::<Value>(&record.line) else {
                    continue;
                };
                if record.dir == Direction::FromAgent && message["id"] == json!(id) && message.get("method").is_none() {
                    return match message.get("result") {
                        Some(result) => Ok(result.clone()),
                        None => Err(format!("{method} failed: {}", message["error"])),
                    };
                }
            }
        })
        .await
        .map_err(|_| format!("timed out waiting for {method}"))?
    }
}

async fn wait_live(service: &ChatService) -> Result<(), String> {
    let mut status = service.status();
    tokio::time::timeout(STEP, status.wait_for(|status| *status == ConnectionStatus::Live))
        .await
        .map_err(|_| {
            format!(
                "timed out waiting for the service to be live; it is {:?}",
                *service.status().borrow()
            )
        })?
        .map(|_| ())
        .map_err(|err| err.to_string())
}

async fn wait(
    service: &ChatService,
    what: &str,
    ready: impl Fn(&CoordinatorChatFetchOutcome) -> bool,
) -> Result<Box<CoordinatorChatFetchOutcome>, String> {
    tokio::time::timeout(STEP, async {
        loop {
            let fetched = service.fetch(0).await.map_err(|err| err.to_string())?;
            if ready(&fetched) {
                return Ok(fetched);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for {what}"))?
}

fn texts(fetched: &CoordinatorChatFetchOutcome, role: CoordinatorChatMessageRole) -> Vec<&str> {
    fetched
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            CoordinatorChatItemKind::Message(message) if message.role == role => Some(message.text.as_str()),
            _ => None,
        })
        .collect()
}

fn turns(fetched: &CoordinatorChatFetchOutcome) -> usize {
    fetched
        .items
        .iter()
        .filter(|item| matches!(item.kind, CoordinatorChatItemKind::TurnBoundary(_)))
        .count()
}

fn pending_permission(fetched: &CoordinatorChatFetchOutcome) -> Option<String> {
    fetched.items.iter().find_map(|item| match &item.kind {
        CoordinatorChatItemKind::PermissionRequest(request)
            if request.resolution == CoordinatorChatPermissionResolution::Pending =>
        {
            Some(request.request_id.clone())
        }
        _ => None,
    })
}

/// Prints what crossed the wire in each direction, without the content.
fn report_journal(generation: &Generation) {
    let Ok(file) = std::fs::File::open(generation.wire_path()) else {
        return;
    };
    let Ok(records) = read_records(std::io::BufReader::new(file)) else {
        return;
    };
    for record in &records {
        let Ok(message) = serde_json::from_str::<Value>(&record.line) else {
            continue;
        };
        let kind = match (message.get("method"), message.get("id")) {
            (Some(method), Some(id)) => format!("request {id} {method}"),
            (Some(method), None) if method != "session/update" => format!("notification {method}"),
            (None, Some(id)) => format!("response {id}"),
            _ => continue,
        };
        println!("LIVE_PROBE: journal {:>4} {:?} {kind}", record.seq, record.dir);
    }
}
