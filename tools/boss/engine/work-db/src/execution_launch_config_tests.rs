use super::*;
use crate::test_support::{create_ready_chore_execution, create_test_product, open_db};
use boss_engine_effort::{SpawnResolutionInput, resolve_spawn_config};

fn run_model(db: &WorkDb, run_id: &str) -> Option<String> {
    db.cost_records_for_window(0, i64::MAX)
        .unwrap()
        .into_iter()
        .find(|record| record.run_id == run_id)
        .unwrap()
        .model
}

#[test]
fn agy_selected_model_is_recorded_on_execution_run_and_allocation() {
    for (effort, reasoning, expected) in [
        (EffortLevel::Small, ReasoningMode::Standard, "gemini-4-hydrogen-medium"),
        (EffortLevel::Large, ReasoningMode::Standard, "gemini-4-neon-xhigh"),
        (EffortLevel::Large, ReasoningMode::Investigation, "gemini-4-carbon-high"),
    ] {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let task = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(&product.id)
                    .name("Gemini selection")
                    .driver("agy")
                    .effort_level(effort)
                    .reasoning(reasoning)
                    .build(),
            )
            .unwrap();
        let execution = create_ready_chore_execution(&db, &task.id);
        let config = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .task_driver("agy")
                .effort_level(effort)
                .reasoning(reasoning)
                .build(),
        )
        .unwrap();
        assert_eq!(config.model, expected);
        let (_, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "repo",
                "lease",
                "workspace",
                "/tmp/gemini-selection",
            )
            .unwrap();
        db.record_execution_launch_config(&execution.id, &config.driver, &config.model, Some(effort))
            .unwrap();
        assert_eq!(
            db.get_execution(&execution.id).unwrap().model.as_deref(),
            Some(expected)
        );
        assert_eq!(run_model(&db, &run.id).as_deref(), Some(expected));
        assert_eq!(
            db.get_execution_driver_decision(&execution.id)
                .unwrap()
                .unwrap()
                .model
                .as_deref(),
            Some(expected)
        );
        db.record_execution_launch_config(&execution.id, "agy", "gemini-4-carbon-low", None)
            .unwrap();
        assert_eq!(run_model(&db, &run.id).as_deref(), Some(expected));
    }
}

#[test]
fn allocation_model_migration_preserves_older_decisions() {
    let (dir, db) = open_db();
    let product = create_test_product(&db);
    let task = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(&product.id)
                .name("Older allocation")
                .driver("agy")
                .build(),
        )
        .unwrap();
    let execution = create_ready_chore_execution(&db, &task.id);
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "ALTER TABLE execution_driver_decisions DROP COLUMN model;
        DELETE FROM schema_migrations WHERE id = '20261122_0432_execution_allocation_model';",
    )
    .unwrap();
    drop(conn);
    // Reopening exercises the real registry upgrade path rather than a duplicate SQL copy.
    let upgraded = WorkDb::open(dir.path().join("state.db")).unwrap();
    let decision = upgraded.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.driver.as_deref(), Some("agy"));
    assert_eq!(decision.model, None);
}

#[test]
fn launch_recording_preserves_observed_or_mixed_model_usage() {
    for observed in [Some("gemini-4-neon-high"), None] {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let task = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(&product.id)
                    .name("Observed Gemini usage")
                    .driver("agy")
                    .build(),
            )
            .unwrap();
        let execution = create_ready_chore_execution(&db, &task.id);
        let (_, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "repo",
                "lease",
                "workspace",
                "/tmp/gemini-selection",
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_runs SET model=?2,input_tokens=100 WHERE id=?1",
                params![run.id, observed],
            )
            .unwrap();
        db.record_execution_launch_config(&execution.id, "agy", "gemini-4-hydrogen-low", None)
            .unwrap();
        assert_eq!(run_model(&db, &run.id).as_deref(), observed);
    }
}
