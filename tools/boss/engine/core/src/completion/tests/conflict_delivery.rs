use super::super::ConflictDeliveryDecision;
use super::*;
use crate::merge_poller::OpenPrMergeability;
use crate::work::{PendingMergeCheck, WorkItemPatch};

#[tokio::test]
async fn conflict_no_op_cannot_complete_after_attempt_retired_while_pr_still_conflicts() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, _, execution_id, attempt) = conflict_revision_fixture(workspace.path(), pr, "head");
    db.mark_conflict_resolution_succeeded(&attempt, Some("head")).unwrap();
    declare_no_changes_needed(&db, &execution_id);
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_merge_probe(Arc::new(FixedMergeabilityProbe {
            mergeability: OpenPrMergeability::Conflict,
        }));
    let execution = db.get_execution(&execution_id).unwrap();
    let outcome = handler
        .finalize_no_op_completion(&execution, Some(ContributionEvidence::ProvenAbsent), None)
        .await;
    assert!(
        matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail } if detail.contains("CONFLICTING")),
        "{outcome:?}"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn conflict_delivery_requires_github_mergeability_even_after_head_moves() {
    for declared in [true, false] {
        for mergeability in [
            OpenPrMergeability::Conflict,
            OpenPrMergeability::Unknown,
            OpenPrMergeability::Clean,
        ] {
            let workspace = tempdir().unwrap();
            let pr = "https://github.com/spinyfin/mono/pull/2070";
            let (_dir, db, _, _, revision, execution_id, _) =
                conflict_revision_fixture(workspace.path(), pr, "old-head");
            let verifier = StubBranchVerifier::ok("my-feature");
            verifier.set_head_oid(Ok("pushed-head".into())).await;
            let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
                .handler
                .with_branch_verifier(verifier)
                .with_pr_state_checker(Arc::new(crate::work::FakePrStateChecker::always(
                    crate::work::PrOpenState::Open,
                )))
                .with_merge_probe(Arc::new(FixedMergeabilityProbe { mergeability }))
                .with_conflict_unknown_backoff(std::time::Duration::ZERO);
            let outcome = if declared {
                handler
                    .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
                    .await
            } else {
                handler
                    .finalize_pr_transition(&execution_id, pr.into(), WorkerPrCompletionTarget::InReview, "test")
                    .await
            };
            if mergeability == OpenPrMergeability::Clean {
                assert!(matches!(outcome, StopOutcome::PrDetected { .. }), "{outcome:?}");
                assert_eq!(
                    db.get_execution(&execution_id).unwrap().status,
                    ExecutionStatus::Completed
                );
                let WorkItem::Task(task) = db.get_work_item(&revision).unwrap() else {
                    panic!("revision missing")
                };
                assert_eq!(task.status, TaskStatus::InReview);
            } else {
                assert!(
                    matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { .. }),
                    "{outcome:?}"
                );
                assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
                let WorkItem::Task(task) = db.get_work_item(&revision).unwrap() else {
                    panic!("revision missing")
                };
                assert_eq!(task.status, TaskStatus::Blocked);
            }
        }
    }
}

#[tokio::test]
async fn conflict_delivery_fails_closed_when_github_is_unavailable() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, _, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    // The harness's NoopMergeProbe returns an error, as a failed GitHub request does.
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None)).handler;
    let outcome = handler
        .finalize_pr_transition(&execution_id, pr.into(), WorkerPrCompletionTarget::InReview, "test")
        .await;
    assert!(
        matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail } if detail.contains("could not be verified")),
        "{outcome:?}"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}

struct MergedProbe;

#[async_trait]
impl MergeProbe for MergedProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        Ok(PrLifecycleProbe::builder()
            .url(url.to_owned())
            .state(PrLifecycleState::Merged)
            .labels(Vec::new())
            .review(crate::merge_poller::PrReviewState::Unknown)
            .build())
    }
}

#[tokio::test]
async fn conflict_delivery_gate_treats_a_merged_pr_as_moot_not_a_delivery_failure() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, _, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_merge_probe(Arc::new(MergedProbe));
    let execution = db.get_execution(&execution_id).unwrap();
    assert!(matches!(
        handler.conflict_delivery_decision(&execution, Some(pr)).await,
        ConflictDeliveryDecision::Moot
    ));
    assert_ne!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn conflict_delivery_gate_reports_a_missing_bound_pr_url_specifically() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, _, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None)).handler;
    let execution = db.get_execution(&execution_id).unwrap();
    let outcome = handler.conflict_delivery_decision(&execution, None).await;
    assert!(
        matches!(
            outcome,
            ConflictDeliveryDecision::Failed(StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail })
                if detail.contains("no bound PR URL")
        ),
        "{outcome:?}"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}

struct ClosedUnmergedProbe;

#[async_trait]
impl MergeProbe for ClosedUnmergedProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        Ok(PrLifecycleProbe::builder()
            .url(url.to_owned())
            .state(PrLifecycleState::ClosedUnmerged)
            .labels(Vec::new())
            .review(crate::merge_poller::PrReviewState::Unknown)
            .build())
    }
}

fn no_open_failure_attention(db: &WorkDb, work_item_id: &str) {
    let items = db.list_attention_items_for_work_item(work_item_id).unwrap();
    assert!(
        items.iter().all(|i| i.status != "open"),
        "closed-unmerged conflict delivery must not file a failure attention item, got {items:?}"
    );
}

#[tokio::test]
async fn conflict_declared_delivery_treats_a_closed_unmerged_pr_as_moot() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, revision, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(StubBranchVerifier::ok("my-feature"))
        .with_pr_state_checker(Arc::new(crate::work::FakePrStateChecker::always(
            crate::work::PrOpenState::ClosedUnmerged,
        )));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(matches!(outcome, StopOutcome::PrMerged { .. }), "{outcome:?}");
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    let WorkItem::Task(task) = db.get_work_item(&revision).unwrap() else {
        panic!("revision missing")
    };
    assert_eq!(task.status, TaskStatus::Done);
    no_open_failure_attention(&db, &revision);
}

#[tokio::test]
async fn conflict_declared_delivery_treats_a_lifecycle_close_between_probes_as_moot() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, revision, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(StubBranchVerifier::ok("my-feature"))
        .with_pr_state_checker(Arc::new(crate::work::FakePrStateChecker::always(
            crate::work::PrOpenState::Open,
        )))
        .with_merge_probe(Arc::new(ClosedUnmergedProbe));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(matches!(outcome, StopOutcome::PrMerged { .. }), "{outcome:?}");
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    let WorkItem::Task(task) = db.get_work_item(&revision).unwrap() else {
        panic!("revision missing")
    };
    assert_eq!(task.status, TaskStatus::Done);
    no_open_failure_attention(&db, &revision);
}

#[tokio::test]
async fn another_work_item_cannot_start_conflict_resolution_while_revision_is_live() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, product, _, _, _, attempt_id) = conflict_revision_fixture(workspace.path(), pr, "head");
    db.mark_conflict_resolution_succeeded(&attempt_id, Some("head"))
        .unwrap();
    let other = create_test_chore_manual(&db, product.clone(), "Another PR observer");
    db.update_work_item(
        &other.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let harness = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let probe = FixedMergeabilityProbe {
        mergeability: OpenPrMergeability::Conflict,
    }
    .probe(pr)
    .await
    .unwrap();
    let spawned = crate::conflict_watch::on_conflict_detected(
        &db,
        harness.publisher.as_ref(),
        None,
        &crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open),
        &PendingMergeCheck {
            product_id: product,
            work_item_id: other.id.clone(),
            pr_url: pr.into(),
        },
        &probe,
    )
    .await;
    assert!(!spawned);
    assert!(
        db.list_conflict_resolutions(None, &[], Some(&other.id), None)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn blocked_revision_with_a_finished_execution_does_not_suppress_conflict_handling() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, revision, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "head");
    assert!(
        db.has_live_conflict_revision_for_pr(pr).unwrap(),
        "a running revision must still suppress a competitor"
    );
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET status = 'blocked' WHERE id = ?1", [&revision])
            .unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    }
    assert!(!db.has_live_conflict_revision_for_pr(pr).unwrap());
}
