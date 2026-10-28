//! A CI-fix revision whose spawning attempt ended before CI went green must
//! still be closed once CI is green on the PR head — see
//! [`retire_moot_ci_fix_revisions`].

use super::helpers::*;

const PARKED: &str = "spawning_attempt_retired_unsuccessfully";

/// Detect a CI failure on `head`, returning `(attempt_id, revision_id)` of
/// the engine-created CI-fix revision.
async fn spawn_ci_fix(db: &WorkDb, product: &str, chore: &str, pr: &str, head: &str) -> (String, String) {
    let pub_ = Arc::new(RecordingPublisher::default());
    assert!(
        on_ci_failure_detected(
            db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(product, chore, pr),
            &probe(pr, head),
            &one_failure(),
        )
        .await
    );
    let attempt = db.active_ci_remediation_for_work_item(chore).unwrap().unwrap();
    (attempt.id, attempt.revision_task_id.unwrap())
}

/// What the revision reconciler does once the spawning attempt has ended
/// unsuccessfully: park the revision as `blocked`.
fn park_blocked(db: &WorkDb, revision: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = ?2 WHERE id = ?1",
            rusqlite::params![revision, PARKED],
        )
        .unwrap();
}

fn assert_closed(db: &WorkDb, revision: &str) {
    assert_eq!(task(db, revision).status, TaskStatus::Archived);
}

#[tokio::test]
async fn retriggered_attempt_then_green_closes_blocked_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/700";
    let (product, chore) = make_in_review(&db, "C-stale-retriggered", pr);
    let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;

    // The CI worker judged the failure flaky and pushed nothing.
    db.mark_ci_remediation_retriggered(&attempt).unwrap().unwrap();
    park_blocked(&db, &revision);
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());

    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        1
    );
    assert_closed(&db, &revision);
    assert!(
        !db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "the closed revision must stop counting as in flight",
    );
}

#[tokio::test]
async fn failed_attempt_then_green_closes_blocked_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/701";
    let (product, chore) = make_in_review(&db, "C-stale-failed", pr);
    let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;

    db.mark_ci_remediation_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &revision);

    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        1
    );
    assert_closed(&db, &revision);
}

#[tokio::test]
async fn head_moved_then_green_closes_revision_of_the_old_head() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/702";
    let (product, chore) = make_in_review(&db, "C-stale-head-moved", pr);
    let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
    db.mark_ci_remediation_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &revision);

    // The PR head moves to `head-2`, which is green; no attempt is active for
    // either head. The merge poller runs this sweep on every green probe
    // (see `merge_poller::tests::stale_revision_sweep_tests`).
    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        1
    );
    assert_closed(&db, &revision);
}

#[tokio::test]
async fn sweep_leaves_revision_alone_while_ci_is_not_green() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/703";
    let (product, chore) = make_in_review(&db, "C-stale-inflight", pr);
    let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
    db.mark_ci_remediation_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &revision);

    let pub_ = RecordingPublisher::default();
    let mut outcome = crate::merge_poller::SweepOutcome::default();
    crate::merge_poller::dispatch_ci_axis(
        &db,
        &pub_,
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &OpenPrCiStatus::InFlight,
        &mut outcome,
    )
    .await;

    assert_eq!(task(&db, &revision).status, TaskStatus::Blocked);
}

#[tokio::test]
async fn revision_of_a_still_active_attempt_is_not_closed() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/704";
    let (product, chore) = make_in_review(&db, "C-stale-active", pr);
    let (_attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;

    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        0
    );
    assert_eq!(task(&db, &revision).status, TaskStatus::Todo);
}

#[tokio::test]
async fn human_filed_revision_is_untouched() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/705";
    let (product, chore) = make_in_review(&db, "C-stale-human", pr);
    let (attempt, ci_revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
    db.mark_ci_remediation_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &ci_revision);

    // A human-filed revision, and one filed for a review finding, on the
    // same parent. Even if the human one is (wrongly) linked from an ended
    // attempt row, the `ci-fix:` provenance check must keep it out.
    let human = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.clone())
                .description("Please rename the helper")
                .force_duplicate(true)
                .build(),
            &fix_checker(),
        )
        .unwrap();
    let review = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.clone())
                .description("Address review findings")
                .created_via(format!("{}exec_review", boss_protocol::CREATED_VIA_PR_REVIEW_PREFIX))
                .force_duplicate(true)
                .build(),
            &fix_checker(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE ci_remediations SET revision_task_id = ?2 WHERE id = ?1",
            rusqlite::params![attempt, human.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked' WHERE id IN (?1, ?2)",
            rusqlite::params![human.id, review.id],
        )
        .unwrap();

    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        0
    );
    assert_eq!(task(&db, &human.id).status, TaskStatus::Blocked);
    assert_eq!(task(&db, &review.id).status, TaskStatus::Blocked);
}

/// A queue-side attempt that has ended (`abandoned`) strands its CI-fix
/// revision; a green PR-head probe closes it so the parent is not held in
/// Review ▸ Blocked.
#[tokio::test]
async fn queue_side_abandoned_attempt_revision_is_closed_when_head_is_green() {
    for (label, failure_kind) in [
        ("C-stale-queue-side-trunk", "trunk_queue_eviction"),
        ("C-stale-queue-side-rebounce", "merge_queue_rebounce"),
    ] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/706";
        let (product, chore) = make_in_review(&db, label, pr);
        let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
        db.mark_ci_remediation_abandoned(&attempt, "queue_side_attempt_ended")
            .unwrap()
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE ci_remediations SET failure_kind = ?2 WHERE id = ?1",
                rusqlite::params![attempt, failure_kind],
            )
            .unwrap();
        park_blocked(&db, &revision);

        assert_eq!(
            retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
            1,
            "{failure_kind} revision must close once the attempt has ended and the PR head is green"
        );
        assert_closed(&db, &revision);
    }
}
