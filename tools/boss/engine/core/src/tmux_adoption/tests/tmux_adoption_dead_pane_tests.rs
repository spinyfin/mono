use super::*;

use crate::work::ExecutionStatus;

fn stamp_tmux_identity(db: &WorkDb, execution_id: &str, token: &str) {
    assert!(
        db.record_tmux_spawn_intent_for_execution(execution_id, "boss", "boss-worker-1", token)
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(execution_id, token, 111)
            .unwrap()
    );
}

fn dead_pane_server(token: &str) -> FakeTmuxServer {
    FakeTmuxServer {
        sessions: vec!["boss-worker-1".to_owned()],
        tokens: HashMap::from([("boss-worker-1".to_owned(), token.to_owned())]),
        schemas: supported_schema("boss-worker-1"),
        pane_pids: HashMap::from([("boss-worker-1".to_owned(), "111".to_owned())]),
        pane_dead: HashMap::from([("boss-worker-1".to_owned(), "1".to_owned())]),
        pane_dead_status: HashMap::from([("boss-worker-1".to_owned(), "127".to_owned())]),
        pane_output: HashMap::from([("boss-worker-1".to_owned(), "zsh: command not found: codex\n".to_owned())]),
        ..Default::default()
    }
}

/// A tmux session whose worker pane is dead is classified dead by the
/// sweep: the execution is terminalized once with the pane exit status
/// and last output, and a second pass does not re-adopt it.
#[tokio::test]
async fn dead_worker_pane_fails_once_and_is_not_readopted() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    stamp_tmux_identity(&db, &execution_id, "tok-dead");
    assert!(
        db.get_execution(&execution_id).unwrap().status.is_live(),
        "precondition: the run is still non-terminal",
    );

    let (tmux, tmux_server) = fake_tmux(dead_pane_server("tok-dead"));
    let coordinator = coordinator_with_one_slot(db.clone());
    let spawner = RecordingSpawner::default();
    let convergence = RecordingConvergence::default();
    let sink = RecordingDispatchEventSink::new();

    let first = run_boot_time_adoption(
        &db,
        &tmux,
        &coordinator,
        &spawner,
        &convergence,
        &sink,
        &FixedEngineOwnerProbe(Some(true)),
    )
    .await;

    assert!(
        first.adopted_execution_ids.is_empty(),
        "a dead pane must not rebuild live-worker bookkeeping"
    );
    assert_eq!(
        first.terminal_handoffs, 0,
        "a dead pane must not be handed to re-adoption"
    );
    assert_eq!(first.dead_panes, 1);
    assert_eq!(
        tmux_server.killed_sessions.lock().unwrap().as_slice(),
        &["boss-worker-1".to_owned()],
        "the retained remain-on-exit session is token-verified-killed after the observation",
    );
    assert!(
        spawner.live_states.snapshot().is_empty(),
        "dead pane must not register a live-state slot",
    );
    assert!(convergence.calls.lock().unwrap().is_empty());

    let after = db.get_execution(&execution_id).unwrap();
    assert!(
        after.status.is_terminal(),
        "the execution must reach a terminal state exactly once, got {}",
        after.status
    );
    let run = db.list_runs(&execution_id).unwrap().pop().expect("run row");
    let recorded = run.error_text.or(run.result_summary).unwrap_or_default();
    assert!(
        recorded.contains("pane_dead_status=127"),
        "reason must include the pane exit status, got {recorded:?}"
    );
    assert!(
        recorded.contains("command not found: codex"),
        "reason must include the dead pane last output, got {recorded:?}"
    );

    let second = run_adoption_pass(&db, &tmux, &coordinator, &spawner, &convergence, &sink).await;
    assert!(second.adopted_execution_ids.is_empty());
    assert_eq!(second.terminal_handoffs, 0);
    assert!(
        convergence.calls.lock().unwrap().is_empty(),
        "a second sweep must not re-adopt the already-terminal dead pane",
    );
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        after.status,
        "the terminal status must not flap",
    );
}

/// An already-orphaned execution whose retained pane is dead must stay
/// terminal: session existence under remain-on-exit is not evidence of a
/// live worker.
#[tokio::test]
async fn orphaned_execution_with_dead_pane_is_not_readopted() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    stamp_tmux_identity(&db, &execution_id, "tok-orphan-dead");
    db.mark_execution_orphaned(&execution_id, "test: inferred death")
        .unwrap();

    let (tmux, _tmux_server) = fake_tmux(dead_pane_server("tok-orphan-dead"));
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

    assert_eq!(outcome.dead_panes, 1);
    assert_eq!(outcome.terminal_handoffs, 0);
    assert!(outcome.adopted_execution_ids.is_empty());
    assert!(convergence.calls.lock().unwrap().is_empty());
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned,
    );
    let run = db.list_runs(&execution_id).unwrap().pop().expect("run row");
    let recorded = run.error_text.or(run.result_summary).unwrap_or_default();
    assert!(
        recorded.contains("pane_dead_status=127"),
        "already-orphaned run must still record the pane exit status, got {recorded:?}"
    );
    assert!(
        recorded.contains("command not found: codex"),
        "already-orphaned run must still record the dead pane last output, got {recorded:?}"
    );
}

/// A Codex execution re-adopted after an engine restart keeps driver Codex,
/// even when live resolution from worker id / pool policy would pick Claude.
#[tokio::test]
async fn codex_launch_config_survives_tmux_adoption() {
    assert_adopted_driver("codex", "OpenAI Codex").await;
}

#[tokio::test]
async fn unregistered_launch_config_is_not_relabelled_by_tmux_adoption() {
    assert_adopted_driver("removed-driver", "Unknown driver").await;
}

async fn assert_adopted_driver(driver: &str, expected_label: &str) {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    stamp_tmux_identity(&db, &execution_id, "tok-codex");
    db.record_execution_launch_config(&execution_id, driver, "test-model", None)
        .unwrap();

    let (tmux, _tmux_server) = fake_tmux(FakeTmuxServer {
        sessions: vec!["boss-worker-1".to_owned()],
        tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-codex".to_owned())]),
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
    let live_state = spawner.live_states.get(1).expect("slot 1 must be registered");
    assert_eq!(
        live_state.model, expected_label,
        "tmux adoption must keep the driver recorded at spawn, got {}",
        live_state.model
    );
    assert!(
        !spawner.live_states.awaiting_input_capable(1),
        "a re-adopted Codex worker must not be paintable as awaiting input",
    );
}

#[tokio::test]
async fn completed_execution_keeps_its_diagnostics_when_retained_pane_is_dead() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    stamp_tmux_identity(&db, &execution_id, "tok-completed");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![&execution_id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET error_text = 'original diagnostic' WHERE execution_id = ?1",
            rusqlite::params![&execution_id],
        )
        .unwrap();
    let (tmux, _) = fake_tmux(dead_pane_server("tok-completed"));
    let outcome = run_boot_time_adoption(
        &db,
        &tmux,
        &coordinator_with_one_slot(db.clone()),
        &RecordingSpawner::default(),
        &NoopLiveWorkerConvergence,
        &RecordingDispatchEventSink::new(),
        &FixedEngineOwnerProbe(Some(true)),
    )
    .await;
    assert_eq!(outcome.dead_panes, 1);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(
        db.list_runs(&execution_id).unwrap()[0].error_text.as_deref(),
        Some("original diagnostic")
    );
}

#[tokio::test]
async fn older_dead_pane_diagnostics_only_update_the_token_matched_run() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    stamp_tmux_identity(&db, &execution_id, "tok-old");
    db.mark_execution_orphaned(&execution_id, "inferred death").unwrap();
    let old_run_id = db.list_runs(&execution_id).unwrap()[0].id.clone();
    db.connect().unwrap().execute(
        "INSERT INTO work_runs (id, execution_id, agent_id, status, error_text, created_at, host_id, tmux_spawn_token)
         VALUES ('run-newer', ?1, 'worker-1', 'failed', 'newer diagnostic', '9999999999', 'local', 'tok-new')",
        rusqlite::params![&execution_id],
    ).unwrap();
    let (tmux, _) = fake_tmux(dead_pane_server("tok-old"));
    let mut outcome = TmuxAdoptionOutcome::default();
    super::super::dead_pane::reconcile_dead_worker_pane(
        &db,
        &tmux,
        &RecordingDispatchEventSink::new(),
        &execution_id,
        "boss-worker-1",
        "tok-old",
        Some("127".into()),
        Some("old pane output".into()),
        &mut outcome,
    )
    .await;
    let runs = db.list_runs(&execution_id).unwrap();
    assert!(
        runs.iter()
            .find(|run| run.id == old_run_id)
            .unwrap()
            .error_text
            .as_deref()
            .unwrap()
            .contains("old pane output")
    );
    assert_eq!(
        runs.iter()
            .find(|run| run.id == "run-newer")
            .unwrap()
            .error_text
            .as_deref(),
        Some("newer diagnostic")
    );
}
