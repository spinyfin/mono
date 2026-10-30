//! Ordinary review revisions obey the same mergeability gate as conflict repairs.
use super::*;

#[tokio::test]
async fn ordinary_revision_requires_mergeability_at_every_completion_boundary() {
    for boundary in ["declared", "stop", "no-op"] {
        for mergeability in [
            OpenPrMergeability::Conflict,
            OpenPrMergeability::Unknown,
            OpenPrMergeability::Clean,
        ] {
            let workspace = tempdir().unwrap();
            let pr = "https://github.com/spinyfin/mono/pull/2070";
            let (_dir, db, _, revision, execution_id) = revision_fixture(workspace.path(), pr, "old-head");
            let verifier = StubBranchVerifier::ok("my-feature");
            // A pure rebase changes the published SHA without adding a commit.
            verifier.set_head_oid(Ok("rebased-head".into())).await;
            let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
                .handler
                .with_branch_verifier(verifier)
                .with_merge_probe(Arc::new(FixedMergeabilityProbe { mergeability }))
                .with_conflict_unknown_backoff(std::time::Duration::ZERO);
            let outcome = match boundary {
                "declared" => {
                    handler
                        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
                        .await
                }
                "stop" => {
                    handler
                        .finalize_pr_transition(&execution_id, pr.into(), WorkerPrCompletionTarget::InReview, "test")
                        .await
                }
                _ => {
                    declare_no_changes_needed(&db, &execution_id);
                    handler
                        .finalize_no_op_completion(
                            &db.get_execution(&execution_id).unwrap(),
                            Some(ContributionEvidence::ProvenAbsent),
                            None,
                        )
                        .await
                }
            };
            if mergeability == OpenPrMergeability::Clean {
                assert_eq!(
                    db.get_execution(&execution_id).unwrap().status,
                    ExecutionStatus::Completed,
                    "{boundary}: {outcome:?}"
                );
            } else {
                assert!(
                    matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { .. }),
                    "{boundary}: {outcome:?}"
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

struct SettlingProbe(std::sync::atomic::AtomicUsize);

#[async_trait]
impl MergeProbe for SettlingProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        let mergeability = if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            OpenPrMergeability::Unknown
        } else {
            OpenPrMergeability::Clean
        };
        FixedMergeabilityProbe { mergeability }.probe(url).await
    }
}

#[tokio::test]
async fn ordinary_revision_rechecks_unknown_before_accepting_delivery() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, execution_id) = revision_fixture(workspace.path(), pr, "old-head");
    let probe = Arc::new(SettlingProbe(AtomicUsize::new(0)));
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_merge_probe(probe.clone())
        .with_conflict_unknown_backoff(std::time::Duration::ZERO);
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(matches!(outcome, StopOutcome::PrDetected { .. }), "{outcome:?}");
    assert_eq!(probe.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn ordinary_revision_cannot_deliver_when_probe_fails() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, execution_id) = revision_fixture(workspace.path(), pr, "old-head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_merge_probe(Arc::new(NoopMergeProbe));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail } if detail.contains("probe failed")),
        "{outcome:?}"
    );
}

struct HangingProbe;

#[async_trait]
impl MergeProbe for HangingProbe {
    async fn probe(&self, _url: &str) -> anyhow::Result<PrLifecycleProbe> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn ordinary_revision_cannot_deliver_when_mergeability_times_out() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, execution_id) = revision_fixture(workspace.path(), pr, "old-head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_merge_probe(Arc::new(HangingProbe));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail } if detail.contains("timed out")),
        "{outcome:?}"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}
