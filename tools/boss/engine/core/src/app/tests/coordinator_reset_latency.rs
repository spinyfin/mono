//! Reset latency through the production Unix-stream RPC dispatcher, with
//! isolated storage and injected tmux servers (no host tmux or model).
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
    let log = std::fs::read_to_string(crate::audit::default_audit_log_path().unwrap()).unwrap();
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["spawn_token"] == token)
        .collect()
}

fn report_stages(token: &str) {
    let events = stages(token);
    let first = events
        .iter()
        .find(|e| e["event"] == "coordinator_reset_rpc_received")
        .unwrap()["ts_epoch_ms"]
        .as_i64()
        .unwrap();
    for event in events {
        eprintln!(
            "reset stage {}: +{}ms phase={}",
            event["event"],
            event["ts_epoch_ms"].as_i64().unwrap() - first,
            event["phase"]
        );
    }
}

async fn legacy_reset(token: &str, legacy_delay: Duration) {
    let (f, mut read, mut write, connection) = rpc_fixture(token).await;
    *f.runner.legacy_delay.lock().unwrap() = legacy_delay;
    // The supervisor owns the lifecycle lock; app attach is next in its queue.
    let supervisor = f.state.coordinator_tmux_lock.lock().await;
    let attach = f.state.coordinator_tmux_lock.lock();
    tokio::pin!(attach);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut attach)
            .await
            .is_err()
    );
    let start = std::time::Instant::now();
    reset(&mut write, token, false).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(supervisor);
    let attach = attach.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(attach);
    tokio::time::timeout(Duration::from_secs(35), async {
        while !f.runner.prompted() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the injected legacy probe must eventually return");
    report_stages(token);
    assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
    assert!(
        !f.runner
            .calls()
            .iter()
            .any(|args| args.first().map(String::as_str) == Some("-L"))
    );
    let events = stages(token);
    for name in [
        "coordinator_reset_rpc_received",
        "coordinator_reset_server_resolved",
        "coordinator_handoff_delivery_started",
        "coordinator_reset_lock_acquired",
        "coordinator_handoff_requested",
    ] {
        assert!(events.iter().any(|event| event["event"] == name), "missing {name}");
    }
    let resolved = events
        .iter()
        .position(|e| e["event"] == "coordinator_reset_server_resolved")
        .unwrap();
    let received = events
        .iter()
        .position(|e| e["event"] == "coordinator_reset_rpc_received")
        .unwrap();
    let delivery = events
        .iter()
        .position(|e| e["event"] == "coordinator_handoff_delivery_started")
        .unwrap();
    assert!(received < resolved && resolved < delivery);
    assert_eq!(events[resolved]["server"], "socket");
    assert_eq!(events[resolved]["phase"], "delivery");
    assert!(events[resolved]["elapsed_ms"].as_u64().unwrap() < 2_000);
    let attachment = read_frame_where(&mut read, |frame| {
        frame["payload"]["type"] == "engine_request"
            && frame["payload"]["request"]["kind"] == "attach_coordinator_pane"
            && frame["payload"]["request"]["spawn_token"] != token
    })
    .await;
    send_frame(
        &mut write,
        "attached",
        serde_json::to_value(FrontendRequest::EngineResponse {
            request_id: attachment["payload"]["request_id"].as_str().unwrap().into(),
            response: EngineToAppResponse::AttachCoordinatorPane {
                result: Ok(boss_protocol::AttachCoordinatorPaneResult {}),
            },
        })
        .unwrap(),
    )
    .await;
    // Let the detached reset finish before dropping its isolated database.
    tokio::time::timeout(Duration::from_secs(5), async {
        while f
            .state
            .coordinator_reset_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
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
        !message.contains(crate::coordinator_tmux::reset_handoff::DELIVERY_FAILED_PREFIX),
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
    report_stages(token);
    drop(guard);
    connection.abort();
}

// Serialize with audit rotation/path tests while each isolated runtime runs.
// Keeping the synchronous guard outside block_on avoids blocking async workers.
fn run_with_audit(future: impl std::future::Future<Output = ()>) {
    let _audit = crate::audit::lock_audit_globals_for_tests();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}

#[test]
fn rpc_skips_slow_legacy_server_under_supervisor_and_attach_contention() {
    run_with_audit(legacy_reset("latency-slow-legacy", Duration::from_secs(30)));
}

#[test]
fn rpc_skips_absent_legacy_server_under_supervisor_and_attach_contention() {
    run_with_audit(legacy_reset("latency-absent-legacy", Duration::ZERO));
}

#[test]
fn forced_rpc_reset_under_contention_does_not_offer_force_again() {
    run_with_audit(busy_reset(true, false, "latency-forced-lock"));
}

#[test]
fn rpc_reset_recreation_lock_is_bounded() {
    run_with_audit(busy_reset(false, true, "latency-recreation-lock"));
}
