//! The chat service end to end: the real `boss-agent-host` binary relaying a
//! scripted child, the service attached through the host's socket.
//!
//! The scripted child replays a recorded journal. A `from_agent` step is a
//! line the agent says on its own; a `to_agent` step is a gate: the child
//! waits for exactly that line from the client, so each test says what the
//! service must write and in which order, and the agent only moves on when it
//! does. A line that does not match fails the child (exit code 3), which
//! stops the host and so the test.
//!
//! Paths to the helper binaries come from the environment because Bazel
//! stages them in the test's runfiles (see this package's `BUILD.bazel`).

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use boss_acp_client::schema::{
    CancelNotification, ContentBlock, PromptRequest, RequestId, RequestPermissionOutcome, RequestPermissionResponse,
    SelectedPermissionOutcome, TextContent,
};
use boss_agent_host::Handshake;
use boss_agent_host::journal::{Direction, WireRecord};
use boss_engine_coordinator_chat::{
    ChatBackend, ChatError, ChatPush, ChatService, ConnectionStatus, CredentialRedactor, Generation, GenerationStore,
    HostEndpoint, IntentLog, IntentRecordKind, PromptOrigin, REDACTED_PLACEHOLDER, ServiceConfig, ServiceTuning,
    WithdrawReason, read_intent_file,
};
use boss_protocol::{
    CoordinatorChatConnectionPhase, CoordinatorChatFetchOutcome, CoordinatorChatItemKind, CoordinatorChatMessageItem,
    CoordinatorChatMessageRole, CoordinatorChatOperation, CoordinatorChatOutcome, CoordinatorChatPermissionRequestItem,
    CoordinatorChatPermissionResolution, CoordinatorChatToolStatus, CoordinatorChatTopicUpdate, TopicEventPayload,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// The generation's write credential. It appears in the agent's output, in a
/// tool's input and result, and in an engine prompt; none of it may leave.
const CREDENTIAL: &str = "wsk-9f8e7d6c5b4a3210";

const SESSION: &str = "sess-1";

fn helper(var: &str) -> PathBuf {
    PathBuf::from(std::env::var(var).unwrap_or_else(|_| panic!("{var} is not set; run this under bazel test")))
}

// ---- the script ---------------------------------------------------------------

fn script_record(dir: Direction, line: String) -> String {
    serde_json::to_string(&WireRecord {
        seq: 0,
        ts_ms: 0,
        dir,
        line,
        invalid: None,
    })
    .unwrap()
}

/// A line the agent says.
fn says(line: String) -> String {
    script_record(Direction::FromAgent, line)
}

/// A line the agent waits for from the client.
fn expects(line: String) -> String {
    script_record(Direction::ToAgent, line)
}

fn rpc(value: Value) -> String {
    value.to_string()
}

fn initialize_request() -> String {
    rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}))
}

fn initialize_response() -> String {
    rpc(json!({"jsonrpc": "2.0", "id": 1, "result": {
        "protocolVersion": 1,
        "agentInfo": {"name": "scripted-agent", "version": "1"},
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["asyncTasks", "backgroundSubagents"]}}},
    }}))
}

fn session_new_request() -> String {
    rpc(json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/session", "mcpServers": []}}))
}

fn session_new_response() -> String {
    rpc(json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": SESSION, "modes": {"currentModeId": "auto"}}}))
}

/// The four lines that set a session up. The service does not set sessions
/// up (the coordinator lifecycle does), so [`Harness::establish_session`]
/// plays that part and every script starts with them.
fn session_setup() -> Vec<String> {
    vec![
        expects(initialize_request()),
        says(initialize_response()),
        expects(session_new_request()),
        says(session_new_response()),
    ]
}

fn update(update: Value) -> String {
    rpc(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SESSION, "update": update}}))
}

fn chunk(text: &str) -> String {
    chunk_of("msg-1", text)
}

fn chunk_of(message_id: &str, text: &str) -> String {
    update(json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {"type": "text", "text": text},
        "messageId": message_id,
    }))
}

fn tool_call(id: &str, command: &str) -> String {
    update(json!({
        "sessionUpdate": "tool_call", "toolCallId": id, "title": command, "kind": "execute",
        "status": "pending", "rawInput": {"command": command},
    }))
}

fn tool_result(id: &str, text: &str) -> String {
    update(json!({
        "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
        "content": [{"type": "content", "content": {"type": "text", "text": text}}],
    }))
}

/// The agent asks for permission. The agent numbers its own requests, from
/// 0 and independently of the client.
fn permission_request(agent_request_id: i64, tool_call_id: &str, title: &str) -> String {
    rpc(json!({
        "jsonrpc": "2.0", "id": agent_request_id, "method": "session/request_permission",
        "params": {
            "sessionId": SESSION,
            "toolCall": {"toolCallId": tool_call_id, "title": title, "kind": "execute", "status": "pending",
                         "rawInput": {"command": title}},
            "options": [
                {"optionId": "allow-once", "name": "Yes", "kind": "allow_once"},
                {"optionId": "reject", "name": "No", "kind": "reject_once"},
            ],
        },
    }))
}

fn prompt_response(id: i64, stop_reason: &str) -> String {
    rpc(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": stop_reason}}))
}

/// The `session/prompt` line the service must write. Built from the typed
/// schema, as the service builds it, so only the wire contract (method,
/// id, session, text) is what this pins.
fn prompt_request(id: i64, text: &str) -> String {
    let params = serde_json::to_value(PromptRequest::new(
        SESSION.to_owned(),
        vec![ContentBlock::Text(TextContent::new(text))],
    ))
    .unwrap();
    rpc(json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": params}))
}

fn permission_answer(agent_request_id: i64, outcome: RequestPermissionOutcome) -> String {
    let result = serde_json::to_value(RequestPermissionResponse::new(outcome)).unwrap();
    rpc(json!({"jsonrpc": "2.0", "id": agent_request_id, "result": result}))
}

fn permission_selected(agent_request_id: i64, option_id: &str) -> String {
    permission_answer(
        agent_request_id,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id.to_owned())),
    )
}

fn permission_cancelled(agent_request_id: i64) -> String {
    permission_answer(agent_request_id, RequestPermissionOutcome::Cancelled)
}

fn cancel_notification() -> String {
    let params = serde_json::to_value(CancelNotification::new(SESSION.to_owned())).unwrap();
    rpc(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": params}))
}

// ---- the host -------------------------------------------------------------------

/// A real agent host over a scripted child, with a generation directory the
/// host's journal is written into.
struct Harness {
    dir: tempfile::TempDir,
    generation: Generation,
    host: Child,
    socket: PathBuf,
}

impl Harness {
    async fn start(script: &[String], child_args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // The service refuses a socket in a directory others can enter, and a
        // temporary directory is not necessarily private.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = GenerationStore::new(dir.path().join("chat"));
        let generation = store.create().unwrap();
        let script_path = dir.path().join("script.jsonl");
        std::fs::write(&script_path, script.join("\n") + "\n").unwrap();
        let socket = dir.path().join("h.sock");
        let host = Command::new(helper("BOSS_AGENT_HOST_TEST_BIN"))
            .arg("--socket")
            .arg(&socket)
            .arg("--journal")
            .arg(generation.wire_path())
            .arg("--")
            .arg(helper("BOSS_AGENT_HOST_TEST_CHILD"))
            .arg(&script_path)
            .args(child_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::timeout(STEP_TIMEOUT, async {
            while UnixStream::connect(&socket).await.is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the host starts listening");
        Self {
            dir,
            generation,
            host,
            socket,
        }
    }

    fn host_pid(&self) -> i32 {
        i32::try_from(self.host.id().expect("the host is running")).unwrap()
    }

    fn config(&self) -> ServiceConfig {
        self.config_with(self.host_pid(), tuning())
    }

    fn config_with(&self, peer_pid: i32, tuning: ServiceTuning) -> ServiceConfig {
        ServiceConfig {
            generation: self.generation.clone(),
            endpoint: HostEndpoint {
                socket_path: self.socket.clone(),
                peer_pid,
            },
            redactor: CredentialRedactor::new(CREDENTIAL),
            tuning,
        }
    }

    async fn start_service(&self) -> ChatService {
        ChatService::start(self.config()).await.unwrap()
    }

    /// Plays the coordinator lifecycle: sends `initialize` and `session/new`
    /// over a connection of its own, which it closes once both are answered.
    async fn establish_session(&self) {
        let mut client = RawClient::connect(&self.socket, 1).await;
        client.send(&initialize_request()).await;
        client.wait_for_response(1).await;
        client.send(&session_new_request()).await;
        client.wait_for_response(2).await;
    }

    fn intent_log(&self) -> Vec<IntentRecordKind> {
        read_intent_file(&self.generation.intent_path())
            .unwrap()
            .into_iter()
            .map(|record| record.kind)
            .collect()
    }
}

fn tuning() -> ServiceTuning {
    ServiceTuning {
        delta_publish_interval: Duration::from_millis(20),
        snapshot_interval_records: 500,
        reconnect_initial_delay: Duration::from_millis(20),
        reconnect_max_delay: Duration::from_millis(100),
    }
}

/// A client that speaks the host's wire contract directly, without any of
/// the service: the part of the coordinator lifecycle that sets a session up,
/// and, by connecting, a way to displace the service's own connection.
struct RawClient {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl RawClient {
    async fn connect(socket: &std::path::Path, replay_from: u64) -> Self {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{}\n", Handshake { replay_from }.line()).as_bytes())
            .await
            .unwrap();
        Self {
            lines: BufReader::new(read).lines(),
            write,
        }
    }

    async fn send(&mut self, line: &str) {
        self.write.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    }

    async fn wait_for_response(&mut self, id: i64) {
        tokio::time::timeout(STEP_TIMEOUT, async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("the host closed the connection");
                let record: WireRecord = serde_json::from_str(&line).unwrap();
                let message: Value = serde_json::from_str(&record.line).unwrap();
                if record.dir == Direction::FromAgent && message["id"] == json!(id) && message.get("result").is_some() {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for the response to request {id}"));
    }
}

// ---- watching the service -----------------------------------------------------

/// Everything the service published, in order.
#[derive(Clone)]
struct Recorder(Arc<Mutex<Vec<ChatPush>>>);

impl Recorder {
    fn attach(service: &ChatService) -> Self {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut pushes = service.subscribe();
        let sink = recorded.clone();
        tokio::spawn(async move {
            loop {
                match pushes.recv().await {
                    Ok(push) => sink.lock().unwrap().push(push),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Self(recorded)
    }

    fn all(&self) -> Vec<ChatPush> {
        self.0.lock().unwrap().clone()
    }

    /// Everything published, as the app would receive it on the wire.
    fn wire_text(&self) -> String {
        self.all()
            .into_iter()
            .map(|push| serde_json::to_string(&TopicEventPayload::from(push)).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

async fn wait_fetch(
    service: &ChatService,
    what: &str,
    ready: impl Fn(&CoordinatorChatFetchOutcome) -> bool,
) -> Box<CoordinatorChatFetchOutcome> {
    tokio::time::timeout(STEP_TIMEOUT, async {
        loop {
            let fetched = service.fetch(0).await.unwrap();
            if ready(&fetched) {
                return fetched;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn wait_status(service: &ChatService, what: &str, ready: impl Fn(&ConnectionStatus) -> bool) {
    let mut status = service.status();
    tokio::time::timeout(STEP_TIMEOUT, status.wait_for(|status| ready(status)))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {what}; the status is {:?}",
                *service.status().borrow()
            )
        })
        .unwrap();
}

async fn wait_live(service: &ChatService) {
    wait_status(service, "the service to be live", |status| {
        *status == ConnectionStatus::Live
    })
    .await;
}

fn messages(
    fetched: &CoordinatorChatFetchOutcome,
    role: CoordinatorChatMessageRole,
) -> Vec<&CoordinatorChatMessageItem> {
    fetched
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            CoordinatorChatItemKind::Message(message) if message.role == role => Some(message),
            _ => None,
        })
        .collect()
}

fn texts(fetched: &CoordinatorChatFetchOutcome, role: CoordinatorChatMessageRole) -> Vec<&str> {
    messages(fetched, role)
        .into_iter()
        .map(|message| message.text.as_str())
        .collect()
}

fn pending_permission(fetched: &CoordinatorChatFetchOutcome) -> Option<&CoordinatorChatPermissionRequestItem> {
    fetched.items.iter().find_map(|item| match &item.kind {
        CoordinatorChatItemKind::PermissionRequest(request)
            if request.resolution == CoordinatorChatPermissionResolution::Pending =>
        {
            Some(request)
        }
        _ => None,
    })
}

fn stop_reasons(fetched: &CoordinatorChatFetchOutcome) -> Vec<&str> {
    fetched
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            CoordinatorChatItemKind::TurnBoundary(boundary) => boundary.stop_reason.as_deref(),
            _ => None,
        })
        .collect()
}

fn turns_ended(fetched: &CoordinatorChatFetchOutcome) -> usize {
    stop_reasons(fetched).len()
}

// ---- tests --------------------------------------------------------------------

#[tokio::test]
async fn a_message_streams_a_permission_is_answered_and_the_credential_never_leaves() {
    let mut script = session_setup();
    script.extend([
        expects(prompt_request(3, "status please")),
        // The credential is split across two chunks.
        says(chunk("Checking. The key is wsk-9f8e7d")),
        says(chunk("6c5b4a3210 so keep it quiet. ")),
        says(tool_call(
            "toolu_1",
            &format!("boss handoff write --authorization {CREDENTIAL} -"),
        )),
        // The agent's request 3 and the client's request 3 (this prompt) are
        // unrelated. The answer must go out under 3 and be taken for the
        // agent's request, not mistaken for anything to do with the prompt.
        says(permission_request(3, "toolu_1", "boss handoff write")),
        expects(permission_selected(3, "allow-once")),
        says(tool_result("toolu_1", &format!("```text\nused {CREDENTIAL}\n```"))),
        says(chunk_of("msg-2", "All done.")),
        says(prompt_response(3, "end_turn")),
    ]);
    let harness = Harness::start(&script, &[]).await;
    harness.establish_session().await;
    let service = harness.start_service().await;
    let recorder = Recorder::attach(&service);
    wait_live(&service).await;

    // Things the service refuses before it queues anything.
    for (text, id) in [("   ", "cm-empty"), ("hello", "")] {
        let refused = service.send_operator_message(text.to_owned(), id.to_owned()).await;
        assert!(
            matches!(refused, Err(ChatError::Rejected(_))),
            "{text:?}/{id:?}: {refused:?}"
        );
    }

    service
        .send_operator_message("status please".to_owned(), "cm-1".to_owned())
        .await
        .unwrap();
    // A retry of an accepted send is answered, not queued again (the script
    // has one prompt: a second would fail the agent).
    service
        .send_operator_message("status please".to_owned(), "cm-1".to_owned())
        .await
        .unwrap();

    let waiting = wait_fetch(&service, "the permission request", |fetched| {
        pending_permission(fetched).is_some()
    })
    .await;
    assert_eq!(
        waiting.state.phase,
        CoordinatorChatConnectionPhase::WaitingForPermission
    );
    let request = pending_permission(&waiting).unwrap();
    assert_eq!(request.request_id, "3");
    assert!(!request.title.contains(CREDENTIAL));

    for (id, option) in [("7", "allow-once"), ("3", "not-an-option")] {
        let refused = service.answer_permission(id, option).await;
        assert!(
            matches!(refused, Err(ChatError::Rejected(_))),
            "{id}/{option}: {refused:?}"
        );
    }
    service.answer_permission("3", "allow-once").await.unwrap();
    let again = service.answer_permission("3", "allow-once").await;
    assert!(
        matches!(again, Err(ChatError::Rejected(_))),
        "answered twice: {again:?}"
    );

    let done = wait_fetch(&service, "the turn to end", |fetched| turns_ended(fetched) == 1).await;
    assert_eq!(stop_reasons(&done), ["end_turn"]);
    assert_eq!(done.state.phase, CoordinatorChatConnectionPhase::Idle);

    // The operator's message is in the conversation as the operator's.
    let operator = messages(&done, CoordinatorChatMessageRole::Operator);
    assert_eq!(operator.len(), 1);
    assert_eq!(operator[0].text, "status please");
    assert_eq!(operator[0].client_message_id.as_deref(), Some("cm-1"));
    // The coordinator's answer is whole, with the credential gone from the
    // middle of a word that arrived in two pieces.
    let answer = texts(&done, CoordinatorChatMessageRole::Coordinator).join("");
    assert_eq!(
        answer,
        format!("Checking. The key is {REDACTED_PLACEHOLDER} so keep it quiet. All done.")
    );
    // The permission request resolved with the option chosen.
    assert!(done.items.iter().any(|item| matches!(
        &item.kind,
        CoordinatorChatItemKind::PermissionRequest(request)
            if matches!(&request.resolution, CoordinatorChatPermissionResolution::Selected { option_id, .. } if option_id == "allow-once")
    )));
    assert!(done.items.iter().any(|item| matches!(
        &item.kind,
        CoordinatorChatItemKind::ToolCall(call) if call.status == CoordinatorChatToolStatus::Completed
    )));

    // Streamed text went out as deltas that carry their offsets and add up to
    // the message they belong to. Wait for the last delta to be published.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let published = recorder.all();
    let mut by_item: Vec<(String, usize, String)> = Vec::new();
    for push in &published {
        if let CoordinatorChatTopicUpdate::Delta { delta } = &push.update {
            match by_item.iter_mut().find(|(id, _, _)| *id == delta.item_id) {
                Some((_, next, text)) => {
                    assert_eq!(delta.offset, *next, "a delta continues where the one before it ended");
                    *next += delta.text.len();
                    text.push_str(&delta.text);
                }
                None => {
                    assert_eq!(delta.offset, 0);
                    by_item.push((delta.item_id.clone(), delta.text.len(), delta.text.clone()));
                }
            }
        }
    }
    assert!(!by_item.is_empty(), "the answer was streamed");
    for (item_id, _, streamed) in &by_item {
        let item = done
            .items
            .iter()
            .find(|item| item.id == *item_id)
            .expect("a streamed message ends as an item");
        let CoordinatorChatItemKind::Message(message) = &item.kind else {
            panic!("{item_id} is not a message");
        };
        assert!(
            message.text.starts_with(streamed.as_str()),
            "{streamed:?} is a prefix of {:?}",
            message.text
        );
    }
    assert!(published.iter().all(|push| push.generation == service.generation_id()));

    // Nothing the app was sent or can fetch holds the credential, or the half
    // of it that one chunk carried.
    let wire = recorder.wire_text();
    let fetched = serde_json::to_string(&service.fetch(0).await.unwrap()).unwrap();
    for text in [&wire, &fetched] {
        assert!(!text.contains(CREDENTIAL), "the credential leaked: {text}");
        assert!(!text.contains("wsk-9f8e7d"), "part of the credential leaked: {text}");
    }
    assert!(wire.contains(REDACTED_PLACEHOLDER));

    // The send was accepted into the intent log once, and logged as sent.
    let log = harness.intent_log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(matches!(
        &log[0],
        IntentRecordKind::Queued {
            origin: PromptOrigin::Operator,
            ..
        }
    ));
    assert!(matches!(&log[1], IntentRecordKind::Sent { request_id, .. } if *request_id == RequestId::Number(3)));
    service.shutdown().await;
}

#[tokio::test]
async fn stop_answers_the_permission_cancels_the_turn_and_returns_queued_messages() {
    let mut script = session_setup();
    script.extend([
        expects(prompt_request(3, "do the long thing")),
        says(chunk("Starting. ")),
        says(tool_call("toolu_1", "touch x")),
        says(permission_request(0, "toolu_1", "touch x")),
        // Stop: pending permission requests are answered as cancelled first.
        expects(permission_cancelled(0)),
        expects(cancel_notification()),
        says(prompt_response(3, "cancelled")),
        // The engine's prompt was queued and still runs. The operator's was
        // returned to the composer and never sent.
        expects(prompt_request(4, "engine nudge")),
        says(chunk_of("msg-2", "Engine ack.")),
        says(prompt_response(4, "end_turn")),
        // The same draft, sent again under the same client message id.
        expects(prompt_request(5, "second draft")),
        says(chunk_of("msg-3", "Draft ack.")),
        says(prompt_response(5, "end_turn")),
    ]);
    let harness = Harness::start(&script, &[]).await;
    harness.establish_session().await;
    let service = harness.start_service().await;
    wait_live(&service).await;

    service
        .send_operator_message("do the long thing".to_owned(), "cm-a".to_owned())
        .await
        .unwrap();
    wait_fetch(&service, "the permission request", |fetched| {
        pending_permission(fetched).is_some()
    })
    .await;
    service
        .send_operator_message("second draft".to_owned(), "cm-b".to_owned())
        .await
        .unwrap();
    service.queue_engine_prompt("engine nudge".to_owned()).await.unwrap();

    let stopped = service.cancel().await.unwrap();
    assert!(stopped.cancelled_turn);
    assert_eq!(stopped.returned_to_composer.len(), 1);
    assert_eq!(
        stopped.returned_to_composer[0].client_message_id.as_deref(),
        Some("cm-b")
    );
    assert_eq!(stopped.returned_to_composer[0].text, "second draft");
    let duplicate = service.answer_permission("0", "allow-once").await;
    assert!(
        matches!(duplicate, Err(ChatError::Rejected(_))),
        "Stop already answered the permission: {duplicate:?}"
    );

    let engine_turn = wait_fetch(&service, "the engine's prompt to run", |fetched| {
        turns_ended(fetched) == 2
    })
    .await;
    assert_eq!(stop_reasons(&engine_turn), ["cancelled", "end_turn"]);
    assert_eq!(
        texts(&engine_turn, CoordinatorChatMessageRole::Engine),
        ["engine nudge"]
    );
    assert!(
        !texts(&engine_turn, CoordinatorChatMessageRole::Operator).contains(&"second draft"),
        "the returned draft was not sent"
    );
    assert!(engine_turn.items.iter().any(|item| matches!(
        &item.kind,
        CoordinatorChatItemKind::PermissionRequest(request)
            if request.resolution == CoordinatorChatPermissionResolution::Cancelled { withdrawn: false }
    )));

    // The composer sends the draft again under the id it already had.
    service
        .send_operator_message("second draft".to_owned(), "cm-b".to_owned())
        .await
        .unwrap();
    let all = wait_fetch(&service, "the draft to run", |fetched| turns_ended(fetched) == 3).await;
    assert_eq!(
        texts(&all, CoordinatorChatMessageRole::Operator),
        ["do the long thing", "second draft"]
    );

    // Stop with nothing running does nothing.
    let idle = service.cancel().await.unwrap();
    assert!(!idle.cancelled_turn && idle.returned_to_composer.is_empty());

    // The withdrawal is in the intent log, with its reason.
    assert!(harness.intent_log().iter().any(|kind| matches!(
        kind,
        IntentRecordKind::Withdrawn {
            reason: WithdrawReason::StopReturnedToComposer,
            ..
        }
    )));
    service.shutdown().await;
}

#[tokio::test]
async fn an_engine_prompt_goes_ahead_of_an_operator_message_queued_before_it() {
    let mut script = session_setup();
    script.extend([
        expects(prompt_request(3, "first")),
        says(permission_request(0, "toolu_1", "touch x")),
        expects(permission_selected(0, "allow-once")),
        says(prompt_response(3, "end_turn")),
        expects(prompt_request(4, "engine nudge")),
        says(prompt_response(4, "end_turn")),
        expects(prompt_request(5, "operator later")),
        says(prompt_response(5, "end_turn")),
    ]);
    let harness = Harness::start(&script, &[]).await;
    harness.establish_session().await;
    let service = harness.start_service().await;
    wait_live(&service).await;

    service
        .send_operator_message("first".to_owned(), "cm-1".to_owned())
        .await
        .unwrap();
    wait_fetch(&service, "the permission request", |fetched| {
        pending_permission(fetched).is_some()
    })
    .await;
    // Queued in this order during the turn; sent in the other.
    service
        .send_operator_message("operator later".to_owned(), "cm-2".to_owned())
        .await
        .unwrap();
    service.queue_engine_prompt("engine nudge".to_owned()).await.unwrap();
    service.answer_permission("0", "allow-once").await.unwrap();

    let all = wait_fetch(&service, "three turns", |fetched| turns_ended(fetched) == 3).await;
    assert_eq!(texts(&all, CoordinatorChatMessageRole::Engine), ["engine nudge"]);
    assert_eq!(
        texts(&all, CoordinatorChatMessageRole::Operator),
        ["first", "operator later"]
    );
    service.shutdown().await;
}

#[tokio::test]
async fn a_fetch_after_a_dropped_push_returns_what_changed_since() {
    let mut script = session_setup();
    script.push(expects(prompt_request(3, "tell me a story")));
    for word in 1..=20 {
        script.push(says(chunk(&format!("word{word:02} "))));
    }
    script.extend([
        says(tool_call("toolu_1", "boss task list")),
        says(tool_result("toolu_1", "T1 running")),
        says(chunk_of("msg-2", "The end.")),
        says(prompt_response(3, "end_turn")),
    ]);
    // The agent speaks every 15 ms, so a fetch can land mid-answer.
    let harness = Harness::start(&script, &["--delay-ms", "15"]).await;
    harness.establish_session().await;
    let service = harness.start_service().await;
    let recorder = Recorder::attach(&service);
    wait_live(&service).await;

    service
        .send_operator_message("tell me a story".to_owned(), "cm-1".to_owned())
        .await
        .unwrap();

    // Mid-answer, a fetch shows the message so far, as the safe text.
    let mid = wait_fetch(&service, "a message in progress", |fetched| {
        fetched
            .in_progress
            .as_ref()
            .is_some_and(|progress| progress.text.len() > 20)
    })
    .await;
    let in_progress = mid.in_progress.unwrap();
    assert!(in_progress.text.starts_with("word01 word02 "), "{:?}", in_progress.text);

    // The app saw the tool call start and then lost every push after it.
    let seen = tokio::time::timeout(STEP_TIMEOUT, async {
        loop {
            let tool_started = recorder.all().into_iter().find_map(|push| match push.update {
                CoordinatorChatTopicUpdate::Item { item }
                    if matches!(item.kind, CoordinatorChatItemKind::ToolCall(_)) =>
                {
                    Some(item.seq)
                }
                _ => None,
            });
            if let Some(seq) = tool_started {
                return seq;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the tool call was published");
    wait_fetch(&service, "the turn to end", |fetched| turns_ended(fetched) == 1).await;

    let recovered = service.fetch(seen).await.unwrap();
    let tool = recovered
        .items
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::ToolCall(call) => Some((item, call)),
            _ => None,
        })
        .expect("a tool call that finished after the app last heard of it is news");
    assert_eq!(tool.1.status, CoordinatorChatToolStatus::Completed);
    assert!(
        tool.0.seq <= seen && tool.0.updated_seq > seen,
        "created before, changed after"
    );
    assert_eq!(stop_reasons(&recovered), ["end_turn"]);
    assert!(
        texts(&recovered, CoordinatorChatMessageRole::Coordinator)
            .join("")
            .contains("The end.")
    );
    assert!(
        messages(&recovered, CoordinatorChatMessageRole::Operator).is_empty(),
        "the operator's message was before the cut"
    );
    assert_eq!(recovered.last_seq, service.fetch(0).await.unwrap().last_seq);
    assert!(recovered.in_progress.is_none());

    // And a fetch that is up to date has nothing to add.
    let nothing = service.fetch(recovered.last_seq).await.unwrap();
    assert!(nothing.items.is_empty());
    service.shutdown().await;
}

async fn restart_with_a_queued_message(delete_snapshot: bool) {
    let mut script = session_setup();
    script.extend([
        expects(prompt_request(3, "first")),
        says(chunk("Working. ")),
        says(tool_call("toolu_1", "touch x")),
        says(permission_request(0, "toolu_1", "touch x")),
        expects(permission_selected(0, "allow-once")),
        says(tool_result("toolu_1", "ok")),
        says(chunk_of("msg-2", "Finished the first.")),
        says(prompt_response(3, "end_turn")),
        expects(prompt_request(4, "second")),
        says(chunk_of("msg-3", "Second answer.")),
        says(prompt_response(4, "end_turn")),
    ]);
    let harness = Harness::start(&script, &[]).await;
    harness.establish_session().await;

    // The first engine: a turn is in flight, waiting on a permission request,
    // and a second message is queued behind it.
    let first = harness.start_service().await;
    wait_live(&first).await;
    first
        .send_operator_message("first".to_owned(), "cm-1".to_owned())
        .await
        .unwrap();
    wait_fetch(&first, "the permission request", |fetched| {
        pending_permission(fetched).is_some()
    })
    .await;
    first
        .send_operator_message("second".to_owned(), "cm-2".to_owned())
        .await
        .unwrap();
    first.shutdown().await;
    assert!(
        harness.generation.snapshot_path().exists(),
        "a clean stop leaves a snapshot"
    );
    if delete_snapshot {
        // As after a crash, which leaves none: recovery replays from the start.
        std::fs::remove_file(harness.generation.snapshot_path()).unwrap();
    }
    assert!(
        matches!(
            first.send_operator_message("late".to_owned(), "cm-3".to_owned()).await,
            Err(ChatError::NotRunning(_))
        ),
        "a stopped service accepts nothing"
    );

    // The second engine finds the turn, the permission request and the queued
    // message where the first left them.
    let second = harness.start_service().await;
    let before = second.fetch(0).await.unwrap();
    assert_eq!(
        pending_permission(&before).map(|request| request.request_id.as_str()),
        Some("0")
    );
    assert_eq!(
        texts(&before, CoordinatorChatMessageRole::Operator),
        ["first"],
        "the queued one is not sent yet"
    );
    wait_live(&second).await;
    second.answer_permission("0", "allow-once").await.unwrap();

    let done = wait_fetch(&second, "both turns", |fetched| turns_ended(fetched) == 2).await;
    assert_eq!(texts(&done, CoordinatorChatMessageRole::Operator), ["first", "second"]);
    let coordinator = texts(&done, CoordinatorChatMessageRole::Coordinator).join("|");
    assert!(
        coordinator.contains("Finished the first.") && coordinator.contains("Second answer."),
        "{coordinator}"
    );
    assert_eq!(
        messages(&done, CoordinatorChatMessageRole::Operator)[1]
            .client_message_id
            .as_deref(),
        Some("cm-2")
    );
    assert_eq!(done.state.phase, CoordinatorChatConnectionPhase::Idle);
    second.shutdown().await;
}

#[tokio::test]
async fn a_restart_with_a_snapshot_keeps_the_turn_the_permission_and_the_queued_message() {
    restart_with_a_queued_message(false).await;
}

#[tokio::test]
async fn a_restart_with_no_snapshot_replays_from_the_start_and_keeps_the_queued_message() {
    restart_with_a_queued_message(true).await;
}

#[tokio::test]
async fn a_reconnect_replays_what_was_missed_and_accepts_messages_meanwhile() {
    let mut script = session_setup();
    script.push(expects(prompt_request(3, "go")));
    for word in [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ] {
        script.push(says(chunk(&format!("{word} "))));
    }
    script.extend([
        says(prompt_response(3, "end_turn")),
        expects(prompt_request(4, "after")),
        says(chunk_of("msg-2", "after ok")),
        says(prompt_response(4, "end_turn")),
    ]);
    // One line every 100 ms: a gap of 200 ms or more misses at least one.
    let harness = Harness::start(&script, &["--delay-ms", "100"]).await;
    harness.establish_session().await;
    let slow_to_return = ServiceTuning {
        reconnect_initial_delay: Duration::from_millis(400),
        reconnect_max_delay: Duration::from_millis(400),
        ..tuning()
    };
    let service = ChatService::start(harness.config_with(harness.host_pid(), slow_to_return))
        .await
        .unwrap();
    wait_live(&service).await;
    service
        .send_operator_message("go".to_owned(), "cm-1".to_owned())
        .await
        .unwrap();
    wait_fetch(&service, "the answer to start", |fetched| fetched.in_progress.is_some()).await;
    let seen_before = service.fetch(0).await.unwrap().last_seq;

    // Another client takes the host's one connection. The host closes the
    // service's, which stays away for the reconnect delay.
    let mut status = service.status();
    let thief = RawClient::connect(&harness.socket, seen_before + 1).await;
    tokio::time::timeout(
        STEP_TIMEOUT,
        status.wait_for(|status| matches!(status, ConnectionStatus::Reconnecting { .. })),
    )
    .await
    .expect("the service notices it was displaced")
    .unwrap();

    // While it is away: a message is accepted (it is in the intent log), but
    // Stop and a permission answer cannot be delivered and change nothing.
    service
        .send_operator_message("after".to_owned(), "cm-2".to_owned())
        .await
        .unwrap();
    assert!(matches!(service.cancel().await, Err(ChatError::Disconnected)));
    assert!(matches!(
        service.answer_permission("0", "allow-once").await,
        Err(ChatError::Disconnected)
    ));
    assert!(harness.intent_log().iter().any(|kind| matches!(
        kind,
        IntentRecordKind::Queued { text, .. } if text == "after"
    )));
    drop(thief);

    wait_live(&service).await;
    let done = wait_fetch(&service, "both turns", |fetched| turns_ended(fetched) == 2).await;
    let coordinator = texts(&done, CoordinatorChatMessageRole::Coordinator);
    assert_eq!(
        coordinator[0], "one two three four five six seven eight nine ten ",
        "every chunk, once and in order, though the service missed some live"
    );
    assert_eq!(coordinator[1], "after ok");
    assert_eq!(texts(&done, CoordinatorChatMessageRole::Operator), ["go", "after"]);
    service.shutdown().await;
}

#[tokio::test]
async fn a_message_is_in_the_intent_log_before_the_send_is_acknowledged() {
    // The agent never gets as far as a session: it waits for a line nobody sends.
    let harness = Harness::start(&[expects("never sent".to_owned())], &[]).await;
    let service = harness.start_service().await;
    wait_live(&service).await;

    service
        .send_operator_message("hello".to_owned(), "cm-1".to_owned())
        .await
        .unwrap();
    let log = harness.intent_log();
    assert!(
        matches!(log.as_slice(), [IntentRecordKind::Queued { text, client_message_id, origin: PromptOrigin::Operator, .. }]
            if text == "hello" && client_message_id.as_deref() == Some("cm-1")),
        "{log:?}"
    );
    // Retrying adds nothing; an engine prompt is logged too.
    service
        .send_operator_message("hello".to_owned(), "cm-1".to_owned())
        .await
        .unwrap();
    assert_eq!(service.queue_engine_prompt("brief".to_owned()).await.unwrap(), 2);
    assert_eq!(harness.intent_log().len(), 2);

    // With no session nothing was sent: the conversation is still empty.
    let fetched = service.fetch(0).await.unwrap();
    assert!(fetched.items.is_empty());
    assert_eq!(fetched.state.session_id, None);
    service.shutdown().await;
}

#[tokio::test]
async fn a_prompt_logged_as_sent_that_never_reached_the_host_is_sent_again() {
    let mut script = session_setup();
    // Request 3 was the abandoned attempt; it is never reused.
    script.extend([
        expects(prompt_request(4, "lost message")),
        says(chunk("Got it.")),
        says(prompt_response(4, "end_turn")),
    ]);
    let harness = Harness::start(&script, &[]).await;
    harness.establish_session().await;
    {
        // What the previous engine left: the message was logged as sent and
        // the engine died before the line reached the host's journal.
        let mut log = IntentLog::open(&harness.generation.intent_path()).unwrap();
        let queued = log
            .append_queued(
                PromptOrigin::Operator,
                "lost message",
                Some("cm-9".to_owned()),
                Vec::new(),
            )
            .unwrap();
        log.append_sent(queued.intent_id(), RequestId::Number(3)).unwrap();
    }
    let service = harness.start_service().await;

    let done = wait_fetch(&service, "the turn to end", |fetched| turns_ended(fetched) == 1).await;
    assert_eq!(texts(&done, CoordinatorChatMessageRole::Operator), ["lost message"]);
    let log = harness.intent_log();
    assert!(
        matches!(
            log.as_slice(),
            [
                IntentRecordKind::Queued { .. },
                IntentRecordKind::Sent { request_id: first, .. },
                IntentRecordKind::Requeued { .. },
                IntentRecordKind::Sent { request_id: second, .. },
            ] if *first == RequestId::Number(3) && *second == RequestId::Number(4)
        ),
        "{log:?}"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn the_rpc_backend_maps_operations_to_outcomes() {
    let mut script = session_setup();
    script.extend([
        expects(prompt_request(3, "via rpc")),
        says(chunk("ok")),
        says(prompt_response(3, "end_turn")),
        // Keeps the agent, and so the host, alive for the rest of the test.
        expects("never sent".to_owned()),
    ]);
    let harness = Harness::start(&script, &[]).await;
    harness.establish_session().await;
    let service = harness.start_service().await;
    wait_live(&service).await;
    let backend: &dyn ChatBackend = &service;

    let sent = backend
        .handle(CoordinatorChatOperation::Send {
            text: "via rpc".to_owned(),
            client_message_id: "cm-rpc".to_owned(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        sent,
        CoordinatorChatOutcome::Sent {
            client_message_id: "cm-rpc".to_owned()
        }
    );
    wait_fetch(&service, "the turn to end", |fetched| turns_ended(fetched) == 1).await;

    // Images are the image entry's: refused, not dropped on the floor.
    let attached = backend
        .handle(CoordinatorChatOperation::Send {
            text: "with a picture".to_owned(),
            client_message_id: "cm-img".to_owned(),
            attachments: vec![boss_protocol::CoordinatorChatSendAttachment {
                path: "/tmp/x.png".to_owned(),
            }],
        })
        .await;
    assert!(matches!(attached, Err(ChatError::Rejected(_))));

    assert_eq!(
        backend.handle(CoordinatorChatOperation::Cancel).await.unwrap(),
        CoordinatorChatOutcome::cancelled(std::iter::empty())
    );
    let fetched = backend
        .handle(CoordinatorChatOperation::Fetch { after_seq: 0 })
        .await
        .unwrap();
    match fetched {
        CoordinatorChatOutcome::Fetch(outcome) => {
            assert_eq!(outcome.generation, service.generation_id());
            assert_eq!(turns_ended(&outcome), 1);
        }
        other => panic!("expected a fetch outcome, got {other:?}"),
    }
    assert!(matches!(
        backend
            .handle(CoordinatorChatOperation::AnswerPermission {
                request_id: "0".to_owned(),
                option_id: "allow-once".to_owned(),
            })
            .await,
        Err(ChatError::Rejected(_))
    ));
    service.shutdown().await;
    assert!(matches!(
        backend.handle(CoordinatorChatOperation::Cancel).await,
        Err(ChatError::NotRunning(_))
    ));
}

#[tokio::test]
async fn a_socket_served_by_the_wrong_process_is_refused() {
    let harness = Harness::start(&[expects("never sent".to_owned())], &[]).await;
    // Not the host's pid.
    let wrong = ChatService::start(harness.config_with(harness.host_pid() + 1, tuning()))
        .await
        .unwrap();
    wait_status(&wrong, "the wrong peer to be refused", |status| {
        matches!(status, ConnectionStatus::Stopped { reason } if reason.contains("not the coordinator pane process"))
    })
    .await;
    let refused = wrong.send_operator_message("hello".to_owned(), "cm-1".to_owned()).await;
    assert!(
        matches!(refused, Err(ChatError::NotRunning(reason)) if reason.contains("not the coordinator pane process"))
    );
    assert!(
        harness.intent_log().is_empty(),
        "nothing was accepted for a host that was refused"
    );
}

#[tokio::test]
async fn stop_through_the_rpc_returns_queued_operator_messages() {
    let mut script = session_setup();
    script.extend([
        expects(prompt_request(3, "running")),
        says(chunk("Working.")),
        says(permission_request(0, "toolu_1", "touch x")),
        expects(permission_cancelled(0)),
        expects(cancel_notification()),
        says(prompt_response(3, "cancelled")),
        expects("never sent".to_owned()),
    ]);
    let harness = Harness::start(&script, &[]).await;
    harness.establish_session().await;
    let service = harness.start_service().await;
    wait_live(&service).await;
    service
        .send_operator_message("running".to_owned(), "cm-run".to_owned())
        .await
        .unwrap();
    wait_fetch(&service, "the permission request", |fetched| {
        pending_permission(fetched).is_some()
    })
    .await;
    service
        .send_operator_message("queued draft".to_owned(), "cm-queued".to_owned())
        .await
        .unwrap();
    let backend: &dyn ChatBackend = &service;
    match backend.handle(CoordinatorChatOperation::Cancel).await.unwrap() {
        CoordinatorChatOutcome::Cancelled { returned_to_composer } => {
            assert_eq!(returned_to_composer.len(), 1);
            assert_eq!(returned_to_composer[0].text, "queued draft");
            assert_eq!(returned_to_composer[0].client_message_id.as_deref(), Some("cm-queued"));
        }
        other => panic!("expected cancelled with drafts, got {other:?}"),
    }
    service.shutdown().await;
}

#[tokio::test]
async fn a_permission_answer_dropped_before_the_journal_can_be_retried() {
    let mut script = session_setup();
    script.extend([
        expects(prompt_request(3, "retry permission")),
        says(permission_request(99, "toolu_1", "touch x")),
        expects(permission_selected(99, "allow-once")),
        says(prompt_response(3, "end_turn")),
    ]);
    let harness = Harness::start(&script, &[]).await;
    harness.establish_session().await;
    let service = harness.start_service().await;
    wait_live(&service).await;
    service
        .send_operator_message("retry permission".to_owned(), "cm-1".to_owned())
        .await
        .unwrap();
    wait_fetch(&service, "the permission request", |fetched| {
        pending_permission(fetched).is_some()
    })
    .await;
    unsafe {
        std::env::set_var("BOSS_COORDINATOR_CHAT_TEST_DROP_WRITE_CONTAINING", "\"id\":99");
    }
    let first = service.answer_permission("99", "allow-once").await;
    assert!(
        matches!(first, Err(ChatError::Disconnected)),
        "the dropped write must surface as disconnected: {first:?}"
    );
    wait_live(&service).await;
    service.answer_permission("99", "allow-once").await.unwrap();
    wait_fetch(&service, "the turn to end", |fetched| turns_ended(fetched) == 1).await;
    service.shutdown().await;
}

#[tokio::test]
async fn a_malformed_journal_line_stops_the_service() {
    use tokio::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = GenerationStore::new(dir.path().join("chat"));
    let generation = store.create().unwrap();
    let socket = dir.path().join("h.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncBufReadExt::read_until(&mut BufReader::new(read), b'\n', &mut buf).await;
        let _ = write.write_all(b"this is not a journal record\n").await;
        std::future::pending::<()>().await
    });
    let service = ChatService::start(ServiceConfig {
        generation,
        endpoint: HostEndpoint {
            socket_path: socket,
            peer_pid: i32::try_from(std::process::id()).unwrap(),
        },
        redactor: CredentialRedactor::new(CREDENTIAL),
        tuning: tuning(),
    })
    .await
    .unwrap();
    wait_status(
        &service,
        "a protocol failure to stop the service",
        |status| matches!(status, ConnectionStatus::Stopped { reason } if reason.contains("not a journal record")),
    )
    .await;
    server.abort();
}

#[tokio::test]
async fn a_socket_in_a_directory_others_can_enter_is_refused() {
    let harness = Harness::start(&[expects("never sent".to_owned())], &[]).await;
    std::fs::set_permissions(harness.dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let service = harness.start_service().await;
    wait_status(&service, "the directory to be refused", |status| {
        matches!(status, ConnectionStatus::Stopped { reason } if reason.contains("private") && reason.contains("mode 0755"))
    })
    .await;
}
