//! `on_resolved` when the PR goes clean again: retiring the attempt, closing
//! (or deliberately not closing) the revision task it spawned, flipping a
//! blocked parent back to Review, and its idempotence — plus the full
//! conflict → resolve → conflict cycle.

use std::sync::Arc;

use tempfile::tempdir;

use super::super::*;
use super::helpers::*;
use crate::merge_poller::{OpenPrStatus, PrLifecycleState};
use crate::test_support::*;
use crate::work::{CreateExecutionInput, ExecutionStatus, WorkDb, WorkItem, WorkItemPatch};

/// New-model: parent was never blocked (revision spawned, stayed in_review).
/// When the PR becomes clean, the crz attempt is retired and the signal
/// cleared. The parent is already in_review — no status-change event fires.
#[tokio::test]
async fn resolution_retires_attempt_when_parent_was_in_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/12";
    let (product, chore) = make_in_review(&db, "C-resolve", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // Parent is in_review (revision spawned). Verify, then resolve.
    let (status_before, _) = chore_status(&db, &chore);
    assert_eq!(status_before, TaskStatus::InReview);

    let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(resolved, "on_resolved must return true (attempt was retired)");

    // Parent still in_review — didn't change status.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // No "merge_conflict_resolved" work-item event (parent didn't transition).
    let events = pub_.events.lock().await.clone();
    assert!(
        !events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
        "merge_conflict_resolved must not fire when parent was already in_review",
    );

    // ConflictResolutionSucceeded typed event must fire.
    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionSucceeded { .. })),
        "ConflictResolutionSucceeded must fire, got {typed:?}",
    );
}

/// T3 (this chore): a resolved merge-conflict attempt must close the
/// revision task it spawned, not just its own `conflict_resolutions` row —
/// otherwise the parent shows a phantom "in revision" badge until the
/// parent's PR eventually merges (or forever, if the revision never
/// advances on its own; see the 2026-07-16 incident). A freshly-spawned,
/// never-dispatched revision has no live execution, so `on_resolved` must
/// close it out (archived as moot) immediately.
#[tokio::test]
async fn resolution_closes_the_revision_task_it_spawned() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/13";
    let (product, chore) = make_in_review(&db, "C-close-revision", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let active = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("worker path must spawn a revision");
    let revision_id = active
        .revision_task_id
        .clone()
        .expect("revision task id must be stamped");

    let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(resolved);

    let t = task(&db, &revision_id);
    assert_eq!(
        t.status,
        TaskStatus::Archived,
        "a never-dispatched (no live execution) revision must be closed, not left todo/active forever",
    );
    assert!(t.archived_reason.is_some());
}

/// A revision task still being actively driven by a worker (a `running`
/// execution) must NOT be closed out from under it — its own on-Stop
/// completion advances it normally when the worker's turn ends.
#[tokio::test]
async fn resolution_leaves_a_live_revision_task_alone() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/14";
    let (product, chore) = make_in_review(&db, "C-live-revision", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let active = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("worker path must spawn a revision");
    let revision_id = active
        .revision_task_id
        .clone()
        .expect("revision task id must be stamped");
    let status_before = match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t.status,
        other => panic!("expected a task-shaped revision, got {other:?}"),
    };
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(revision_id.clone())
            .kind(ExecutionKind::RevisionImplementation)
            .status(ExecutionStatus::Running)
            .build(),
    )
    .unwrap();

    let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(resolved);

    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(
                t.status, status_before,
                "a revision with a live (running) execution must be left untouched",
            );
            assert!(t.archived_reason.is_none());
        }
        other => panic!("expected a task-shaped revision, got {other:?}"),
    }
}

/// A revision task already `in_review` (its own worker already finished and
/// pushed) is left alone here — it rides the parent's eventual real merge
/// to `done` via `flip_in_review_revisions_to_done`, not archived by the
/// conflict-resolution retire path.
#[tokio::test]
async fn resolution_leaves_an_in_review_revision_task_alone() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/15";
    let (product, chore) = make_in_review(&db, "C-in-review-revision", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let active = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("worker path must spawn a revision");
    let revision_id = active
        .revision_task_id
        .clone()
        .expect("revision task id must be stamped");
    db.update_work_item(
        &revision_id,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(resolved);

    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(
                t.status,
                TaskStatus::InReview,
                "an in_review revision must ride the eventual real merge to done, not be \
                 archived by the conflict-resolution retire path",
            );
        }
        other => panic!("expected a task-shaped revision, got {other:?}"),
    }
}

/// Old-model compatibility: when the parent IS blocked (revision_create_failed,
/// churn cap), on_resolved flips it back to in_review and emits
/// "merge_conflict_resolved".
#[tokio::test]
async fn resolution_flips_blocked_parent_back_to_in_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/12b";
    let (product, chore) = make_in_review(&db, "C-resolve-blocked", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

    // Drive into blocked via create_revision failure.
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let (status_before, reason_before) = chore_status(&db, &chore);
    assert_eq!(status_before, TaskStatus::Blocked);
    assert_eq!(reason_before.as_deref(), Some("merge_conflict"));

    // Now manually install a running attempt (simulates legacy worker) and resolve.
    let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-x");
    let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(resolved);

    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    let events = pub_.events.lock().await.clone();
    assert!(
        events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
        "merge_conflict_resolved must fire when parent was blocked, got {events:?}",
    );
    // Verify attempt was retired.
    let attempt_row = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_eq!(attempt_row.status, "succeeded");
}

#[tokio::test]
async fn resolution_is_idempotent_on_repeated_clean_probes() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/13";
    let (product, chore) = make_in_review(&db, "C-clean-noop", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First call: row is in_review (not blocked), so resolution is
    // a no-op — the WHERE guard misses, no event published.
    let r1 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(!r1);
    assert!(pub_.events.lock().await.is_empty());

    // Drive a full conflict-resolve cycle, then call resolution
    // twice — the second call must also be a no-op.
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let r2 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    let r3 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(r2);
    assert!(!r3);
}

#[tokio::test]
async fn resolution_skipped_when_human_moved_row_off_blocked() {
    // Use closed_checker so the parent actually ends up blocked
    // (revision_create_failed → no fix vehicle). The human then moves
    // the blocked row to `active` (manual override). on_resolved must
    // be a no-op because the active crz is abandoned (not pending/running).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/16";
    let (product, chore) = make_in_review(&db, "C-human-2", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let (status_before, _) = chore_status(&db, &chore);
    assert_eq!(
        status_before,
        TaskStatus::Blocked,
        "sanity: closed_checker must cause blocked"
    );
    // Human moves the blocked row to `active`.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("active".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let before_count = pub_.events.lock().await.len();
    // on_resolved: abandoned crz → no active_conflict_resolution → clear_chore
    // WHERE guard misses (status='active') → no-op.
    let r = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(!r);
    assert_eq!(pub_.events.lock().await.len(), before_count);
}

#[tokio::test]
async fn cycle_conflict_resolve_conflict() {
    // Integration: conflict detected (revision in flight) → PR resolved →
    // conflict again (same base sha → UNIQUE collision, crz was succeeded,
    // no new active crz → parent flips to blocked this time).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/14";
    let (product, chore) = make_in_review(&db, "C-cycle", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1st conflict: revision spawns, parent stays in_review.
    assert!(
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await
    );
    let (s, _) = chore_status(&db, &chore);
    assert_eq!(s, TaskStatus::InReview);

    // Resolve: PR goes clean, attempt retired, signal cleared.
    assert!(on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await);
    let (s, _) = chore_status(&db, &chore);
    assert_eq!(s, TaskStatus::InReview);

    // 2nd conflict: same base sha → UNIQUE collision. The previous crz is
    // now succeeded (no active crz). The upfront flip goes to blocked and
    // no revision is spawned (no fresh active crz to dispatch). Parent ends
    // up blocked because there is no fix vehicle.
    assert!(
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await
    );
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));

    let reasons: Vec<String> = pub_.events.lock().await.iter().map(|(_, _, r)| r.clone()).collect();
    // 1st conflict → "conflict_revision_in_flight"
    // resolve    → no work-item event (parent was in_review)
    // 2nd conflict → "blocked_merge_conflict" (UNIQUE collision, no active crz)
    assert_eq!(
        reasons,
        vec![
            "conflict_revision_in_flight".to_owned(),
            "blocked_merge_conflict".to_owned(),
        ],
    );
}
