//! Admission deferral: the producing task is held in PendingReview, then
//! recovered once a review-pool reservation frees.

use std::sync::Arc;

use async_trait::async_trait;
use tempfile::tempdir;

use super::*;
use crate::completion::ReviewBatchEnqueuer;
use crate::work::{
    CreateExecutionInput, ReviewBatchCreateInput, ReviewBatchDispatch, ReviewBatchMemberCreateInput,
    WorkerPrCompletionTarget,
};
use boss_protocol::{
    ExecutionKind, ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification,
    ReviewLanguageBucket, ReviewProfile, TaskStatus,
};

const PR_URL: &str = "https://github.com/example/repo/pull/42";

struct FixedShaReviewBatchEnqueuer {
    base_sha: String,
    target_sha: String,
}

#[async_trait]
impl ReviewBatchEnqueuer for FixedShaReviewBatchEnqueuer {
    async fn enqueue(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
    ) -> anyhow::Result<ReviewBatchDispatch> {
        let pr_number = boss_github::pr_url::pr_number_from_url(pr_url)
            .ok_or_else(|| anyhow::anyhow!("could not parse PR number from {pr_url}"))?
            .try_into()?;
        let input = ReviewBatchCreateInput::builder()
            .cycle_root_id(work_db.review_cycle_root_id(work_item_id))
            .base_sha(self.base_sha.clone())
            .classification(
                ReviewClassification::builder()
                    .changed_files(vec!["src/lib.rs".to_owned()])
                    .complexity_flags(vec![])
                    .has_production_code(true)
                    .metadata_missing(vec![])
                    .production_languages(vec![ReviewLanguageBucket::Rust])
                    .profile(ReviewProfile::Light)
                    .subsystem_buckets(vec!["src".to_owned()])
                    .build(),
            )
            .phase(ReviewBatchPhase::PreMerge)
            .pr_number(pr_number)
            .pr_url(pr_url)
            .target_sha(self.target_sha.clone())
            .build();
        work_db.create_pre_merge_review_batch_for_pool(input, repo_remote_url, review_pool_size)
    }
}

fn fill_pre_merge_pool(db: &WorkDb, product_id: &str) -> Vec<String> {
    let member = ReviewBatchMemberCreateInput::builder()
        .attempt(1)
        .provider_effort("medium")
        .requested_driver("claude")
        .resolved_model("test-model")
        .role(ReviewBatchMemberRole::ClaudeReviewer)
        .status(ReviewBatchMemberStatus::Pending)
        .build();
    let mut ids = Vec::new();
    for i in 0..4 {
        let cycle_root = create_test_chore_manual(db, product_id.to_owned(), format!("pool occupant {i}"));
        let (batch, _) = db
            .create_review_batch(
                ReviewBatchCreateInput::builder()
                    .cycle_root_id(cycle_root.id)
                    .base_sha("base-sha")
                    .classification(
                        ReviewClassification::builder()
                            .changed_files(vec!["src/lib.rs".to_owned()])
                            .complexity_flags(vec![])
                            .has_production_code(true)
                            .metadata_missing(vec![])
                            .production_languages(vec![ReviewLanguageBucket::Rust])
                            .profile(ReviewProfile::Light)
                            .subsystem_buckets(vec!["src".to_owned()])
                            .build(),
                    )
                    .phase(ReviewBatchPhase::PreMerge)
                    .pr_number(i as i64 + 1)
                    .pr_url(format!("https://github.com/example/repo/pull/{}", i + 1))
                    .target_sha(format!("head-sha-{i}"))
                    .build(),
                std::slice::from_ref(&member),
            )
            .unwrap();
        ids.push(batch.id);
    }
    ids
}

fn fanout_flags(path: &std::path::Path) -> Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        path.join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("review_batch_fanout", true).unwrap();
    flags
}

#[tokio::test]
async fn finalize_pr_transition_holds_on_admission_deferral_then_sweep_admits() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let product_id = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(task) | WorkItem::Task(task) => task.product_id,
        other => panic!("expected chore, got {other:?}"),
    };
    let occupying = fill_pre_merge_pool(&db, &product_id);
    assert!(!db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap());

    let flags_dir = tempdir().unwrap();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(fanout_flags(flags_dir.path()))
        .with_review_batch_enqueuer(Arc::new(FixedShaReviewBatchEnqueuer {
            base_sha: "base-sha".to_owned(),
            target_sha: "new-head-sha".to_owned(),
        }));

    let outcome = handler
        .finalize_pr_transition(
            &execution_id,
            PR_URL.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "test",
        )
        .await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "deferred admission still holds the producer pending review; got {outcome:?}"
    );

    let chore = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(task) | WorkItem::Task(task) => task,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(chore.status, TaskStatus::Active, "task must stay in Doing");
    assert_eq!(chore.pr_url.as_deref(), Some(PR_URL));
    assert!(
        db.review_batch_for_target(&chore_id, ReviewBatchPhase::PreMerge, "new-head-sha")
            .unwrap()
            .is_none(),
        "a deferred admission must not write a batch"
    );
    let deferred = db.list_tasks_awaiting_pre_merge_review_admission().unwrap();
    assert!(
        deferred.iter().any(|candidate| candidate.task_id == chore_id),
        "held task must be a deferred-admission candidate: {deferred:?}"
    );
    let attentions = db.list_attention_items_for_work_item(&chore_id).unwrap();
    assert!(
        attentions
            .iter()
            .any(|item| item.kind == crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND),
        "deferral must file an attention item; got {attentions:?}"
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
            rusqlite::params![occupying[0]],
        )
        .unwrap();

    let recovered = db
        .create_pre_merge_review_batch(
            ReviewBatchCreateInput::builder()
                .cycle_root_id(chore_id.clone())
                .base_sha("base-sha")
                .classification(
                    ReviewClassification::builder()
                        .changed_files(vec!["src/lib.rs".to_owned()])
                        .complexity_flags(vec![])
                        .has_production_code(true)
                        .metadata_missing(vec![])
                        .production_languages(vec![ReviewLanguageBucket::Rust])
                        .profile(ReviewProfile::Light)
                        .subsystem_buckets(vec!["src".to_owned()])
                        .build(),
                )
                .phase(ReviewBatchPhase::PreMerge)
                .pr_number(42)
                .pr_url(PR_URL)
                .target_sha("new-head-sha")
                .build(),
            "https://github.com/example/repo",
        )
        .unwrap();
    match recovered {
        ReviewBatchDispatch::Created { batch, executions } => {
            assert_eq!(batch.target_sha, "new-head-sha");
            assert_eq!(executions.len(), 3);
        }
        other => panic!("expected a newly created batch after a reservation freed, got {other:?}"),
    }
    assert!(
        db.list_tasks_awaiting_pre_merge_review_admission()
            .unwrap()
            .iter()
            .all(|candidate| candidate.task_id != chore_id),
        "once a live batch exists the held task is no longer a deferred-admission candidate"
    );
}

#[tokio::test]
async fn deferred_admission_query_includes_a_prior_cycle_terminal_review() {
    let workspace = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(workspace.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "prior-cycle");
    db.update_work_item(
        &chore.id,
        crate::work::WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(PR_URL.into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::PrReview)
            .status(crate::work::ExecutionStatus::Completed)
            .build(),
    )
    .unwrap();

    let deferred = db.list_tasks_awaiting_pre_merge_review_admission().unwrap();
    assert!(
        deferred.iter().any(|candidate| candidate.task_id == chore.id),
        "a previous cycle's terminal pr_review must not hide a new PendingReview hold: {deferred:?}"
    );
}
