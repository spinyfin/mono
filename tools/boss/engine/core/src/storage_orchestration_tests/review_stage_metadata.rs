//! The live-worker metadata the engine reports for a review run: the parallel
//! reviewers first, then the consolidating supervisor.

use boss_protocol::{ExecutionStatus, FinishExecutionRunInput, ReviewBatchMemberRole, ReviewBatchPhase, ReviewStage};

use super::{batch_input, raw, submit_review_report};
use crate::test_support::{create_test_chore_manual, create_test_product, open_db};
use crate::work::{ReviewBatchDispatch, WorkDb};

fn review_card(db: &WorkDb, product_id: &str, chore_id: &str) -> boss_protocol::Task {
    db.get_work_tree(product_id)
        .unwrap()
        .chores
        .into_iter()
        .find(|chore| chore.id == chore_id)
        .expect("chore present in work tree")
}

fn start_review_run(db: &WorkDb, execution_id: &str, slot: usize) -> boss_protocol::WorkRun {
    db.start_execution_run(
        execution_id,
        &format!("worker-{slot}"),
        "mono",
        &format!("lease-{slot}"),
        &format!("mono-agent-{slot:03}"),
        &format!("/tmp/mono-agent-{slot:03}"),
    )
    .unwrap()
    .1
}

/// The card and the live-worker metadata report the review stage the engine
/// recorded at launch: the parallel reviewers first, then the consolidating
/// supervisor — never the supervisor as one more reviewer.
#[test]
fn review_stage_reports_reviewers_then_consolidation() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "review target");
    raw(&db)
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/example/repo/pull/42' WHERE id = ?1",
            rusqlite::params![chore.id],
        )
        .unwrap();
    let ReviewBatchDispatch::Created { batch, executions } = db
        .create_pre_merge_review_batch(
            batch_input(chore.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    else {
        panic!("expected new batch")
    };
    let runs: Vec<_> = executions
        .iter()
        .enumerate()
        .map(|(slot, execution)| start_review_run(&db, &execution.id, slot))
        .collect();

    // Parallel reviewers running: the plain "reviewing" presentation.
    let card = review_card(&db, &product.id, &chore.id);
    assert!(card.ai_reviewing);
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewing"));
    assert_eq!(card.ai_review_stage.as_deref(), Some("reviewers"));
    assert_eq!(card.ai_review_badge.as_ref().unwrap().label, "AI reviewing…");
    for execution in &executions {
        let member = db.review_batch_member_for_execution(&execution.id).unwrap().unwrap();
        assert_eq!(member.role.stage(), ReviewStage::Reviewers);
    }

    // Both reviewers report and finish; the supervisor is queued, then runs.
    for (index, (execution, run)) in executions.iter().zip(&runs).enumerate() {
        submit_review_report(
            &db,
            &execution.id,
            &chore.id,
            &batch.id,
            "head-sha",
            &format!("report-{index}"),
        );
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&execution.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::Completed)
                .run_status("completed")
                .clear_workspace_lease(true)
                .build(),
        )
        .unwrap();
    }
    let supervisor = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .unwrap();
    let supervisor_execution = supervisor.execution_id.clone().unwrap();
    start_review_run(&db, &supervisor_execution, 9);

    let card = review_card(&db, &product.id, &chore.id);
    assert!(card.ai_reviewing);
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewing"));
    assert_eq!(card.ai_review_stage.as_deref(), Some("consolidation"));
    assert_eq!(
        card.ai_review_badge.as_ref().unwrap().label,
        "Consolidating review findings"
    );
    let execution = db.get_execution(&supervisor_execution).unwrap();
    let metadata = crate::live_worker_metadata::resolve(&db, &execution, "local");
    assert_eq!(metadata.review_stage.as_deref(), Some("consolidation"));
    let reviewer = db.get_execution(&executions[0].id).unwrap();
    let metadata = crate::live_worker_metadata::resolve(&db, &reviewer, "local");
    assert_eq!(metadata.review_stage.as_deref(), Some("reviewers"));
}
