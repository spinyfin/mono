use super::tests::{db_with_started_execution, handle_payload};
use super::*;
use tempfile::TempDir;

#[tokio::test]
async fn agy_tool_origin_tracks_claimed_parent() {
    let (_dir, db, run_id) = db_with_started_execution(Some("agy"));
    let registry = DriverRegistry::default();
    for (event, conversation, child) in [
        ("PreToolUse", "unclaimed", false),
        ("SessionStart", "parent", false),
        ("PreToolUse", "child", true),
        ("PreToolUse", "parent", false),
        ("PostToolUse", "child", true),
        ("PostToolUse", "parent", false),
    ] {
        let payload = serde_json::json!({
            "hookEventName": event, "conversationId": conversation,
            "toolCall": {"name": "run_command", "args": {"CommandLine": "true"}},
            "_boss_run_id": run_id,
        });
        let incoming = handle_payload(&db, &registry, &payload.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(incoming.connection.from_subagent, child, "{event} {conversation}");
    }
}

#[tokio::test]
async fn oversized_sender_finishes_writing_and_receives_permanent_rejection() {
    let (_dir, db, _) = db_with_started_execution(None);
    let registry = DriverRegistry::default();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("events.sock");
    let listener = bind_events_socket(&path).unwrap();
    let client = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(path).unwrap();
        // Larger than both the read cap and the socket send buffer.
        stream.write_all(&vec![b'x'; 2 * 1024 * 1024]).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let (stream, _) = listener.accept().await.unwrap();
    assert!(matches!(
        handle_connection(stream, &registry, &db).await,
        Err(SocketError::PayloadTooLarge { .. })
    ));
    assert!(client.await.unwrap().starts_with(REJECTION_PREFIX));
}

#[tokio::test]
async fn local_shim_bounds_large_hook_and_heals_poisoned_buffer_before_stop() {
    let (_dir, db, run_id) = db_with_started_execution(Some("claude"));
    let registry = DriverRegistry::default();
    let workspace = TempDir::new().unwrap();
    let path = workspace.path().join("events.sock");
    let listener = bind_events_socket(&path).unwrap();
    let buffer = workspace.path().join(".boss/events-pending.jsonl");
    std::fs::create_dir_all(buffer.parent().unwrap()).unwrap();
    let large = serde_json::json!({
        "hook_event_name": "PostToolUse", "session_id": "parent",
        "tool_name": "Write", "tool_input": {"content": "x".repeat(300 * 1024)},
        "tool_response": {},
    });
    // Simulate a record buffered by an older shim, which must be quarantined.
    std::fs::write(&buffer, format!("{large}\n")).unwrap();
    for payload in [
        large,
        serde_json::json!({
            "hook_event_name": "Stop", "session_id": "parent", "stop_hook_active": false,
        }),
    ] {
        let socket = path.clone();
        let workspace_path = workspace.path().to_owned();
        let run_id = run_id.clone();
        let is_stop = payload["hook_event_name"] == "Stop";
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            use std::process::{Command, Stdio};
            let mut child = Command::new(std::env::var("BOSS_TEST_EVENT_SHIM").unwrap())
                .env("BOSS_EVENTS_SOCKET", socket)
                .env("BOSS_WORKSPACE", workspace_path)
                .env("BOSS_RUN_ID", run_id)
                .env_remove("BOSS_EVENT_SPOOL")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.to_string().as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        });
        let (stream, _) = tokio::time::timeout(std::time::Duration::from_secs(20), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let incoming = handle_connection(stream, &registry, &db).await.unwrap().unwrap();
        assert_eq!(matches!(incoming.event, WorkerEvent::Stop { .. }), is_stop);
        client.await.unwrap();
        assert!(std::fs::read(&buffer).unwrap().is_empty());
    }
    let rejected = std::fs::read_to_string(workspace.path().join(".boss/events-rejected.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(rejected.trim()).unwrap();
    assert_eq!(record["rejected_reason"], "hook exceeds ingress size limit");
}

fn incoming_stop(run_id: &str, transcript_path: Option<&str>) -> IncomingHookEvent {
    IncomingHookEvent::for_test(
        WorkerEvent::Stop {
            session_id: "s".to_owned(),
            stop_hook_active: false,
            stop_reason: boss_protocol::StopReason::Completed,
        },
        Some(run_id.to_owned()),
        transcript_path.map(str::to_owned),
    )
}

fn write_transcript(dir: &TempDir, name: &str, lines: &[&str]) -> String {
    use std::io::Write;
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
    path.to_string_lossy().into_owned()
}

const CODEX_API_ERROR_PROSE: &str = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"API Error handling is now fixed"}]}}"#;
const GROK_API_ERROR_PROSE: &str = r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"API Error handling is now fixed"}}}}"#;
const AGY_QUOTA_ROW: &str = r#"{"step_index":7,"source":"MODEL","type":"ERROR_MESSAGE","status":"ERROR","error":"You have exhausted your quota on this model."}"#;
const AGY_TOOL_CALL_ROW: &str = r#"{"step_index":8,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"echo recovered"}}]}"#;
const AGY_TOOL_RESULT_ROW: &str = r#"{"step_index":9,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Command exited with code 0.\n\nCommand output:\nrecovered"}"#;

async fn assert_stop_publishes_no_idle(db: &WorkDb, run_id: &str, transcript: &str) {
    let bus = EventBus::new();
    let mut sub = bus.subscribe(boss_event_bus::TopicFilter::all());
    publish_hook_derived_events(&bus, db, &incoming_stop(run_id, Some(transcript))).await;
    bus.publish(Event::DispatchReady);
    let event = sub.recv().await.expect("sentinel event");
    assert_eq!(
        event,
        Event::DispatchReady,
        "no TransientErrorIdle should have been published"
    );
}

#[tokio::test]
async fn native_codex_api_error_prose_does_not_publish_idle_error() {
    let (dir, db, run_id) = db_with_started_execution(Some("codex"));
    let transcript = write_transcript(&dir, "t.jsonl", &[CODEX_API_ERROR_PROSE]);
    assert_stop_publishes_no_idle(&db, &run_id, &transcript).await;
}

#[tokio::test]
async fn native_grok_api_error_prose_does_not_publish_idle_error() {
    let (dir, db, run_id) = db_with_started_execution(Some("grok"));
    let transcript = write_transcript(&dir, "t.jsonl", &[GROK_API_ERROR_PROSE]);
    assert_stop_publishes_no_idle(&db, &run_id, &transcript).await;
}

#[tokio::test]
async fn raw_agy_quota_then_tool_progress_does_not_publish_idle_error() {
    let (dir, db, run_id) = db_with_started_execution(Some("agy"));
    let transcript = write_transcript(
        &dir,
        "t.jsonl",
        &[AGY_QUOTA_ROW, AGY_TOOL_CALL_ROW, AGY_TOOL_RESULT_ROW],
    );
    assert_stop_publishes_no_idle(&db, &run_id, &transcript).await;
}
