//! The re-arm branch: what detection does when the parent has already come
//! to rest `blocked: merge_conflict` — dispatching a fresh attempt behind a
//! frozen base SHA, invalidating a stale success, reconciling a live
//! revision, and superseding a dead one.

use std::sync::Arc;

use tempfile::tempdir;

use super::super::*;
use super::helpers::*;
use crate::merge_poller::{OpenPrStatus, PrLifecycleState};
use crate::test_support::*;
use crate::work::{WorkDb, WorkItemPatch};

/// Regression test for T2396 / PR #1874: the stale-base re-arm path
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

/// Reconciliation path (T791/T898 scenario): parent is in `blocked: merge_conflict`
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
    // exists (T898-style). Manually flip to blocked, insert a crz, create a
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
    // simulate T898 being active. It must be a real, live task — not a
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
    // `is_conflict_resolution_revision_live` reports it dead — standing in
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
