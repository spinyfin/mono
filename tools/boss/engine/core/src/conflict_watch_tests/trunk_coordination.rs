//! Trunk merge-queue coordination (design §"Coordination with conflict_watch
//! / ci_watch"): a conflict detected while a Trunk merge intent is still
//! live must supersede it (poller cancels the entry, no eviction
//! remediation); once the conflict resolves, the intent must be cleared to
//! auto-resubmit.

use super::helpers::*;

fn seed_trunk_intent(db: &WorkDb, chore: &str, pr: &str, pr_number: i64) {
    db.insert_trunk_merge_intent(
        crate::work::TrunkMergeIntentInsertInput::builder()
            .work_item_id(chore.to_owned())
            .pr_url(pr)
            .pr_number(pr_number)
            .repo("foo/bar")
            .target_branch("main")
            .build(),
    )
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn conflict_detection_supersedes_a_live_trunk_intent() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/9001";
    let (product, chore) = make_in_review(&db, "C-trunk-conflict", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    seed_trunk_intent(&db, &chore, pr, 9001);

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    let intent = db.get_active_trunk_merge_intent(&chore).unwrap().expect("still active");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(crate::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT),
        "a live intent must be marked superseded so the poller cancels it",
    );
    // Conflict resolution proceeds normally — no eviction remediation, this
    // is purely additive bookkeeping on the intent.
    assert!(db.active_conflict_resolution_for_work_item(&chore).unwrap().is_some());
}

#[tokio::test]
async fn conflict_detection_does_not_clobber_an_intent_already_evicted() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/9002";
    let (product, chore) = make_in_review(&db, "C-trunk-conflict-evicted", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    seed_trunk_intent(&db, &chore, pr, 9002);
    let intent = db.get_active_trunk_merge_intent(&chore).unwrap().unwrap();
    db.record_trunk_merge_intent_state(&intent.id, "failed").unwrap();

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    let intent = db.get_active_trunk_merge_intent(&chore).unwrap().expect("still active");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some("failed"),
        "an eviction already owns this intent — a conflict detection must not steal it",
    );
}

/// Regression guard: on a path where `on_conflict_detected` declines to
/// take ownership of the row (here, a genuinely higher-priority foreign
/// block — design §Q2: dependency > review_feedback > merge_conflict), the
/// Trunk intent's sentinel must NOT be set. Marking it here — as the
/// original placement ahead of every early return used to do — would
/// strand the intent in `boss:superseded_by_conflict` forever: no
/// `conflict_resolutions` row exists to eventually clear it via
/// `on_resolved`, so the poller would cancel the entry out of the queue
/// and never resubmit it.
#[tokio::test]
async fn conflict_detection_that_declines_ownership_does_not_strand_the_trunk_intent() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/9004";
    let (product, chore) = make_in_review(&db, "C-trunk-declined", pr);
    seed_trunk_intent(&db, &chore, pr, 9004);
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some("dependency".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let pub_ = Arc::new(RecordingPublisher::default());
    let took_over = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(
        !took_over,
        "conflict_watch must not steal a higher-priority foreign block"
    );

    let intent = db.get_active_trunk_merge_intent(&chore).unwrap().expect("still active");
    assert_eq!(
        intent.last_trunk_state, None,
        "a slot conflict_watch never took ownership of must not carry the superseded sentinel",
    );
}

/// Regression guard for the second, narrower "manually moved" path: the
/// parent was flipped away from `in_review` directly to a terminal status
/// (not `blocked: merge_conflict`) by a human, so `mark_chore_blocked_merge_conflict`'s
/// `status = 'in_review'` WHERE guard misses AND `rearm_blocked_merge_conflict_signal`
/// finds `is_blocked == false` — the "row not blocked:merge_conflict
/// (manually moved); skipping" branch. `on_conflict_detected` declines
/// ownership here too (no `conflict_resolutions` row is created), so the
/// Trunk intent's sentinel must NOT be set — the same failure mode covered
/// above for the foreign-bucket-owned path, but reached below the
/// `mark_chore_blocked_merge_conflict` match instead of above it.
#[tokio::test]
async fn conflict_detection_on_a_manually_moved_row_does_not_strand_the_trunk_intent() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/9005";
    let (product, chore) = make_in_review(&db, "C-trunk-manually-moved", pr);
    seed_trunk_intent(&db, &chore, pr, 9005);
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("done".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let pub_ = Arc::new(RecordingPublisher::default());
    let took_over = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(
        !took_over,
        "conflict_watch must not flip a row a human already moved away from in_review"
    );
    assert!(
        db.active_conflict_resolution_for_work_item(&chore).unwrap().is_none(),
        "no conflict-resolution attempt should have been created"
    );

    let intent = db.get_active_trunk_merge_intent(&chore).unwrap().expect("still active");
    assert_eq!(
        intent.last_trunk_state, None,
        "a slot conflict_watch never took ownership of must not carry the superseded sentinel",
    );
}

#[tokio::test]
async fn conflict_resolution_clears_a_superseded_intent_for_resubmit() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/9003";
    let (product, chore) = make_in_review(&db, "C-trunk-conflict-resolve", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    seed_trunk_intent(&db, &chore, pr, 9003);

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let intent = db.get_active_trunk_merge_intent(&chore).unwrap().unwrap();
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(crate::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT),
    );

    let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(resolved, "on_resolved must retire the conflict-resolution attempt");

    let intent = db.get_active_trunk_merge_intent(&chore).unwrap().expect("still active");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(crate::trunk_merge::TRUNK_INTENT_AWAITING_RESUBMIT),
        "the fix landed — the intent must be marked ready for the poller to auto-resubmit",
    );
}
