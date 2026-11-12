//! A stale `pending` conflict attempt must not keep a mergeable PR blocked.
//!
//! Sequence under test: an old attempt is left `pending`, newer attempts
//! come and go, re-detection collides on the UNIQUE key, and the PR finally
//! becomes mergeable. The task must leave `blocked`; a PR that is still
//! conflicting must keep blocking.
use super::helpers::*;

const PR: &str = "https://github.com/foo/bar/pull/810";

fn insert_attempt(db: &WorkDb, product: &str, chore: &str, base: &str, head: &str) -> crate::work::ConflictResolution {
    db.insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
        product_id: product.to_owned(),
        work_item_id: chore.to_owned(),
        pr_url: PR.into(),
        pr_number: 810,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some(base.into()),
        head_sha_before: Some(head.into()),
    })
    .unwrap()
    .expect("fresh key must insert")
}

/// Put the chore into `blocked: merge_conflict` with an armed signal row.
fn block_on_merge_conflict(db: &WorkDb, chore: &str) {
    db.mark_chore_blocked_merge_conflict(chore, PR).unwrap().unwrap();
    db.record_merge_conflict_in_flight(chore, "seed").unwrap();
}

fn blocked_attempt_id(db: &WorkDb, chore: &str) -> Option<String> {
    task(db, chore).blocked_attempt_id
}

#[tokio::test]
async fn newer_attempt_retires_older_pending_attempts() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (product, chore) = make_in_review(&db, "C-supersede-pending", PR);
    block_on_merge_conflict(&db, &chore);

    let stale = insert_attempt(&db, &product, &chore, "base-1", "head-1");
    let newer = insert_attempt(&db, &product, &chore, "base-1", "head-2");

    let stale_row = db.get_conflict_resolution(&stale.id).unwrap().unwrap();
    assert_eq!(stale_row.status, "abandoned");
    assert_eq!(stale_row.failure_reason.as_deref(), Some("superseded_by_newer_attempt"));
    assert!(stale_row.base_sha_at_trigger.is_none(), "key slot must be freed");
    eprintln!(
        "DBG status={:?} attempts={:?}",
        chore_status(&db, &chore),
        db.list_conflict_resolutions(None, &[], Some(&chore), None).map(|v| v
            .into_iter()
            .map(|a| (a.id, a.status, a.revision_task_id))
            .collect::<Vec<_>>())
    );
    assert_eq!(
        db.active_conflict_resolution_for_work_item(&chore)
            .unwrap()
            .map(|a| a.id),
        Some(newer.id.clone()),
        "the newer attempt is the only live one",
    );
    assert_eq!(blocked_attempt_id(&db, &chore), Some(newer.id));
}

#[tokio::test]
async fn running_attempt_is_not_retired_by_a_newer_attempt() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (product, chore) = make_in_review(&db, "C-keep-running", PR);
    block_on_merge_conflict(&db, &chore);

    let running = install_running_attempt(&db, &product, &chore, PR, "lease-1");
    insert_attempt(&db, &product, &chore, "base-2", "head-2");

    let row = db.get_conflict_resolution(&running).unwrap().unwrap();
    assert_eq!(row.status, "running", "a running attempt holds a worker lease");
}

#[tokio::test]
async fn collision_does_not_adopt_a_stale_attempt_with_a_different_key() {
    // A succeeded attempt holds the probe's (base, head) key, while a stale
    // `pending` attempt (legacy row, planted past the insert-time retirement)
    // holds a different head. Re-detection collides on the succeeded row and
    // must not adopt the stale one.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let (product, chore) = make_in_review(&db, "C-no-adopt", PR);
    block_on_merge_conflict(&db, &chore);

    let stale = insert_attempt(&db, &product, &chore, "abc123", "head-old");
    let holder = insert_attempt(&db, &product, &chore, "abc123", "head-A");
    db.mark_conflict_resolution_succeeded(&holder.id, None)
        .unwrap()
        .unwrap();
    // Re-create the legacy state: the stale row is pending again.
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE conflict_resolutions
                SET status = 'pending', failure_reason = NULL, finished_at = NULL,
                    base_sha_at_trigger = 'abc123'
              WHERE id = ?1",
            rusqlite::params![stale.id],
        )
        .unwrap();
    rewind_attempt_created_at(&db_path, &stale.id, 600);

    let pub_ = Arc::new(RecordingPublisher::default());
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, PR),
        &probe_with_head(PR, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-A"),
    )
    .await;

    let stale_row = db.get_conflict_resolution(&stale.id).unwrap().unwrap();
    assert!(
        stale_row.revision_task_id.is_none(),
        "no fix may be spawned against the stale attempt",
    );
    assert_ne!(
        blocked_attempt_id(&db, &chore).as_deref(),
        Some(stale.id.as_str()),
        "the stale attempt must not become the task's blocked attempt",
    );
}

#[tokio::test]
async fn adopting_a_live_attempt_records_it_as_the_blocked_attempt() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (product, chore) = make_in_review(&db, "C-record-adopted", PR);
    block_on_merge_conflict(&db, &chore);

    let attempt = insert_attempt(&db, &product, &chore, "abc123", "head-A");
    // A legacy / lost stamp.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET blocked_attempt_id = NULL WHERE id = ?1",
            rusqlite::params![chore],
        )
        .unwrap();

    let pub_ = Arc::new(RecordingPublisher::default());
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, PR),
        &probe_with_head(PR, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-A"),
    )
    .await;

    // The key holder was the live attempt, so it is the one adopted: the fix
    // vehicle is spawned against it.
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert!(
        row.revision_task_id.is_some(),
        "the fix must be spawned against the adopted key holder",
    );
}

#[tokio::test]
async fn recording_an_adopted_attempt_stamps_blocked_attempt_id() {
    // The adoption path's stamp, asserted directly: the parent is still
    // blocked, so the outcome is observable (a spawned fix would otherwise
    // return the parent to review and clear it).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (product, chore) = make_in_review(&db, "C-record-direct", PR);
    block_on_merge_conflict(&db, &chore);
    let attempt = insert_attempt(&db, &product, &chore, "abc123", "head-A");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET blocked_attempt_id = NULL WHERE id = ?1",
            rusqlite::params![chore],
        )
        .unwrap();

    assert!(db.record_blocked_conflict_attempt(&chore, &attempt.id).unwrap());
    assert_eq!(blocked_attempt_id(&db, &chore), Some(attempt.id));
}

#[tokio::test]
async fn mergeable_pr_unblocks_task_despite_stale_pending_attempt_and_null_blocked_attempt_id() {
    // The reported sequence: a stale pending attempt, newer attempts that
    // succeeded, and a task whose `blocked_attempt_id` is NULL. The PR becomes
    // mergeable; the task must leave `blocked`.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let (product, chore) = make_in_review(&db, "C-stale-mergeable", PR);
    block_on_merge_conflict(&db, &chore);

    let stale = insert_attempt(&db, &product, &chore, "abc123", "head-1");
    for head in ["head-2", "head-3"] {
        let a = insert_attempt(&db, &product, &chore, "abc123", head);
        db.mark_conflict_resolution_succeeded(&a.id, None).unwrap().unwrap();
    }
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE conflict_resolutions
            SET status = 'pending', failure_reason = NULL, finished_at = NULL
          WHERE id = ?1",
        rusqlite::params![stale.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET blocked_attempt_id = NULL WHERE id = ?1",
        rusqlite::params![chore],
    )
    .unwrap();
    rewind_attempt_created_at(&db_path, &stale.id, 600);

    assert_eq!(chore_status(&db, &chore).0, TaskStatus::Blocked);
    assert!(blocked_attempt_id(&db, &chore).is_none());

    let pub_ = Arc::new(RecordingPublisher::default());
    let resolved = on_resolved(
        &db,
        pub_.as_ref(),
        None,
        &candidate(&product, &chore, PR),
        &[],
        "MERGEABLE",
        "CLEAN",
    )
    .await;

    assert!(resolved, "the retire path must report a transition");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview, "a mergeable PR must unblock the task");
    assert!(reason.is_none());
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .all(|s| s.reason != "merge_conflict"),
        "the merge_conflict signal must be cleared",
    );
    assert_eq!(
        db.get_conflict_resolution(&stale.id).unwrap().unwrap().status,
        "succeeded",
        "the stale attempt is retired",
    );
}

#[tokio::test]
async fn genuinely_conflicting_pr_still_blocks() {
    // Churn-exhausted conflict: no fix vehicle, so the parent rests in
    // `blocked: merge_conflict`. Re-detecting the still-conflicting PR must
    // leave it blocked.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (product, chore) = make_in_review(&db, "C-still-conflicting", PR);
    for head in ["s1", "s2", "s3"] {
        insert_attempt(&db, &product, &chore, "abc123", head);
    }
    let pub_ = Arc::new(RecordingPublisher::default());
    for _ in 0..2 {
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, PR),
            &probe_with_head(PR, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-now"),
        )
        .await;
    }

    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::Blocked,
        "a conflicting PR with no fix vehicle stays blocked"
    );
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
}

/// Seed the legacy state: a `pending` attempt left behind by an older build
/// (insert-time retirement would have abandoned it), holding `base` with
/// `head`, alongside a `succeeded` attempt holding the key re-detection will
/// probe with.
fn seed_legacy_stale_and_holder(
    db: &WorkDb,
    db_path: &std::path::Path,
    product: &str,
    chore: &str,
) -> (crate::work::ConflictResolution, crate::work::ConflictResolution) {
    let stale = insert_attempt(db, product, chore, "abc123", "head-old");
    for head in ["head-2", "head-3"] {
        let a = insert_attempt(db, product, chore, "abc123", head);
        db.mark_conflict_resolution_succeeded(&a.id, None).unwrap().unwrap();
    }
    let holder = insert_attempt(db, product, chore, "abc123", "head-A");
    db.mark_conflict_resolution_succeeded(&holder.id, None).unwrap();
    rusqlite::Connection::open(db_path)
        .unwrap()
        .execute(
            "UPDATE conflict_resolutions
                SET status = 'pending', failure_reason = NULL, finished_at = NULL,
                    base_sha_at_trigger = 'abc123'
              WHERE id = ?1",
            rusqlite::params![stale.id],
        )
        .unwrap();
    rewind_attempt_created_at(db_path, &stale.id, 600);
    (stale, holder)
}

#[tokio::test]
async fn full_sequence_stale_attempt_collision_then_mergeable_unblocks_task() {
    // The reported bug end to end: a stale pending attempt, newer succeeded
    // attempts, a UNIQUE collision on re-detection, then the PR turns
    // mergeable. The collision must not adopt the stale row, and the task
    // must leave `blocked` once GitHub reports the PR mergeable.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let (product, chore) = make_in_review(&db, "C-full-sequence", PR);
    block_on_merge_conflict(&db, &chore);
    let (stale, holder) = seed_legacy_stale_and_holder(&db, &db_path, &product, &chore);
    if chore_status(&db, &chore).0 != TaskStatus::Blocked {
        block_on_merge_conflict(&db, &chore);
    }

    let pub_ = Arc::new(RecordingPublisher::default());
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, PR),
        &probe_with_head(PR, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-A"),
    )
    .await;

    // Collision outcome: the stale row is not adopted as the task's owner and
    // no fix is spawned against it.
    let stale_row = db.get_conflict_resolution(&stale.id).unwrap().unwrap();
    assert!(stale_row.revision_task_id.is_none());
    assert_ne!(blocked_attempt_id(&db, &chore).as_deref(), Some(stale.id.as_str()));
    assert_ne!(blocked_attempt_id(&db, &chore).as_deref(), Some(holder.id.as_str()));

    // Re-assert the "blocked on a conflict" state the poller would observe,
    // whatever the detection step did with the parent, then resolve.
    if chore_status(&db, &chore).0 != TaskStatus::Blocked {
        block_on_merge_conflict(&db, &chore);
    }
    let resolved = on_resolved(
        &db,
        pub_.as_ref(),
        None,
        &candidate(&product, &chore, PR),
        &[],
        "MERGEABLE",
        "CLEAN",
    )
    .await;

    assert!(resolved, "a mergeable PR must report a transition");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview, "a mergeable PR must unblock the task");
    assert!(reason.is_none());
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .all(|s| s.reason != "merge_conflict"),
        "the merge_conflict signal must be cleared",
    );
}

#[tokio::test]
async fn revision_backed_pending_attempt_is_not_retired_by_a_newer_attempt() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (product, chore) = make_in_review(&db, "C-keep-revision-backed", PR);
    block_on_merge_conflict(&db, &chore);

    let backed = insert_attempt(&db, &product, &chore, "base-1", "head-1");
    db.set_conflict_resolution_revision_task_id(&backed.id, &chore).unwrap();
    insert_attempt(&db, &product, &chore, "base-1", "head-2");

    let row = db.get_conflict_resolution(&backed.id).unwrap().unwrap();
    assert_eq!(row.status, "pending", "an in-flight revision must not be orphaned");
    assert!(row.base_sha_at_trigger.is_some());
}
