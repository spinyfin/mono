use super::*;
use crate::population_timing::PopulationTrace;
use boss_protocol::BoardPopulationPhase;

fn session_sink() -> Arc<SessionSink> {
    let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
    Arc::new(SessionSink::new(
        shutdown_tx,
        crate::app::frontend_metrics::test_registry(),
    ))
}

async fn next_progress(sink: &SessionSink) -> FrontendEvent {
    tokio::time::timeout(std::time::Duration::from_secs(10), sink.next())
        .await
        .expect("no progress push within 10s")
        .expect("sink closed before a progress push arrived")
        .payload
}

/// Run one board load for `product_id` on a blocking thread, as the engine
/// does, with a trace that reports progress to the product's work topic.
async fn load_with_topic_progress(state: &Arc<ServerState>, product_id: &str) -> PopulationTrace {
    let state = state.clone();
    let product_id = product_id.to_owned();
    let sink = topic_progress_sink(&state, &product_id);
    tokio::task::spawn_blocking(move || {
        let mut trace = PopulationTrace::new(product_id.clone(), "req-progress", None, std::time::Instant::now());
        trace.set_progress_sink(sink);
        state
            .work_tree_loads
            .load(
                &state.work_db,
                &state.work_revision,
                &state.metrics,
                &product_id,
                &mut trace,
            )
            .unwrap();
        trace
    })
    .await
    .unwrap()
}

#[test]
fn progress_event_rounds_elapsed_and_drops_negative_counts() {
    let progress = PopulationProgress {
        phase: BoardPopulationPhase::Stage,
        stage: Some("db.tasks"),
        elapsed_ms: 1234.6,
        items: Some(1949),
        bytes: Some(-1),
    };
    match progress_event("prod_1", &progress) {
        FrontendEvent::BoardPopulationProgress {
            product_id,
            phase,
            stage,
            elapsed_ms,
            items,
            bytes,
        } => {
            assert_eq!(product_id, "prod_1");
            assert_eq!(phase, BoardPopulationPhase::Stage);
            assert_eq!(stage.as_deref(), Some("db.tasks"));
            assert_eq!(elapsed_ms, 1235);
            assert_eq!(items, Some(1949));
            assert_eq!(bytes, None);
        }
        other => panic!("expected BoardPopulationProgress, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_load_publishes_ordered_progress_to_the_product_topic() {
    let (state, _dir) = crate::app::tests::test_server_state();
    let product = state
        .work_db
        .create_product(boss_protocol::CreateProductInput::builder().name("Cold").build())
        .unwrap();
    let watcher = session_sink();
    state.topic_broker.register_session("watcher", watcher.clone()).await;
    state
        .topic_broker
        .subscribe("watcher", &[work_product_topic(&product.id)])
        .await;

    let trace = load_with_topic_progress(&state, &product.id).await;
    drop(trace);

    let mut phases = Vec::new();
    let mut stages = Vec::new();
    while let Ok(Some(envelope)) = tokio::time::timeout(std::time::Duration::from_millis(500), watcher.next()).await {
        let FrontendEvent::BoardPopulationProgress {
            product_id,
            phase,
            stage,
            ..
        } = envelope.payload
        else {
            panic!(
                "only progress pushes expected on this topic, got {:?}",
                envelope.payload
            );
        };
        assert_eq!(product_id, product.id);
        phases.push(phase);
        stages.extend(stage);
    }
    assert_eq!(phases.first(), Some(&BoardPopulationPhase::Started));
    assert!(
        phases[1..].iter().all(|p| *p == BoardPopulationPhase::Stage),
        "the writer, not the load, sends Done: {phases:?}"
    );
    let position = |name: &str| {
        stages
            .iter()
            .position(|s| s == name)
            .unwrap_or_else(|| panic!("no {name} stage in {stages:?}"))
    };
    assert!(
        position("db.tasks") < position("db.task_runtimes"),
        "stages arrive in build order: {stages:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn warm_refresh_publishes_no_progress() {
    let (state, _dir) = crate::app::tests::test_server_state();
    let product = state
        .work_db
        .create_product(boss_protocol::CreateProductInput::builder().name("Warm").build())
        .unwrap();
    // Prime the cached projection, then watch only the second load.
    drop(load_with_topic_progress(&state, &product.id).await);
    let watcher = session_sink();
    state.topic_broker.register_session("watcher", watcher.clone()).await;
    state
        .topic_broker
        .subscribe("watcher", &[work_product_topic(&product.id)])
        .await;

    drop(load_with_topic_progress(&state, &product.id).await);

    let quiet = tokio::time::timeout(std::time::Duration::from_millis(500), next_progress(&watcher)).await;
    assert!(quiet.is_err(), "an incremental refresh must not report a cold build");
}
