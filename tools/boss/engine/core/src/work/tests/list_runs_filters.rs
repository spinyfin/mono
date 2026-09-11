//! `WorkDb::list_runs` returns every `work_runs` row for an execution.

use super::*;

/// A completed run under a still-`running` execution, with `model` left
/// NULL, must still be returned. The CLI used to hide this shape by skipping
/// `ListRuns` for non-terminal parents; the store itself never filtered it.
#[test]
fn list_runs_includes_completed_row_under_running_execution_with_null_model() {
    let db = WorkDb::open(temp_db_path("list-runs-null-model")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Investigate");
    let running = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let completed = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();

    let running_transcript = "/tmp/running-exec/transcripts/updates.jsonl";
    let running_run = db
        .create_run(
            CreateRunInput::builder()
                .execution_id(running.id.clone())
                .agent_id("worker-2")
                .status("completed")
                .transcript_path(running_transcript)
                .started_at("1000")
                .finished_at("2000")
                .build(),
        )
        .unwrap();
    let completed_run = db
        .create_run(
            CreateRunInput::builder()
                .execution_id(completed.id.clone())
                .agent_id("worker-1")
                .status("completed")
                .transcript_path("/tmp/completed-exec/transcripts/updates.jsonl")
                .started_at("900")
                .finished_at("1100")
                .build(),
        )
        .unwrap();

    {
        let conn = db.connect().unwrap();
        let model: Option<String> = conn
            .query_row("SELECT model FROM work_runs WHERE id = ?1", [&running_run.id], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(model.is_none(), "fixture must leave work_runs.model NULL");
    }

    let running_runs = db.list_runs(&running.id).unwrap();
    assert_eq!(running_runs.len(), 1);
    assert_eq!(running_runs[0].id, running_run.id);
    assert_eq!(running_runs[0].transcript_path.as_deref(), Some(running_transcript));

    let completed_runs = db.list_runs(&completed.id).unwrap();
    assert_eq!(completed_runs.len(), 1);
    assert_eq!(completed_runs[0].id, completed_run.id);
}
