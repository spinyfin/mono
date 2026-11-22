use super::{
    FakeTmux, GrokEnv, ListingCube, MemoryStore, grok_env, grok_spawn_command, grok_supervisor, live, session_name,
    stop_command, stored,
};
use crate::homes;
use crate::worker::Supervisor;
use crate::worker::grok_teardown::override_settle_window;
use boss_engine_driver::grok::GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON;
use boss_protocol_spoke::v1::ResultCode;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn grok_turn_completed_line(prompt_id: &str) -> String {
    serde_json::json!({
        "timestamp": 1785193977,
        "method": "_x.ai/session/update",
        "params": {
            "sessionId": "sess-grok-1",
            "update": {
                "sessionUpdate": "turn_completed",
                "prompt_id": prompt_id,
                "stopReason": "cancelled",
                "usage": {
                    "inputTokens": 13600,
                    "outputTokens": 33,
                    "cachedReadTokens": 13440,
                    "modelCalls": 2,
                    "modelUsage": {
                        "grok-4.6": {
                            "inputTokens": 13600,
                            "outputTokens": 33,
                            "cachedReadTokens": 13440,
                            "totalTokens": 13632
                        }
                    }
                }
            }
        }
    })
    .to_string()
}

fn write_open_turn(events: &std::path::Path, updates: &std::path::Path) {
    if let Some(parent) = events.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(
        events,
        concat!(
            r#"{"type":"turn_started","session_id":"sess-grok-1","turn_number":0}"#,
            "\n",
        ),
    )
    .unwrap();
    std::fs::write(
        updates,
        concat!(
            r#"{"method":"session/update","params":{"sessionId":"sess-grok-1","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"run_terminal_command"}}}"#,
            "\n",
        ),
    )
    .unwrap();
}

async fn spawned_grok(
    root: &tempfile::TempDir,
    fake: Arc<FakeTmux>,
) -> (Supervisor<ListingCube, MemoryStore>, GrokEnv) {
    let supervisor = grok_supervisor(root.path(), fake.clone()).await;
    *fake.delivered_home.lock().unwrap() = Some(
        homes::grok_root(&supervisor.homes_root)
            .join("run-test")
            .join("grok-home"),
    );
    let env = grok_env(&homes::grok_root(&supervisor.homes_root), root.path());
    let result = supervisor.execute(&grok_spawn_command(), false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    (supervisor, env)
}

fn session_files(supervisor: &Supervisor<ListingCube, MemoryStore>) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = stored(&supervisor.store).home.expect("grok home");
    boss_engine_driver::grok::session_files_for_home(&home).expect("session files")
}

#[tokio::test]
async fn grok_stop_cancels_open_turn_and_waits_for_usage_before_kill() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let (supervisor, _env) = spawned_grok(&root, fake.clone()).await;
    let (events, updates) = session_files(&supervisor);
    write_open_turn(&events, &updates);
    let updates_for_cb = updates.clone();
    *fake.on_send_keys.lock().unwrap() = Some(Arc::new(move || {
        let mut file = std::fs::OpenOptions::new().append(true).open(&updates_for_cb).unwrap();
        writeln!(file, "{}", grok_turn_completed_line("prompt-spoke")).unwrap();
        let mut events_file = std::fs::OpenOptions::new()
            .append(true)
            .open(updates_for_cb.with_file_name("events.jsonl"))
            .unwrap();
        writeln!(events_file, r#"{{"type":"turn_ended","outcome":"cancelled"}}"#).unwrap();
    }));

    let started = Instant::now();
    let stopped = supervisor.execute(&stop_command(), false, 1).await;
    let elapsed = started.elapsed();
    assert_eq!(stopped.code, ResultCode::Ok as i32, "{}", stopped.reason);
    assert!(stopped.reason.is_empty(), "{}", stopped.reason);
    assert!(
        elapsed < Duration::from_secs(2),
        "stop should finish once turn_completed arrives, not wait out the settle bound (elapsed={elapsed:?})"
    );

    let sent = fake.sent.lock().unwrap().clone();
    assert!(
        sent.iter()
            .any(|args| args.iter().any(|arg| arg == "send-keys") && args.last().map(String::as_str) == Some("Escape")),
        "Grok stop must send the interrupt gesture: {sent:?}"
    );
    assert!(
        !fake.killed.lock().unwrap().is_empty(),
        "Grok stop must still kill the session after capturing usage"
    );
    let body = std::fs::read_to_string(&updates).unwrap();
    assert!(
        body.contains("turn_completed"),
        "turn_completed must land before kill so usage can be journaled: {body}"
    );
    assert!(
        body.contains("\"inputTokens\":13600") || body.contains("\"inputTokens\": 13600"),
        "cancelled-turn usage must be on disk before kill: {body}"
    );
}

#[tokio::test]
async fn grok_stop_times_out_within_bound_when_turn_completed_never_arrives() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let (supervisor, _env) = spawned_grok(&root, fake.clone()).await;
    let (events, updates) = session_files(&supervisor);
    write_open_turn(&events, &updates);
    let _guard = override_settle_window(Duration::from_millis(80));

    let started = Instant::now();
    let stopped = supervisor.execute(&stop_command(), false, 1).await;
    let elapsed = started.elapsed();
    assert_eq!(stopped.code, ResultCode::Ok as i32, "{}", stopped.reason);
    assert_eq!(stopped.reason, GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON);
    assert!(
        elapsed >= Duration::from_millis(50),
        "stop should wait out the settle bound (elapsed={elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_millis(800),
        "stop must not overrun the advertised settle bound (elapsed={elapsed:?})"
    );

    let sent = fake.sent.lock().unwrap().clone();
    assert!(
        sent.iter()
            .any(|args| args.last().map(String::as_str) == Some("Escape")),
        "Escape must still be sent: {sent:?}"
    );
    assert!(
        !fake.killed.lock().unwrap().is_empty(),
        "session must still be killed after the bound"
    );
}

#[tokio::test]
async fn grok_stop_with_foreign_live_token_sends_no_keys_and_refuses_kill() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let (supervisor, _env) = spawned_grok(&root, fake.clone()).await;
    let (events, updates) = session_files(&supervisor);
    write_open_turn(&events, &updates);
    fake.sessions
        .lock()
        .unwrap()
        .insert(session_name("token"), "other-token".into());

    let started = Instant::now();
    let stopped = supervisor.execute(&stop_command(), false, 1).await;
    let elapsed = started.elapsed();
    assert_eq!(stopped.code, ResultCode::Rejected as i32, "{}", stopped.reason);
    assert!(stopped.reason.contains("live token"), "{}", stopped.reason);
    assert!(
        elapsed < Duration::from_millis(500),
        "foreign token must not enter settle wait (elapsed={elapsed:?})"
    );
    let sent = fake.sent.lock().unwrap().clone();
    assert!(
        !sent.iter().any(|args| args.iter().any(|arg| arg == "send-keys")),
        "foreign live token must never receive send-keys: {sent:?}"
    );
    assert!(fake.killed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn grok_stop_on_exited_pane_skips_send_keys_and_settle_delay() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let (supervisor, _env) = spawned_grok(&root, fake.clone()).await;
    let (events, updates) = session_files(&supervisor);
    write_open_turn(&events, &updates);
    fake.pane_dead
        .lock()
        .unwrap()
        .insert(session_name("token"), ("1".into(), Some(0)));

    let started = Instant::now();
    let stopped = supervisor.execute(&stop_command(), false, 1).await;
    let elapsed = started.elapsed();
    assert_eq!(stopped.code, ResultCode::Ok as i32, "{}", stopped.reason);
    assert!(stopped.reason.is_empty(), "{}", stopped.reason);
    assert!(
        elapsed < Duration::from_millis(500),
        "exited pane must skip the 5s settle wait (elapsed={elapsed:?})"
    );
    let sent = fake.sent.lock().unwrap().clone();
    assert!(
        !sent.iter().any(|args| args.iter().any(|arg| arg == "send-keys")),
        "exited pane must not receive send-keys: {sent:?}"
    );
    assert!(!fake.killed.lock().unwrap().is_empty());
}
