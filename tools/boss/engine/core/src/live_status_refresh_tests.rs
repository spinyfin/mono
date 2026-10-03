//! Native transcript replays through the running loop. The mock model uses
//! the newest action in the HTTP prompt, so stale input cannot pass the test.

use super::*;
use crate::driver::{ClaudeDriver, CodexDriver, GrokDriver};
use boss_protocol::WorkerEvent;
use serde_json::json;
use std::io::Write;
use std::path::Path;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const RUN: &str = "summary-replay";

struct Resolver(PathBuf);

#[async_trait]
impl TranscriptPathResolver for Resolver {
    async fn transcript_path(&self, _: &str) -> Option<PathBuf> {
        Some(self.0.clone())
    }
}

struct Broadcasts {
    registry: Arc<LiveWorkerStateRegistry>,
    summaries: StdMutex<Vec<String>>,
}

#[async_trait]
impl LiveStatusBroadcaster for Broadcasts {
    async fn broadcast_live_worker_states(&self) {
        if let Some(summary) = self.registry.get(1).and_then(|state| state.live_status) {
            self.summaries.lock().unwrap().push(summary);
        }
    }
}

fn append_action(path: &Path, driver: &str, action: &str) {
    let command = format!("echo {action}");
    let record = match driver {
        "claude" => json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Bash","input":{"command":command}}
        ]}}),
        "codex" => json!({"type":"response_item","payload":{
            "type":"function_call","call_id":action,"name":"exec_command",
            "arguments":json!({"cmd":command}).to_string()
        }}),
        "grok" => json!({"method":"session/update","params":{"update":{
            "sessionUpdate":"tool_call","toolCallId":action,
            "title":"run_terminal_command","rawInput":{"command":command}
        }}}),
        _ => unreachable!(),
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{record}").unwrap();
}

async fn await_summary(registry: &LiveWorkerStateRegistry, expected: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if registry.get(1).and_then(|state| state.live_status).as_deref() == Some(expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {expected:?}, got {:?}", registry.get(1)));
}

async fn advance(seconds: u64) {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(seconds)).await;
    tokio::time::resume();
    tokio::task::yield_now().await;
}

async fn replay(driver: Arc<dyn AgentDriver>, transcript: PathBuf) {
    let name = driver.descriptor().name;
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let prompt = body["messages"][0]["content"].to_string();
            if prompt.contains("retry-marker") {
                return ResponseTemplate::new(401);
            }
            let summary = if prompt.contains("validating") {
                "running validation"
            } else if prompt.contains("editing") {
                "editing the parser"
            } else {
                assert!(prompt.contains("inspecting"), "native action missing: {prompt}");
                "inspecting the module"
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "content":[{"type":"text","text":summary}]
            }))
        })
        .mount(&server)
        .await;
    let utility: Arc<dyn UtilityModel> = Arc::new(
        crate::utility_model::AnthropicUtilityModel::from_lookup(Some("test-key".into()), |_| None)
            .with_endpoint(format!("{}/v1/messages", server.uri())),
    );
    let registry = Arc::new(LiveWorkerStateRegistry::new());
    registry.register_spawn(1, RUN, name, 0, None);
    registry.apply_event(
        1,
        &WorkerEvent::PreToolUse {
            session_id: "session".into(),
            tool_name: "Bash".into(),
            tool_input: json!({}),
        },
    );
    let broadcasts = Arc::new(Broadcasts {
        registry: registry.clone(),
        summaries: StdMutex::new(Vec::new()),
    });
    let manager = LiveStatusManager::new();
    let resolver: Arc<dyn TranscriptPathResolver> = Arc::new(Resolver(transcript.clone()));
    append_action(&transcript, name, "inspecting");
    // Already Working before task installation, as when a hook races startup
    // or adoption replaces the loop. No activity transition follows.
    manager.start_slot(
        1,
        LiveStatusRun::new(RUN, driver.clone()),
        utility.clone(),
        registry.clone(),
        broadcasts.clone(),
        resolver.clone(),
    );
    await_summary(&registry, "inspecting the module").await;

    append_action(&transcript, name, "editing");
    manager.start_slot(
        1,
        LiveStatusRun::new(RUN, driver),
        utility,
        registry.clone(),
        broadcasts.clone(),
        resolver,
    );
    await_summary(&registry, "editing the parser").await;

    append_action(&transcript, name, "validating");
    // A long tool call supplies no PostToolUse. One timer period must refresh.
    advance(61).await;
    await_summary(&registry, "running validation").await;
    assert_eq!(
        *broadcasts.summaries.lock().unwrap(),
        ["inspecting the module", "editing the parser", "running validation"]
    );
    println!("{name}: inspecting the module -> editing the parser -> running validation");

    append_action(&transcript, name, "retry-marker");
    advance(61).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.debug_store().snapshot_for(1).last_outcome_tag.as_deref() != Some("api_error") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        registry.get(1).unwrap().live_status.as_deref(),
        Some("running validation")
    );
    assert_eq!(
        broadcasts.summaries.lock().unwrap().len(),
        3,
        "failed regeneration must retain the cached summary"
    );
    let requests = server.received_requests().await.unwrap().len();
    advance(10).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests,
        "failure must not spin on an expired timer"
    );
    manager.stop_slot(1);
}

fn replay_driver(driver: Arc<dyn AgentDriver>, homes: &Path) {
    let sessions = homes.join(RUN).join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(replay(driver, sessions.join("events.jsonl")));
}

#[test]
fn claude_refresh_replay() {
    let tmp = tempfile::tempdir().unwrap();
    replay_driver(Arc::new(ClaudeDriver), tmp.path());
}

#[test]
fn codex_refresh_replay() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::driver::test_support::codex_homes_override(tmp.path());
    replay_driver(Arc::new(CodexDriver::default()), tmp.path());
}

#[test]
fn grok_refresh_replay() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::driver::test_support::grok_homes_override(tmp.path());
    replay_driver(Arc::new(GrokDriver::default()), tmp.path());
}

#[tokio::test]
async fn first_summary_deadline_survives_sparse_tool_events() {
    let tmp = tempfile::tempdir().unwrap();
    let transcript = tmp.path().join("late.jsonl");
    let registry = Arc::new(LiveWorkerStateRegistry::new());
    registry.register_spawn(1, RUN, "claude", 0, None);
    let manager = LiveStatusManager::new();
    let broadcasts = Arc::new(Broadcasts {
        registry: registry.clone(),
        summaries: StdMutex::new(Vec::new()),
    });
    manager.start_slot(
        1,
        LiveStatusRun::new(RUN, Arc::new(ClaudeDriver)),
        Arc::new(crate::utility_model::AnthropicUtilityModel::from_lookup(None, |_| None)),
        registry,
        broadcasts,
        Arc::new(Resolver(transcript.clone())),
    );
    manager.notify(1, Trigger::ActivityChanged(WorkerActivity::Working));
    // The initial attempt has no transcript yet. Sparse tool notifications
    // must neither reset its deadline nor consume synthetic timer ticks.
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.debug_store().snapshot_for(1).transcript_path.is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..2 {
        advance(20).await;
        manager.notify(1, Trigger::PostToolUse);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    append_action(&transcript, "claude", "inspecting");
    advance(21).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.debug_store().snapshot_for(1).last_outcome_tag.is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let snapshot = manager.debug_store().snapshot_for(1);
    assert_eq!(snapshot.last_outcome_tag.as_deref(), Some("no_api_key"));
    assert!(snapshot.last_redacted_bytes.unwrap() > 0);
    assert!(snapshot.last_synthetic_trigger_at_epoch_s.is_some());
    manager.stop_slot(1);
}
