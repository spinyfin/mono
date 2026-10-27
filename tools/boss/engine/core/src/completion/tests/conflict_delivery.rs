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
