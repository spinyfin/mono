use super::*;

async fn wait_for_passes(db: &WorkDb, expected: u64) {
    // Candidate gathering uses the blocking pool. Yield without advancing the
    // paused safety clock until the real gather has returned to the loop.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while db
        .metrics_registry()
        .counter_value("terminal_work_sweep.passes")
        .unwrap_or(0)
        < expected
    {
        assert!(
            std::time::Instant::now() < deadline,
            "candidate gathering did not finish"
        );
        tokio::task::yield_now().await;
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn targeted_events_keep_independent_confirmation_clocks_and_teardown_ownership() {
    let (_dir, db, product) = setup();
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    let reaper = Arc::new(RecordingReaper::new(live_states.clone(), true));
    let teardown = Arc::new(TeardownRegistry::new());
    let bus = boss_event_bus::EventBus::new();
    let sub = bus.subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::ExecutionTerminal,
    ));
    let handle = spawn_loop(
        SweepContext {
            work_db: db.clone(),
            live_states: live_states.clone(),
            reaper: reaper.clone(),
            cube_client: Arc::new(RecordingCubeClient::default()),
            dispatch_events: Arc::new(RecordingDispatchEventSink::new()),
            teardown_registry: teardown.clone(),
        },
        Duration::from_secs(60),
        sub,
    );
    wait_for_passes(&db, 1).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut ids = Vec::new();
    for slot in 1..=3 {
        let item = create_active_chore(&db, &product, &format!("event-{slot}"));
        let id = create_execution(&db, &item);
        register_live_worker(&live_states, slot, &id, &item);
        force_execution_status(&db, &id, "completed");
        ids.push(id);
    }
    let guard = teardown.begin(&ids[2]);
    // Two burst events inside five seconds must each start their own clock.
    for id in &ids[..2] {
        bus.publish(boss_event_bus::Event::ExecutionTerminal {
            execution_id: id.clone(),
            task_id: String::new(),
            host_id: String::new(),
            pool_claim: None,
        });
    }
    wait_for_passes(&db, 3).await;
    assert!(reaper.reaped().is_empty());
    assert_eq!(
        db.metrics_registry().gauge_value("terminal_work_sweep.items_per_pass"),
        Some(1)
    );
    tokio::time::advance(Duration::from_secs(50)).await;
    wait_for_passes(&db, 4).await;
    assert!(
        reaper.reaped().is_empty(),
        "the periodic pass cannot shorten confirmation"
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    wait_for_passes(&db, 6).await;
    let mut reaped = reaper.reaped();
    reaped.sort();
    let mut expected = ids[..2].to_vec();
    expected.sort();
    assert_eq!(reaped, expected, "each event retries at its own confirmation deadline");
    assert!(live_states.snapshot().iter().any(|state| state.run_id == ids[2]));
    drop(guard);
    handle.abort();
}
