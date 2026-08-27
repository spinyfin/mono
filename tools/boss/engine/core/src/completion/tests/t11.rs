//! Recovery ownership coverage for completed reviewer give-ups.

use super::*;

/// After the auto-nudge breaker trips, a completed give-up stays in Doing but
/// is reserved for review recovery rather than orphan-active implementation
/// redispatch.
#[tokio::test]
async fn pr_review_give_up_stays_doing_and_is_owned_by_review_recovery() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) = pr_review_exec_fixture(workspace.path(), None);
    let out_dir = tempdir().unwrap();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker())
        .with_structured_output_dir(out_dir.path().to_path_buf())
        .with_max_unproductive_nudges(1);

    assert!(matches!(
        handler.on_stop(&pr_review_exec_id).await,
        StopOutcome::ReviewPassAwaitingResult
    ));
    assert!(matches!(
        handler.on_stop(&pr_review_exec_id).await,
        StopOutcome::ReviewPassCompleted { .. }
    ));

    let task = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(task) | WorkItem::Task(task) => task,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::Active);
    assert!(
        db.list_attention_items(&pr_review_exec_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == REVIEW_RESULT_GIVEUP_ATTENTION_KIND)
    );
    let verdict = db.review_verdict_for_execution(&pr_review_exec_id).unwrap().unwrap();
    assert_eq!(verdict.gate_outcome, crate::work::REVIEW_GATE_OUTCOME_GAVE_UP);
    assert!(!db.list_orphan_active_candidates(0).unwrap().contains(&chore_id));
    assert!(
        db.list_dead_pr_review_candidates()
            .unwrap()
            .iter()
            .any(|candidate| candidate.work_item_id == chore_id && candidate.execution_id == pr_review_exec_id)
    );
}
