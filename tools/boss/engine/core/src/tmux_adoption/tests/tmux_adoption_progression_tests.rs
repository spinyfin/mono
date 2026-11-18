//! A re-adopted `progression` pane must come back in the confined RPC tier with
//! its neighbourhood captured, on both re-adoption paths. Registered as an
//! ordinary worker it would be unconfined whenever `worker_rpc_tier` is off.
use super::*;

/// Turn the run `start_local_run` made into a leaseless progression run and
/// record a live tmux identity for it.
fn make_progression(db: &WorkDb, execution_id: &str, session: &str, token: &str, pane_pid: i64) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'progression' WHERE id = ?1",
            [execution_id],
        )
        .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(execution_id, "boss", session, token)
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(execution_id, token, pane_pid)
            .unwrap()
    );
}

#[tokio::test]
async fn a_readopted_progression_pane_is_registered_confined_with_its_scope_captured() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    make_progression(&db, &execution_id, "boss-worker-1", "tok-p", 4242);
    let item = db.get_execution(&execution_id).unwrap().work_item_id;
    let (tmux, _server) = fake_tmux(FakeTmuxServer {
        sessions: vec!["boss-worker-1".to_owned()],
        tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-p".to_owned())]),
        schemas: supported_schema("boss-worker-1"),
        pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
        ..Default::default()
    });
    let coordinator = coordinator_with_one_slot(db.clone());
    let spawner = RecordingSpawner::default();
    let sink = RecordingDispatchEventSink::new();

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

    assert_eq!(outcome.adopted_execution_ids, HashSet::from([execution_id.clone()]));
    assert_eq!(
        spawner.registry.registered_tier(4321),
        Some(true),
        "the re-adopted progression pane is registered at the confined tier"
    );
    db.check_progression_scope(&execution_id, &item)
        .expect("the neighbourhood was captured before the pane was exposed");
}

#[tokio::test]
async fn a_progression_pane_handed_off_after_terminal_status_is_registered_confined() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    make_progression(&db, &execution_id, "boss-worker-1", "tok-term", 111);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET shell_pid = NULL WHERE execution_id = ?1",
            rusqlite::params![&execution_id],
        )
        .unwrap();
    db.mark_execution_orphaned(&execution_id, "test: engine wrongly inferred death")
        .unwrap();
    let item = db.get_execution(&execution_id).unwrap().work_item_id;
    let (tmux, _server) = fake_tmux(FakeTmuxServer {
        sessions: vec!["boss-worker-1".to_owned()],
        tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-term".to_owned())]),
        schemas: supported_schema("boss-worker-1"),
        pane_pids: HashMap::from([("boss-worker-1".to_owned(), "111".to_owned())]),
        ..Default::default()
    });
    let coordinator = coordinator_with_one_slot(db.clone());
    let spawner = RecordingSpawner::default();
    let convergence = RecordingConvergence::default();
    let sink = RecordingDispatchEventSink::new();

    let outcome = run_boot_time_adoption(
        &db,
        &tmux,
        &coordinator,
        &spawner,
        &convergence,
        &sink,
        &FixedEngineOwnerProbe(Some(true)),
    )
    .await;

    assert_eq!(outcome.terminal_handoffs, 1);
    assert_eq!(spawner.registry.registered_tier(111), Some(true));
    db.check_progression_scope(&execution_id, &item).unwrap();
}

#[tokio::test]
async fn an_ordinary_readopted_worker_stays_at_the_ordinary_tier() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-o")
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, "tok-o", 4242)
            .unwrap()
    );
    let (tmux, _server) = fake_tmux(FakeTmuxServer {
        sessions: vec!["boss-worker-1".to_owned()],
        tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-o".to_owned())]),
        schemas: supported_schema("boss-worker-1"),
        pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
        ..Default::default()
    });
    let coordinator = coordinator_with_one_slot(db.clone());
    let spawner = RecordingSpawner::default();
    let sink = RecordingDispatchEventSink::new();

    run_boot_time_adoption(
        &db,
        &tmux,
        &coordinator,
        &spawner,
        &NoopLiveWorkerConvergence,
        &sink,
        &FixedEngineOwnerProbe(Some(true)),
    )
    .await;

    assert_eq!(spawner.registry.registered_tier(4321), Some(false));
}

#[test]
fn an_unreadable_execution_row_registers_the_pane_confined_not_as_an_ordinary_worker() {
    let (_dir, db) = open_db_arc();
    let spawner = RecordingSpawner::default();

    register_adopted_pid(&spawner, &db, 777, "exec_that_does_not_exist");

    assert_eq!(
        spawner.registry.registered_tier(777),
        Some(true),
        "a failed kind lookup must fail closed to the confined tier, never the ordinary one"
    );
}
