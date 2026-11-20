//! A revision that pushed and finished always leaves `active`, whatever order
//! the read of the head it pushed, the end of its run and the review of that
//! head arrive in.
//!
//! Each test follows one order end to end: the run ends through the
//! completion handler, admission runs against the work database with the
//! pull-request read replaced by a fixed head, the reviewers report, and the
//! supervisor's verdict is applied. Every one ends with the revision in
//! review and the revision queued behind it dispatchable.
//!
//! On 2026-11-09 (spinyfin/mono#3502) a revision ended its run by declaring
//! delivery, its review was admitted only minutes later, and the verdict of
//! that review released nothing: the revision stayed `active` with no live
//! run, and the revision the verdict created waited on it until someone moved
//! it by hand.

use std::time::Duration;

use boss_protocol::{CreateRevisionInput, ReviewBatch};

use super::*;
use crate::work::{PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND, ReviewBatchDispatch, WorkExecution};

const PR: &str = "https://github.com/spinyfin/mono/pull/2070";
const OTHER_PR: &str = "https://github.com/spinyfin/mono/pull/2071";
const REPO: &str = "git@github.com:spinyfin/mono.git";
/// The head the revision started from, already reviewed.
const REVIEWED: &str = "reviewed-head";
/// The head the revision pushed.
const PUSHED: &str = "pushed-head";
/// A head pushed to the PR after the revision's.
const LATER: &str = "later-head";

/// A mergeable open PR whose answer carries the scene's current head.
struct HeadProbe(Arc<std::sync::Mutex<&'static str>>);

#[async_trait]
impl MergeProbe for HeadProbe {
    async fn probe(&self, url: &str) -> Result<boss_observation_contracts::PrLifecycleProbe> {
        let mut probe = FixedMergeabilityProbe {
            mergeability: OpenPrMergeability::Clean,
        }
        .probe(url)
        .await?;
        probe.head_ref_oid = Some((*self.0.lock().unwrap()).to_owned());
        Ok(probe)
    }
}

/// Admission as production performs it, with the pull-request metadata read
/// replaced by the scene's current head.
struct AdmitAtCurrentHead(Arc<std::sync::Mutex<&'static str>>);

#[async_trait]
impl ReviewBatchEnqueuer for AdmitAtCurrentHead {
    async fn enqueue(
        &self,
        db: &WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
    ) -> Result<ReviewBatchDispatch> {
        let head = *self.0.lock().unwrap();
        let input = review_batch_input_from_metadata(db, work_item_id, pr_url, pr_metadata(head))?;
        db.create_pre_merge_review_batch_for_pool(input, repo_remote_url, review_pool_size)
    }
}

fn pr_metadata(head: &str) -> serde_json::Value {
    serde_json::json!({
        "baseRefOid": "base-sha",
        "headRefOid": head,
        "files": [{"path": "src/lib.rs"}],
        "additions": 12,
        "deletions": 3
    })
}

struct Scene {
    /// The database's directory and the worker's workspace, kept alive.
    dirs: (TempDir, TempDir),
    db: Arc<WorkDb>,
    revision_id: String,
    execution_id: String,
    /// What a read of the pull request's head returns right now.
    head: Arc<std::sync::Mutex<&'static str>>,
}

impl Scene {
    /// A revision whose worker is live on a PR last reviewed at [`REVIEWED`],
    /// and has pushed [`PUSHED`].
    fn new() -> Self {
        let workspace = tempdir().unwrap();
        let (dir, db, _product, revision_id, execution_id) = revision_fixture(workspace.path(), PR, REVIEWED);
        // Dispatch leaves one run for the revision; the fixture's second
        // queued row is not something production has.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'abandoned' WHERE work_item_id = ?1 AND id != ?2",
                rusqlite::params![revision_id, execution_id],
            )
            .unwrap();
        Self {
            dirs: (dir, workspace),
            db,
            revision_id,
            execution_id,
            head: Arc::new(std::sync::Mutex::new(PUSHED)),
        }
    }

    /// The task that owns the PR the revision pushes to.
    fn root_id(&self) -> String {
        self.db.review_cycle_root_id(&self.revision_id)
    }

    /// Another revision of the same PR, created while this one's worker is
    /// running and so queued behind it.
    fn queue_revision_behind(&self) -> String {
        let later = self
            .db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(self.root_id())
                    .description("more to change on the same PR")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        assert_eq!(
            self.db.gating_prereqs_for(&later.id).unwrap(),
            vec![self.revision_id.clone()]
        );
        later.id
    }

    fn set_head(&self, head: &'static str) {
        *self.head.lock().unwrap() = head;
    }

    fn enqueuer(&self) -> Arc<AdmitAtCurrentHead> {
        Arc::new(AdmitAtCurrentHead(self.head.clone()))
    }

    /// The completion handler, holding a delivering revision for review as
    /// production does. `review_pool_size` bounds how many batches admission
    /// lets run at once.
    fn handler(&self, review_pool_size: usize) -> WorkerCompletionHandler {
        let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            self.dirs.0.path().join("flags.toml"),
        ));
        flags.load().unwrap();
        flags.set("review_batch_fanout", true).unwrap();
        let verifier = StubBranchVerifier::ok("boss/test");
        let head = (*self.head.lock().unwrap()).to_owned();
        *verifier.head_oid_result.try_lock().unwrap() = Ok(head.clone());
        *verifier.fresh_head_oid_result.try_lock().unwrap() = Ok(head);
        engine_harness(self.db.clone(), StubPrDetector::ok(None))
            .handler
            .with_enable_revision_triggered_reviews(true)
            .with_feature_flags(flags)
            .with_branch_verifier(verifier)
            .with_merge_probe(Arc::new(HeadProbe(self.head.clone())))
            .with_review_pool_size(review_pool_size)
            .with_review_batch_enqueuer(self.enqueuer())
    }

    /// The worker declares its run delivered, the way a run normally ends.
    async fn declare_delivered(&self, handler: &WorkerCompletionHandler) {
        let outcome = handler
            .finalize_declared_run_done(&self.execution_id, boss_protocol::RunDoneOutcome::Delivered)
            .await;
        assert!(matches!(outcome, StopOutcome::PrDetected { .. }), "{outcome:?}");
        assert_eq!(self.run().status, ExecutionStatus::Completed);
    }

    fn run(&self) -> WorkExecution {
        self.db.get_execution(&self.execution_id).unwrap()
    }

    fn task(&self, id: &str) -> boss_protocol::Task {
        match self.db.get_work_item(id).unwrap() {
            WorkItem::Task(task) | WorkItem::Chore(task) => task,
            other => panic!("expected a task, got {other:?}"),
        }
    }

    fn status(&self) -> TaskStatus {
        self.task(&self.revision_id).status
    }

    fn awaited_head(&self) -> Option<String> {
        self.db.get_revision_stop_contributed_head(&self.execution_id).unwrap()
    }

    /// The pre-merge batch for the root at `head`, once one exists.
    fn batch_at(&self, head: &str) -> Option<ReviewBatch> {
        self.db
            .review_batches_for_cycle_root(&self.root_id())
            .unwrap()
            .into_iter()
            .find(|batch| batch.target_sha == head)
    }

    /// A review started for the root alone, as `bossctl review start` or
    /// another row's push admits it. It names no revision.
    fn admit_for_root(&self, head: &str) -> ReviewBatch {
        let input = review_batch_input_from_metadata(&self.db, &self.root_id(), PR, pr_metadata(head)).unwrap();
        match self.db.request_pre_merge_review_batch_for_pool(input, 16).unwrap() {
            ReviewBatchDispatch::Created { batch, .. } => batch,
            other => panic!("expected a new batch, got {other:?}"),
        }
    }

    /// Both reviewers of `batch` report and the supervisor's verdict is
    /// applied against a PR still at the batch's head. Returns the revision
    /// the verdict created, when it found something.
    fn review_to_a_verdict(&self, batch: &ReviewBatch, findings: bool) -> Option<String> {
        let (batch_id, head) = (&batch.id, &batch.target_sha);
        let reviewers: Vec<String> = self
            .db
            .review_batch_members(batch_id)
            .unwrap()
            .into_iter()
            .filter(|member| member.role != ReviewBatchMemberRole::Supervisor)
            .filter_map(|member| member.execution_id)
            .collect();
        assert_eq!(reviewers.len(), 2);
        let mut sources = Vec::new();
        for reviewer in &reviewers {
            let leaf_findings = if findings {
                serde_json::json!([{
                    "severity": "high", "category": "correctness", "confidence": "high",
                    "file": "src/lib.rs", "title": "Unchecked index", "problem": "Out of bounds read.",
                    "impact": "The worker crashes.", "suggested_fix": "Check the index.",
                    "static_evidence": "The index is not checked.", "needs_runtime_verification": false
                }])
            } else {
                serde_json::json!([])
            };
            let report = self.db
                .submit_worker_proposal(SubmitWorkerProposalInput {
                    execution_id: reviewer,
                    work_item_id: &self.root_id(),
                    kind: ProposalKind::ReviewReport,
                    payload_json: &format!(
                        r#"{{"batch_id":"{batch_id}","target_sha":"{head}","report":{{"batch_id":"{batch_id}","pr_url":"{PR}","target_sha":"{head}","phase":"pre_merge","summary":"Reviewed.","coverage":{{"files_inspected":[],"files_omitted":[],"limitations":[]}},"findings":{leaf_findings}}}}}"#
                    ),
                    idempotency_key: "report",
                })
                .unwrap()
                .unwrap();
            assert_eq!(report.proposal.state, boss_protocol::ProposalState::Applied);
            let payload: serde_json::Value = serde_json::from_str(&report.proposal.payload_json).unwrap();
            for finding in payload["report"]["findings"].as_array().unwrap() {
                sources.push(finding["id"].as_str().unwrap().to_owned());
            }
        }
        let sources = serde_json::to_string(&sources).unwrap();
        let supervisor = self
            .db
            .review_batch_members(batch_id)
            .unwrap()
            .into_iter()
            .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
            .and_then(|member| member.execution_id)
            .expect("both reports dispatch the supervisor");
        let verdict = if findings {
            format!(
                r#"{{"batch_id":"{batch_id}","verdict":{{"batch_id":"{batch_id}","pr_url":"{PR}","target_sha":"{head}","phase":"pre_merge","summary":"One defect.","revision_warranted":true,"findings":[{{"id":"consolidated-1","severity":"high","category":"correctness","confidence":"high","file":"src/lib.rs","title":"Unchecked index","detail":"Out of bounds read.","sources":{sources}}}],"contradictions":[]}}}}"#
            )
        } else {
            format!(
                r#"{{"batch_id":"{batch_id}","verdict":{{"batch_id":"{batch_id}","pr_url":"{PR}","target_sha":"{head}","phase":"pre_merge","summary":"Clean.","revision_warranted":false,"findings":[],"contradictions":[]}}}}"#
            )
        };
        let submitted = self
            .db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &supervisor,
                work_item_id: &self.root_id(),
                kind: ProposalKind::ReviewVerdict,
                payload_json: &verdict,
                idempotency_key: "verdict",
            })
            .unwrap()
            .unwrap();
        assert_eq!(submitted.proposal.state, boss_protocol::ProposalState::Proposed);
        let created = crate::test_support::apply_review_verdict(
            &self.db,
            &submitted.proposal.id,
            &FakePrStateChecker::always(PrOpenState::Open).with_head_sha(head.clone()),
        )
        .unwrap();
        assert_eq!(
            self.db.review_batch(batch_id).unwrap().unwrap().status,
            boss_protocol::ReviewBatchStatus::Completed
        );
        created
    }

    /// The batch at `head` failed for good: both automatic generations ended
    /// without a verdict, so no replacement will be minted.
    fn fail_for_good(&self, batch: &ReviewBatch) {
        for member in self.db.review_batch_members(&batch.id).unwrap() {
            if let Some(execution_id) = member.execution_id {
                self.db.mark_execution_redundant(&execution_id).unwrap();
            }
        }
        let conn = self.db.connect().unwrap();
        conn.execute(
            "UPDATE pr_review_batch_members SET status = 'failed' WHERE batch_id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE pr_review_batches SET status = 'failed', generation = ?2 WHERE id = ?1",
            rusqlite::params![batch.id, crate::work::MAX_AUTOMATIC_PRE_MERGE_BATCH_GENERATIONS],
        )
        .unwrap();
    }

    /// The normal state after a revision's push was reviewed, and what it
    /// unblocks: `task_id` waits on nothing and its run can be claimed.
    fn assert_released_and_dispatchable(&self, task_id: &str) {
        assert_eq!(self.status(), TaskStatus::InReview, "the revision left `active`");
        assert!(
            self.db.gating_prereqs_for(task_id).unwrap().is_empty(),
            "nothing gates the revision queued behind it"
        );
        let runs = self.db.list_executions(Some(task_id)).unwrap();
        let pending: Vec<_> = runs.iter().filter(|run| !run.status.is_terminal()).collect();
        assert_eq!(pending.len(), 1, "one pending run: {runs:?}");
        assert_eq!(pending[0].status, ExecutionStatus::Ready, "its run can be claimed");
    }
}

/// Wait for work a completion leaves to a background task.
async fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A Stop boundary read the pushed head while the worker was still running,
/// and the run then ended by declaring delivery.
#[tokio::test]
async fn a_head_read_before_the_run_ended_releases_the_revision_at_its_verdict() {
    let scene = Scene::new();
    let later = scene.queue_revision_behind();
    scene
        .db
        .set_revision_stop_contributed_head(&scene.execution_id, PUSHED)
        .unwrap();

    scene.declare_delivered(&scene.handler(16)).await;
    eventually("the review of the pushed head is admitted", || {
        scene.batch_at(PUSHED).is_some()
    })
    .await;
    assert_eq!(scene.status(), TaskStatus::Active, "held while its review runs");

    scene.review_to_a_verdict(&scene.batch_at(PUSHED).unwrap(), false);

    scene.assert_released_and_dispatchable(&later);
}

/// Nothing read the head before the run ended. The completion reads it, and
/// records it on the run and as the head the revision is held for, before the
/// review is admitted.
#[tokio::test]
async fn a_head_first_read_as_the_run_ended_releases_the_revision_at_its_verdict() {
    let scene = Scene::new();
    let later = scene.queue_revision_behind();
    assert_eq!(scene.awaited_head(), None);

    scene.declare_delivered(&scene.handler(16)).await;
    assert_eq!(scene.run().pr_head_after.as_deref(), Some(PUSHED));
    assert_eq!(scene.awaited_head().as_deref(), Some(PUSHED));
    eventually("the review of the pushed head is admitted", || {
        scene.batch_at(PUSHED).is_some()
    })
    .await;

    scene.review_to_a_verdict(&scene.batch_at(PUSHED).unwrap(), false);

    scene.assert_released_and_dispatchable(&later);
}

/// The PR moved again between the end of the run and the read admission
/// made. The review that covers the revision's push is the one of the head
/// the PR has now, so the hold follows that read.
#[tokio::test]
async fn a_newer_head_read_after_the_run_ended_is_the_one_the_hold_follows() {
    let scene = Scene::new();
    let later = scene.queue_revision_behind();
    let handler = scene.handler(16);
    // The completion reads PUSHED; every read after it sees LATER.
    scene.set_head(LATER);

    scene.declare_delivered(&handler).await;
    assert_eq!(scene.run().pr_head_after.as_deref(), Some(PUSHED));
    eventually("the review of the head the PR has now is admitted", || {
        scene.batch_at(LATER).is_some()
    })
    .await;
    eventually("the hold follows that review", || {
        scene.awaited_head().as_deref() == Some(LATER)
    })
    .await;
    assert!(scene.batch_at(PUSHED).is_none());

    scene.review_to_a_verdict(&scene.batch_at(LATER).unwrap(), false);

    scene.assert_released_and_dispatchable(&later);
}

/// The order of 2026-11-09. The review pool is full when the run ends, so the
/// completion's admission is deferred and creates nothing. The batch is
/// admitted later by the deferred-admission sweep, finds something, and its
/// verdict creates the next revision. That verdict releases the revision
/// that pushed, and the revision it created is dispatchable.
#[tokio::test]
async fn a_review_admitted_only_after_the_run_ended_releases_the_revision_at_its_verdict() {
    let scene = Scene::new();
    // Another PR's review occupies the one reservation this pool has.
    let product = scene.task(&scene.root_id()).product_id;
    let other = create_test_chore_manual(&scene.db, product, "another PR in review");
    scene
        .db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![other.id, OTHER_PR],
        )
        .unwrap();
    let pool = crate::work::PRE_MERGE_BATCH_RESERVATION_UNITS as usize;
    let other_input = review_batch_input_from_metadata(&scene.db, &other.id, OTHER_PR, pr_metadata("other")).unwrap();
    assert!(matches!(
        scene
            .db
            .create_pre_merge_review_batch_for_pool(other_input, REPO, pool)
            .unwrap(),
        ReviewBatchDispatch::Created { .. }
    ));

    scene.declare_delivered(&scene.handler(pool)).await;
    eventually("the deferral is recorded on the revision", || {
        scene
            .db
            .list_open_attention_items_of_kind(PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
            .unwrap()
            .iter()
            .any(|item| item.work_item_id.as_deref() == Some(scene.revision_id.as_str()))
    })
    .await;
    assert!(scene.batch_at(PUSHED).is_none(), "the completion admitted nothing");
    assert_eq!(scene.status(), TaskStatus::Active);

    let swept =
        boss_engine_merge_poller::test_support::sweep_deferred_review_admission(&scene.db, scene.enqueuer().as_ref())
            .await;
    assert_eq!(swept.review_admission_recovered, 1, "{swept:?}");
    let batch = scene.batch_at(PUSHED).expect("the sweep admits the review");

    let created = scene
        .review_to_a_verdict(&batch, true)
        .expect("a verdict with findings creates the next revision");

    scene.assert_released_and_dispatchable(&created);
}

/// The review of the pushed head was started for the root while the
/// revision's worker was still running, and is still running when the run
/// ends. Its verdict releases the revision although it was admitted before
/// the revision could be held for anything.
#[tokio::test]
async fn a_batch_admitted_before_the_run_ended_releases_the_revision_at_its_verdict() {
    let scene = Scene::new();
    let later = scene.queue_revision_behind();
    let batch = scene.admit_for_root(PUSHED);
    assert_eq!(batch.producing_work_item_id, None, "admitted for the root alone");

    scene.declare_delivered(&scene.handler(16)).await;
    assert_eq!(scene.awaited_head().as_deref(), Some(PUSHED));
    assert_eq!(scene.status(), TaskStatus::Active, "held while that review runs");

    scene.review_to_a_verdict(&batch, false);

    scene.assert_released_and_dispatchable(&later);
    assert_eq!(
        scene.db.review_batches_for_cycle_root(&scene.root_id()).unwrap().len(),
        1,
        "the push is not reviewed a second time"
    );
}

/// The review of the pushed head was started for the root and reached its
/// verdict while the revision's worker was still running. No decision is
/// left to release the revision, so the end of its run does, at either
/// completion boundary.
#[tokio::test]
async fn a_batch_decided_before_the_run_ended_releases_the_revision_as_it_finishes() {
    for boundary in ["declared", "stop"] {
        let scene = Scene::new();
        let later = scene.queue_revision_behind();
        let batch = scene.admit_for_root(PUSHED);
        scene.review_to_a_verdict(&batch, false);
        assert_eq!(
            scene.status(),
            TaskStatus::Active,
            "{boundary}: its worker is still running"
        );

        let handler = scene.handler(16);
        if boundary == "declared" {
            scene.declare_delivered(&handler).await;
        } else {
            // The worker stops having pushed: the Stop boundary reads the
            // head, finds it moved, and finalizes the run.
            let outcome = handler.on_stop(&scene.execution_id).await;
            assert_eq!(outcome, StopOutcome::PrDetected { pr_url: PR.into() }, "{boundary}");
        }
        eventually("the revision leaves its hold", || {
            scene.status() == TaskStatus::InReview
        })
        .await;

        scene.assert_released_and_dispatchable(&later);
        assert_eq!(
            scene.db.review_batches_for_cycle_root(&scene.root_id()).unwrap().len(),
            1,
            "{boundary}: a decided head is not reviewed again"
        );
    }
}

/// The review of the pushed head failed for good while the revision's worker
/// was still running: it has no verdict and no replacement will be minted.
/// A failure is a decision too, so the end of the run releases the revision
/// rather than holding it for a review that is never coming.
#[tokio::test]
async fn a_batch_that_failed_before_the_run_ended_releases_the_revision_as_it_finishes() {
    for boundary in ["declared", "stop"] {
        let scene = Scene::new();
        let later = scene.queue_revision_behind();
        let batch = scene.admit_for_root(PUSHED);
        scene.fail_for_good(&batch);

        let handler = scene.handler(16);
        if boundary == "declared" {
            scene.declare_delivered(&handler).await;
        } else {
            // The worker stops having pushed: the Stop boundary reads the
            // head, finds it moved, and finalizes the run.
            let outcome = handler.on_stop(&scene.execution_id).await;
            assert_eq!(outcome, StopOutcome::PrDetected { pr_url: PR.into() }, "{boundary}");
        }
        eventually("the revision leaves its hold", || {
            scene.status() == TaskStatus::InReview
        })
        .await;

        scene.assert_released_and_dispatchable(&later);
    }
}
