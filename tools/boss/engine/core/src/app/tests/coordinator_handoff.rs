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
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, None, None, None, None).unwrap();
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
        FrontendRequest::SetCoordinatorHandoff { body: body.to_owned() },
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

    let written = set_view(set(&state, "  - greyarea is shut down\n- tmux re-enabled\n\n").await);
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
