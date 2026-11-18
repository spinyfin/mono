//! The head a finished revision delivered, and the review hold taken on it:
//! what each completion path records, and what it does when the review of
//! that head has already been decided or the head cannot be read.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tempfile::tempdir;

use super::*;
use crate::ReviewBatchEnqueuer;
use crate::run_done_declaration::maybe_enqueue_declared_delivery_reviewer;
use crate::work::{ReviewBatchDispatch, WorkItemPatch};
use boss_observation_contracts::OpenPrMergeability;
use boss_protocol::TaskStatus;

const PR: &str = "https://github.com/spinyfin/mono/pull/2070";
/// The head the revision started from, already reviewed.
const REVIEWED: &str = "reviewed-head";
/// The head the revision pushed.
const PUSHED: &str = "pushed-head";

/// A mergeable open PR whose answer carries `head` as its head commit.
struct HeadProbe(Option<&'static str>);

#[async_trait]
impl MergeProbe for HeadProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        let mut probe = FixedMergeabilityProbe {
            mergeability: OpenPrMergeability::Clean,
        }
        .probe(url)
        .await?;
        probe.head_ref_oid = self.0.map(str::to_owned);
        Ok(probe)
    }
}

/// Admission as production performs it, with the PR metadata read replaced by
/// a fixed head.
struct AdmitAtHead {
    head: &'static str,
    calls: AtomicUsize,
}

impl AdmitAtHead {
    fn at(head: &'static str) -> Arc<Self> {
        Arc::new(Self {
            head,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ReviewBatchEnqueuer for AdmitAtHead {
    async fn enqueue(
        &self,
        db: &WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
    ) -> anyhow::Result<ReviewBatchDispatch> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let input = crate::review_batch_input_from_metadata(
            db,
            work_item_id,
            pr_url,
            serde_json::json!({
                "baseRefOid": "base-sha",
                "headRefOid": self.head,
                "files": [{"path": "src/lib.rs"}],
                "additions": 12,
                "deletions": 3
            }),
        )?;
        db.create_pre_merge_review_batch_for_pool(input, repo_remote_url, review_pool_size)
    }
}

/// The PR metadata read fails for every admission attempt.
struct MetadataUnreadable;

#[async_trait]
impl ReviewBatchEnqueuer for MetadataUnreadable {
    async fn enqueue(&self, _: &WorkDb, _: &str, _: &str, _: &str, _: usize) -> anyhow::Result<ReviewBatchDispatch> {
        anyhow::bail!("gh pr view: could not read the pull request")
    }
}

fn fanout_flags(path: &std::path::Path) -> Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        path.join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("review_batch_fanout", true).unwrap();
    flags
}

struct Scene {
    /// The database's and the feature flags' directories, kept alive.
    _dirs: (TempDir, TempDir),
    db: Arc<WorkDb>,
    revision_id: String,
    execution_id: String,
    flags: Arc<crate::feature_flags::FeatureFlagsStore>,
}

/// A revision whose worker is live, started from [`REVIEWED`], with nothing
/// else queued for it: what dispatch leaves behind in production.
fn scene(workspace: &std::path::Path) -> Scene {
    let (dir, db, _product, revision_id, execution_id) = revision_fixture(workspace, PR, REVIEWED);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'abandoned' WHERE work_item_id = ?1 AND id != ?2",
            rusqlite::params![revision_id, execution_id],
        )
        .unwrap();
    let flags_dir = tempdir().unwrap();
    let flags = fanout_flags(flags_dir.path());
    Scene {
        _dirs: (dir, flags_dir),
        db,
        revision_id,
        execution_id,
        flags,
    }
}

impl Scene {
    /// A handler that holds a delivering revision for review, with the two
    /// reads of the PR head a completion makes under the test's control.
    fn handler(
        &self,
        direct_read: Result<&str, &str>,
        probe: HeadProbe,
        enqueuer: Arc<dyn ReviewBatchEnqueuer>,
    ) -> WorkerCompletionHandler {
        let verifier = StubBranchVerifier::ok("boss/test");
        *verifier.fresh_head_oid_result.try_lock().unwrap() = direct_read.map(str::to_owned).map_err(str::to_owned);
        *verifier.head_oid_result.try_lock().unwrap() = direct_read.map(str::to_owned).map_err(str::to_owned);
        TestHarness::new(self.db.clone(), StubPrDetector::ok(None))
            .handler
            .with_enable_revision_triggered_reviews(true)
            .with_feature_flags(self.flags.clone())
            .with_branch_verifier(verifier)
            .with_merge_probe(Arc::new(probe))
            .with_review_batch_enqueuer(enqueuer)
    }

    /// The task that owns the PR the revision pushes to.
    fn root_id(&self) -> String {
        self.db.review_cycle_root_id(&self.revision_id)
    }

    fn status(&self) -> TaskStatus {
        self.task().status
    }

    fn task(&self) -> boss_protocol::Task {
        match self.db.get_work_item(&self.revision_id).unwrap() {
            WorkItem::Task(task) | WorkItem::Chore(task) => task,
            other => panic!("expected a task, got {other:?}"),
        }
    }

    fn run(&self) -> crate::work::WorkExecution {
        self.db.get_execution(&self.execution_id).unwrap()
    }

    fn awaited_head(&self) -> Option<String> {
        self.db.get_revision_stop_contributed_head(&self.execution_id).unwrap()
    }

    /// A pre-merge batch for the root at `head` that has already reached a
    /// terminal status: admitted for another row and decided before this
    /// revision's run ended.
    fn decided_batch(&self, head: &str, status: &str) -> String {
        let input = crate::review_batch_input_from_metadata(
            &self.db,
            &self.root_id(),
            PR,
            serde_json::json!({
                "baseRefOid": "base-sha",
                "headRefOid": head,
                "files": [{"path": "src/lib.rs"}],
                "additions": 12,
                "deletions": 3
            }),
        )
        .unwrap();
        let ReviewBatchDispatch::Created { batch, executions } = self
            .db
            .create_pre_merge_review_batch(input, "git@github.com:spinyfin/mono.git")
            .unwrap()
        else {
            panic!("expected a new batch");
        };
        for execution in &executions {
            self.db.mark_execution_redundant(&execution.id).unwrap();
        }
        self.db
            .connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_batches SET status = ?2, completed_at = '3' WHERE id = ?1",
                rusqlite::params![batch.id, status],
            )
            .unwrap();
        batch.id
    }

    /// The state a declared delivery leaves a revision in before its
    /// post-effects run: run finished, row held `active` on the PR.
    fn hold_as_a_declared_delivery_does(&self) {
        self.db
            .update_work_item(
                &self.revision_id,
                WorkItemPatch {
                    status: Some("active".into()),
                    pr_url: Some(PR.into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        self.db
            .force_execution_status_for_test(&self.revision_id, ExecutionStatus::Completed)
            .unwrap();
    }

    async fn run_post_effects(&self, head_read: &str, enqueuer: &dyn ReviewBatchEnqueuer) {
        let verifier = StubBranchVerifier::ok("boss/test");
        verifier.set_head_oid(Ok(head_read.to_owned())).await;
        let execution = self.run();
        maybe_enqueue_declared_delivery_reviewer(
            &self.db,
            &RecordingPublisher::default(),
            verifier.as_ref(),
            enqueuer,
            &self.flags,
            5,
            0,
            16,
            &execution,
            &self.revision_id,
            &execution.repo_remote_url,
            PR,
        )
        .await;
    }
}

/// A declared delivery used to finish the run with no head recorded and hold
/// the revision for a head only a later admission could name. It now records
/// the head it finished at, on the run and as the head the hold is for,
/// before anything else has a chance to run.
#[tokio::test]
async fn a_declared_delivery_records_the_head_it_finished_at_and_holds_for_it() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    let handler = scene.handler(Ok(PUSHED), HeadProbe(None), Arc::new(MetadataUnreadable));

    let outcome = handler
        .finalize_declared_run_done(&scene.execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;

    assert!(matches!(outcome, StopOutcome::PrDetected { .. }), "{outcome:?}");
    let run = scene.run();
    assert_eq!(run.status, ExecutionStatus::Completed);
    assert_eq!(run.pr_head_before.as_deref(), Some(REVIEWED));
    assert_eq!(
        run.pr_head_after.as_deref(),
        Some(PUSHED),
        "the run records the head it delivered"
    );
    assert_eq!(
        scene.awaited_head().as_deref(),
        Some(PUSHED),
        "the hold names the head whose review releases it"
    );
}

/// The direct read of the head is preferred. When it fails, the head GitHub
/// reported while verifying the delivery mergeable stands in, so one failed
/// read does not fail a delivery GitHub has just described.
#[tokio::test]
async fn a_declared_delivery_uses_the_verified_head_when_the_direct_read_fails() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    let handler = scene.handler(
        Err("gh api: 502 Bad Gateway"),
        HeadProbe(Some(PUSHED)),
        Arc::new(MetadataUnreadable),
    );

    let outcome = handler
        .finalize_declared_run_done(&scene.execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;

    assert!(matches!(outcome, StopOutcome::PrDetected { .. }), "{outcome:?}");
    assert_eq!(scene.run().pr_head_after.as_deref(), Some(PUSHED));
    assert_eq!(scene.awaited_head().as_deref(), Some(PUSHED));
}

/// With no readable head the engine cannot say which review the push is held
/// for. It must not complete the run and leave the revision `active` with
/// nothing live: the run fails, and the failure is named on the task, on the
/// run's ledger and in an attention item.
#[tokio::test]
async fn a_declared_delivery_with_an_unreadable_head_fails_visibly_instead_of_holding() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    let enqueuer = AdmitAtHead::at(PUSHED);
    let handler = scene.handler(Err("gh api: 502 Bad Gateway"), HeadProbe(None), enqueuer.clone());

    let outcome = handler
        .finalize_declared_run_done(&scene.execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;

    let StopOutcome::RunDoneDeclaredWithoutDelivery { detail } = outcome else {
        panic!("an unreadable head must fail the delivery, got {outcome:?}");
    };
    assert!(detail.contains("head commit could not be read"), "{detail}");
    assert_eq!(scene.run().status, ExecutionStatus::Failed);
    let task = scene.task();
    assert_eq!(task.status, TaskStatus::Blocked, "not left `active` with nothing live");
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    assert!(
        task.blocked_detail
            .as_deref()
            .is_some_and(|shown| shown.contains("head commit could not be read")),
        "the card carries the diagnostic: {:?}",
        task.blocked_detail
    );
    assert_eq!(
        scene
            .db
            .latest_status_audit_basis(&scene.revision_id, "blocked_reason")
            .unwrap()
            .as_deref(),
        Some("delivered_without_pr"),
        "the stuck report names the failure by its basis"
    );
    let attentions = scene.db.list_attention_items(&scene.execution_id).unwrap();
    assert!(
        attentions
            .iter()
            .any(|item| item.kind == RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND && item.status == "open"),
        "an attention item names the failure: {attentions:?}"
    );
    assert_eq!(
        enqueuer.calls.load(Ordering::SeqCst),
        0,
        "no review is admitted for a failed delivery"
    );
}

/// The batch for the pushed head was admitted for another row and reached its
/// decision before this run's Stop boundary. Holding the revision for it
/// would wait for a decision that has already been made, so the completion
/// sends the revision to `in_review` directly.
#[tokio::test]
async fn a_stop_completion_does_not_hold_for_a_batch_that_already_decided() {
    for status in ["completed", "failed"] {
        let workspace = tempdir().unwrap();
        let scene = scene(workspace.path());
        // A failed batch at its generation cap, so admission reports it
        // rather than minting a replacement.
        let batch_id = scene.decided_batch(PUSHED, status);
        scene
            .db
            .connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_batches SET generation = 2 WHERE id = ?1",
                rusqlite::params![batch_id],
            )
            .unwrap();
        let handler = scene.handler(Ok(PUSHED), HeadProbe(Some(PUSHED)), AdmitAtHead::at(PUSHED));

        let outcome = handler
            .finalize_pr_transition(
                &scene.execution_id,
                PR.into(),
                WorkerPrCompletionTarget::InReview,
                "stop_sha_delta",
            )
            .await;

        assert_eq!(outcome, StopOutcome::PrDetected { pr_url: PR.into() }, "{status}");
        assert_eq!(scene.run().status, ExecutionStatus::Completed, "{status}");
        assert_eq!(
            scene.status(),
            TaskStatus::InReview,
            "{status}: held for a decided batch"
        );
    }
}

/// The Stop boundary's own stamp is a separate, earlier read of the head. A
/// hold keyed on it would not be released by the batch admission just
/// created at the head it read, so the completion points the hold at that
/// batch's head.
#[tokio::test]
async fn a_stop_completion_holds_for_the_head_of_the_batch_it_admitted() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    scene
        .db
        .set_revision_stop_contributed_head(&scene.execution_id, "earlier-stop-read")
        .unwrap();
    let handler = scene.handler(Ok(PUSHED), HeadProbe(Some(PUSHED)), AdmitAtHead::at(PUSHED));

    let outcome = handler
        .finalize_pr_transition(
            &scene.execution_id,
            PR.into(),
            WorkerPrCompletionTarget::InReview,
            "stop_sha_delta",
        )
        .await;

    assert_eq!(outcome, StopOutcome::ReviewerEnqueued { pr_url: PR.into() });
    assert_eq!(scene.status(), TaskStatus::Active, "held while the batch runs");
    assert_eq!(scene.awaited_head().as_deref(), Some(PUSHED));
    assert_eq!(scene.run().pr_head_after.as_deref(), Some(PUSHED));
    let batches = scene.db.review_batches_for_cycle_root(&scene.root_id()).unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].target_sha, PUSHED);
}

/// A declared delivery's admission finds the batch for its head already
/// decided. Nothing pending would release the hold the completion took, so
/// the post-effects release it.
#[tokio::test]
async fn post_effects_release_a_revision_whose_head_was_already_decided() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    scene.decided_batch(PUSHED, "completed");
    scene.hold_as_a_declared_delivery_does();
    let enqueuer = AdmitAtHead::at(PUSHED);

    scene.run_post_effects(PUSHED, enqueuer.as_ref()).await;

    assert_eq!(enqueuer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(scene.status(), TaskStatus::InReview);
    assert_eq!(scene.awaited_head().as_deref(), Some(PUSHED));
}

/// While the batch for the head is still running the post-effects leave the
/// hold in place, keyed on that batch's head.
#[tokio::test]
async fn post_effects_hold_a_revision_for_the_running_batch_at_its_head() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    scene.hold_as_a_declared_delivery_does();

    scene.run_post_effects(PUSHED, AdmitAtHead::at(PUSHED).as_ref()).await;

    assert_eq!(scene.status(), TaskStatus::Active);
    assert_eq!(scene.awaited_head().as_deref(), Some(PUSHED));
}

/// The review of the push is skipped because the head is the one already
/// reviewed. The skip does not depend on a verdict row, and the cycle root's
/// newest reviewer run need not have one. The revision is released either
/// way, as the Stop boundary releases it.
#[tokio::test]
async fn post_effects_release_a_skipped_revision_without_a_verdict_on_the_root() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    scene.hold_as_a_declared_delivery_does();
    scene
        .db
        .increment_task_review_cycle(&scene.root_id(), Some(REVIEWED))
        .unwrap();
    let enqueuer = AdmitAtHead::at(REVIEWED);

    scene.run_post_effects(REVIEWED, enqueuer.as_ref()).await;

    assert_eq!(
        enqueuer.calls.load(Ordering::SeqCst),
        0,
        "an unchanged head is not reviewed again"
    );
    assert_eq!(scene.status(), TaskStatus::InReview);
}

/// Admission is deferred and names no batch.
struct AlwaysDefers;

#[async_trait]
impl ReviewBatchEnqueuer for AlwaysDefers {
    async fn enqueue(&self, _: &WorkDb, _: &str, _: &str, _: &str, _: usize) -> anyhow::Result<ReviewBatchDispatch> {
        Ok(ReviewBatchDispatch::AdmissionDeferred)
    }
}

/// The Stop boundary finalized without having stamped a head, and admission
/// was deferred, so no batch names one either. The hold falls back to the
/// head the run finished at, so the batch later admitted for that head, by
/// whichever path, releases the revision.
#[tokio::test]
async fn a_stop_completion_with_no_batch_holds_for_the_head_the_run_finished_at() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    assert_eq!(scene.awaited_head(), None);
    let handler = scene.handler(Ok(PUSHED), HeadProbe(Some(PUSHED)), Arc::new(AlwaysDefers));

    let outcome = handler
        .finalize_pr_transition(
            &scene.execution_id,
            PR.into(),
            WorkerPrCompletionTarget::InReview,
            "stop_sha_delta",
        )
        .await;

    assert_eq!(outcome, StopOutcome::ReviewerEnqueued { pr_url: PR.into() });
    assert_eq!(scene.status(), TaskStatus::Active);
    assert_eq!(scene.run().pr_head_after.as_deref(), Some(PUSHED));
    assert_eq!(scene.awaited_head().as_deref(), Some(PUSHED));
}

/// The Stop boundary could read no head either: it stamped none, the direct
/// read fails and GitHub's answer carried none. It fails the same way a
/// declared delivery does, before any review is admitted.
#[tokio::test]
async fn a_stop_completion_with_an_unreadable_head_fails_visibly_instead_of_holding() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    let enqueuer = AdmitAtHead::at(PUSHED);
    let handler = scene.handler(Err("gh api: 502 Bad Gateway"), HeadProbe(None), enqueuer.clone());

    let outcome = handler
        .finalize_pr_transition(
            &scene.execution_id,
            PR.into(),
            WorkerPrCompletionTarget::InReview,
            "stop_sha_delta",
        )
        .await;

    let StopOutcome::RunDoneDeclaredWithoutDelivery { detail } = outcome else {
        panic!("an unreadable head must fail the delivery, got {outcome:?}");
    };
    assert!(detail.contains("head commit could not be read"), "{detail}");
    assert_eq!(scene.run().status, ExecutionStatus::Failed);
    let task = scene.task();
    assert_eq!(task.status, TaskStatus::Blocked, "not left `active` with nothing live");
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    assert_eq!(enqueuer.calls.load(Ordering::SeqCst), 0);
}

/// The head the Stop boundary stamped is a head. With it the completion can
/// say what the hold is for even when its own reads fail, so it proceeds.
#[tokio::test]
async fn a_stop_completion_proceeds_on_the_head_the_stop_boundary_stamped() {
    let workspace = tempdir().unwrap();
    let scene = scene(workspace.path());
    scene
        .db
        .set_revision_stop_contributed_head(&scene.execution_id, PUSHED)
        .unwrap();
    let handler = scene.handler(Err("gh api: 502 Bad Gateway"), HeadProbe(None), AdmitAtHead::at(PUSHED));

    let outcome = handler
        .finalize_pr_transition(
            &scene.execution_id,
            PR.into(),
            WorkerPrCompletionTarget::InReview,
            "stop_sha_delta",
        )
        .await;

    assert_eq!(outcome, StopOutcome::ReviewerEnqueued { pr_url: PR.into() });
    assert_eq!(scene.status(), TaskStatus::Active);
    assert_eq!(scene.awaited_head().as_deref(), Some(PUSHED));
}
