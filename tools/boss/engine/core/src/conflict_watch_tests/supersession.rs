use super::helpers::*;

#[tokio::test]
async fn stale_head_sha_supersedes_pending_crz() {
    // Regression test for T1795 / T1764.
    //
    // Scenario: a crz is spawned for head SHA A.  The revision pushes a
    // commit (head moves to B) but doesn't resolve the conflict; then
    // the exec is abandoned by the orphan sweep (NudgeBreakerParked was
    // never the stop outcome), leaving the crz `pending` with
    // `revision_task_id` set and `head_sha_before = A`.
    //
    // On the next sweep the probe reports head SHA B.  conflict_watch
    // must detect the mismatch, abandon the stale crz, and spawn a
    // fresh resolution against B rather than returning false (no-op).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/40";
    let (product, chore) = make_in_review(&db, "C-stale-sha", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First detection: probe reports head SHA "head-A".  crz spawned, revision
    // created, parent stays in_review.
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-A"),
    )
    .await;
    assert!(first, "first detection must return true");

    let original_crz = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("crz must exist after first detection");
    assert_eq!(original_crz.head_sha_before.as_deref(), Some("head-A"));
    let original_id = original_crz.id.clone();

    // Simulate: revision pushed (head moves to "head-B"), exec abandoned.
    // We leave the crz as `pending` with `revision_task_id` set (the orphan
    // sweep does not call finalize_conflict_resolution_attempt).

    // Second sweep: probe reports head SHA "head-B" (head moved).
    // This must abandon the stale crz and spawn a fresh one.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-B"),
    )
    .await;
    assert!(second, "second probe with new head SHA must re-detect (return true)");

    // Same base SHA (both probes use the default "abc123"): the stale crz
    // is abandoned (base_sha_at_trigger nullified) and a fresh row is
    // inserted with the current head SHA.  Two rows in total:
    // one abandoned (the original) and one pending (the fresh one).
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all.len(), 2, "stale crz abandoned, fresh crz created");
    let abandoned = all
        .iter()
        .find(|r| r.id == original_id)
        .expect("original crz must still exist");
    assert_eq!(abandoned.status, "abandoned", "original crz must be abandoned");
    assert_eq!(abandoned.failure_reason.as_deref(), Some("superseded_stale_head"));
    let fresh = all.iter().find(|r| r.id != original_id).expect("fresh crz must exist");
    assert_eq!(fresh.status, "pending", "fresh crz must be pending");
    assert_eq!(
        fresh.head_sha_before.as_deref(),
        Some("head-B"),
        "fresh crz carries the current head SHA"
    );
    assert!(
        fresh.revision_task_id.is_some(),
        "fresh revision must be stamped on the new crz"
    );

    // Parent stays in_review (fresh revision spawned).
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

#[tokio::test]
async fn terminal_revision_supersedes_pending_crz_even_without_head_move() {
    // Regression test for the terminal-revision case.
    //
    // Scenario: a crz is spawned for head SHA A.  The revision completes
    // (task moves to in_review) but the execution was abandoned before
    // NudgeBreakerParked fired, so finalize_conflict_resolution_attempt
    // was never called and the crz stays `pending` with `revision_task_id`
    // set.  The head SHA did NOT change.
    //
    // On the next sweep, conflict_watch must detect that the linked
    // revision task is terminal, abandon the stale crz ("revision_terminal"),
    // and spawn a fresh resolution.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/41";
    let (product, chore) = make_in_review(&db, "C-stale-terminal", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First detection: crz spawned, revision created.
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(first, "first detection must return true");

    let original_crz = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("crz must exist after first detection");
    let original_id = original_crz.id.clone();
    let revision_id = original_crz.revision_task_id.clone().expect("revision must be spawned");

    // Simulate: revision task completed (e.g. moved to in_review) but the
    // crz was never finalised (exec abandoned outside NudgeBreakerParked).
    db.update_work_item(
        &revision_id,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Second sweep: same head SHA — but revision is terminal.
    // Must abandon stale crz and spawn fresh resolution.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(
        second,
        "second probe with terminal revision must re-detect (return true)"
    );

    // Same base SHA, terminal revision (head didn't move): the stale crz is
    // abandoned (base_sha_at_trigger nullified) and a fresh row inserted.
    // Two rows total: one abandoned (original) and one pending (fresh).
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all.len(), 2, "stale crz abandoned, fresh crz created");
    let abandoned = all
        .iter()
        .find(|r| r.id == original_id)
        .expect("original crz must still exist");
    assert_eq!(abandoned.status, "abandoned", "original crz must be abandoned");
    assert_eq!(abandoned.failure_reason.as_deref(), Some("superseded_stale_head"));
    let fresh = all.iter().find(|r| r.id != original_id).expect("fresh crz must exist");
    assert_eq!(fresh.status, "pending", "fresh crz must be pending");
    assert!(
        fresh.revision_task_id.is_some(),
        "fresh revision must be stamped on the new crz"
    );
    // revision_task_id must be different from the original stale revision.
    assert_ne!(
        fresh.revision_task_id.as_deref(),
        Some(revision_id.as_str()),
        "fresh revision must be a new task, not the old stale one",
    );

    // Parent stays in_review (fresh revision spawned).
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

#[tokio::test]
async fn stale_head_sha_and_base_advance_supersedes_pending_crz() {
    // Regression test for the real incident path (T1764: crz pending ~11h).
    //
    // Scenario: a crz is spawned for head SHA "head-A" / base SHA "base-1".
    // Over the next ~11 h main advances to "base-2" AND the PR author pushes
    // a new commit ("head-B") without resolving the conflict.  The exec is
    // abandoned by the orphan sweep, leaving the crz `pending` with
    // `revision_task_id` set.
    //
    // On the next sweep the probe reports head="head-B", base="base-2".
    // conflict_watch must:
    //   1. Detect that both head AND base moved.
    //   2. Abandon the stale crz (NOT leave it dangling as `pending`).
    //   3. Spawn a fresh crz+revision against the current (head-B, base-2).
    //   4. Exactly one active crz remaining; the stale row is terminal.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/43";
    let (product, chore) = make_in_review(&db, "C-base-advance", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First detection: probe reports head="head-A", base="base-1".
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head_and_base(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            "head-A",
            "base-1",
        ),
    )
    .await;
    assert!(first, "first detection must return true");

    let original_crz = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("crz must exist after first detection");
    assert_eq!(original_crz.head_sha_before.as_deref(), Some("head-A"));
    assert_eq!(original_crz.base_sha_at_trigger.as_deref(), Some("base-1"));
    let original_id = original_crz.id.clone();

    // Simulate: revision pushed (head moves to "head-B"), exec abandoned;
    // meanwhile main advanced to "base-2".  The crz stays `pending` with
    // `revision_task_id` set — finalize_conflict_resolution_attempt was
    // never called by the orphan sweep.

    // Second sweep: both head AND base moved.
    // Must abandon the stale crz and spawn a fresh one.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head_and_base(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            "head-B",
            "base-2",
        ),
    )
    .await;
    assert!(
        second,
        "second probe with new head+base SHA must re-detect (return true)"
    );

    // Stale row abandoned; fresh row inserted with the new (work_item_id, base-2) key.
    // Exactly two rows: one terminal (the old one) and one pending (the new one).
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(
        all.len(),
        2,
        "stale crz abandoned, fresh crz inserted with new base SHA"
    );

    let abandoned = all
        .iter()
        .find(|r| r.id == original_id)
        .expect("original crz must still exist");
    assert_eq!(
        abandoned.status, "abandoned",
        "original crz must be abandoned, not left pending"
    );
    assert_eq!(abandoned.failure_reason.as_deref(), Some("superseded_stale_head"));
    // base_sha_at_trigger is untouched on a base-changed abandon (the row is
    // purely terminal; its key slot is not reused).
    assert_eq!(abandoned.base_sha_at_trigger.as_deref(), Some("base-1"));

    let fresh = all.iter().find(|r| r.id != original_id).expect("fresh crz must exist");
    assert_eq!(fresh.status, "pending", "fresh crz must be pending");
    assert_eq!(
        fresh.base_sha_at_trigger.as_deref(),
        Some("base-2"),
        "fresh crz uses the new base SHA"
    );
    assert_eq!(
        fresh.head_sha_before.as_deref(),
        Some("head-B"),
        "fresh crz uses the new head SHA"
    );
    assert!(fresh.revision_task_id.is_some(), "fresh revision must be spawned");

    // Parent stays in_review (fresh revision in flight).
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

#[tokio::test]
async fn live_revision_same_head_sha_remains_no_op() {
    // Idempotency guard: if the crz's head SHA matches the current probe
    // and the revision task is still live, the pre-flight must NOT supersede.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/42";
    let (product, chore) = make_in_review(&db, "C-noop-live", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First detection: crz + revision spawned.
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(first);

    // Second probe: same head SHA ("head456"), revision still live (todo/active).
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!second, "same head + live revision must remain a no-op (false)");

    // Only the original crz exists — no supersede.
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(
        all.len(),
        1,
        "no second crz must be created when revision is still live"
    );
    assert_eq!(all[0].status, "pending");
}
