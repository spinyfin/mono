//! Callers of `record_worker_pr_completion` must not treat a stale-merge
//! binding mismatch as a real merge of the bound PR.

use super::*;
use boss_engine_storage_values::attention_kinds::ATTENTION_KIND_PR_BINDING_REPAIR;

const OWN: &str = "https://github.com/spinyfin/mono/pull/7";
const BOUND: &str = "https://github.com/spinyfin/mono/pull/8";

fn bind_other_pr(db: &WorkDb, chore_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore_id, BOUND],
        )
        .unwrap();
}

fn task_row(db: &WorkDb, chore_id: &str) -> boss_protocol::Task {
    match db.get_work_item(chore_id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task/chore, got {other:?}"),
    }
}

#[tokio::test]
async fn finalize_pr_transition_does_not_treat_a_binding_mismatch_as_merged() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    bind_other_pr(&db, &chore_id);
    let prior_status = task_row(&db, &chore_id).status;
    let TestHarness { handler, publisher, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));

    let outcome = handler
        .finalize_pr_transition(&execution_id, OWN.to_owned(), WorkerPrCompletionTarget::Done, "stop")
        .await;

    assert_eq!(outcome, StopOutcome::PrDetected { pr_url: OWN.to_owned() });
    let task = task_row(&db, &chore_id);
    assert_eq!(task.pr_url.as_deref(), Some(BOUND));
    assert_eq!(task.status, prior_status);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert!(db.get_merge_evidence(&chore_id).unwrap().is_none());
    assert!(
        db.list_attention_items_for_work_item(&chore_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == ATTENTION_KIND_PR_BINDING_REPAIR),
        "a merged own-branch PR that is not the bound PR must file attention"
    );
    let reasons: Vec<String> = publisher
        .publish_calls
        .lock()
        .await
        .iter()
        .map(|(_, _, _, reason)| reason.clone())
        .collect();
    assert!(
        reasons.iter().any(|reason| reason == "worker_pr_completed"),
        "mismatch must publish a non-merged reason, got {reasons:?}"
    );
    assert!(
        reasons.iter().all(|reason| reason != "worker_pr_merged"),
        "mismatch must not publish a merged reason, got {reasons:?}"
    );
    db.reconcile_stale_attention_signals().unwrap();
    assert!(
        db.list_attention_items_for_work_item(&chore_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == ATTENTION_KIND_PR_BINDING_REPAIR && item.status == "open"),
        "reconciliation must not clear a still-wrong binding just because pr_url is set"
    );
}

#[tokio::test]
async fn finalize_declared_delivery_does_not_treat_a_binding_mismatch_as_merged() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    bind_other_pr(&db, &chore_id);
    let prior_status = task_row(&db, &chore_id).status;
    let handler = TestHarness::new(
        db.clone(),
        StubPrDetector::ok_status(PrStatus::Merged { url: OWN.to_owned() }),
    )
    .handler;

    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;

    assert_eq!(outcome, StopOutcome::PrDetected { pr_url: OWN.to_owned() });
    let task = task_row(&db, &chore_id);
    assert_eq!(task.pr_url.as_deref(), Some(BOUND));
    assert_eq!(task.status, prior_status);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert!(db.get_merge_evidence(&chore_id).unwrap().is_none());
    assert!(
        db.list_attention_items_for_work_item(&chore_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == ATTENTION_KIND_PR_BINDING_REPAIR),
        "a merged own-branch PR that is not the bound PR must file attention"
    );
}
