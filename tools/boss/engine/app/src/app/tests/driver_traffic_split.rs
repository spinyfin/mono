use super::*;

async fn next_split(sink: &SessionSink, request_id: Option<&str>) -> boss_protocol::DriverTrafficSplit {
    let envelope = tokio::time::timeout(std::time::Duration::from_secs(10), sink.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(envelope.request_id.as_deref(), request_id);
    match envelope.payload {
        FrontendEvent::DriverTrafficSplitResult { split } => split,
        other => panic!("expected split result, got {other:?}"),
    }
}

#[tokio::test]
async fn set_driver_traffic_split_replies_and_publishes_to_subscribers() {
    let (state, _dir) = test_server_state();
    let caller = make_session_sink();
    let subscriber = make_session_sink();
    let unrelated = make_session_sink();
    for (id, sink) in [
        ("caller", &caller),
        ("subscriber", &subscriber),
        ("unrelated", &unrelated),
    ] {
        state.topic_broker.register_session(id, sink.clone()).await;
    }
    state
        .topic_broker
        .subscribe("subscriber", &[boss_protocol::TOPIC_DRIVER_TRAFFIC_SPLIT.to_owned()])
        .await;
    let dispatch = |request_id: &str| {
        Dispatch::builder()
            .server_state(state.clone())
            .work_db(state.work_db.clone())
            .sink(caller.clone())
            .session_id("caller")
            .request_id(request_id)
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build()
    };

    let incoming = boss_protocol::DriverTrafficSplit::new(0, 40, 30, 30);
    crate::app::engine_meta::handle_set_driver_traffic_split(
        dispatch("shares"),
        FrontendRequest::SetDriverTrafficSplit { split: incoming },
    )
    .await;
    let applied = next_split(&caller, Some("shares")).await;
    assert_eq!((applied.claude, applied.codex, applied.agy), (40, 30, 30));
    assert_eq!(next_split(&subscriber, None).await, applied);
    assert_eq!(state.work_db.get_driver_traffic_split().unwrap(), applied);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), unrelated.next())
            .await
            .is_err()
    );
}
