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
    let (sink, flush) = topic_progress_sink(&state, &product_id);
    let trace = tokio::task::spawn_blocking(move || {
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
    .unwrap();
    flush.await;
    trace
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

#[tokio::test(flavor = "multi_thread")]
async fn stage_entry_arrives_while_stage_is_held_open() {
    let (state, _dir) = crate::app::tests::test_server_state();
    let watcher = session_sink();
    state.topic_broker.register_session("watcher", watcher.clone()).await;
    state
        .topic_broker
        .subscribe("watcher", &[work_product_topic("prod")])
        .await;
    let (sink, _flush) = topic_progress_sink(&state, "prod");
    let (release, held) = std::sync::mpsc::channel();
    let build = tokio::task::spawn_blocking(move || {
        let mut trace = PopulationTrace::new("prod", "held-stage", None, std::time::Instant::now());
        trace.set_progress_sink(sink);
        trace.begin_cold_build();
        trace.enter_stage(crate::population_timing::segment::DB_TASK_RUNTIMES);
        held.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        trace.record_plain(crate::population_timing::segment::DB_TASK_RUNTIMES, 1.0);
    });
    assert!(matches!(
        next_progress(&watcher).await,
        FrontendEvent::BoardPopulationProgress {
            phase: BoardPopulationPhase::Started,
            ..
        }
    ));
    assert!(matches!(next_progress(&watcher).await,
        FrontendEvent::BoardPopulationProgress { stage: Some(stage), .. } if stage == "db.task_runtimes"));
    assert!(!build.is_finished(), "stage entry must arrive before completion");
    release.send(()).unwrap();
    build.await.unwrap();
}

#[tokio::test]
async fn writer_stage_frames_precede_the_snapshot() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let sink = session_sink();
    let mut trace = PopulationTrace::new("prod", "writer-stages", None, std::time::Instant::now());
    trace.set_progress_sink(Arc::new(|_| {}));
    trace.begin_cold_build();
    let (mut writer, reader) = tokio::io::duplex(4096);
    let mut lines = BufReader::new(reader).lines();
    for stage in [
        crate::population_timing::segment::SERIALIZE,
        crate::population_timing::segment::SOCKET_WRITE,
    ] {
        write_stage(&sink, &mut writer, "session", Some(&trace), stage)
            .await
            .unwrap();
        let line = lines.next_line().await.unwrap().unwrap();
        let event: FrontendEventEnvelope = serde_json::from_str(&line).unwrap();
        assert!(matches!(event.payload,
            FrontendEvent::BoardPopulationProgress { stage: Some(actual), .. } if actual == stage));
    }
    sink.write_frame(&mut writer, r#"{"payload":{"type":"work_tree"}}"#, "session")
        .await
        .unwrap();
    assert!(lines.next_line().await.unwrap().unwrap().contains("work_tree"));
}

/// `GetWorkTree` must enqueue every build progress push before its reply, or
/// a late `started` reaches the app after the snapshot and leaves the loaded
/// board refreshing.
#[tokio::test(flavor = "multi_thread")]
async fn get_work_tree_enqueues_all_progress_before_the_reply() {
    for delta in [false, true] {
        let (state, _dir) = crate::app::tests::test_server_state();
        let product = state
            .work_db
            .create_product(boss_protocol::CreateProductInput::builder().name("Ordered").build())
            .unwrap();
        let sink = session_sink();
        state.topic_broker.register_session("board", sink.clone()).await;
        state
            .topic_broker
            .subscribe("board", &[work_product_topic(&product.id)])
            .await;
        let ctx = Dispatch::builder()
            .server_state(state.clone())
            .work_db(state.work_db.clone())
            .sink(sink.clone())
            .session_id("board")
            .request_id("ordered-read")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build();
        work_items::handle_get_work_tree(
            ctx,
            FrontendRequest::GetWorkTree {
                product_id: product.id.clone(),
                fetch_seq: None,
                delta,
            },
        )
        .await;

        let mut progress = 0;
        let mut replies = 0;
        while let Ok(Some(envelope)) = tokio::time::timeout(std::time::Duration::from_millis(500), sink.next()).await {
            match envelope.payload {
                FrontendEvent::BoardPopulationProgress { phase, .. } => {
                    assert_eq!(
                        replies, 0,
                        "progress ({phase:?}) arrived after the reply (delta={delta})"
                    );
                    progress += 1;
                }
                FrontendEvent::WorkTree { .. } | FrontendEvent::WorkTreeUpdate { .. } => replies += 1,
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(replies, 1, "delta={delta}");
        assert!(progress >= 2, "a cold build reports Started and stages (delta={delta})");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn broad_input_rebuild_publishes_no_progress() {
    let (state, _dir) = crate::app::tests::test_server_state();
    let product = state
        .work_db
        .create_product(boss_protocol::CreateProductInput::builder().name("Loaded").build())
        .unwrap();
    drop(load_with_topic_progress(&state, &product.id).await);
    // Any product write journals a broad (item-less) change, which forces the
    // next refresh down the full-rebuild path even though a board is retained.
    state
        .work_db
        .create_product(boss_protocol::CreateProductInput::builder().name("Other").build())
        .unwrap();
    let watcher = session_sink();
    state.topic_broker.register_session("watcher", watcher.clone()).await;
    state
        .topic_broker
        .subscribe("watcher", &[work_product_topic(&product.id)])
        .await;

    let trace = load_with_topic_progress(&state, &product.id).await;
    drop(trace);

    let quiet = tokio::time::timeout(std::time::Duration::from_millis(500), next_progress(&watcher)).await;
    assert!(
        quiet.is_err(),
        "a rebuild of a retained board must not report a cold build"
    );
}
