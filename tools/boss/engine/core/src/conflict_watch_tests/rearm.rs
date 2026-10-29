use super::helpers::*;

/// Regression test for PR #1874: the stale-base re-arm path
/// must not permanently no-op when a succeeded crz's resolution has
/// gone stale (PR still CONFLICTING) but the PR's `baseRefOid` — fixed
/// at PR-open time — hasn't moved. GitHub never advances `baseRefOid`
/// as `main` moves under an in-review PR, so keying the re-arm insert
/// on `base_sha_at_trigger` alone collided with the succeeded row's
/// UNIQUE slot forever. `head_sha_before` DOES vary — a real resolution
/// attempt pushes a fix commit — so folding it into the key lets this
/// re-arm create a fresh row and spawn a second revision.
#[tokio::test]
async fn rearm_dispatches_fresh_attempt_when_succeeded_crz_has_stale_frozen_base() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/16";
    let (product, chore) = make_in_review(&db, "C-rearm-stale-base", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First conflict: head is "head-before-resolution". Revision spawns,
    // parent stays in_review.
    assert!(
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe_with_head(
                pr,
                PrLifecycleState::Open(OpenPrStatus::conflict_only()),
                "head-before-resolution"
            ),
        )
        .await
    );
    let first_attempt = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("first attempt must exist");

    // The revision resolves the conflict — the worker pushes a fix commit,
    // so the head moves. The PR briefly reports clean, retiring the attempt
    // to `succeeded` (parent was in_review the whole time, so it stays there).
    assert!(on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await);
    let succeeded = db.get_conflict_resolution(&first_attempt.id).unwrap().unwrap();
    assert_eq!(succeeded.status, "succeeded");

    // Simulate the parent having been left `blocked: merge_conflict` by an
    // earlier sweep (the direct-flip UNIQUE-collision path in
    // `cycle_conflict_resolve_conflict` demonstrates how this happens).
    // This routes the next detection through the re-arm branch rather
    // than the primary WHERE-guard flip.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some("merge_conflict".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // `main` moves further before any new push happens: the PR is
    // CONFLICTING again, GitHub's baseRefOid is UNCHANGED (still
    // "abc123" — it never tracks `main`), but the head now reflects
    // the fix commit the resolved attempt pushed.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            "head-after-resolution",
        ),
    )
    .await;
    assert!(second, "re-arm must report a state change (revision spawned)");

    // A second, distinct attempt row must exist with a fresh revision.
    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(
        attempts.len(),
        2,
        "re-arm must create a second attempt row, got {attempts:?}"
    );
    let second_attempt = attempts
        .iter()
        .find(|a| a.id != first_attempt.id)
        .expect("a second, distinct attempt row must exist");
    assert_eq!(second_attempt.status, "pending");
    assert!(
        second_attempt.revision_task_id.is_some(),
        "re-arm must spawn a fresh revision, got {second_attempt:?}",
    );
    assert_eq!(second_attempt.base_sha_at_trigger.as_deref(), Some("abc123"));
    assert_eq!(second_attempt.head_sha_before.as_deref(), Some("head-after-resolution"));

    // Parent must be unblocked back to in_review — the fresh revision is
    // now the fix vehicle in flight.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

/// Regression test for the mono#1398/#1764 wedge: a `succeeded` crz whose
/// head NEVER advanced (a false success — e.g. the signal-cleared gate
/// retired on `mergeable=UNKNOWN`, or `main` re-conflicted the PR at the
/// same head) permanently occupies the `UNIQUE (work_item_id,
/// base_sha_at_trigger, head_sha_before)` slot. The stale-base re-arm then
/// fell through to a colliding INSERT every ~6s forever ("succeeded crz but
/// PR still CONFLICTING" → UNIQUE collision → no fresh attempt). The fix
/// invalidates the stale success (freeing the slot) so exactly one
/// churn-guarded fresh attempt lands and the loop breaks.
#[tokio::test]
async fn rearm_invalidates_stale_success_when_head_unchanged_and_breaks_wedge() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1398";
    let (product, chore) = make_in_review(&db, "C-wedge", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First conflict at head "head-stuck". Revision spawns, parent in_review.
    assert!(
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-stuck"),
        )
        .await
    );
    let first_attempt = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("first attempt must exist");
    assert_eq!(first_attempt.head_sha_before.as_deref(), Some("head-stuck"));

    // Simulate a FALSE success: the attempt is marked succeeded WITHOUT the
    // head advancing (head_sha_after stays NULL) — exactly the premature
    // retire the signal-cleared gate used to record on mergeable=UNKNOWN.
    db.mark_conflict_resolution_succeeded(&first_attempt.id, None).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE work_item_id = ?1",
            rusqlite::params![first_attempt.revision_task_id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![first_attempt.revision_task_id],
        )
        .unwrap();
    db.clear_merge_conflict_signal_only(&chore).unwrap();
    let succeeded = db.get_conflict_resolution(&first_attempt.id).unwrap().unwrap();
    assert_eq!(succeeded.status, "succeeded");
    assert!(succeeded.head_sha_after.is_none(), "head must not have advanced");

    // Parent comes to rest `blocked: merge_conflict` (routes the next
    // detection through the re-arm branch, not the primary flip).
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some("merge_conflict".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Second conflict at the SAME head "head-stuck" and SAME base "abc123":
    // the succeeded row's UNIQUE key still matches. Before the fix this
    // collided and returned false (the permanent ~6s no-op loop). After the
    // fix it invalidates the stale success and lands one fresh attempt.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-stuck"),
    )
    .await;
    assert!(second, "re-arm must break the wedge and report a state change");

    // The stale success was invalidated: flipped to `failed` with the wedge
    // reason and its UNIQUE slot freed (base_sha_at_trigger NULLed).
    let invalidated = db.get_conflict_resolution(&first_attempt.id).unwrap().unwrap();
    assert_eq!(invalidated.status, "failed");
    assert_eq!(
        invalidated.failure_reason.as_deref(),
        Some("stale_success_still_conflicting"),
    );
    assert!(
        invalidated.base_sha_at_trigger.is_none(),
        "invalidation must free the UNIQUE slot",
    );

    // A second, distinct attempt row exists — a genuine fresh resolution.
    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 2, "one fresh attempt must be created, got {attempts:?}");
    let fresh = attempts
        .iter()
        .find(|a| a.id != first_attempt.id)
        .expect("a second, distinct attempt row must exist");
    assert_eq!(fresh.status, "pending");
    assert!(fresh.revision_task_id.is_some(), "fresh attempt must spawn a revision");
    assert_eq!(fresh.base_sha_at_trigger.as_deref(), Some("abc123"));
    assert_eq!(fresh.head_sha_before.as_deref(), Some("head-stuck"));

    // Parent is back in_review — the fresh revision is the fix vehicle.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // The wedge is broken: a THIRD probe at the same state is now the
    // idempotent no-op (active revision in flight), NOT another invalidate +
    // insert — no third attempt row is created.
    let third = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-stuck"),
    )
    .await;
    assert!(
        !third,
        "with a fresh attempt in flight, re-detection is an idempotent no-op"
    );
    assert_eq!(
        db.list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap()
            .len(),
        2,
        "no further attempts once one is in flight",
    );
}

/// Reconciliation path: parent is in `blocked: merge_conflict`
/// but an active revision is already in flight. The next CONFLICTING probe should
/// flip the parent BACK to `in_review` without spawning a second revision.
#[tokio::test]
async fn rearm_reconciles_blocked_parent_when_revision_is_in_flight() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/20r";
    let (product, chore) = make_in_review(&db, "C-rearm-reconcile", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Simulate the pre-model-change state: parent is blocked AND a revision
    // exists (a live revision behind a blocked parent). Manually flip to blocked, insert a crz, create a
    // revision, stamp the crz's revision_task_id.
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 20,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("abc123".into()),
            head_sha_before: Some("head456".into()),
        })
        .unwrap()
        .expect("fresh insert");
    // Stamp a genuinely live (status='active') task as the revision to
    // simulate that revision being active. It must be a real, live task — not a
    // dangling id — otherwise `supersede_if_stale`'s dead-revision check
    // (added for the engine-restart fix) correctly treats it as dead and
    // supersedes instead of reconciling, which is a different scenario
    // covered by `rearm_supersedes_blocked_parent_when_revision_is_dead`.
    let revision_task_id = create_active_chore(&db, &product, "fake revision in flight");
    db.set_conflict_resolution_revision_task_id(&attempt.id, &revision_task_id)
        .unwrap();
    let (s, _) = chore_status(&db, &chore);
    assert_eq!(s, TaskStatus::Blocked, "sanity: parent must be blocked before probe");

    // Now fire on_conflict_detected for the same PR (still CONFLICTING).
    // The re-arm path should find the active revision and reconcile.
    let reconciled = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    assert!(reconciled, "reconciliation must return true (state changed)");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent must be back in_review after reconcile"
    );
    assert!(reason.is_none());

    // Event emitted is "conflict_revision_in_flight".
    let events = pub_.events.lock().await.clone();
    assert!(
        events.iter().any(|(_, _, r)| r == "conflict_revision_in_flight"),
        "conflict_revision_in_flight event must fire during reconcile, got {events:?}",
    );
    // No second revision was spawned (task_fake_revision is still the only one).
    let all_crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all_crz.len(), 1, "reconcile must not insert a new crz");
}

/// Regression for the engine-restart fix: when the parent is `blocked:
/// merge_conflict` and the active crz's linked revision is DEAD (e.g. its
/// execution died across an engine restart while the task itself never left
/// a live status, or — as modeled here — the revision task simply no longer
/// exists), the re-arm path must supersede the stale attempt and spawn a
/// fresh revision on THIS pass, not blindly reconcile the parent back to
/// `in_review` with a dead fix vehicle silently left in place.
///
/// Before the fix, this branch never checked staleness at all — it always
/// took the same path as `rearm_reconciles_blocked_parent_when_revision_is_in_flight`
/// regardless of whether the revision was alive, so a dead revision behind a
/// blocked parent would only get superseded once some later pass happened to
/// observe the parent back in `in_review` (the only branch that checked).
#[tokio::test]
async fn rearm_supersedes_blocked_parent_when_revision_is_dead() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/20d";
    let (product, chore) = make_in_review(&db, "C-rearm-dead", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 20,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("abc123".into()),
            head_sha_before: Some("head456".into()),
        })
        .unwrap()
        .expect("fresh insert");
    let original_id = attempt.id.clone();
    // Dangling revision_task_id: no task exists at this id, so
    // `is_conflict_revision_running_or_queued` reports it dead — standing in
    // for a revision whose execution died across an engine restart.
    db.set_conflict_resolution_revision_task_id(&attempt.id, "task_dead_revision")
        .unwrap();
    let (s, _) = chore_status(&db, &chore);
    assert_eq!(s, TaskStatus::Blocked, "sanity: parent must be blocked before probe");

    // First post-restart probe (same head SHA, still CONFLICTING) must
    // supersede on THIS pass, not just reconcile.
    let result = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(result, "superseding a dead attempt must return true (state changed)");

    // The dead attempt is abandoned; a fresh one is inserted with a new
    // (live) revision, not the dangling one.
    let all_crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(
        all_crz.len(),
        2,
        "dead attempt abandoned and a fresh attempt spawned on the first pass"
    );
    let original = all_crz
        .iter()
        .find(|r| r.id == original_id)
        .expect("original crz must still exist");
    assert_eq!(
        original.status, "abandoned",
        "dead attempt must be abandoned, not left pending"
    );
    let fresh = all_crz
        .iter()
        .find(|r| r.id != original_id)
        .expect("fresh crz must exist");
    assert_eq!(fresh.status, "pending");
    assert!(
        fresh.revision_task_id.is_some() && fresh.revision_task_id.as_deref() != Some("task_dead_revision"),
        "fresh crz must carry a new, live revision, not the dead one"
    );

    // Parent ends up in_review (fresh revision spawned and unblocked it).
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent must be in_review with the fresh revision in flight"
    );
    assert!(reason.is_none());
}

/// Live-repro regression for "conflict_watch emits zero scan activity after
/// engine restart". The revision task the crz points at still EXISTS in a
/// live status (`active`) — its status column survived the restart — but its
/// backing execution died and was reaped to `orphaned`. Before the fix,
/// `is_conflict_revision_running_or_queued` looked only at the paper-live task
/// status, so `supersede_if_stale` treated this dead fix vehicle as in flight
/// and every post-boot probe was an idempotent no-op: the blocked row could
/// never recover. The watcher must now recognise the fix vehicle is dead,
/// supersede the stale attempt, and spawn a fresh revision on the FIRST pass.
///
/// This is the real-world shape (`rearm_supersedes_blocked_parent_when_revision_is_dead`
/// models the simpler dangling-revision-id case where the task row is gone;
/// here the task row survives with a live status and only its execution died).
#[tokio::test]
async fn rearm_supersedes_blocked_parent_when_revision_execution_died_in_restart() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/20x";
    let (product, chore) = make_in_review(&db, "C-restart-dead-exec", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 20,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("abc123".into()),
            head_sha_before: Some("head456".into()),
        })
        .unwrap()
        .expect("fresh insert");
    let original_id = attempt.id.clone();

    // A genuinely-live revision TASK whose backing execution was running when
    // the engine restarted and has since been reaped to `orphaned`. The task
    // status column stayed `active` across the restart — exactly the paper-live
    // shape the old status-only check misread as "revision in flight".
    let dead_revision = create_active_chore(&db, &product, "revision whose worker died on restart");
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(dead_revision.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    db.mark_execution_orphaned(&exec.id, "engine_restart_orphan").unwrap();
    db.set_conflict_resolution_revision_task_id(&attempt.id, &dead_revision)
        .unwrap();

    let (s, _) = chore_status(&db, &chore);
    assert_eq!(s, TaskStatus::Blocked, "sanity: parent must be blocked before probe");

    // First post-restart probe (same head SHA, still CONFLICTING). The dead
    // execution behind a still-active task must be superseded on THIS pass.
    let result = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(
        result,
        "a revision whose execution died in a restart must be superseded (state changed)"
    );

    // The dead attempt is abandoned; a fresh one is inserted with a new
    // (live) revision, not the dead one.
    let all_crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(
        all_crz.len(),
        2,
        "dead attempt abandoned and a fresh attempt spawned on the first post-boot pass"
    );
    let original = all_crz
        .iter()
        .find(|r| r.id == original_id)
        .expect("original crz must still exist");
    assert_eq!(
        original.status, "abandoned",
        "dead attempt must be abandoned, not left pending"
    );
    let fresh = all_crz
        .iter()
        .find(|r| r.id != original_id)
        .expect("fresh crz must exist");
    assert_eq!(fresh.status, "pending");
    assert!(
        fresh.revision_task_id.is_some() && fresh.revision_task_id.as_deref() != Some(dead_revision.as_str()),
        "fresh crz must carry a new, live revision, not the dead one"
    );

    // Parent recovers to in_review with the fresh revision in flight — the
    // blocked row is no longer permanently stranded.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent recovers to in_review once the dead attempt is superseded"
    );
    assert!(reason.is_none());
}

fn seed_lockfile_failure(db: &WorkDb, product: &str, chore: &str, pr: &str, head: &str, base: &str) -> String {
    db.mark_chore_blocked_merge_conflict(chore, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.to_owned(),
            work_item_id: chore.to_owned(),
            pr_url: pr.into(),
            pr_number: 81,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some(base.into()),
            head_sha_before: Some(head.into()),
        })
        .unwrap()
        .expect("fresh lockfile-failure attempt");
    db.mark_conflict_resolution_failed(&attempt.id, "lockfile_regeneration_failed: MODULE.bazel.lock")
        .unwrap()
        .expect("stamp lockfile failure");
    attempt.id
}

#[tokio::test]
async fn rearm_does_not_retry_lockfile_failure_for_the_same_base_head_pair() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/81";
    let (product, chore) = make_in_review(&db, "C-lockfile-same-pair", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let failed_id = seed_lockfile_failure(&db, &product, &chore, pr, "head456", "abc123");

    let retried = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!retried, "the same (base, head) pair must not mint a new attempt");
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all.len(), 1, "failed row is kept and not duplicated: {all:?}");
    assert_eq!(all[0].id, failed_id);
    assert_eq!(all[0].status, "failed");
    assert!(db.active_conflict_resolution_for_work_item(&chore).unwrap().is_none());
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
}

#[tokio::test]
async fn rearm_retries_lockfile_failure_when_head_moves() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/82";
    let (product, chore) = make_in_review(&db, "C-lockfile-moved-pair", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let failed_id = seed_lockfile_failure(&db, &product, &chore, pr, "head-old", "abc123");

    let retried = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-new"),
    )
    .await;
    assert!(retried, "a moved head must start a fresh attempt");
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all.len(), 2, "old failure kept, fresh attempt inserted: {all:?}");
    let failed = all.iter().find(|r| r.id == failed_id).expect("failed row kept");
    assert_eq!(failed.status, "failed");
    let fresh = all.iter().find(|r| r.id != failed_id).expect("fresh attempt");
    assert_eq!(fresh.status, "pending");
    assert_eq!(fresh.head_sha_before.as_deref(), Some("head-new"));
    assert!(fresh.revision_task_id.is_some(), "fresh attempt must spawn a revision");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

/// The probe's base oid mirrors GitHub's `baseRefOid`, which does not track
/// `main` moving; only a head push (or a manual retry) re-arms the attempt, so
/// a base-only difference must not mint a new attempt.
#[tokio::test]
async fn rearm_does_not_retry_lockfile_failure_when_only_base_differs() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/85";
    let (product, chore) = make_in_review(&db, "C-lockfile-moved-base", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    seed_lockfile_failure(&db, &product, &chore, pr, "head456", "abc123");

    let retried = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head_and_base(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            "head456",
            "base-new",
        ),
    )
    .await;
    assert!(!retried, "a base-only difference must not start a fresh attempt");
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all.len(), 1, "failed row kept, nothing inserted: {all:?}");
}

/// Failure on head A, re-arm on head B, failure again: exactly one open
/// attention item remains, carrying the latest reason.
#[tokio::test]
async fn repeated_lockfile_failures_keep_one_open_attention_item() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/86";
    let (product, chore) = make_in_review(&db, "C-lockfile-dup-attention", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    seed_lockfile_failure(&db, &product, &chore, pr, "head-a", "abc123");
    let kind = crate::conflict_ladder::LOCKFILE_REGENERATION_FAILED_ATTENTION_KIND;
    let candidate_row = candidate(&product, &chore, pr);
    let file = vec!["MODULE.bazel.lock".to_owned()];
    let file_ref = &file;

    for (round, reason) in [(1, "first failure"), (2, "second failure")] {
        let attempt = if round == 1 {
            db.latest_conflict_resolution_for_work_item(&chore).unwrap().unwrap()
        } else {
            // Re-arm on a moved head, then fail that fresh attempt too.
            let retried = on_conflict_detected(
                &db,
                pub_.as_ref(),
                None,
                &open_checker(),
                &candidate_row,
                &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-b"),
            )
            .await;
            assert!(retried);
            db.active_conflict_resolution_for_work_item(&chore)
                .unwrap()
                .expect("fresh attempt")
        };
        crate::conflict_ladder::halt_attempt_for_lockfile_failure(
            &db,
            pub_.as_ref(),
            &candidate_row,
            &attempt,
            file_ref,
            reason,
        )
        .await;
    }

    let open: Vec<_> = db
        .list_attention_items_for_work_item(&chore)
        .unwrap()
        .into_iter()
        .filter(|i| i.kind == kind && i.status == "open")
        .collect();
    assert_eq!(open.len(), 1, "one open item, not a pile: {open:?}");
    assert!(
        open[0].body_markdown.contains("second failure"),
        "{:?}",
        open[0].body_markdown
    );
}

/// A `blocked` conflict revision that still owns a running execution is a
/// live writer: its attempt must not be superseded and no competing
/// remediation may start.
#[tokio::test]
async fn rearm_keeps_blocked_revision_with_running_execution() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/87";
    let (product, chore) = make_in_review(&db, "C-blocked-live-revision", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 87,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("abc123".into()),
            head_sha_before: Some("head456".into()),
        })
        .unwrap()
        .expect("fresh insert");
    let revision = create_active_chore(&db, &product, "blocked revision, worker still running");
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(revision.clone())
            .kind(ExecutionKind::RevisionImplementation)
            .status(ExecutionStatus::Running)
            .build(),
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked' WHERE id = ?1",
            rusqlite::params![revision],
        )
        .unwrap();
    db.set_conflict_resolution_revision_task_id(&attempt.id, &revision)
        .unwrap();

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all.len(), 1, "no competing attempt may start: {all:?}");
    assert_eq!(all[0].id, attempt.id);
    assert_eq!(all[0].status, "pending", "the live attempt must not be superseded");
    assert!(db.has_live_conflict_revision_for_pr(pr).unwrap());
}
