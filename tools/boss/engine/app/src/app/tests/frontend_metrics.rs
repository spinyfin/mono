use super::*;

#[tokio::test]
async fn decoded_requests_count_ordered_and_read_lanes_once() {
    let (state, _dir) = test_server_state();
    let (engine, client) = tokio::net::UnixStream::pair().unwrap();
    let connection = tokio::spawn(handle_frontend_connection(engine, state.clone(), None));
    let (reader, mut writer) = client.into_split();
    let mut lines = BufReader::new(reader).lines();
    let hello = lines.next_line().await.unwrap().unwrap();
    assert!(hello.contains("hello"));

    assert_eq!(
        read_admission::read_lane(&FrontendRequest::Subscribe { topics: vec![] }),
        read_admission::ReadLane::Ordered
    );
    assert_eq!(
        read_admission::read_lane(&FrontendRequest::ListProducts),
        read_admission::ReadLane::Bulk
    );
    for (id, payload) in [
        ("ordered", serde_json::json!({"type": "subscribe", "topics": []})),
        ("read", serde_json::json!({"type": "list_products"})),
        ("metrics", serde_json::json!({"type": "metrics_list_live"})),
    ] {
        let request = serde_json::json!({"request_id": id, "payload": payload});
        writer.write_all(format!("{request}\n").as_bytes()).await.unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["request_id"], id);
        if id == "metrics" {
            let entries = response["payload"]["entries"].as_array().unwrap();
            for name in [
                "frontend_requests_total.subscribe",
                "frontend_requests_total.listproducts",
            ] {
                let entry = entries.iter().find(|entry| entry["name"] == name).unwrap();
                assert_eq!(entry["value"], 1);
                assert_eq!(entry["stale"], false);
            }
        }
    }
    assert_eq!(state.metrics.counter_value("frontend_requests_total"), Some(3));
    assert_eq!(
        state.metrics.counter_value("frontend_pushes_total.hello.enqueued"),
        Some(1)
    );
    assert_eq!(
        state.metrics.counter_value("frontend_pushes_total"),
        Some(1),
        "correlated replies are not pushes"
    );

    writer.write_all(b"invalid json\n").await.unwrap();
    let error = lines.next_line().await.unwrap().unwrap();
    assert!(error.contains("invalid request payload"));
    assert_eq!(
        state.metrics.counter_value("frontend_requests_total"),
        Some(3),
        "decode failures are not requests"
    );
    drop(writer);
    tokio::time::timeout(std::time::Duration::from_secs(5), connection)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[test]
fn sink_counts_each_push_kind_and_enqueue_outcome() {
    let registry = crate::app::frontend_metrics::test_registry();
    let (tx, _rx) = oneshot::channel();
    let sink = SessionSink::new(tx, registry.clone());
    assert_eq!(sink.enqueue(topic_envelope("a", 1)), EnqueueOutcome::Enqueued);
    assert_eq!(sink.enqueue(topic_envelope("a", 2)), EnqueueOutcome::Coalesced);
    for i in 1..MAX_SESSION_QUEUE {
        assert_eq!(
            sink.enqueue(topic_envelope(&format!("topic-{i}"), 1)),
            EnqueueOutcome::Enqueued
        );
    }
    assert_eq!(sink.enqueue(topic_envelope("overflow", 1)), EnqueueOutcome::Degraded);
    sink.queue
        .lock()
        .unwrap()
        .backdate_oldest_bulk_entry(STUCK_CLIENT_AGE_MS + 1);
    assert_eq!(sink.enqueue(topic_envelope("stuck", 1)), EnqueueOutcome::Slow);
    assert_eq!(sink.enqueue(topic_envelope("closed", 1)), EnqueueOutcome::Closed);
    for (outcome, expected) in [
        ("enqueued", MAX_SESSION_QUEUE as u64),
        ("coalesced", 1),
        ("degraded", 1),
        ("slow", 1),
        ("closed", 1),
    ] {
        assert_eq!(
            registry.counter_value(&format!("frontend_pushes_total.topicevent.{outcome}")),
            Some(expected)
        );
    }
    let (tx, _rx) = oneshot::channel();
    let other = SessionSink::new(tx, registry.clone());
    assert_eq!(
        other.enqueue(engine_request_envelope("control")),
        EnqueueOutcome::Enqueued
    );
    assert_eq!(other.enqueue(response_envelope("reply")), EnqueueOutcome::Enqueued);
    assert_eq!(
        registry.counter_value("frontend_pushes_total.enginerequest.enqueued"),
        Some(1)
    );
    assert_eq!(
        registry.counter_value("frontend_pushes_total.productslist.enqueued"),
        None
    );
    assert_eq!(
        registry.counter_value("frontend_pushes_total"),
        Some(MAX_SESSION_QUEUE as u64 + 5)
    );
}

#[test]
fn frontend_counters_survive_flush_and_rehydrate() {
    let (state, _dir) = test_server_state();
    let before = &state.metrics;
    crate::app::frontend_metrics::record_request(before, "ListProducts");
    crate::app::frontend_metrics::record_push(before, "Hello", &EnqueueOutcome::Enqueued);
    boss_metrics::flush_all(before, state.work_db.as_ref()).unwrap();
    let after = boss_metrics::Registry::new();
    crate::metrics_init::init_all(&after);
    boss_metrics::seed_from_db(&after, state.work_db.as_ref()).unwrap();
    crate::app::frontend_metrics::record_request(&after, "ListProducts");
    crate::app::frontend_metrics::record_push(&after, "Hello", &EnqueueOutcome::Enqueued);
    for name in [
        "frontend_requests_total",
        "frontend_requests_total.listproducts",
        "frontend_pushes_total",
        "frontend_pushes_total.hello.enqueued",
    ] {
        let snapshot = after.counter_snapshot_one(name).unwrap();
        assert_eq!(snapshot.value, 2);
        assert!(!snapshot.stale);
    }
}
