//! The merge poller's stalled-reviewer fallback, run against a real `WorkDb`:
//! when a review pass died or timed out the sweep re-fires a replacement
//! review (restoring the reviewing state), parks on the churn guard, and never
//! double-dispatches over a queued or running pass.

use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus, Task, TaskStatus, WorkItem};
use rusqlite::params;
use tempfile::tempdir;

use super::{now_string, raw};
use crate::execution_publisher::NoopExecutionPublisher;
use crate::test_support::{create_test_chore_manual, create_test_product_with_repo};
use crate::work::{
    FakePrStateChecker, PrOpenState, REVIEW_GATE_OUTCOME_COMPLETED_CLEAN, REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD,
    REVIEW_GATE_OUTCOME_GAVE_UP, ReviewVerdictInput, WorkDb, WorkItemPatch, WorkerPrCompletionTarget,
};

/// Create a `pr_review` execution on `work_item_id` with the given status, then
/// force its `created_at` to `created_at_secs` (a unix-seconds string) so the
/// stale-cutoff arm is deterministic regardless of wall-clock timing.
fn pr_review_execution(db: &WorkDb, work_item_id: &str, status: ExecutionStatus, created_at_secs: &str) -> String {
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id.to_owned())
                .kind(ExecutionKind::PrReview)
                .status(status)
                .build(),
        )
        .unwrap();
    raw(db)
        .execute(
            "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
            params![exec.id, created_at_secs],
        )
        .unwrap();
    exec.id
}

/// Seed a completed `pr_review` verdict row directly: the durable row the
/// completion path writes atomically with a finished review pass.
fn insert_completed_pr_review_verdict(db: &WorkDb, execution_id: &str, work_item_id: &str, gate_outcome: &'static str) {
    raw(db)
        .execute(
            "INSERT INTO pr_review_verdicts (
                id, execution_id, work_item_id, head_sha, findings_count,
                revision_warranted, gate_outcome, revision_task_id, created_at
             ) VALUES (?1, ?2, ?3, 'reviewed-head', 0, ?4, ?5, NULL, ?6)",
            params![
                format!("rvv_{execution_id}"),
                execution_id,
                work_item_id,
                (gate_outcome == REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD) as i64,
                gate_outcome,
                now_string(),
            ],
        )
        .unwrap();
}

fn task_of(item: WorkItem) -> Task {
    match item {
        WorkItem::Chore(task) | WorkItem::Task(task) => task,
        other => panic!("expected a task/chore work item, got {other:?}"),
    }
}

#[tokio::test]
async fn stalled_reviewer_fallback_refires_and_restores_reviewing_state() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1766";
    let product = create_test_product_with_repo(&db, "review-recovery", Some("https://github.com/foo/bar"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "failed-review");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(pr.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Failed)
            .build(),
    )
    .unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let publisher = NoopExecutionPublisher;
    let mut outcome = crate::merge_poller::SweepOutcome::default();
    crate::merge_poller::sweep_stalled_reviewer(
        &db,
        &publisher,
        crate::merge_poller::StalledReview {
            task_id: &chore.id,
            product_id: &product.id,
            pr_url: pr,
            review_result_written: false,
            skip_unbatched_review_holds: false,
        },
        &checker,
        &mut outcome,
    )
    .await;

    assert_eq!(outcome.reviewer_fallback_advanced, 0);
    assert_eq!(outcome.reviewer_fallback_review_refired, 1);
    let executions = db.list_executions(Some(&chore.id)).unwrap();
    let refired = executions
        .iter()
        .find(|execution| execution.kind == ExecutionKind::PrReview && execution.status == ExecutionStatus::Ready)
        .expect("fresh review execution");
    db.start_execution_run(
        &refired.id,
        "review-worker",
        "review-host",
        "review-lease",
        "review-workspace",
        "/tmp/review-workspace",
    )
    .unwrap();

    let tree = db.get_work_tree(&product.id).unwrap();
    let card = tree.chores.iter().find(|card| card.id == chore.id).unwrap();
    assert_eq!(card.status, TaskStatus::Active);
    assert!(card.ai_reviewing);
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewing"));

    // A clean verdict must cover the head currently observed on the PR.
    raw(&db).execute(
        "UPDATE tasks SET pr_head_sha = 'reviewed-head', ci_required_state = 'success', pr_mergeable_state = 'mergeable' WHERE id = ?1",
        [&chore.id],
    ).unwrap();
    db.record_worker_pr_completion(
        &refired.id,
        pr,
        None,
        None,
        WorkerPrCompletionTarget::InReview,
        Some(ReviewVerdictInput {
            head_sha: Some("reviewed-head".to_owned()),
            findings_count: 0,
            revision_warranted: false,
            gate_outcome: REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
        }),
    )
    .unwrap()
    .expect("live review completion");

    let tree = db.get_work_tree(&product.id).unwrap();
    let card = tree.chores.iter().find(|card| card.id == chore.id).unwrap();
    assert_eq!(card.status, TaskStatus::InReview);
    assert!(!card.ai_reviewing);
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewed_all_clear"));
    assert!(
        db.list_attention_items_for_work_item(&chore.id)
            .unwrap()
            .iter()
            .any(|attention| attention.kind == crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND)
    );
}

#[tokio::test]
async fn stalled_running_reviewer_stays_doing_without_double_dispatch() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1767";
    let product = create_test_product_with_repo(&db, "timed-out-review", Some("https://github.com/foo/bar"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "timed-out-review");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(pr.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Running)
            .build(),
    )
    .unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let publisher = NoopExecutionPublisher;
    let mut outcome = crate::merge_poller::SweepOutcome::default();
    crate::merge_poller::sweep_stalled_reviewer(
        &db,
        &publisher,
        crate::merge_poller::StalledReview {
            task_id: &chore.id,
            product_id: &product.id,
            pr_url: pr,
            review_result_written: false,
            skip_unbatched_review_holds: false,
        },
        &checker,
        &mut outcome,
    )
    .await;

    assert_eq!(outcome.reviewer_fallback_advanced, 0);
    assert_eq!(outcome.reviewer_fallback_review_refired, 0);
    let task = task_of(db.get_work_item(&chore.id).unwrap());
    assert_eq!(task.status, TaskStatus::Active);
    assert!(
        db.list_attention_items_for_work_item(&chore.id).unwrap().is_empty(),
        "a failed immediate re-fire must not create a duplicate attention item on every sweep"
    );
}

/// Seed enough completed give-ups to trip the reviewer-fallback churn guard:
/// the sweep must not re-fire, and must file an open `churn_guard_parked` item.
#[tokio::test]
async fn stalled_reviewer_churn_guard_parks_without_refire() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1768";
    let product = create_test_product_with_repo(&db, "churn-park-review", Some("https://github.com/foo/bar"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "churn-park-review");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(pr.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    for i in 0..crate::work::ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
        let exec_id = pr_review_execution(&db, &chore.id, ExecutionStatus::Completed, &(now - i).to_string());
        insert_completed_pr_review_verdict(&db, &exec_id, &chore.id, REVIEW_GATE_OUTCOME_GAVE_UP);
    }
    let before = db.list_executions(Some(&chore.id)).unwrap().len();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let publisher = NoopExecutionPublisher;
    let mut outcome = crate::merge_poller::SweepOutcome::default();
    crate::merge_poller::sweep_stalled_reviewer(
        &db,
        &publisher,
        crate::merge_poller::StalledReview {
            task_id: &chore.id,
            product_id: &product.id,
            pr_url: pr,
            review_result_written: false,
            skip_unbatched_review_holds: false,
        },
        &checker,
        &mut outcome,
    )
    .await;

    assert_eq!(outcome.reviewer_fallback_review_refired, 0);
    assert_eq!(
        db.list_executions(Some(&chore.id)).unwrap().len(),
        before,
        "churn guard must not mint a replacement pr_review"
    );
    assert!(
        db.list_attention_items_for_work_item(&chore.id)
            .unwrap()
            .iter()
            .any(
                |attention| attention.kind == crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND
                    && attention.status == "open"
            ),
        "expected an open churn_guard_parked attention item"
    );
}

/// One short of the churn threshold must still re-fire a replacement review.
#[tokio::test]
async fn stalled_reviewer_churn_guard_below_threshold_still_refires() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1769";
    let product = create_test_product_with_repo(&db, "churn-below-review", Some("https://github.com/foo/bar"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "churn-below-review");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(pr.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    for i in 0..(crate::work::ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD - 1) {
        let exec_id = pr_review_execution(&db, &chore.id, ExecutionStatus::Completed, &(now - i).to_string());
        insert_completed_pr_review_verdict(&db, &exec_id, &chore.id, REVIEW_GATE_OUTCOME_GAVE_UP);
    }

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let publisher = NoopExecutionPublisher;
    let mut outcome = crate::merge_poller::SweepOutcome::default();
    crate::merge_poller::sweep_stalled_reviewer(
        &db,
        &publisher,
        crate::merge_poller::StalledReview {
            task_id: &chore.id,
            product_id: &product.id,
            pr_url: pr,
            review_result_written: false,
            skip_unbatched_review_holds: false,
        },
        &checker,
        &mut outcome,
    )
    .await;

    assert_eq!(outcome.reviewer_fallback_review_refired, 1);
    assert!(
        db.list_executions(Some(&chore.id))
            .unwrap()
            .iter()
            .any(|execution| execution.kind == ExecutionKind::PrReview && execution.status == ExecutionStatus::Ready)
    );
}

/// A `ready` pr_review older than the stale cutoff is already queued, not
/// dead. Sweeping it twice must not mint a second execution, bump the
/// re-fire counter, or accumulate `pr_review_died_without_findings` items.
#[tokio::test]
async fn stale_queued_reviewer_is_not_refired_on_repeated_sweeps() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1770";
    let product = create_test_product_with_repo(&db, "queued-review", Some("https://github.com/foo/bar"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "queued-review");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(pr.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let now: u64 = now_string().parse().unwrap();
    let ready_id = pr_review_execution(&db, &chore.id, ExecutionStatus::Ready, &(now - 7200).to_string());

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let publisher = NoopExecutionPublisher;
    for _ in 0..2 {
        let mut outcome = crate::merge_poller::SweepOutcome::default();
        crate::merge_poller::sweep_stalled_reviewer(
            &db,
            &publisher,
            crate::merge_poller::StalledReview {
                task_id: &chore.id,
                product_id: &product.id,
                pr_url: pr,
                review_result_written: false,
                skip_unbatched_review_holds: false,
            },
            &checker,
            &mut outcome,
        )
        .await;
        assert_eq!(
            outcome.reviewer_fallback_review_refired, 0,
            "returning the existing ready pr_review is not a re-fire"
        );
    }

    let reviews: Vec<_> = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .filter(|execution| execution.kind == ExecutionKind::PrReview)
        .collect();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].id, ready_id);
    assert_eq!(reviews[0].status, ExecutionStatus::Ready);
    let died = db
        .list_attention_items_for_work_item(&chore.id)
        .unwrap()
        .into_iter()
        .filter(|attention| attention.kind == crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND)
        .count();
    assert_eq!(
        died, 0,
        "a queued review is not a death; two sweeps must leave no died-without-findings items"
    );
}

/// With review-batch fan-out, a previous cycle's durable verdict must not
/// advance the task to in_review while no live batch covers the new head.
#[tokio::test]
async fn stalled_reviewer_does_not_skip_a_new_head_when_fanout_owns_the_hold() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1771";
    let product = create_test_product_with_repo(&db, "prior-cycle-hold", Some("https://github.com/foo/bar"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "prior-cycle-hold");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(pr.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let exec_id = pr_review_execution(&db, &chore.id, ExecutionStatus::Completed, &now_string());
    insert_completed_pr_review_verdict(&db, &exec_id, &chore.id, REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let publisher = NoopExecutionPublisher;
    let mut outcome = crate::merge_poller::SweepOutcome::default();
    crate::merge_poller::sweep_stalled_reviewer(
        &db,
        &publisher,
        crate::merge_poller::StalledReview {
            task_id: &chore.id,
            product_id: &product.id,
            pr_url: pr,
            review_result_written: true,
            skip_unbatched_review_holds: true,
        },
        &checker,
        &mut outcome,
    )
    .await;

    assert_eq!(outcome.reviewer_fallback_advanced, 0);
    let task = task_of(db.get_work_item(&chore.id).unwrap());
    assert_eq!(
        task.status,
        TaskStatus::Active,
        "a prior-cycle verdict must not skip review of a new PendingReview hold"
    );
}
