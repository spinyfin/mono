use super::*;

use crate::semantic_progress::SemanticToolCondition;
use boss_protocol::{WorkerActivity, WorkerEvent};

fn pre_tool() -> WorkerEvent {
    WorkerEvent::PreToolUse {
        session_id: "s".into(),
        tool_name: "Bash".into(),
        tool_input: serde_json::Value::Null,
    }
}

fn stop() -> WorkerEvent {
    WorkerEvent::Stop {
        session_id: "s".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::Completed,
    }
}

fn session_start() -> WorkerEvent {
    WorkerEvent::SessionStart {
        session_id: "s".into(),
        source: boss_protocol::SessionStartSource::Startup,
        model: None,
    }
}

async fn adopt_local_run(db: &std::sync::Arc<WorkDb>, execution_id: &str) -> RecordingSpawner {
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
    let outcome = run_boot_time_adoption(
        db,
        &tmux,
        &coordinator,
        &spawner,
        &NoopLiveWorkerConvergence,
        &sink,
        &FixedEngineOwnerProbe(Some(true)),
    )
    .await;
    assert!(
        outcome.adopted_execution_ids.contains(execution_id),
        "expected tmux boot adoption to claim {execution_id}",
    );
    let _ = _tmux_server;
    spawner
}

fn stamp_tmux_identity(db: &WorkDb, execution_id: &str) {
    assert!(
        db.record_tmux_spawn_intent_for_execution(execution_id, "boss", "boss-worker-1", "tok-1")
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(execution_id, "tok-1", 4242)
            .unwrap()
    );
}

/// Engine restart: a worker that was mid-tool must come back `Working` from
/// the durable checkpoint, without treating a display timestamp as progress.
#[tokio::test]
async fn restart_seeds_in_flight_tool_condition_without_display_timestamp() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    stamp_tmux_identity(&db, &execution_id);
    db.record_semantic_progress(&execution_id, &pre_tool()).unwrap();

    let spawner = adopt_local_run(&db, &execution_id).await;
    let live_state = spawner.live_states.get(1).expect("slot 1 must be registered");
    assert_eq!(live_state.activity, WorkerActivity::Working);
    assert!(
        live_state.last_event_at.is_none(),
        "re-adoption must not seed last_event_at from the semantic checkpoint",
    );
    let seeded = spawner
        .live_states
        .semantic_progress_for_slot(1)
        .expect("checkpoint must seed SlotMeta");
    assert_eq!(seeded.tool_condition, SemanticToolCondition::InFlight);
}

#[tokio::test]
async fn restart_seeds_idle_tool_condition() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    stamp_tmux_identity(&db, &execution_id);
    db.record_semantic_progress(&execution_id, &pre_tool()).unwrap();
    db.record_semantic_progress(&execution_id, &stop()).unwrap();

    let spawner = adopt_local_run(&db, &execution_id).await;
    let live_state = spawner.live_states.get(1).unwrap();
    assert_eq!(live_state.activity, WorkerActivity::Idle);
    assert!(live_state.last_event_at.is_none());
    assert_eq!(
        spawner
            .live_states
            .semantic_progress_for_slot(1)
            .unwrap()
            .tool_condition,
        SemanticToolCondition::Idle,
    );
}

#[tokio::test]
async fn restart_of_legacy_null_row_stays_unknown_not_idle() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    stamp_tmux_identity(&db, &execution_id);
    assert!(
        db.get_run_semantic_progress_checkpoint(&execution_id)
            .unwrap()
            .is_none()
    );

    let spawner = adopt_local_run(&db, &execution_id).await;
    let live_state = spawner.live_states.get(1).unwrap();
    assert_eq!(
        live_state.activity,
        WorkerActivity::Spawning,
        "a legacy-null checkpoint must leave activity unknown, never idle",
    );
    assert!(live_state.last_event_at.is_none());
    assert!(spawner.live_states.semantic_progress_for_slot(1).is_none());
}

#[tokio::test]
async fn restart_after_session_start_only_stays_unknown() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    stamp_tmux_identity(&db, &execution_id);
    db.record_semantic_progress(&execution_id, &session_start()).unwrap();

    let spawner = adopt_local_run(&db, &execution_id).await;
    let live_state = spawner.live_states.get(1).unwrap();
    assert_eq!(live_state.activity, WorkerActivity::Spawning);
    assert!(live_state.last_event_at.is_none());
    assert_eq!(
        spawner
            .live_states
            .semantic_progress_for_slot(1)
            .unwrap()
            .tool_condition,
        SemanticToolCondition::Unknown,
    );
}
