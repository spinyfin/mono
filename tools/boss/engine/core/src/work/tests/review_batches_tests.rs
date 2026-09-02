//! `pr_review_batches` / `pr_review_batch_members`: immutable target,
//! classification, membership, and uniqueness contracts.

use boss_protocol::{
    ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket,
    ReviewProfile,
};

use super::*;

fn classification() -> ReviewClassification {
    ReviewClassification::builder()
        .changed_files(vec!["tools/boss/engine/pr-review/src/parsing.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
        .additions(12)
        .deletions(3)
        .build()
}

fn batch_input(cycle_root_id: String, target_sha: &str, phase: ReviewBatchPhase) -> ReviewBatchCreateInput {
    ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root_id)
        .base_sha("base-sha")
        .classification(classification())
        .phase(phase)
        .pr_number(42)
        .pr_url("https://github.com/example/repo/pull/42")
        .target_sha(target_sha)
        .build()
}

fn member(role: ReviewBatchMemberRole, execution_id: Option<String>) -> ReviewBatchMemberCreateInput {
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
        .status(ReviewBatchMemberStatus::Pending)
        .maybe_execution_id(execution_id)
        .build()
}

/// Batch persistence preserves the raw classifier result and the resolved
/// per-member model/effort rather than referring back to mutable task policy.
#[test]
fn review_batch_round_trips_classification_and_member_policy() {
    let db = WorkDb::open(temp_db_path("review-batch-roundtrip")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let (created, members) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[
                member(ReviewBatchMemberRole::ClaudeReviewer, Some(execution.id.clone())),
                member(ReviewBatchMemberRole::CodexReviewer, None),
                member(ReviewBatchMemberRole::GrokReviewer, None),
            ],
        )
        .unwrap();

    assert_eq!(created.classification, classification());
    assert_eq!(created.phase, ReviewBatchPhase::PreMerge);
    assert_eq!(members.len(), 3);

    let by_target = db
        .review_batch_for_target(&cycle_root.id, ReviewBatchPhase::PreMerge, "head-sha")
        .unwrap()
        .expect("batch is queryable by immutable target");
    assert_eq!(by_target.id, created.id);
    assert_eq!(by_target.classification.changed_files, classification().changed_files);

    let stored_members = db.review_batch_members(&created.id).unwrap();
    assert_eq!(stored_members.len(), 3);
    assert_eq!(stored_members[0].role, ReviewBatchMemberRole::ClaudeReviewer);
    assert_eq!(stored_members[0].execution_id.as_deref(), Some(execution.id.as_str()));
    assert_eq!(stored_members[0].provider_effort, "medium");
    assert_eq!(stored_members[0].resolved_model, "test-model");

    let by_execution = db
        .review_batch_member_for_execution(&execution.id)
        .unwrap()
        .expect("execution is linked to exactly one review member");
    assert_eq!(by_execution.id, stored_members[0].id);
}

/// These are the design's idempotency keys: one immutable target per cycle
/// phase, and one explicit retry number for each member role in a batch.
#[test]
fn review_batch_uniqueness_rules_are_enforced_by_sqlite() {
    let db = WorkDb::open(temp_db_path("review-batch-unique")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");

    let (created, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();

    let duplicate_target = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap_err();
    assert!(
        duplicate_target.to_string().to_lowercase().contains("unique"),
        "immutable target uniqueness must be SQLite-enforced: {duplicate_target}"
    );

    let duplicate_member = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "next-head-sha", ReviewBatchPhase::PreMerge),
            &[
                member(ReviewBatchMemberRole::ClaudeReviewer, None),
                member(ReviewBatchMemberRole::ClaudeReviewer, None),
            ],
        )
        .unwrap_err();
    assert!(
        duplicate_member.to_string().to_lowercase().contains("unique"),
        "member retry uniqueness must be SQLite-enforced: {duplicate_member}"
    );
    assert!(
        db.review_batch_for_target(&cycle_root.id, ReviewBatchPhase::PreMerge, "next-head-sha")
            .unwrap()
            .is_none(),
        "a failed member insert must roll the batch row back"
    );

    assert!(db.review_batch(&created.id).unwrap().is_some());
}

#[test]
fn review_batch_rejects_a_role_not_allowed_in_its_phase() {
    let db = WorkDb::open(temp_db_path("review-batch-role-phase")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let input = ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root.id)
        .base_sha("base-sha")
        .classification(classification())
        .phase(ReviewBatchPhase::PostMerge)
        .pr_number(42)
        .pr_url("https://github.com/example/repo/pull/42")
        .target_sha("merge-sha")
        .merge_sha("merge-sha")
        .build();

    let error = db
        .create_review_batch(input, &[member(ReviewBatchMemberRole::ClaudeReviewer, None)])
        .unwrap_err();
    assert!(error.to_string().contains("invalid for post_merge batch"));
}

#[test]
fn review_batch_rejects_invalid_phase_merge_sha_combinations_and_empty_members() {
    let db = WorkDb::open(temp_db_path("review-batch-invariants")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");

    let missing_merge_sha = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "merge-sha", ReviewBatchPhase::PostMerge),
            &[member(ReviewBatchMemberRole::PostMergeReviewer, None)],
        )
        .unwrap_err();
    assert!(missing_merge_sha.to_string().contains("require a merge SHA"));

    let pre_merge_with_merge_sha = ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root.id.clone())
        .base_sha("base-sha")
        .classification(classification())
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url("https://github.com/example/repo/pull/42")
        .target_sha("head-sha")
        .merge_sha("merge-sha")
        .build();
    let unexpected_merge_sha = db
        .create_review_batch(
            pre_merge_with_merge_sha,
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap_err();
    assert!(
        unexpected_merge_sha
            .to_string()
            .contains("must not include a merge SHA")
    );

    let empty_members = db
        .create_review_batch(batch_input(cycle_root.id, "head-sha", ReviewBatchPhase::PreMerge), &[])
        .unwrap_err();
    assert!(empty_members.to_string().contains("require at least one member"));
}
