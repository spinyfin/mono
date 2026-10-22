// Behaviour tests for `FrontendRequest::GetCoordinatorHandoff` /
// `SetCoordinatorHandoff` — `boss handoff show` / `boss handoff write`,
// dispatched into `app::coordinator_handoff`.

use boss_protocol::CoordinatorHandoffView;

use super::*;
use crate::app::coordinator_handoff;
use crate::coordinator_handoff::{HANDOFF_METADATA_KEY, MAX_HANDOFF_BYTES};

fn server_state() -> (Arc<ServerState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build(),
        None,
    ));
    let state =
        ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, ServerStateOverrides::default()).unwrap();
    state.set_boss_pid(std::process::id() as libc::pid_t);
    state
        .work_db
        .record_coordinator_tmux_spawn_intent("boss-coordinator", "test-token", "opus", None)
        .unwrap();
    (state, temp)
}

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .peer_pid(std::process::id() as libc::pid_t)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(sink.next().await.is_none(), "handler must send exactly one response");
    response
}

async fn get(state: &Arc<ServerState>) -> FrontendEvent {
    let sink = make_session_sink();
    coordinator_handoff::handle_get_coordinator_handoff(dispatch(state, &sink), FrontendRequest::GetCoordinatorHandoff)
        .await;
    sole_response(&sink).await
}

async fn set(state: &Arc<ServerState>, body: &str) -> FrontendEvent {
    let sink = make_session_sink();
    coordinator_handoff::handle_set_coordinator_handoff(
        dispatch(state, &sink),
        FrontendRequest::SetCoordinatorHandoff {
            body: body.to_owned(),
            authorization: state.work_db.coordinator_handoff_authorization().unwrap().unwrap(),
        },
    )
    .await;
    sole_response(&sink).await
}

fn set_view(event: FrontendEvent) -> CoordinatorHandoffView {
    match event {
        FrontendEvent::CoordinatorHandoffSet { handoff } => handoff,
        other => panic!("expected CoordinatorHandoffSet, got {other:?}"),
    }
}

fn result_view(event: FrontendEvent) -> Option<CoordinatorHandoffView> {
    match event {
        FrontendEvent::CoordinatorHandoffResult { handoff } => handoff,
        other => panic!("expected CoordinatorHandoffResult, got {other:?}"),
    }
}

fn error_message(event: FrontendEvent) -> String {
    match event {
        FrontendEvent::WorkError { message } => message,
        other => panic!("expected WorkError, got {other:?}"),
    }
}

async fn write_as(state: &Arc<ServerState>, peer: Option<libc::pid_t>, authorization: &str) -> FrontendEvent {
    let sink = make_session_sink();
    let mut ctx = dispatch(state, &sink);
    ctx.peer_pid = peer;
    coordinator_handoff::handle_set_coordinator_handoff(
        ctx,
        FrontendRequest::SetCoordinatorHandoff {
            body: "unauthorized replacement".to_owned(),
            authorization: authorization.to_owned(),
        },
    )
    .await;
    sole_response(&sink).await
}

#[tokio::test]
async fn inherited_spawn_token_missing_and_stale_credentials_cannot_overwrite() {
    let (state, _dir) = server_state();
    set_view(set(&state, "preserve operator facts").await);
    let stale = state.work_db.coordinator_handoff_authorization().unwrap().unwrap();
    state
        .work_db
        .record_coordinator_tmux_spawn_intent("boss-coordinator", "new-spawn", "opus", None)
        .unwrap();
    for token in ["", "test-token", "new-spawn", stale.as_str()] {
        let error = error_message(write_as(&state, Some(std::process::id() as libc::pid_t), token).await);
        assert!(error.contains("private session authorization"), "{error}");
        assert_eq!(result_view(get(&state).await).unwrap().body, "preserve operator facts");
    }
}

#[tokio::test]
async fn valid_credential_does_not_authorize_missing_peer_or_unrelated_process() {
    let (state, _dir) = server_state();
    let token = state.work_db.coordinator_handoff_authorization().unwrap().unwrap();
    for peer in [None, Some(1)] {
        assert!(error_message(write_as(&state, peer, &token).await).contains("private session authorization"));
    }
    assert!(result_view(get(&state).await).is_none());
    *state.boss_pid.lock().unwrap() = None;
    assert!(
        error_message(write_as(&state, Some(std::process::id() as libc::pid_t), &token).await)
            .contains("private session authorization")
    );
}

#[tokio::test]
async fn worker_with_valid_credential_is_denied_read_and_write() {
    let (state, _dir) = server_state();
    state.feature_flags.set("worker_rpc_tier", false).unwrap();
    set_view(set(&state, "preserve operator facts").await);
    let pid = std::process::id() as libc::pid_t;
    state.worker_registry.register(pid, "worker-execution".to_owned());
    let token = state.work_db.coordinator_handoff_authorization().unwrap().unwrap();
    assert!(error_message(write_as(&state, Some(pid), &token).await).contains("private session authorization"));
    assert!(error_message(get(&state).await).contains("unavailable to workers"));
    match state.work_db.coordinator_handoff_state() {
        crate::coordinator_handoff::HandoffState::Present(handoff) => {
            assert_eq!(handoff.body, "preserve operator facts")
        }
        other => panic!("lost handoff: {other:?}"),
    }
}

#[tokio::test]
async fn subagent_or_child_with_only_inherited_environment_is_rejected() {
    let (state, _dir) = server_state();
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("10")
        .env("BOSS_SPAWN_TOKEN", "test-token")
        .spawn()
        .unwrap();
    let result = write_as(&state, Some(child.id() as libc::pid_t), "").await;
    let credential = state.work_db.coordinator_handoff_authorization().unwrap().unwrap();
    // A normal coordinator CLI is also a child, but receives explicit authority
    // for this invocation instead of inheriting it from its environment.
    let authorized = write_as(&state, Some(child.id() as libc::pid_t), &credential).await;
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(error_message(result).contains("private session authorization"));
    assert!(matches!(authorized, FrontendEvent::CoordinatorHandoffSet { .. }));
}

#[test]
fn shrink_and_rejected_caller_are_audited() {
    let _audit = crate::audit::lock_audit_globals_for_tests();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let token = crate::engine_control::generate_token();
    runtime.block_on(async {
        let (state, _dir) = server_state();
        state
            .work_db
            .record_coordinator_tmux_spawn_intent("boss-coordinator", &token, "opus", None)
            .unwrap();
        set_view(set(&state, &"a".repeat(1000)).await);
        set_view(set(&state, &"b".repeat(200)).await); // Exactly 80% is not over 80%.
        set_view(set(&state, &"c".repeat(39)).await);
        assert_eq!(result_view(get(&state).await).unwrap().body.len(), 39);
        assert!(matches!(
            write_as(&state, Some(std::process::id() as libc::pid_t), "").await,
            FrontendEvent::WorkError { .. }
        ));
    });
    let log = std::fs::read_to_string(crate::audit::default_audit_log_path().unwrap()).unwrap();
    let events: Vec<serde_json::Value> = log.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    let warnings: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "coordinator_handoff_shrink_warning" && event["writer_spawn_token"] == token)
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0]["previous_bytes"], 200);
    assert_eq!(warnings[0]["bytes"], 39);
    let denial = events
        .iter()
        .rev()
        .find(|event| event["event"] == "coordinator_handoff_rejected")
        .unwrap();
    assert_eq!(denial["pid"], std::process::id());
    assert_eq!(denial["caller"]["parent_chain"][0], std::process::id());
    assert!(denial["caller"]["argv"].is_string());
    assert!(
        denial["caller"]["argv"]
            .as_str()
            .unwrap()
            .contains(&std::env::args().next().unwrap()),
        "the caller's real argv must be captured inside the test sandbox: {denial}"
    );
}

#[tokio::test]
async fn nothing_stored_reads_back_as_an_explicit_none() {
    let (state, _dir) = server_state();
    assert_eq!(result_view(get(&state).await), None);
}

#[tokio::test]
async fn write_then_read_round_trips_and_attributes_the_live_coordinator() {
    let (state, _dir) = server_state();
    state
        .work_db
        .record_coordinator_tmux_spawn_intent("boss-coordinator", "token-live", "opus", None)
        .unwrap();

    let notified = state.coordinator_handoff_written.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let written = set_view(set(&state, "  - greyarea is shut down\n- tmux re-enabled\n\n").await);
    tokio::time::timeout(std::time::Duration::from_millis(30), notified)
        .await
        .expect("persisting a handoff must wake reset waiters");
    assert_eq!(
        written.body, "- greyarea is shut down\n- tmux re-enabled",
        "body must be trimmed"
    );
    assert_eq!(written.writer_spawn_token, "token-live");
    assert!(written.written_by_current_session);
    assert!(written.written_at > 0);
    assert!(written.age_secs >= 0);

    let read = result_view(get(&state).await).expect("a handoff was just written");
    assert_eq!(read.body, written.body);
    assert_eq!(read.written_at, written.written_at);
    assert!(read.written_by_current_session);

    // A later coordinator session sees the same handoff as not its own.
    state
        .work_db
        .record_coordinator_tmux_spawn_intent("boss-coordinator", "token-next", "opus", None)
        .unwrap();
    let read = result_view(get(&state).await).unwrap();
    assert!(!read.written_by_current_session);
    assert_eq!(read.writer_spawn_token, "token-live");
}

#[tokio::test]
async fn a_write_replaces_the_previous_handoff_wholesale() {
    let (state, _dir) = server_state();
    set_view(set(&state, "- first").await);
    set_view(set(&state, "- second").await);
    assert_eq!(result_view(get(&state).await).unwrap().body, "- second");
}

#[tokio::test]
async fn blank_and_oversize_bodies_are_rejected_without_touching_the_store() {
    let (state, _dir) = server_state();
    set_view(set(&state, "- keep me").await);

    let blank = error_message(set(&state, " \n\t").await);
    assert!(blank.contains("empty"), "got: {blank}");
    let big = error_message(set(&state, &"x".repeat(MAX_HANDOFF_BYTES + 1)).await);
    assert!(big.contains(&MAX_HANDOFF_BYTES.to_string()), "got: {big}");

    assert_eq!(result_view(get(&state).await).unwrap().body, "- keep me");
}

#[tokio::test]
async fn an_unreadable_stored_handoff_is_an_error_not_an_empty_result() {
    let (state, _dir) = server_state();
    state.work_db.set_metadata(HANDOFF_METADATA_KEY, "{corrupt").unwrap();
    let message = error_message(get(&state).await);
    assert!(message.contains("not valid JSON"), "got: {message}");
}
