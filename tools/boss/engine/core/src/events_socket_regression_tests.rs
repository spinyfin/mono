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
