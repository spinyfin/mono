use super::*;
use crate::app::engine_meta::handle_get_engine_pool_config;
use boss_protocol::SelectedProductState;

/// The read the app sends after a `resync_required` to recover the
/// registration-time `engine_pool_config` push. It must answer the caller's
/// request id with the server's configured sizes and model, and unlike
/// `RegisterAppSession` it must not register a session.
#[tokio::test]
async fn get_engine_pool_config_replies_correlated_with_server_config() {
    let (state, _dir) = test_server_state();
    let sink = make_session_sink();
    let dispatch = Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-reader")
        .request_id("req-pool-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build();

    handle_get_engine_pool_config(dispatch, FrontendRequest::GetEnginePoolConfig).await;

    sink.close();
    let envelope = sink.next().await.expect("handler must send a response");
    assert!(sink.next().await.is_none(), "handler must send exactly one response");
    assert_eq!(envelope.request_id.as_deref(), Some("req-pool-1"));
    let FrontendEvent::EnginePoolConfig {
        worker_slots,
        automation_slots,
        review_slots,
        coordinator_model,
    } = envelope.payload
    else {
        panic!("expected EnginePoolConfig, got {:?}", envelope.payload);
    };
    assert_eq!(worker_slots, state.worker_pool_size);
    assert_eq!(automation_slots, state.automation_pool_size);
    assert_eq!(review_slots, state.review_pool_size);
    assert_eq!(coordinator_model, state.coordinator_model);
    assert_eq!(
        state.selected_product_state().await,
        SelectedProductState::AppNotConnected,
        "a read must not register an app session",
    );
}
