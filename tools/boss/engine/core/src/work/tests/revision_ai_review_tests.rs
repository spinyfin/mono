//! A revision held in Doing is "under AI review" only while the review of its
//! pushed commit is actually running.

use boss_protocol::{
    ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket,
    ReviewProfile,
};

use super::*;

const PR_URL: &str = "https://github.com/example/repo/pull/42";

/// An `in_review` chain root with a bound PR and a revision that has finished
/// implementing and is held `active` with the PR URL stamped on it.
struct HeldRevision {
    db: WorkDb,
    root_id: String,
    revision_id: String,
}

fn held_revision(label: &str) -> HeldRevision {
    let db = WorkDb::open(temp_db_path(label)).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "revision root");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![root.id, PR_URL],
        )
        .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head-sha");
    let revision = db.create_revision(revision_input(&root.id), &checker).unwrap();
    let implementation = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![implementation.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![revision.id, PR_URL],
        )
        .unwrap();
    HeldRevision {
        db,
        root_id: root.id,
        revision_id: revision.id,
    }
}

/// A pre-merge review batch on the chain root, as the reviewers are launched.
fn start_review_batch(held: &HeldRevision) -> String {
    let classification = ReviewClassification::builder()
        .changed_files(vec!["tools/boss/engine/pr-review/src/parsing.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
        .additions(12)
        .deletions(3)
        .build();
    let member = ReviewBatchMemberCreateInput::builder()
        .attempt(1)
        .provider_effort("medium")
        .requested_driver("claude")
        .resolved_model("test-model")
        .role(ReviewBatchMemberRole::ClaudeReviewer)
        .status(ReviewBatchMemberStatus::Pending)
        .build();
    let input = ReviewBatchCreateInput::builder()
        .cycle_root_id(held.root_id.clone())
        .base_sha("base-sha")
        .classification(classification)
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url(PR_URL)
        .target_sha("head-sha")
        .build();
    held.db.create_review_batch(input, &[member]).unwrap().0.id
}

#[test]
fn a_held_revision_is_under_review_while_the_roots_batch_is_live() {
    let held = held_revision("revision-ai-review-live-batch");
    assert!(
        held.db.list_revisions_under_ai_review().unwrap().is_empty(),
        "a held revision with no review running is not being worked on"
    );

    let batch_id = start_review_batch(&held);
    assert_eq!(
        held.db.list_revisions_under_ai_review().unwrap(),
        vec![held.revision_id.clone()]
    );

    // Consolidation and verdict apply are still the review in progress.
    for status in ["supervising", "applying"] {
        held.db
            .connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_batches SET status = ?2 WHERE id = ?1",
                rusqlite::params![batch_id, status],
            )
            .unwrap();
        assert_eq!(
            held.db.list_revisions_under_ai_review().unwrap(),
            vec![held.revision_id.clone()],
            "{status}"
        );
    }

    // Once the review is consolidated the hold is released and nothing is
    // left to group.
    held.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
            rusqlite::params![batch_id],
        )
        .unwrap();
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
}

#[test]
fn a_running_review_execution_on_the_revision_itself_counts() {
    let held = held_revision("revision-ai-review-own-execution");
    let review = held
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(held.revision_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    assert_eq!(
        held.db.list_revisions_under_ai_review().unwrap(),
        vec![held.revision_id.clone()]
    );
    held.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![review.id],
        )
        .unwrap();
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
}

#[test]
fn a_revision_being_implemented_again_is_not_under_review() {
    let held = held_revision("revision-ai-review-reimplementation");
    start_review_batch(&held);
    held.db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(held.revision_id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert!(
        held.db.list_revisions_under_ai_review().unwrap().is_empty(),
        "a new implementation run supersedes the held push"
    );
}

#[test]
fn only_a_held_active_revision_with_the_stamped_pr_is_considered() {
    let held = held_revision("revision-ai-review-not-held");
    start_review_batch(&held);
    let set = |sql: &str| {
        held.db
            .connect()
            .unwrap()
            .execute(sql, rusqlite::params![held.revision_id])
            .unwrap();
    };

    set("UPDATE tasks SET pr_url = NULL WHERE id = ?1");
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
    set("UPDATE tasks SET pr_url = 'https://github.com/example/repo/pull/42' WHERE id = ?1");
    assert_eq!(held.db.list_revisions_under_ai_review().unwrap().len(), 1);

    // Released by the verdict: folded into the parent, no longer in Doing.
    set("UPDATE tasks SET status = 'in_review' WHERE id = ?1");
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
}
