// Behaviour tests for `FrontendRequest::SetDispatchPaused`'s reason
// requirement — dispatch must never be found paused with no record of what
// paused it or why, so a pause request with a missing or whitespace-only
// `reason` must be rejected outright, not silently accepted with no
// reason on record.

use super::*;
use crate::app::engine_meta;

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
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

async fn set_dispatch_paused(state: &Arc<ServerState>, paused: bool, reason: Option<&str>) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch(state, &sink);
    engine_meta::handle_set_dispatch_paused(
        ctx,
        FrontendRequest::SetDispatchPaused {
            paused,
            reason: reason.map(str::to_owned),
        },
    )
    .await;
    sole_response(&sink).await
}

#[tokio::test]
async fn pause_with_no_reason_is_rejected_and_dispatch_stays_unpaused() {
    let (server_state, _dir) = test_server_state();

    let response = set_dispatch_paused(&server_state, true, None).await;

    assert!(
        matches!(response, FrontendEvent::WorkError { .. }),
        "a reason-less pause must be rejected, got {response:?}",
    );
    assert!(
        !server_state.execution_coordinator.is_dispatch_paused(),
        "a rejected pause must not leave dispatch paused",
    );
}

#[tokio::test]
async fn pause_with_whitespace_only_reason_is_rejected_and_dispatch_stays_unpaused() {
    let (server_state, _dir) = test_server_state();

    let response = set_dispatch_paused(&server_state, true, Some("   ")).await;

    assert!(
        matches!(response, FrontendEvent::WorkError { .. }),
        "a whitespace-only reason must be rejected same as no reason at all, got {response:?}",
    );
    assert!(
        !server_state.execution_coordinator.is_dispatch_paused(),
        "a rejected pause must not leave dispatch paused",
    );
}
