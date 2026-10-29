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
        .with_branch_verifier(published_verifier(StubBranchVerifier::ok("my-feature"), Ok("head")))
        .with_merge_probe(Arc::new(HeadProbe {
            head: "head",
            mergeability: OpenPrMergeability::Conflict,
        }));
    let execution = db.get_execution(&execution_id).unwrap();
    let outcome = handler
        .finalize_no_op_completion(&execution, Some(ContributionEvidence::ProvenAbsent), None)
        .await;
    assert!(
        matches!(outcome, StopOutcome::ConflictDeliveryUnverified { ref detail } if detail.contains("CONFLICTING")),
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
                .with_branch_verifier(published_verifier(verifier, Ok("pushed-head")))
                .with_pr_state_checker(Arc::new(crate::work::FakePrStateChecker::always(
                    crate::work::PrOpenState::Open,
                )))
                .with_merge_probe(Arc::new(HeadProbe {
                    head: "pushed-head",
                    mergeability,
                }))
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
            } else if mergeability == OpenPrMergeability::Unknown {
                assert!(matches!(outcome, StopOutcome::DetectorFailed), "{outcome:?}");
                assert_ne!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
            } else {
                assert!(
                    matches!(outcome, StopOutcome::ConflictDeliveryUnverified { .. }),
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
async fn conflict_delivery_rechecks_unavailable_probes_before_failing() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, _, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    let mut handler = TestHarness::new(db.clone(), StubPrDetector::ok(None)).handler;
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let start = std::time::Instant::now();
    handler.now_fn = Arc::new(move || {
        start + std::time::Duration::from_secs(clock.fetch_add(60, std::sync::atomic::Ordering::Relaxed))
    });
    let execution = db.get_execution(&execution_id).unwrap();
    for _ in 0..3 {
        assert!(matches!(
            handler.conflict_delivery_decision(&execution, Some(pr)).await,
            ConflictDeliveryDecision::Unsettled(StopOutcome::DetectorFailed)
        ));
        assert_ne!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    }
    assert!(matches!(
        handler.conflict_delivery_decision(&execution, Some(pr)).await,
        ConflictDeliveryDecision::Failed(StopOutcome::ConflictDeliveryUnverified { .. })
    ));
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
            ConflictDeliveryDecision::Failed(StopOutcome::ConflictDeliveryUnverified { ref detail })
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

/// A probe that reports a fixed mergeability for one specific head.
struct HeadProbe {
    head: &'static str,
    mergeability: OpenPrMergeability,
}

#[async_trait]
impl MergeProbe for HeadProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        let mut probe = FixedMergeabilityProbe {
            mergeability: self.mergeability,
        }
        .probe(url)
        .await?;
        probe.head_ref_oid = Some(self.head.to_owned());
        Ok(probe)
    }
}

#[tokio::test]
async fn conflict_delivery_gate_does_not_fail_on_a_conflicting_read_of_a_stale_head() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, _, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    let verifier = StubBranchVerifier::ok("my-feature");
    let verifier = published_verifier(verifier, Ok("pushed-head"));
    let execution = db.get_execution(&execution_id).unwrap();

    // The read still describes the pre-push head: unsettled, execution stays live.
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier.clone())
        .with_merge_probe(Arc::new(HeadProbe {
            head: "old-head",
            mergeability: OpenPrMergeability::Conflict,
        }))
        .with_conflict_unknown_backoff(std::time::Duration::ZERO);
    let decision = handler.conflict_delivery_decision(&execution, Some(pr)).await;
    assert!(
        matches!(
            decision,
            ConflictDeliveryDecision::Unsettled(StopOutcome::DetectorFailed)
        ),
        "{decision:?}"
    );
    assert_ne!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);

    // A CONFLICTING answer for the pushed head is definitive and fails the run.
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(HeadProbe {
            head: "pushed-head",
            mergeability: OpenPrMergeability::Conflict,
        }))
        .with_conflict_unknown_backoff(std::time::Duration::ZERO);
    let decision = handler.conflict_delivery_decision(&execution, Some(pr)).await;
    assert!(
        matches!(
            decision,
            ConflictDeliveryDecision::Failed(StopOutcome::ConflictDeliveryUnverified { .. })
        ),
        "{decision:?}"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}

struct MissingHeadProbe;
#[async_trait]
impl MergeProbe for MissingHeadProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        let mut probe = FixedMergeabilityProbe {
            mergeability: OpenPrMergeability::Conflict,
        }
        .probe(url)
        .await?;
        probe.head_ref_oid = None;
        Ok(probe)
    }
}

struct StaleThenError(std::sync::atomic::AtomicBool);
#[async_trait]
impl MergeProbe for StaleThenError {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        if self.0.swap(true, std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("probe unavailable");
        }
        HeadProbe {
            head: "old-head",
            mergeability: OpenPrMergeability::Conflict,
        }
        .probe(url)
        .await
    }
}

#[tokio::test]
async fn conflict_delivery_rechecks_missing_head_failed_lookup_and_stale_then_error() {
    for case in 0..4 {
        let workspace = tempdir().unwrap();
        let pr = "https://github.com/spinyfin/mono/pull/2070";
        let (_dir, db, _, _, _, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
        let verifier = StubBranchVerifier::ok("my-feature");
        // The PR-record head is stale even while git reports the published head.
        verifier.set_head_oid(Ok("old-head".into())).await;
        verifier.set_fresh_head_oid(Ok("old-head".into())).await;
        let verifier = published_verifier(
            verifier,
            if case == 1 {
                Err("git unavailable")
            } else {
                Ok("pushed-head")
            },
        );
        let probe: Arc<dyn MergeProbe> = match case {
            0 => Arc::new(MissingHeadProbe),
            2 => Arc::new(StaleThenError(std::sync::atomic::AtomicBool::new(false))),
            _ => Arc::new(HeadProbe {
                head: "old-head",
                mergeability: OpenPrMergeability::Conflict,
            }),
        };
        let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
            .handler
            .with_branch_verifier(verifier)
            .with_merge_probe(probe)
            .with_conflict_unknown_backoff(std::time::Duration::ZERO);
        let execution = db.get_execution(&execution_id).unwrap();
        assert!(
            matches!(
                handler.conflict_delivery_decision(&execution, Some(pr)).await,
                ConflictDeliveryDecision::Unsettled(StopOutcome::DetectorFailed)
            ),
            "case {case}"
        );
        assert_ne!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    }
}

struct PendingProbe;
#[async_trait]
impl MergeProbe for PendingProbe {
    async fn probe(&self, _url: &str) -> anyhow::Result<PrLifecycleProbe> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn conflict_delivery_timeout_retains_execution_for_recheck() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, _, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_merge_probe(Arc::new(PendingProbe));
    let execution = db.get_execution(&execution_id).unwrap();
    assert!(matches!(
        handler.conflict_delivery_decision(&execution, Some(pr)).await,
        ConflictDeliveryDecision::Unsettled(StopOutcome::DetectorFailed)
    ));
    assert_ne!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}

// Keep the git transport fixture independent of all GitHub PR-record reads.
pub(super) fn published_verifier(inner: Arc<StubBranchVerifier>, head: Result<&str, &str>) -> Arc<dyn BranchVerifier> {
    Arc::new(PublishedVerifier {
        inner,
        head: head.map(str::to_owned).map_err(str::to_owned),
    })
}

struct PublishedVerifier {
    inner: Arc<StubBranchVerifier>,
    head: Result<String, String>,
}

#[async_trait]
impl BranchVerifier for PublishedVerifier {
    async fn fetch_remote_head(&self, _remote: &str, _branch: &str) -> Result<String> {
        self.head.clone().map_err(|error| anyhow::anyhow!(error))
    }
    async fn fetch_pr_head_ref(&self, repo: &str, pr: u64) -> Result<String> {
        self.inner.fetch_pr_head_ref(repo, pr).await
    }
    async fn fetch_pr_head_oid(&self, repo: &str, pr: u64) -> Result<String> {
        self.inner.fetch_pr_head_oid(repo, pr).await
    }
    async fn fetch_pr_head_oid_fresh(&self, repo: &str, pr: u64) -> Result<String> {
        self.inner.fetch_pr_head_oid_fresh(repo, pr).await
    }
    async fn fetch_diff_line_count(&self, repo: &str, base: &str, head: &str) -> Result<u64> {
        self.inner.fetch_diff_line_count(repo, base, head).await
    }
    async fn fetch_pr_base_ref(&self, repo: &str, pr: u64) -> Result<String> {
        self.inner.fetch_pr_base_ref(repo, pr).await
    }
    async fn fetch_diff_signature(&self, repo: &str, base: &str, head: &str) -> Result<String> {
        self.inner.fetch_diff_signature(repo, base, head).await
    }
    async fn fetch_pr_title_and_body(&self, repo: &str, pr: u64) -> Result<(String, String)> {
        self.inner.fetch_pr_title_and_body(repo, pr).await
    }
}

pub(super) fn published_probe(inner: Arc<dyn MergeProbe>, head: &str) -> Arc<dyn MergeProbe> {
    Arc::new(PublishedProbe {
        inner,
        head: head.to_owned(),
    })
}

struct PublishedProbe {
    inner: Arc<dyn MergeProbe>,
    head: String,
}

#[async_trait]
impl MergeProbe for PublishedProbe {
    async fn probe(&self, url: &str) -> Result<PrLifecycleProbe> {
        let mut probe = self.inner.probe(url).await?;
        probe.head_ref_oid = Some(self.head.clone());
        Ok(probe)
    }
}
