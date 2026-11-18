//! Reset latency and lock contention through the production Unix-stream RPC
//! dispatcher, with isolated storage and an injected tmux server (no host
//! tmux or model).
use super::*;

async fn rpc_fixture(
    token: &str,
) -> (
    Fixture,
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let f = fixture(Duration::from_millis(500)).await;
    *f.runner.token.lock().unwrap() = token.into();
    f.state
        .work_db
        .record_coordinator_tmux_spawn_intent("boss-coordinator", token, "opus", None)
        .unwrap();
    f.state.work_db.record_coordinator_tmux_session_created(token).unwrap();
    let (engine, app) = tokio::net::UnixStream::pair().unwrap();
    let connection = tokio::spawn(handle_frontend_connection(engine, f.state.clone(), None));
    let (read, mut write) = app.into_split();
    let mut read = BufReader::new(read);
    send_frame(
        &mut write,
        "register",
        serde_json::json!({"type": "register_app_session"}),
    )
    .await;
    read_frame_where(&mut read, |f| f["payload"]["type"] == "app_session_registered").await;
    (f, read, write, connection)
}

async fn reset(write: &mut tokio::net::unix::OwnedWriteHalf, token: &str, force: bool) {
    send_frame(
        write,
        token,
        serde_json::to_value(FrontendRequest::RecreateCoordinator {
            expected_spawn_token: token.into(),
            reason: CoordinatorRecreateReason::OperatorReset,
            force_without_handoff: force,
        })
        .unwrap(),
    )
    .await;
}

fn stages(token: &str) -> Vec<serde_json::Value> {
    let log = std::fs::read_to_string(boss_engine_runtime::audit::default_audit_log_path().unwrap()).unwrap();
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["spawn_token"] == token)
        .collect()
}

/// The durable socket hosts the coordinator, so a hung retired `-L boss`
/// server must neither be probed nor delay the reset.
async fn slow_legacy_reset(token: &str) {
    let (f, _read, mut write, connection) = rpc_fixture(token).await;
    f.runner.faults.lock().unwrap().legacy_delay = Duration::from_secs(30);
    let start = std::time::Instant::now();
    reset(&mut write, token, false).await;
    wait_for_prompt(&f.runner).await;
    assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
    assert!(
        !f.runner
            .calls()
            .iter()
            .any(|args| args.first().map(String::as_str) == Some("-L")),
        "the retired server must not be probed while the socket has the coordinator"
    );
    connection.abort();
}

async fn busy_reset(force: bool, recreation: bool, token: &str) {
    let (f, mut read, mut write, connection) = rpc_fixture(token).await;
    let guard = if recreation {
        None
    } else {
        Some(f.state.coordinator_tmux_lock.lock().await)
    };
    reset(&mut write, token, force).await;
    let guard = if recreation {
        wait_for_prompt(&f.runner).await;
        Some(f.state.coordinator_tmux_lock.lock().await)
    } else {
        guard
    };
    let start = std::time::Instant::now();
    let reply = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame["request_id"] == token {
                let message = frame["payload"]["message"].as_str().unwrap_or("");
                assert!(
                    !message.contains("Restarting with potentially missing context"),
                    "missing-handoff warning must wait until recreation holds the lock: {message}"
                );
                if message.contains("lifecycle busy") {
                    break frame;
                }
            }
        }
    })
    .await
    .expect("both lock phases must have a bounded RPC error");
    let message = reply["payload"]["message"].as_str().unwrap();
    assert!(
        !message.contains(boss_engine_runtime::coordinator_tmux::reset_handoff::DELIVERY_FAILED_PREFIX),
        "{message}"
    );
    assert!(start.elapsed() < Duration::from_secs(12));
    assert!(f.runner.position("kill-session").is_none());
    assert!(f.runner.position("new-session").is_none());
    let events = stages(token);
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "coordinator_reset_lock_wait_timeout"
                && e["phase"] == if recreation { "recreation" } else { "delivery" })
    );
    drop(guard);
    connection.abort();
}

// Serialize with audit rotation/path tests while each isolated runtime runs.
// Keeping the synchronous guard outside block_on avoids blocking async workers.
fn run_with_audit(future: impl std::future::Future<Output = ()>) {
    let _audit = boss_engine_runtime::audit::lock_audit_globals_for_tests();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}

#[test]
fn rpc_reset_skips_slow_legacy_server_when_the_socket_has_the_coordinator() {
    run_with_audit(slow_legacy_reset("latency-slow-legacy"));
}

#[test]
fn forced_rpc_reset_under_contention_does_not_offer_force_again() {
    run_with_audit(busy_reset(true, false, "latency-forced-lock"));
}

#[test]
fn rpc_reset_recreation_lock_is_bounded() {
    run_with_audit(busy_reset(false, true, "latency-recreation-lock"));
}
