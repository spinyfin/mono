//! End-to-end behaviour of connect/send recovery against a scripted fake
//! engine on a real Unix socket. Budgets are millisecond-scale so the suite
//! stays fast; nothing here sleeps on the production 10-minute default.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boss_protocol::{
    FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope, Task, TaskKind, TaskStatus,
    WorkItem,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::*;

fn fast_policy(max_wait: Duration, notices: &Arc<Mutex<Vec<String>>>) -> RetryPolicy {
    let sink = Arc::clone(notices);
    RetryPolicy::default()
        .with_max_wait(max_wait)
        .with_delays(Duration::from_millis(10), Duration::from_millis(40))
        .with_notices(NoticeSink::new(move |line| sink.lock().unwrap().push(line.to_owned())))
}

fn discovery(dir: &Path, retry: RetryPolicy) -> Discovery {
    Discovery {
        socket_path: dir.join("engine.sock").to_string_lossy().into_owned(),
        pid_file_path: dir.join("engine.pid").to_string_lossy().into_owned(),
        legacy_socket_path: None,
        legacy_pid_file_path: None,
        control_token_path: dir.join("engine.token"),
        autostart: false,
        engine: EngineCommand {
            program: "unused".into(),
            args: Vec::new(),
            source: "test".into(),
            attempted: Vec::new(),
        },
        launch_directory: dir.to_path_buf(),
        start_timeout: Duration::from_millis(50),
        retry,
        worker_environment: false,
        duplicate_guard_window: DUPLICATE_GUARD_WINDOW,
    }
}

fn version_event() -> FrontendEvent {
    FrontendEvent::EngineVersionResult {
        version: "1".into(),
        git_sha: "sha".into(),
        build_time: "now".into(),
        binary_fingerprint: "fp".into(),
    }
}

/// Read one request line from `stream`; `None` on EOF.
async fn read_request(stream: &mut BufReader<UnixStream>) -> Option<FrontendRequestEnvelope> {
    let mut line = String::new();
    let n = stream.read_line(&mut line).await.ok()?;
    (n > 0).then(|| serde_json::from_str(&line).expect("client sent a valid request envelope"))
}

async fn reply(stream: &mut BufReader<UnixStream>, request: &FrontendRequestEnvelope, event: FrontendEvent) {
    let line = serde_json::to_string(&FrontendEventEnvelope::response(request.request_id.clone(), event)).unwrap();
    stream
        .get_mut()
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
}

/// Accept connections forever; the first `drop_first` of them read the
/// request and hang up without answering, the rest answer `event`.
fn spawn_engine(
    listener: UnixListener,
    drop_first: usize,
    accepted: Arc<AtomicUsize>,
    event: FrontendEvent,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = accepted.fetch_add(1, Ordering::SeqCst);
            let event = event.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                while let Some(request) = read_request(&mut stream).await {
                    if seen < drop_first {
                        return; // hang up after the request was delivered
                    }
                    reply(&mut stream, &request, event.clone()).await;
                }
            });
        }
    })
}

#[tokio::test]
async fn connect_succeeds_once_the_socket_appears() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let discovery = discovery(dir.path(), fast_policy(Duration::from_secs(10), &notices));

    let socket = discovery.socket_path.clone();
    let accepted = Arc::new(AtomicUsize::new(0));
    let engine_accepted = Arc::clone(&accepted);
    let engine = tokio::spawn(async move {
        // The engine "restarts": absent for several backoff rounds first.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let listener = UnixListener::bind(&socket).unwrap();
        spawn_engine(listener, 0, engine_accepted, version_event()).await.ok();
    });

    let mut client = BossClient::connect(&discovery)
        .await
        .expect("connects once the engine is up");
    let (version, ..) = client.get_engine_version().await.unwrap();
    assert_eq!(version, "1");

    let notices = notices.lock().unwrap();
    assert!(!notices.is_empty(), "a first-retry notice is printed");
    assert!(notices[0].contains(&discovery.socket_path), "{notices:?}");
    assert!(
        notices.len() <= 2,
        "notices are rate-limited, not one per attempt: {notices:?}"
    );
    engine.abort();
}

#[tokio::test]
async fn exhausted_budget_fails_loudly_naming_socket_and_time_waited() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let discovery = discovery(dir.path(), fast_policy(Duration::from_millis(300), &notices));

    let started = Instant::now();
    let err = BossClient::connect(&discovery).await.expect_err("no engine → error");
    let waited = started.elapsed();

    let unreachable = err.downcast_ref::<EngineUnreachable>().expect("typed error");
    assert!(unreachable.retry_enabled);
    assert!(unreachable.attempts > 1, "{unreachable:?}");
    assert!(unreachable.waited >= Duration::from_millis(300), "{unreachable:?}");
    assert!(waited < Duration::from_secs(5), "budget bounds the wait: {waited:?}");
    let message = format!("{err:#}");
    assert!(message.contains(&discovery.socket_path), "{message}");
    assert!(message.contains("waiting"), "{message}");
}

#[tokio::test]
async fn disabled_retry_fails_on_the_first_attempt_without_notices() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let discovery = discovery(dir.path(), fast_policy(Duration::ZERO, &notices));

    let started = Instant::now();
    let err = BossClient::connect(&discovery).await.expect_err("no engine → error");
    assert!(started.elapsed() < Duration::from_millis(500));
    let unreachable = err.downcast_ref::<EngineUnreachable>().expect("typed error");
    assert!(!unreachable.retry_enabled);
    assert_eq!(unreachable.attempts, 1);
    assert!(notices.lock().unwrap().is_empty());
}

#[tokio::test]
async fn post_send_disconnect_of_a_non_idempotent_request_is_outcome_unknown_and_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let discovery = discovery(dir.path(), fast_policy(Duration::from_secs(10), &notices));
    let accepted = Arc::new(AtomicUsize::new(0));
    let listener = UnixListener::bind(&discovery.socket_path).unwrap();
    let engine = spawn_engine(listener, usize::MAX, Arc::clone(&accepted), version_event());

    let mut client = BossClient::connect(&discovery).await.unwrap();
    let request = FrontendRequest::CreateProject {
        input: boss_protocol::CreateProjectInput::builder()
            .product_id("prod_1")
            .name("p")
            .build(),
    };
    let err = client.send_request(&request).await.expect_err("dropped reply → error");

    let unknown = err.downcast_ref::<OutcomeUnknown>().expect("typed error");
    assert_eq!(unknown.request, "create_project");
    assert!(format!("{err:#}").contains("outcome unknown"), "{err:#}");
    assert_eq!(accepted.load(Ordering::SeqCst), 1, "the request was not resent");
    assert!(notices.lock().unwrap().is_empty(), "no retry, so no retry notice");
    engine.abort();
}

#[tokio::test]
async fn post_send_disconnect_of_an_idempotent_request_is_resent_on_a_fresh_connection() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let discovery = discovery(dir.path(), fast_policy(Duration::from_secs(10), &notices));
    let accepted = Arc::new(AtomicUsize::new(0));
    let listener = UnixListener::bind(&discovery.socket_path).unwrap();
    // The first connection takes the request and dies; the second answers.
    let engine = spawn_engine(listener, 1, Arc::clone(&accepted), version_event());

    let mut client = BossClient::connect(&discovery).await.unwrap();
    let (version, ..) = client.get_engine_version().await.expect("resent after the drop");
    assert_eq!(version, "1");
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
    assert_eq!(notices.lock().unwrap().len(), 1, "one notice for the one outage");
    engine.abort();
}

#[tokio::test]
async fn write_failure_is_retried_even_for_a_non_idempotent_request() {
    // The engine accepted the connection and then went away *before* the
    // request was written: nothing was delivered, so any request may retry.
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let discovery = discovery(dir.path(), fast_policy(Duration::from_secs(10), &notices));
    let listener = UnixListener::bind(&discovery.socket_path).unwrap();

    let mut client = BossClient::connect(&discovery).await.unwrap();
    // Accept and immediately close the first connection, then serve properly.
    let (first, _) = listener.accept().await.unwrap();
    drop(first);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let accepted = Arc::new(AtomicUsize::new(0));
    let engine = spawn_engine(listener, 0, Arc::clone(&accepted), version_event());

    // Writing to a fully closed peer fails with EPIPE: provably undelivered,
    // so even `Shutdown` (never replayable after a send) is sent afresh.
    let event = client
        .send_request(&FrontendRequest::Shutdown { token: "t".into() })
        .await
        .expect("an undelivered request is retried on a fresh connection");
    assert!(matches!(event, FrontendEvent::EngineVersionResult { .. }));
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "only the second connection served it"
    );
    engine.abort();
}

#[tokio::test]
async fn bare_socket_client_does_not_recover() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let engine = spawn_engine(listener, usize::MAX, Arc::clone(&accepted), version_event());

    let mut client = BossClient::connect_socket(path.to_str().unwrap()).await.unwrap();
    let err = client.get_engine_version().await.expect_err("dropped → error");
    assert!(err.downcast_ref::<OutcomeUnknown>().is_some(), "{err:#}");
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    engine.abort();
}

#[tokio::test]
async fn autostart_spawns_at_most_one_engine_while_waiting_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("spawns");
    let notices = Arc::new(Mutex::new(Vec::new()));
    let mut discovery = discovery(dir.path(), fast_policy(Duration::from_millis(600), &notices));
    discovery.autostart = true;
    // A stand-in "engine" that records each launch and never serves.
    discovery.engine = EngineCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), format!("echo x >> '{}'; exec sleep 5", counter.display())],
        source: "test".into(),
        attempted: Vec::new(),
    };

    let err = BossClient::connect(&discovery).await.expect_err("engine never serves");
    assert!(err.downcast_ref::<EngineUnreachable>().is_some(), "{err:#}");
    let launches = std::fs::read_to_string(&counter).unwrap_or_default().lines().count();
    assert_eq!(launches, 1, "one engine started, then waited on across retries");
}

#[tokio::test]
async fn autostart_with_a_missing_engine_binary_fails_promptly_with_the_resolution_chain() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    // A long budget: the failure must come from the spawn error, not from
    // running the budget out.
    let mut discovery = discovery(dir.path(), fast_policy(Duration::from_secs(60), &notices));
    discovery.autostart = true;
    discovery.engine = EngineCommand {
        program: dir.path().join("no-such-engine").to_string_lossy().into_owned(),
        args: Vec::new(),
        source: "test source".into(),
        attempted: vec!["BOSS_ENGINE_BIN (unset)".into()],
    };

    let started = Instant::now();
    let err = BossClient::connect(&discovery).await.expect_err("engine cannot start");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(err.downcast_ref::<EngineUnreachable>().is_none(), "{err:#}");
    let message = format!("{err:#}");
    assert!(message.contains("failed to start engine"), "{message}");
    assert!(message.contains("Resolution chain"), "{message}");
    assert!(message.contains("BOSS_ENGINE_BIN"), "{message}");
}

#[tokio::test]
async fn autostart_fails_at_once_when_the_engine_it_started_exits() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let mut discovery = discovery(dir.path(), fast_policy(Duration::from_secs(60), &notices));
    discovery.autostart = true;
    discovery.start_timeout = Duration::from_secs(30);
    discovery.engine = EngineCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exit 3".into()],
        source: "test".into(),
        attempted: Vec::new(),
    };

    let started = Instant::now();
    let err = BossClient::connect(&discovery).await.expect_err("engine exits");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    let message = format!("{err:#}");
    assert!(message.contains("exited before becoming ready"), "{message}");
    assert!(message.contains("exit status: 3"), "{message}");
}

#[test]
fn worker_environment_cannot_enable_autostart() {
    let dir = tempfile::tempdir().unwrap();
    let mut worker = discovery(dir.path(), RetryPolicy::disabled());
    worker.worker_environment = true;
    assert!(!worker.with_autostart(true).autostart);

    let human = discovery(dir.path(), RetryPolicy::disabled());
    assert!(human.with_autostart(true).autostart);
}

fn guarded_create() -> FrontendRequest {
    FrontendRequest::CreateTask {
        input: boss_protocol::CreateTaskInput::builder()
            .product_id("prod_1")
            .project_id("proj_1")
            .name("a task")
            .build(),
    }
}

#[tokio::test]
async fn guarded_create_is_not_resent_after_the_window_closes_during_the_outage() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    // A shrunk guard window: replay is allowed for ~250ms after the send.
    let mut discovery = discovery(dir.path(), fast_policy(Duration::from_secs(30), &notices));
    discovery.duplicate_guard_window = Duration::from_millis(300);
    let listener = UnixListener::bind(&discovery.socket_path).unwrap();

    let mut client = BossClient::connect(&discovery).await.unwrap();
    // The engine takes the create, then goes away and stays away.
    let socket = discovery.socket_path.clone();
    let accepted = Arc::new(AtomicUsize::new(0));
    let engine_accepted = Arc::clone(&accepted);
    let engine = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        engine_accepted.fetch_add(1, Ordering::SeqCst);
        let mut stream = BufReader::new(stream);
        read_request(&mut stream).await.expect("create delivered");
        drop(stream);
        drop(listener);
        std::fs::remove_file(&socket).unwrap();
    });

    let started = Instant::now();
    let err = client
        .send_request(&guarded_create())
        .await
        .expect_err("the window closes before the engine is back");
    engine.await.unwrap();

    let unknown = err.downcast_ref::<OutcomeUnknown>().expect("typed error");
    assert_eq!(unknown.request, "create_task");
    assert_eq!(accepted.load(Ordering::SeqCst), 1, "exactly one delivery");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the reconnect wait is capped to the window, not the 30s budget: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn replayed_guarded_create_blocked_by_the_guard_is_reported_as_the_created_item() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let discovery = discovery(dir.path(), fast_policy(Duration::from_secs(10), &notices));
    let listener = UnixListener::bind(&discovery.socket_path).unwrap();
    let item = WorkItem::Task(
        Task::builder()
            .id("task_1")
            .product_id("prod_1")
            .kind(TaskKind::Task)
            .name("a task")
            .description("")
            .status(TaskStatus::Todo)
            .created_at("")
            .updated_at("")
            .build(),
    );
    let engine_item = item.clone();
    let accepted = Arc::new(AtomicUsize::new(0));
    let engine_accepted = Arc::clone(&accepted);
    let engine = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = engine_accepted.fetch_add(1, Ordering::SeqCst);
            let item = engine_item.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                while let Some(request) = read_request(&mut stream).await {
                    if seen == 0 {
                        return; // applied, then died before replying
                    }
                    let event = match request.payload {
                        FrontendRequest::GetWorkItem { .. } => FrontendEvent::WorkItemResult { item: item.clone() },
                        _ => FrontendEvent::WorkItemDuplicateBlocked {
                            existing_id: "task_1".into(),
                            existing_short_id: 7,
                            name: "a task".into(),
                            age_secs: 0,
                        },
                    };
                    reply(&mut stream, &request, event).await;
                }
            });
        }
    });

    let mut client = BossClient::connect(&discovery).await.unwrap();
    let event = client.send_request(&guarded_create()).await.expect("resent");
    assert!(
        matches!(&event, FrontendEvent::WorkItemCreated { item } if item.primary_id() == "task_1"),
        "{event:?}"
    );
    engine.abort();
}

#[tokio::test]
async fn duplicate_blocked_without_a_replay_is_left_for_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let discovery = discovery(dir.path(), fast_policy(Duration::from_secs(10), &notices));
    let listener = UnixListener::bind(&discovery.socket_path).unwrap();
    let blocked = FrontendEvent::WorkItemDuplicateBlocked {
        existing_id: "task_1".into(),
        existing_short_id: 7,
        name: "a task".into(),
        age_secs: 3,
    };
    let engine = spawn_engine(listener, 0, Arc::new(AtomicUsize::new(0)), blocked);

    let mut client = BossClient::connect(&discovery).await.unwrap();
    let event = client.send_request(&guarded_create()).await.unwrap();
    assert!(
        matches!(event, FrontendEvent::WorkItemDuplicateBlocked { .. }),
        "{event:?}"
    );
    engine.abort();
}

#[tokio::test]
async fn worker_environment_never_launches_an_engine() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("spawns");
    let mut worker = discovery(dir.path(), RetryPolicy::disabled());
    worker.worker_environment = true;
    worker.engine = EngineCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), format!("echo x >> '{}'; exec sleep 5", counter.display())],
        source: "test".into(),
        attempted: Vec::new(),
    };

    let err = ensure_engine_running(&worker)
        .await
        .expect_err("workers cannot start an engine");
    assert!(format!("{err:#}").contains("worker session"), "{err:#}");
    assert!(!counter.exists(), "no process was launched");
}
