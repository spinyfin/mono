//! Migration, query, and legacy-null coverage for the per-run semantic
//! progress checkpoint.

use super::*;

use boss_engine_storage_values::SemanticToolCondition;
use boss_protocol::WorkerEvent;

fn started_execution(db: &WorkDb) -> String {
    let product = create_test_product(db);
    let chore = create_test_chore(db, product.id.clone(), "Cleanup");
    let execution = create_ready_chore_execution(db, chore.id.clone());
    db.start_execution_run(
        &execution.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
    execution.id
}

fn pre_tool() -> WorkerEvent {
    WorkerEvent::PreToolUse {
        session_id: "s".into(),
        tool_name: "Bash".into(),
        tool_input: serde_json::Value::Null,
    }
}

fn post_tool() -> WorkerEvent {
    WorkerEvent::PostToolUse {
        session_id: "s".into(),
        tool_name: "Bash".into(),
        tool_input: serde_json::Value::Null,
        tool_response: serde_json::Value::Null,
    }
}

fn session_start() -> WorkerEvent {
    WorkerEvent::SessionStart {
        session_id: "s".into(),
        source: boss_protocol::SessionStartSource::Startup,
        model: None,
    }
}

#[test]
fn semantic_progress_query_round_trips_and_legacy_null_is_none() {
    let db = WorkDb::open(temp_db_path("semantic-progress-query")).unwrap();
    let execution_id = started_execution(&db);

    assert!(
        db.get_run_semantic_progress_checkpoint(&execution_id)
            .unwrap()
            .is_none(),
        "a run that has never seen a driver event has no checkpoint",
    );

    db.record_semantic_progress(&execution_id, &pre_tool()).unwrap();
    let in_flight = db
        .get_run_semantic_progress_checkpoint(&execution_id)
        .unwrap()
        .expect("pre-tool-use must persist a checkpoint");
    assert_eq!(in_flight.tool_condition, SemanticToolCondition::InFlight);
    assert!(
        in_flight.progress_at.ends_with('Z'),
        "progress_at must be an ISO-8601 UTC stamp, got {}",
        in_flight.progress_at
    );

    db.record_semantic_progress(&execution_id, &post_tool()).unwrap();
    let idle = db.get_run_semantic_progress_checkpoint(&execution_id).unwrap().unwrap();
    assert_eq!(idle.tool_condition, SemanticToolCondition::Idle);
    assert!(idle.progress_at >= in_flight.progress_at);
}

#[test]
fn session_start_does_not_coerce_legacy_null_to_idle() {
    let db = WorkDb::open(temp_db_path("semantic-progress-session-start")).unwrap();
    let execution_id = started_execution(&db);

    db.record_semantic_progress(&execution_id, &session_start()).unwrap();
    let checkpoint = db
        .get_run_semantic_progress_checkpoint(&execution_id)
        .unwrap()
        .expect("session start is driver-originated progress time");
    assert_eq!(
        checkpoint.tool_condition,
        SemanticToolCondition::Unknown,
        "unknown must never be coerced to idle by a non-tool-state event",
    );
}

#[test]
fn record_semantic_progress_returns_false_when_the_execution_has_no_run_row() {
    let db = WorkDb::open(temp_db_path("semantic-progress-missing-run")).unwrap();
    let recorded = db.record_semantic_progress("exec_missing", &pre_tool()).unwrap();
    assert!(!recorded, "missing run must be a benign no-op, not an error");
}

#[test]
fn notification_does_not_touch_a_previously_established_tool_condition() {
    let db = WorkDb::open(temp_db_path("semantic-progress-notification")).unwrap();
    let execution_id = started_execution(&db);

    db.record_semantic_progress(&execution_id, &pre_tool()).unwrap();
    let notification = WorkerEvent::Notification {
        session_id: "s".into(),
        message: "guard-trace replay".into(),
    };
    db.record_semantic_progress(&execution_id, &notification).unwrap();

    let checkpoint = db.get_run_semantic_progress_checkpoint(&execution_id).unwrap().unwrap();
    assert_eq!(
        checkpoint.tool_condition,
        SemanticToolCondition::InFlight,
        "a Notification must not durably clear an in-flight tool",
    );
}

#[test]
fn unfinished_prompt_confirmation_survives_restart_but_legacy_workers_are_exempt() {
    let (_dir, path) = disk_db_path("prompt-confirmation-restart");
    let db = WorkDb::open(path.clone()).unwrap();
    let legacy = started_execution(&db);
    let pending = started_execution(&db);
    assert!(!db.prompt_confirmation_pending(&legacy).unwrap());
    db.begin_prompt_confirmation(&pending).unwrap();
    db.record_semantic_progress(&pending, &session_start()).unwrap();
    drop(db);
    let db = WorkDb::open(path.clone()).unwrap();
    assert!(!db.prompt_confirmation_pending(&legacy).unwrap());
    assert!(db.prompt_confirmation_pending(&pending).unwrap());
    assert!(!db.prompt_acceptance_recorded(&pending).unwrap());
    db.record_semantic_progress(&pending, &pre_tool()).unwrap();
    assert!(!db.prompt_confirmation_pending(&pending).unwrap());
    assert!(db.prompt_acceptance_recorded(&pending).unwrap());
}

#[test]
fn confirmation_success_is_durable_and_cannot_be_rearmed_by_a_spawn_retry() {
    let (_dir, path) = disk_db_path("prompt-confirmation-success");
    let db = WorkDb::open(path.clone()).unwrap();
    let execution = started_execution(&db);
    db.begin_prompt_confirmation(&execution).unwrap();
    db.finish_prompt_confirmation(&execution).unwrap();
    drop(db);
    let db = WorkDb::open(path.clone()).unwrap();
    db.begin_prompt_confirmation(&execution).unwrap();
    assert!(!db.prompt_confirmation_pending(&execution).unwrap());
    assert!(db.prompt_acceptance_recorded(&execution).unwrap());
}

#[test]
fn upgrading_existing_runs_does_not_arm_prompt_confirmation() {
    let (_dir, path) = disk_db_path("prompt-confirmation-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let execution = started_execution(&db);
    db.record_semantic_progress(&execution, &session_start()).unwrap();
    db.connect()
        .unwrap()
        .execute_batch(
            "ALTER TABLE work_runs DROP COLUMN prompt_confirmed;
         DELETE FROM schema_migrations WHERE id = '20261110_2300_prompt_confirmation';",
        )
        .unwrap();
    drop(db);
    let db = WorkDb::open(path.clone()).unwrap();
    assert!(!db.prompt_confirmation_pending(&execution).unwrap());
    assert!(!db.prompt_acceptance_recorded(&execution).unwrap());
}
