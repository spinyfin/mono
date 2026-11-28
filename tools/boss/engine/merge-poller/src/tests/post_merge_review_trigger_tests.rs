//! `maybe_trigger_post_merge_review`: Deep-production eligibility, the
//! idempotent merge-SHA batch creation it gates, and the merge-commit /
//! head-SHA fallback for the immutable target it keys on.

use super::*;
use crate::work::{ReviewBatchCreateInput, ReviewBatchMemberCreateInput};
use boss_protocol::{
    ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket,
    ReviewProfile,
};

fn classification(profile: ReviewProfile) -> ReviewClassification {
    ReviewClassification::builder()
        .changed_files(vec!["tools/boss/engine/core/src/lib.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(profile)
        .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
        .additions(400)
        .deletions(40)
        .build()
}

/// Freeze a completed pre-merge batch for `cycle_root_id` at `head_sha`
/// with the given `profile` — the durable classification snapshot
/// [`maybe_trigger_post_merge_review`] reads eligibility from, mirroring
/// what a real pre-merge review cycle leaves behind. Uses the low-level
/// [`WorkDb::create_review_batch`] (a single pre-pinned `ClaudeReviewer`
/// member) rather than the three-leaf dispatch path — only the batch row's
/// `phase`/`classification` matter here.
fn seed_pre_merge_batch(db: &WorkDb, cycle_root_id: &str, head_sha: &str, profile: ReviewProfile) {
    db.create_review_batch(
        ReviewBatchCreateInput::builder()
            .cycle_root_id(cycle_root_id.to_owned())
            .base_sha("base-sha")
            .classification(classification(profile))
            .phase(ReviewBatchPhase::PreMerge)
            .pr_number(1)
            .pr_url("https://github.com/foo/bar/pull/1")
            .target_sha(head_sha.to_owned())
            .build(),
        &[ReviewBatchMemberCreateInput::builder()
            .attempt(1)
            .provider_effort("medium")
            .requested_driver("claude")
            .resolved_model("sonnet")
            .role(ReviewBatchMemberRole::ClaudeReviewer)
            .status(ReviewBatchMemberStatus::Pending)
            .build()],
    )
    .unwrap();
}

fn candidate(product_id: String, work_item_id: String, pr_url: &str) -> PendingMergeCheck {
    PendingMergeCheck {
        work_item_id,
        product_id,
        pr_url: pr_url.to_owned(),
    }
}

fn merged_probe(pr_url: &str, merge_commit_oid: Option<&str>, head_ref_oid: Option<&str>) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url.to_owned())
        .state(PrLifecycleState::Merged)
        .maybe_base_ref_oid(Some("base-sha".to_owned()))
        .maybe_head_ref_oid(head_ref_oid.map(str::to_owned))
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .maybe_merge_commit_oid(merge_commit_oid.map(str::to_owned))
        .build()
}

/// No pre-merge batch on record (batch-mode review disabled for this PR, or
/// a manual/human-driven row) means there is nothing to gate eligibility on
/// — the trigger must not guess, it must skip.
#[tokio::test]
async fn no_post_merge_review_without_a_pre_merge_batch_on_record() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/501";
    let (product_id, work_item_id) = make_chore_in_review(&db, "C-no-pre-merge-batch", pr);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(product_id, work_item_id.clone(), pr);
    let probe = merged_probe(pr, Some("merge-sha-1"), Some("head-sha-1"));

    maybe_trigger_post_merge_review(&db, &publisher, &candidate, &probe, |_| false).await;

    assert_eq!(
        db.review_batches_for_cycle_root(&work_item_id).unwrap().len(),
        0,
        "no batch of any phase should exist"
    );
}

/// A `Standard`/`Light` pre-merge classification is not "large or complex"
/// — no post-merge review for it.
#[tokio::test]
async fn no_post_merge_review_when_pre_merge_profile_is_not_deep() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/502";
    let (product_id, work_item_id) = make_chore_in_review(&db, "C-standard-profile", pr);
    seed_pre_merge_batch(&db, &work_item_id, "head-sha-1", ReviewProfile::Standard);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(product_id, work_item_id.clone(), pr);
    let probe = merged_probe(pr, Some("merge-sha-1"), Some("head-sha-1"));

    maybe_trigger_post_merge_review(&db, &publisher, &candidate, &probe, |_| false).await;

    assert!(
        db.review_batch_for_target(&work_item_id, ReviewBatchPhase::PostMerge, "merge-sha-1")
            .unwrap()
            .is_none()
    );
}

/// A `Deep` pre-merge classification is exactly "large or complex" — the
/// merge trigger creates a single-member post-merge batch keyed on the
/// merge commit SHA, and a redundant call (a late-PR recheck racing the
/// main sweep) is a no-op rather than a duplicate batch.
#[tokio::test]
async fn deep_pr_gets_a_post_merge_batch_keyed_on_the_merge_commit_and_is_idempotent() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/503";
    let (product_id, work_item_id) = make_chore_in_review(&db, "C-deep-profile", pr);
    seed_pre_merge_batch(&db, &work_item_id, "head-sha-1", ReviewProfile::Deep);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(product_id, work_item_id.clone(), pr);
    let probe = merged_probe(pr, Some("merge-sha-1"), Some("head-sha-1"));

    maybe_trigger_post_merge_review(&db, &publisher, &candidate, &probe, |_| false).await;

    let batch = db
        .review_batch_for_target(&work_item_id, ReviewBatchPhase::PostMerge, "merge-sha-1")
        .unwrap()
        .expect("Deep PR must get a post-merge batch keyed on the merge commit");
    assert_eq!(batch.target_sha, "merge-sha-1");
    assert_eq!(batch.merge_sha.as_deref(), Some("merge-sha-1"));
    assert_eq!(batch.classification.profile, ReviewProfile::Deep);
    let members = db.review_batch_members(&batch.id).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].role, ReviewBatchMemberRole::PostMergeReviewer);
    assert_eq!(members[0].provider_effort, "large");

    // Idempotent: a redundant trigger call for the same merge must not mint
    // a second batch.
    maybe_trigger_post_merge_review(&db, &publisher, &candidate, &probe, |_| false).await;
    assert_eq!(
        db.review_batches_for_cycle_root(&work_item_id)
            .unwrap()
            .iter()
            .filter(|b| b.phase == ReviewBatchPhase::PostMerge)
            .count(),
        1,
        "a redundant trigger call must not create a second post-merge batch"
    );
}

/// GitHub's `mergeCommit` field can be omitted on rare merged PRs. Unlike a
/// merge commit, a bare PR head SHA is not guaranteed reachable after a
/// squash merge (GitHub deletes the head branch), so `cube workspace goto
/// --revision` could never fetch or position on it — the trigger must skip
/// the pass rather than schedule a review batch doomed to fail positioning.
#[tokio::test]
async fn skips_when_only_head_ref_oid_is_available() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/504";
    let (product_id, work_item_id) = make_chore_in_review(&db, "C-no-merge-commit", pr);
    seed_pre_merge_batch(&db, &work_item_id, "head-sha-1", ReviewProfile::Deep);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(product_id, work_item_id.clone(), pr);
    let probe = merged_probe(pr, None, Some("head-sha-1"));

    maybe_trigger_post_merge_review(&db, &publisher, &candidate, &probe, |_| false).await;

    assert_eq!(
        db.review_batches_for_cycle_root(&work_item_id)
            .unwrap()
            .iter()
            .filter(|batch| batch.phase == ReviewBatchPhase::PostMerge)
            .count(),
        0,
        "must not schedule a post-merge review batch keyed on an unfetchable head SHA",
    );
}

/// Neither a merge commit nor a head SHA leaves nothing to key the
/// immutable target on — the trigger must skip rather than guess or panic.
#[tokio::test]
async fn skips_when_neither_merge_commit_nor_head_sha_is_available() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/505";
    let (product_id, work_item_id) = make_chore_in_review(&db, "C-no-sha-at-all", pr);
    seed_pre_merge_batch(&db, &work_item_id, "head-sha-1", ReviewProfile::Deep);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(product_id, work_item_id.clone(), pr);
    let probe = merged_probe(pr, None, None);

    maybe_trigger_post_merge_review(&db, &publisher, &candidate, &probe, |_| false).await;

    assert_eq!(
        db.review_batches_for_cycle_root(&work_item_id)
            .unwrap()
            .iter()
            .filter(|b| b.phase == ReviewBatchPhase::PostMerge)
            .count(),
        0
    );
}

/// The guard belongs to the whole PR family, including a revision that merges it.
#[tokio::test]
async fn review_minted_families_are_skipped_only_in_treatment() {
    for treated in [false, true] {
        for minted in [false, true] {
            for revision in [false, true] {
                let dir = tempdir().unwrap();
                let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
                let pr = "https://github.com/foo/bar/pull/506";
                let (product_id, root) = make_chore_in_review(&db, "follow-up", pr);
                if minted {
                    db.connect()
                        .unwrap()
                        .execute(
                            "UPDATE tasks SET created_via = 'pr_review:post_merge:verdict' WHERE id = ?1",
                            [&root],
                        )
                        .unwrap();
                }
                seed_pre_merge_batch(&db, &root, "head", ReviewProfile::Deep);
                let item = if revision {
                    let (_, child) = make_chore_in_review(&db, "revision", pr);
                    db.connect()
                        .unwrap()
                        .execute(
                            "UPDATE tasks SET kind = 'revision', parent_task_id = ?1 WHERE id = ?2",
                            [&root, &child],
                        )
                        .unwrap();
                    child
                } else {
                    root.clone()
                };
                let publisher = RecordingPublisher::default();
                maybe_trigger_post_merge_review(
                    &db,
                    &publisher,
                    &candidate(product_id, item, pr),
                    &merged_probe(pr, Some("merged"), Some("head")),
                    |cycle_root_id| {
                        assert_eq!(cycle_root_id, root);
                        treated
                    },
                )
                .await;
                assert_eq!(
                    db.review_batch_for_target(&root, ReviewBatchPhase::PostMerge, "merged")
                        .unwrap()
                        .is_some(),
                    !(treated && minted),
                    "treated={treated}, minted={minted}, revision={revision}",
                );
            }
        }
    }
}

#[tokio::test]
async fn legacy_review_provenance_is_resolved_through_the_verdict_batch() {
    for treated in [false, true] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/507";
        let (product, origin) = make_chore_in_review(&db, "origin", pr);
        let source = db
            .create_post_merge_review_batch(
                ReviewBatchCreateInput::builder()
                    .cycle_root_id(&origin)
                    .base_sha("base")
                    .classification(classification(ReviewProfile::Deep))
                    .phase(ReviewBatchPhase::PostMerge)
                    .pr_number(507)
                    .pr_url(pr)
                    .target_sha("origin-merge")
                    .merge_sha("origin-merge")
                    .build(),
                "https://github.com/foo/bar.git",
            )
            .unwrap();
        let crate::work::ReviewBatchDispatch::Created { executions, .. } = source else {
            panic!("source post-merge review must be created");
        };
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO worker_proposals
             (id, execution_id, kind, payload_json, idempotency_key, state, created_at)
             VALUES ('legacy-verdict', ?1, 'review_verdict', '{}', 'legacy-verdict', 'applied', '1')",
                [&executions[0].id],
            )
            .unwrap();
        let (_, followup) = make_chore_in_review(&db, "legacy follow-up", pr);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET created_via = 'pr_review:legacy-verdict' WHERE id = ?1",
                [&followup],
            )
            .unwrap();
        seed_pre_merge_batch(&db, &followup, "followup-head", ReviewProfile::Deep);
        maybe_trigger_post_merge_review(
            &db,
            &RecordingPublisher::default(),
            &candidate(product, followup.clone(), pr),
            &merged_probe(pr, Some("followup-merge"), Some("followup-head")),
            |cycle_root_id| {
                assert_eq!(cycle_root_id, followup);
                treated
            },
        )
        .await;
        assert_eq!(
            db.review_batch_for_target(&followup, ReviewBatchPhase::PostMerge, "followup-merge")
                .unwrap()
                .is_some(),
            !treated,
        );
    }
}
