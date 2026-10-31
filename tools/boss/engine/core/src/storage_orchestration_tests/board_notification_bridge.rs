use crate::work::ReviewVerdictObservation;
//! The board notification bridge, driven by committed `WorkDb` changes: a
//! committed dependency edge reaches the board publisher, and a clean review
//! verdict invalidates the card it advances.

use std::path::PathBuf;

use boss_protocol::{
    CreateExecutionInput, ExecutionKind, ExecutionStatus, ProposalKind, ReviewBatchMemberRole, ReviewBatchMemberStatus,
    ReviewBatchPhase, ReviewBatchStatus, Task, TaskStatus, WorkItem,
};

use super::{batch_input, bind_open_pr, clean_verdict_payload, force_batch_supervising, raw};
use crate::test_support::{create_test_chore, create_test_chore_manual, create_test_product, open_db};
use crate::work::{
    AddDependencyInput, FakePrStateChecker, PrOpenState, ReviewBatchMemberCreateInput, SubmitWorkerProposalInput,
    WorkDb,
};

fn task_from_item(item: WorkItem) -> Task {
    match item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        _ => panic!("expected task"),
    }
}

#[tokio::test]
async fn committed_dependency_change_reaches_board_publisher() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product(&db);
    let prerequisite = create_test_chore(&db, product.id.clone(), "Prerequisite");
    let dependent = create_test_chore(&db, product.id.clone(), "Dependent");
    let publisher = std::sync::Arc::new(crate::test_support::RecordingPublisher::default());
    let bridge = crate::board_notification_bridge::spawn_notifications(db.event_bus().clone(), publisher.clone());
    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: prerequisite.id,
        relation: None,
    })
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if !publisher.events.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        *publisher.events.lock().await,
        vec![(product.id, dependent.id.clone(), "work_item_changed".into())]
    );
    assert_eq!(
        task_from_item(db.get_work_item(&dependent.id).unwrap()).status,
        TaskStatus::Blocked
    );
    bridge.abort();
    let _ = bridge.await;
}

fn member(
    role: ReviewBatchMemberRole,
    execution_id: Option<String>,
    status: ReviewBatchMemberStatus,
) -> ReviewBatchMemberCreateInput {
    ReviewBatchMemberCreateInput::builder()
        .attempt(1)
        .provider_effort("medium")
        .requested_driver(match role {
            ReviewBatchMemberRole::ClaudeReviewer => "claude",
            ReviewBatchMemberRole::CodexReviewer => "codex",
            ReviewBatchMemberRole::GrokReviewer => "grok",
            ReviewBatchMemberRole::Supervisor => "claude",
            ReviewBatchMemberRole::PostMergeReviewer => "claude",
        })
        .resolved_model("test-model")
        .role(role)
        .status(status)
        .maybe_execution_id(execution_id)
        .build()
}

#[tokio::test]
async fn clean_verdict_advances_the_origin_to_review_without_a_revision() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let product_id = product.id.clone();
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_open_pr(&db, &cycle_root.id);
    let publisher = std::sync::Arc::new(crate::test_support::RecordingPublisher::default());
    let notifications =
        crate::board_notification_bridge::spawn_notifications(db.event_bus().clone(), publisher.clone());
    let supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(
                ReviewBatchMemberRole::Supervisor,
                Some(supervisor.id.clone()),
                ReviewBatchMemberStatus::Pending,
            )],
        )
        .unwrap();
    force_batch_supervising(&db, &batch.id);
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &clean_verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();

    let error = db
        .apply_observed_review_verdict_proposal(
            &outcome.proposal.id,
            &FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head-sha"),
            ReviewVerdictObservation {
                inputs: db.review_verdict_comparison(&outcome.proposal.id).unwrap(),
                freshness: None,
                deletions: Some(vec![]),
            },
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("pre-merge verdict observation is missing target freshness"),
        "{error:#}"
    );
    assert_eq!(
        db.worker_proposal(&outcome.proposal.id).unwrap().unwrap().state,
        ProposalState::Proposed
    );

    // The supervisor has stopped before asynchronous application. Neither
    // the root's status nor a later worker event can refresh this clean pass.
    raw(&db)
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&supervisor.id],
        )
        .unwrap();
    raw(&db).execute(
        "UPDATE tasks SET pr_head_sha = 'head-sha', ci_required_state = 'success', pr_mergeable_state = 'mergeable' WHERE id = ?1",
        [&cycle_root.id],
    ).unwrap();
    let created = db
        .apply_review_verdict_proposal(
            &outcome.proposal.id,
            &FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head-sha"),
        )
        .unwrap();
    assert_eq!(created, None);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if !publisher.events.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("committed clean verdict must invalidate the card");
    assert_eq!(
        *publisher.events.lock().await,
        vec![(
            product_id.clone(),
            cycle_root.id.clone(),
            "review_verdict_applied".to_owned()
        )]
    );
    notifications.abort();
    let tree = db.get_work_tree(&product_id).unwrap();
    let card = tree.chores.iter().find(|task| task.id == cycle_root.id).unwrap();
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewed_all_clear"));

    let after = task_from_item(db.get_work_item(&cycle_root.id).unwrap());
    assert_eq!(after.status, TaskStatus::InReview);
    let (cycle, sha) = db.get_task_review_cycle_state(&cycle_root.id).unwrap();
    assert_eq!(cycle, 1);
    assert_eq!(sha.as_deref(), Some("head-sha"));
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
    let revision_count: i64 = raw(&db)
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1 AND kind = 'revision'",
            rusqlite::params![cycle_root.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(revision_count, 0, "clean verdict must not mint a revision");
}
