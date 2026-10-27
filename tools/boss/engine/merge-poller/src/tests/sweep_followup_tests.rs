use super::*;
use crate::work::test_support::assert_cancel_provenance;
use crate::work::{LatePrCandidate, ReviewBatchDispatch, TaskStatus};
use boss_observation_contracts::{CompletionRequests, ReviewBatchEnqueuer, StopOutcome};
use boss_protocol::{CancelProvenance, ExecutionStatus as WireExecutionStatus, TaskKind, WorkExecution};

/// Completion handler that only unblocks `stop_active_revision_executions`.
/// Lease release is a no-op: the cancel itself is owned by work-db.
struct ReleaseOnlyHandler {
    enqueuer: NoopBatchEnqueuer,
}

struct NoopBatchEnqueuer;

#[async_trait]
impl ReviewBatchEnqueuer for NoopBatchEnqueuer {
    async fn enqueue(
        &self,
        _work_db: &WorkDb,
        _work_item_id: &str,
        _repo_remote_url: &str,
        _pr_url: &str,
        _review_pool_size: usize,
    ) -> Result<ReviewBatchDispatch> {
        panic!("parent-merge followup test must not enqueue a review batch")
    }
}

#[async_trait]
impl CompletionRequests for ReleaseOnlyHandler {
    fn review_guide_source_capture_enabled(&self) -> bool {
        false
    }
    fn pending_background_nudge_execution_ids(&self) -> Vec<String> {
        Vec::new()
    }
    async fn recheck_background_nudge(&self, _execution_id: &str) -> Option<StopOutcome> {
        None
    }
    fn review_batch_fanout_enabled(&self) -> bool {
        false
    }
    fn review_pool_size(&self) -> usize {
        0
    }
    async fn force_release(&self, _execution_id: &str) {}
    async fn record_parent_pr_merge_cancellation(
        &self,
        _execution: &WorkExecution,
        _prior_status: WireExecutionStatus,
        _chain_root_id: &str,
        _provenance: &CancelProvenance,
    ) {
    }
    async fn force_stop_execution(&self, _execution_id: &str, _provenance: CancelProvenance) {}
    async fn recheck_for_pr(&self, _execution_id: &str) -> StopOutcome {
        StopOutcome::DbError
    }
    async fn recheck_for_pr_late(&self, _candidate: &LatePrCandidate) -> StopOutcome {
        StopOutcome::DbError
    }
    fn reconcile_review_guide_source_from_probe(
        &self,
        _root_task_id: &str,
        _pr_url: &str,
        _base_sha: &str,
        _head_sha: &str,
        _observation_sequence: i64,
    ) {
    }
    fn speculative_conflict_prediction_enabled(&self) -> bool {
        false
    }
    fn stacked_pr_auto_structuring_enabled(&self) -> bool {
        false
    }
    fn mechanical_rebase_enabled(&self) -> bool {
        false
    }
    fn review_batch_enqueuer(&self) -> &dyn ReviewBatchEnqueuer {
        &self.enqueuer
    }
}

/// A PR-review revision whose target merges while its worker is live must
/// cross execution families automatically: cancel the obsolete
/// revision_implementation, preserve the work item as an autostart followup,
/// and enqueue a chore_implementation without any manual start.
#[tokio::test]
async fn merged_pr_redispatches_live_review_revision_as_followup_and_records_cancel() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let pr = "https://github.com/foo/bar/pull/1501";
    let (product_id, parent_id) = make_chore_in_review(&db, "merged-review-followup", pr);
    let checker = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open);
    let revision = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address every review finding")
                .created_via(format!(
                    "{}exec_review_origin",
                    boss_protocol::CREATED_VIA_PR_REVIEW_PREFIX
                ))
                .build(),
            &checker,
        )
        .unwrap();
    let original = db
        .list_executions(Some(&revision.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.kind == ExecutionKind::RevisionImplementation)
        .expect("revision creation must enqueue its implementation");
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&revision.id])
            .unwrap();
        conn.execute(
            "UPDATE work_executions
             SET status = 'running', cube_lease_id = 'lease-review',
                 cube_workspace_id = 'workspace-review', workspace_path = '/tmp/workspace-review'
             WHERE id = ?1",
            [&original.id],
        )
        .unwrap();
    }

    let publisher = Arc::new(RecordingPublisher::default());
    let handler = ReleaseOnlyHandler {
        enqueuer: NoopBatchEnqueuer,
    };
    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Merged);

    let outcome = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&handler),
        None,
    )
    .await;

    assert_eq!(outcome.merged, 1);
    assert_eq!(outcome.revision_invalidated, 1);
    let cancelled = db.get_execution(&original.id).unwrap();
    assert_cancel_provenance(
        &cancelled,
        boss_protocol::CancelSource::MergePollerParentMerged,
        "merge_poller",
        "merged, so this revision execution is obsolete",
    );
    let followup = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(task) => task,
        other => panic!("expected converted followup, got {other:?}"),
    };
    assert_eq!(followup.kind, TaskKind::Followup);
    assert_eq!(followup.status, TaskStatus::Todo);
    assert!(followup.autostart);

    let executions = db.list_executions(Some(&revision.id)).unwrap();
    let redispatched: Vec<_> = executions
        .iter()
        .filter(|execution| execution.kind == ExecutionKind::ChoreImplementation)
        .collect();
    assert_eq!(redispatched.len(), 1, "followup must receive one fresh execution");
    assert_eq!(redispatched[0].status, ExecutionStatus::Ready);
    assert_eq!(
        redispatched[0].preferred_workspace_id.as_deref(),
        Some("workspace-review"),
        "followup must prefer the exact workspace assigned to the cancelled revision",
    );
    assert!(
        redispatched[0].allow_dirty,
        "preferred workspace must retain uncommitted edits"
    );
    assert!(
        redispatched[0].prefer_is_soft,
        "an unavailable preferred workspace must fall back to a fresh workspace",
    );
    assert_eq!(followup.product_id, product_id);
}
