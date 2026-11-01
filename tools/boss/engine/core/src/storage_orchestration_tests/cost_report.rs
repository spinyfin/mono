use super::raw;
use crate::test_support::{create_product, create_ready_chore_execution, create_test_chore, open_db};
use crate::work::WorkDb;
use boss_engine_effort::sol::{ASTRA_MODEL, SOL_MODEL};
use boss_protocol::WorkExecution;
use rusqlite::params;

fn measured_run(db: &WorkDb, execution: &WorkExecution, model: &str, tokens: i64, time: i64) {
    db.record_execution_launch_config(&execution.id, "codex", model, None)
        .unwrap();
    let run = db
        .create_run(
            boss_protocol::CreateRunInput::builder()
                .execution_id(&execution.id)
                .agent_id("worker")
                .status("completed")
                .build(),
        )
        .unwrap();
    let conn = raw(db);
    conn.execute(
        "UPDATE work_executions SET created_at=?2,started_at=?2,status='completed' WHERE id=?1",
        params![execution.id, time.to_string()],
    )
    .unwrap();
    conn.execute(
        "UPDATE work_runs SET model=?2,input_tokens=?3,output_tokens=0,created_at=?4 WHERE id=?1",
        params![run.id, model, tokens, time.to_string()],
    )
    .unwrap();
}

#[test]
fn report_joins_revisions_retries_and_findings_once_and_observes_only_first_ci() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let task = create_test_chore(&db, &product, "root");
    let first = create_ready_chore_execution(&db, &task.id);
    db.set_codex_sol_percentage(100).unwrap();
    raw(&db)
        .execute("UPDATE tasks SET pr_head_sha='stale' WHERE id=?1", [&task.id])
        .unwrap();
    assert_eq!(
        db.select_codex_sol_model(&first.id, true, ASTRA_MODEL).unwrap(),
        SOL_MODEL
    );
    measured_run(&db, &first, SOL_MODEL, 100, 10);
    let historical = create_ready_chore_execution(&db, &task.id);
    db.record_execution_launch_config(&historical.id, "codex", ASTRA_MODEL, None)
        .unwrap();
    let conn = raw(&db);
    conn.execute(
        "UPDATE work_executions SET created_at='5',started_at='5',status='completed' WHERE id=?1",
        [&historical.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET pr_url='https://github.com/org/repo/pull/1',status='in_review' WHERE id=?1",
        [&task.id],
    )
    .unwrap();
    drop(conn);
    // A stale poll from before enrollment must not enter the experiment.
    db.update_task_pr_poll_state(
        &task.id,
        crate::work::PrPollStateInput {
            pr_head_sha: Some("stale"),
            ci_required_state: "success",
            ..Default::default()
        },
    )
    .unwrap();
    db.update_task_pr_poll_state(
        &task.id,
        crate::work::PrPollStateInput {
            pr_head_sha: Some("first"),
            ci_required_state: "in_progress",
            ..Default::default()
        },
    )
    .unwrap();
    db.update_task_pr_poll_state(
        &task.id,
        crate::work::PrPollStateInput {
            pr_head_sha: Some("first"),
            ci_required_state: "fail",
            ..Default::default()
        },
    )
    .unwrap();
    db.update_task_pr_poll_state(
        &task.id,
        crate::work::PrPollStateInput {
            pr_head_sha: Some("first"),
            ci_required_state: "success",
            ..Default::default()
        },
    )
    .unwrap();
    let conn = raw(&db);
    conn.execute("UPDATE work_executions SET status='failed' WHERE id=?1", [&first.id])
        .unwrap();
    drop(conn);
    let retry = create_ready_chore_execution(&db, &task.id);
    assert_eq!(
        db.select_codex_sol_model(&retry.id, true, ASTRA_MODEL).unwrap(),
        ASTRA_MODEL
    );
    measured_run(&db, &retry, ASTRA_MODEL, 200, 20);
    // Queue creation order and random execution IDs must not move a
    // PR to the retry's arm when both launches have the same second.
    raw(&db)
        .execute(
            "UPDATE work_executions SET created_at='1',started_at='10' WHERE id=?1",
            [&retry.id],
        )
        .unwrap();
    let revision = create_test_chore(&db, &product, "revision");
    raw(&db)
        .execute(
            "UPDATE tasks SET kind='revision',parent_task_id=?2 WHERE id=?1",
            params![revision.id, task.id],
        )
        .unwrap();
    let revision_execution = create_ready_chore_execution(&db, &revision.id);
    db.select_codex_sol_model(&revision_execution.id, true, ASTRA_MODEL)
        .unwrap();
    measured_run(&db, &revision_execution, SOL_MODEL, 300, 30);
    for (id, findings, outcome) in [
        ("verdict", 3, crate::work::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS),
        ("duplicate", 5, crate::work::REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD),
        ("stale", 4, crate::work::REVIEW_GATE_OUTCOME_STALE_HEAD),
    ] {
        raw(&db)
            .execute(
                "INSERT INTO pr_review_verdicts(id,execution_id,work_item_id,findings_count,gate_outcome,created_at)
                VALUES (?1,?2,?3,?4,?5,'25')",
                params![id, retry.id, task.id, findings, outcome],
            )
            .unwrap();
    }
    db.mark_chore_pr_merged(&task.id, "https://github.com/org/repo/pull/1")
        .unwrap();
    raw(&db)
        .execute(
            "UPDATE sol_pr_outcomes SET merged_at='40' WHERE work_item_id=?1",
            [&task.id],
        )
        .unwrap();
    let records = db.sol_comparison_records().unwrap();
    assert_eq!(records.len(), 1);
    let report = crate::cost_report::sol::build_sol_report(&records);
    let sol = &report.arms[0];
    assert_eq!(sol.prs, 1);
    assert_eq!(sol.first_attempt_ci_passes, 0);
    assert_eq!(sol.ci_observed_prs, 1);
    assert_eq!(sol.revisions_before_merge, 1);
    assert_eq!(sol.review_findings, 3);
    assert_eq!(sol.tokens_per_merged_pr, Some(600.0));
    assert_eq!(report.arms[1].prs, 0);
}
