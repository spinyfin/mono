//! A merge-conflict revision whose spawning attempt ended before the PR
//! became mergeable again must still be closed — see
//! [`retire_moot_conflict_revisions`].

use super::helpers::*;

/// Detect a conflict, returning `(attempt_id, revision_id)` of the
/// engine-created merge-conflict revision.
async fn spawn_conflict_revision(db: &WorkDb, product: &str, chore: &str, pr: &str) -> (String, String) {
    let pub_ = Arc::new(RecordingPublisher::default());
    on_conflict_detected(
        db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(product, chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let attempt = db.active_conflict_resolution_for_work_item(chore).unwrap().unwrap();
    (attempt.id, attempt.revision_task_id.unwrap())
}

fn park_blocked(db: &WorkDb, revision: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'spawning_attempt_retired_unsuccessfully' \
             WHERE id = ?1",
            [revision],
        )
        .unwrap();
}

#[tokio::test]
async fn failed_attempt_then_mergeable_closes_blocked_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/710";
    let (product, chore) = make_in_review(&db, "C-stale-conflict", pr);
    let (attempt, revision) = spawn_conflict_revision(&db, &product, &chore, pr).await;
    db.mark_conflict_resolution_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &revision);
    assert!(db.active_conflict_resolution_for_work_item(&chore).unwrap().is_none());

    assert_eq!(
        retire_moot_conflict_revisions(&db, &candidate(&product, &chore, pr), &[]),
        1
    );

    let closed = task(&db, &revision);
    assert_eq!(closed.status, TaskStatus::Archived);
    assert!(closed.deleted_at.is_some(), "the closed revision must be tombstoned");
}

#[tokio::test]
async fn revision_of_a_still_active_attempt_is_not_closed() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/711";
    let (product, chore) = make_in_review(&db, "C-stale-conflict-active", pr);
    let (_attempt, revision) = spawn_conflict_revision(&db, &product, &chore, pr).await;

    assert_eq!(
        retire_moot_conflict_revisions(&db, &candidate(&product, &chore, pr), &[]),
        0
    );
    assert_eq!(task(&db, &revision).status, TaskStatus::Todo);
}

#[tokio::test]
async fn human_filed_revision_is_untouched() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/712";
    let (product, chore) = make_in_review(&db, "C-stale-conflict-human", pr);
    let (attempt, conflict_revision) = spawn_conflict_revision(&db, &product, &chore, pr).await;
    db.mark_conflict_resolution_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &conflict_revision);

    let human = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.clone())
                .description("Please rename the helper")
                .force_duplicate(true)
                .build(),
            &open_checker(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'blocked' WHERE id = ?1", [&human.id])
        .unwrap();

    assert_eq!(
        retire_moot_conflict_revisions(&db, &candidate(&product, &chore, pr), &[]),
        1
    );
    assert_eq!(task(&db, &conflict_revision).status, TaskStatus::Archived);
    // The human revision was queued behind the conflict revision (the chain
    // tail), so closing the latter releases it to `todo` via the normal
    // dependency cascade — but it is never archived or tombstoned.
    let human = task(&db, &human.id);
    assert_eq!(human.status, TaskStatus::Todo);
    assert!(human.deleted_at.is_none());
}
