//! Reset latency and lock contention through the production Unix-stream RPC
//! dispatcher, with isolated storage and an injected tmux server (no host
//! tmux or model).
use super::*;
use std::sync::Arc;

use crate::app::sessions;

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

fn report_stages(token: &str) {
    let events = stages(token);
    let Some(first) = events
        .iter()
        .find(|e| e["event"] == "coordinator_reset_rpc_received")
        .and_then(|e| e["ts_epoch_ms"].as_i64())
    else {
        eprintln!("reset stages: no coordinator_reset_rpc_received for {token}");
        return;
    };
    for event in events {
        eprintln!(
            "reset stage {}: +{}ms phase={}",
            event["event"],
            event["ts_epoch_ms"].as_i64().unwrap_or(first) - first,
            event["phase"]
        );
    }
}

fn stage_gap_ms(events: &[serde_json::Value], from: &str, to: &str) -> i64 {
    let ts = |name: &str| {
        events
            .iter()
            .find(|e| e["event"] == name)
            .and_then(|e| e["ts_epoch_ms"].as_i64())
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    ts(to) - ts(from)
}

/// A supervisor pass: take the lifecycle lock, resolve the active handle,
/// and run `restart_if_dead` the way the production child-exit supervisor does.
async fn supervisor_pass(state: Arc<ServerState>) {
    let tmux = state.tmux_from_program("/nonexistent/tmux".into()).unwrap();
    let legacy = boss_tmux::Tmux::for_legacy_label_server_with_runner(tmux.program().to_path_buf(), tmux.runner()).ok();
    let working_directory = state.coordinator_session_directory().unwrap();
    let _guard = state.coordinator_tmux_lock.lock().await;
    let active = boss_engine_runtime::coordinator_tmux::resolve_active_handle(&tmux, legacy.as_ref()).await;
    let _ = boss_engine_runtime::coordinator_tmux::restart_if_dead(
        &boss_engine_runtime::coordinator_tmux::CoordinatorSpawn::builder()
            .work_db(state.work_db.as_ref())
            .tmux(active)
            .create_tmux(&tmux)
            .model(&state.coordinator_model)
            .working_directory(&working_directory)
            .version_probe(&boss_engine_runtime::coordinator_tmux::RealClaudeVersionProbe)
            .design_docs(&state.design_docs)
            .build(),
        0,
    )
    .await;
}

/// The durable socket hosts the coordinator, so a hung or absent retired
/// `-L boss` server must neither be probed nor delay the reset, even while
/// a real supervisor pass and an app-registration attach hold or queue on
/// the lifecycle lock.
async fn contended_legacy_reset(token: &str, legacy_delay: Duration) {
    let (f, _read, mut write, connection) = rpc_fixture(token).await;
    f.runner.faults.lock().unwrap().legacy_delay = legacy_delay;
    let supervisor = tokio::spawn(supervisor_pass(f.state.clone()));
    let attach = tokio::spawn(sessions::attach_coordinator_to_registered_app(f.state.clone()));
    tokio::task::yield_now().await;
    let start = std::time::Instant::now();
    reset(&mut write, token, false).await;
    wait_for_prompt(&f.runner).await;
    report_stages(token);
    assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
    assert!(
        !f.runner
            .calls()
            .iter()
            .any(|args| args.first().map(String::as_str) == Some("-L")),
        "the retired server must not be probed while the socket has the coordinator"
    );
    let events = stages(token);
    for name in [
        "coordinator_reset_rpc_received",
        "coordinator_handoff_delivery_started",
        "coordinator_reset_lock_acquired",
        "coordinator_handoff_requested",
    ] {
        assert!(events.iter().any(|event| event["event"] == name), "missing {name}");
    }
    let gap = stage_gap_ms(
        &events,
        "coordinator_reset_rpc_received",
        "coordinator_handoff_requested",
    );
    assert!(gap < 1000, "rpc_received through handoff_requested was {gap}ms");
    tokio::time::timeout(Duration::from_secs(2), supervisor)
        .await
        .expect("supervisor pass must not wait out a hung legacy server")
        .expect("supervisor pass panicked");
    attach.abort();
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
fn rpc_skips_slow_legacy_server_under_supervisor_and_attach_contention() {
    run_with_audit(contended_legacy_reset("latency-slow-legacy", Duration::from_secs(30)));
}

#[test]
fn rpc_skips_absent_legacy_server_under_supervisor_and_attach_contention() {
    run_with_audit(contended_legacy_reset("latency-absent-legacy", Duration::ZERO));
}

#[test]
fn forced_rpc_reset_under_contention_does_not_offer_force_again() {
    run_with_audit(busy_reset(true, false, "latency-forced-lock"));
}

#[test]
fn rpc_reset_recreation_lock_is_bounded() {
    run_with_audit(busy_reset(false, true, "latency-recreation-lock"));
}
