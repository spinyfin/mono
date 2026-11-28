use super::*;

#[tokio::test(start_paused = true)]
async fn event_retries_at_grace_deadline_and_only_releases_its_execution() {
    for live_backed in [false, true] {
        grace_deadline_case(live_backed).await;
    }
}

async fn grace_deadline_case(live_backed: bool) {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 0);
    let pool = coordinator.automation_worker_pool();
    let bus = boss_event_bus::EventBus::new();
    let subscription = bus.subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::ExecutionTerminal,
    ));
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    let handle = spawn_loop(
        db.clone(),
        live_states.clone(),
        coordinator.clone(),
        Arc::new(RecordingDispatchEventSink::new()),
        Duration::from_secs(3600),
        Arc::new(NoViewers),
        subscription,
    );
    // Finish the boot scan before adding claims: only event work can free them.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let target = create_execution(&db, &create_active_chore(&db, &product, "target"));
    let other = create_execution(&db, &create_active_chore(&db, &product, "other"));
    for id in [&target, &other] {
        let worker = pool.claim_worker(id, None).await.unwrap();
        if live_backed && id == &target {
            register_live_pane(
                &live_states,
                crate::coordinator::slot_id_from_worker_id(&worker).unwrap(),
                id,
            );
        }
        force_completed(&db, id);
        age_finished_at(&db, id, 0);
    }
    bus.publish(boss_event_bus::Event::ExecutionTerminal {
        execution_id: target.clone(),
        task_id: String::new(),
        host_id: String::new(),
        pool_claim: None,
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(pool.claims().await.len(), 2, "fresh terminal claims retain their grace");
    tokio::time::advance(Duration::from_secs(59)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(pool.claims().await.len(), 2, "no handback before grace deadline");
    // SQLite uses the epoch clock; advance its fixture timestamps alongside
    // Tokio's paused retry clock so both safety checks see grace expiry.
    for id in [&target, &other] {
        age_finished_at(&db, id, LEAK_GRACE_SECS);
    }
    live_states.release_slot_for_run(&target);
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        pool.claimed_execution_ids().await,
        std::collections::HashSet::from([other])
    );
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
}
