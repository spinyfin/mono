use super::*;

#[tokio::test]
async fn tmux_adoption_backfills_old_rows_around_persisted_leases_and_preserves_readoption() {
    let (_dir, db) = open_db_arc();
    let old = start_local_run(&db, "worker-1");
    let saved = start_local_run(&db, "worker-2");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET persona = NULL, persona_lease_active = 0 WHERE execution_id = ?1",
            [&old],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET persona = 'Riker' WHERE execution_id = ?1",
            [&saved],
        )
        .unwrap();
    for (execution, session, token) in [
        (&old, "boss-worker-1", "old-token"),
        (&saved, "boss-worker-2", "saved-token"),
    ] {
        assert!(
            db.record_tmux_spawn_intent_for_execution(execution, "boss", session, token)
                .unwrap()
        );
    }
    let (tmux, _server) = fake_tmux(FakeTmuxServer {
        // Reverse session enumeration: DB created_at/id determines backfill order.
        sessions: vec!["boss-worker-2".into(), "boss-worker-1".into()],
        tokens: HashMap::from([
            ("boss-worker-1".into(), "old-token".into()),
            ("boss-worker-2".into(), "saved-token".into()),
        ]),
        schemas: HashMap::from([
            ("boss-worker-1".into(), TMUX_SESSION_SCHEMA.into()),
            ("boss-worker-2".into(), TMUX_SESSION_SCHEMA.into()),
        ]),
        pane_pids: HashMap::from([
            ("boss-worker-1".into(), "4321".into()),
            ("boss-worker-2".into(), "4322".into()),
        ]),
        ..Default::default()
    });
    let coordinator =
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(2), Arc::new(NoopCube), Arc::new(NoopRunner));
    let spawner = RecordingSpawner {
        live_states: LiveWorkerStateRegistry::with_work_db(db.clone()),
        ..Default::default()
    };
    let sink = RecordingDispatchEventSink::new();
    for _ in 0..2 {
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
        assert_eq!(
            outcome.adopted_execution_ids,
            HashSet::from([old.clone(), saved.clone()])
        );
        assert_eq!(spawner.live_states.get(1).unwrap().name, "Data");
        assert_eq!(spawner.live_states.get(2).unwrap().name, "Riker");
        spawner.live_states.set_held(&old, true);
    }
    assert!(spawner.live_states.get(1).unwrap().held);
    assert_eq!(db.list_runs(&old).unwrap()[0].persona.as_deref(), Some("Data"));
}

/// A name is leased only for a run that will actually be adopted. A row whose
/// session is schema-skewed (refused and reaped) or has no live session at all
/// must end the pass with no persona lease, or the name would leak.
#[tokio::test]
async fn tmux_sweep_leases_personas_only_for_adoptable_live_sessions() {
    let (_dir, db) = open_db_arc();
    let live = start_local_run(&db, "worker-1");
    let skewed = start_local_run(&db, "worker-2");
    let missing = start_local_run(&db, "worker-3");
    for (execution, session, token) in [
        (&live, "boss-worker-1", "live-token"),
        (&skewed, "boss-worker-2", "skewed-token"),
        (&missing, "boss-worker-3", "missing-token"),
    ] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_runs SET persona = NULL, persona_lease_active = 0 WHERE execution_id = ?1",
                [execution],
            )
            .unwrap();
        assert!(
            db.record_tmux_spawn_intent_for_execution(execution, "boss", session, token)
                .unwrap()
        );
    }
    let (tmux, _server) = fake_tmux(FakeTmuxServer {
        sessions: vec!["boss-worker-1".into(), "boss-worker-2".into()],
        tokens: HashMap::from([
            ("boss-worker-1".into(), "live-token".into()),
            ("boss-worker-2".into(), "skewed-token".into()),
        ]),
        // The skewed session carries no schema at all, which is always refused.
        schemas: HashMap::from([("boss-worker-1".into(), TMUX_SESSION_SCHEMA.into())]),
        pane_pids: HashMap::from([
            ("boss-worker-1".into(), "4321".into()),
            ("boss-worker-2".into(), "4322".into()),
        ]),
        ..Default::default()
    });
    let coordinator =
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(3), Arc::new(NoopCube), Arc::new(NoopRunner));
    let spawner = RecordingSpawner {
        live_states: LiveWorkerStateRegistry::with_work_db(db.clone()),
        ..Default::default()
    };
    let outcome = run_adoption_pass(
        &db,
        &tmux,
        &coordinator,
        &spawner,
        &NoopLiveWorkerConvergence,
        &RecordingDispatchEventSink::new(),
    )
    .await;

    assert_eq!(outcome.refused_schema_skew, 1);
    let leased = |execution: &str| -> bool {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM work_runs WHERE execution_id = ?1 AND persona_lease_active = 1)",
                [execution],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert!(leased(&live));
    assert!(!leased(&skewed), "a schema-skewed session must not hold a persona");
    assert!(!leased(&missing), "a row with no live session must not hold a persona");
}
