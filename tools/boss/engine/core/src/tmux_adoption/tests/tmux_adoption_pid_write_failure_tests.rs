use super::*;

/// A durable write failure (e.g. a transient DB error) must not throw
/// away a pid that was positively observed on the live tmux pane:
/// `adopt_one` still completes in-memory adoption — the slot claim, the
/// `WorkerRegistry` pid map, and the `LiveWorkerState` entry are all
/// rebuilt — even though the durable `shell_pid` write itself failed.
#[tokio::test]
async fn write_failed_pid_snapshot_does_not_block_in_memory_adoption() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-1")
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, "tok-1", 4242)
            .unwrap()
    );

    let (tmux, _tmux_server) = fake_tmux(FakeTmuxServer {
        sessions: vec!["boss-worker-1".to_owned()],
        tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-1".to_owned())]),
        schemas: supported_schema("boss-worker-1"),
        pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
        ..Default::default()
    });
    let coordinator = coordinator_with_one_slot(db.clone());
    let spawner = RecordingSpawner::default();
    let sink = RecordingDispatchEventSink::new();

    // Make the durable shell-pid write fail while leaving reads working,
    // so `persist_observed_pane_pid` takes its `Err` branch.
    db.connect().unwrap().execute_batch("PRAGMA query_only = ON;").unwrap();

    let outcome = run_boot_time_adoption(
        &db,
        &tmux,
        &coordinator,
        &spawner,
        &NoopLiveWorkerConvergence,
        &sink,
        &FixedEngineOwnerProbe(Some(true)),
    )
    .await;

    db.connect().unwrap().execute_batch("PRAGMA query_only = OFF;").unwrap();

    assert_eq!(
        outcome.adopted_execution_ids,
        HashSet::from([execution_id.clone()]),
        "a failed durable write must not stop in-memory adoption from completing",
    );

    assert_eq!(spawner.registry.lookup(4321).as_deref(), Some(execution_id.as_str()));
    assert_eq!(
        spawner.registry.pane_for_run(&execution_id),
        Some(crate::worker_registry::RegisteredWorkerPane {
            slot_id: 1,
            tmux_hosted: false,
            tmux_session_name: Some("boss-worker-1".to_owned()),
        }),
    );
    let live_state = spawner
        .live_states
        .get(1)
        .expect("slot 1 must be registered despite the write failure");
    assert_eq!(live_state.run_id, execution_id);
    assert_eq!(live_state.shell_pid, 4321);
    assert!(
        coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id),
        "the pool slot claim must be rebuilt despite the write failure",
    );

    let events = sink.events_for(&execution_id).await;
    assert_eq!(
        events.len(),
        2,
        "the write-failure error event, then the adoption-ok event"
    );
    assert_eq!(events[0].outcome, Outcome::Error.as_str());
    assert_eq!(events[0].details["adoption_proceeded_without_write"], true);
    assert_eq!(events[1].outcome, Outcome::Ok.as_str());
    assert_eq!(events[1].details["observed_shell_pid"], 4321);
}
