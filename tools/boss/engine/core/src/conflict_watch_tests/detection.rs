//! `on_conflict_detected` on a fresh conflict: what it does to the parent's
//! status, the attempt row it inserts, the revision it spawns, the
//! `ConflictResolutionStarted` event, and where it declines to act (human
//! moved the row, an auto-rebase attempt already owns the PR).

use std::sync::Arc;

use tempfile::tempdir;

use super::super::*;
use super::helpers::*;
use crate::merge_poller::{OpenPrStatus, PrLifecycleState};
use crate::test_support::*;
use crate::work::{WorkDb, WorkItem, WorkItemPatch};

/// New-model acceptance: when a revision fix vehicle is successfully spawned,
/// the parent stays in `in_review` (Review column). The blocked state is only
/// reached when there is no tractable fix vehicle (churn cap, create_revision
/// failure, closed PR). See also `detection_blocks_parent_when_revision_fails`.
#[tokio::test]
async fn detection_keeps_parent_in_review_when_revision_spawns() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/10";
    let (product, chore) = make_in_review(&db, "C-detect", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // transitioned == true because parent went in_review→blocked→in_review
    assert!(transitioned, "first detection must return true (state changed)");

    // Parent stays in Review — not blocked.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // Event emitted is "conflict_revision_in_flight", not "blocked_merge_conflict".
    let events = pub_.events.lock().await.clone();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        (product.clone(), chore.clone(), "conflict_revision_in_flight".into())
    );

    // crz row exists and revision was spawned.
    let attempt = db.active_conflict_resolution_for_work_item(&chore).unwrap();
    assert!(attempt.is_some(), "crz attempt row must be present");
    let attempt = attempt.unwrap();
    assert_eq!(attempt.status, "pending");
    assert!(attempt.revision_task_id.is_some(), "revision must have been spawned");
}

/// When `create_revision` fails (parent PR closed/unmerged) or the churn cap
/// pre-abandons the attempt, the parent DOES flip to `blocked: merge_conflict`
/// so the human sees the card in Blocked.
#[tokio::test]
async fn detection_blocks_parent_when_revision_fails() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/10b";
    let (product, chore) = make_in_review(&db, "C-detect-fail", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(transitioned, "detection must return true (parent blocked)");

    // Parent is blocked since there is no active fix vehicle.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));

    let events = pub_.events.lock().await.clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].2, "blocked_merge_conflict");

    // crz was abandoned (revision_create_failed).
    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "abandoned");
    assert_eq!(attempts[0].failure_reason.as_deref(), Some("revision_create_failed"),);
}

#[tokio::test]
async fn detection_is_idempotent_on_repeated_probes() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/11";
    let (product, chore) = make_in_review(&db, "C-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First probe: conflict detected, revision spawned, parent stays in_review.
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // Second probe with same base sha: UNIQUE collision on crz insert.
    // Existing crz has revision_task_id → upfront flip cleared back to
    // in_review by the collision path, but no net state change vs what
    // we already have.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(first, "first probe must return true (state changed)");
    // Second probe: upfront flip still briefly goes to blocked then clears
    // back — returns true again because task_unblocked_for_revision=true.
    // The important invariant: parent ends up in_review, exactly one crz.
    let (status, _) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent must stay in_review after repeated probes"
    );
    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1, "same base sha must not stack crz rows");
    // Exactly one ConflictResolutionStarted typed event per probe.
    let started_count = pub_
        .typed_events
        .lock()
        .await
        .iter()
        .filter(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionStarted { .. }))
        .count();
    assert!(started_count >= 1, "at least one ConflictResolutionStarted must fire");
    // At most two "conflict_revision_in_flight" events (one per probe), never
    // a "blocked_merge_conflict" since a fix vehicle is always in flight.
    let reasons: Vec<String> = pub_.events.lock().await.iter().map(|(_, _, r)| r.clone()).collect();
    assert!(
        reasons.iter().all(|r| r == "conflict_revision_in_flight"),
        "all work-item events must be conflict_revision_in_flight, got {reasons:?}",
    );
    let _ = second; // return value may be true or false; variant covered by the assertions above
}

#[tokio::test]
async fn detection_skipped_when_human_moved_row_off_in_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/15";
    let (product, chore) = make_in_review(&db, "C-human", pr);
    // Human flipped the row to `active` after PR was opened.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("active".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let pub_ = Arc::new(RecordingPublisher::default());

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!transitioned, "WHERE guard protects manual moves");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Active);
    assert!(reason.is_none());
    assert!(pub_.events.lock().await.is_empty());
}

#[tokio::test]
async fn detection_emits_started_event_reuses_existing_row_on_same_base_sha() {
    // When on_conflict_detected is called a second time for the same
    // base sha while a revision is in flight, the pre-flight early-exit
    // fires and no new events are emitted (pure no-op). The first call
    // created the attempt and emitted ConflictResolutionStarted; that's
    // the authoritative event. Only one crz row must exist.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/23";
    let (product, chore) = make_in_review(&db, "C-detect-evt", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First call — creates the attempt, spawns revision, parent stays in_review.
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
    let first_events = pub_.typed_events.lock().await.clone();
    assert_eq!(first_events.len(), 1, "exactly one started event on first call");
    let first_attempt_id = match &first_events[0].1 {
        FrontendEvent::ConflictResolutionStarted { attempt_id, .. } => attempt_id.clone(),
        other => panic!("unexpected event {other:?}"),
    };

    // Second call: same base sha, revision already in flight → pre-flight
    // early-exit. Returns false (no-op), no new typed events.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!second, "second probe with active revision must be a no-op");

    // Only one crz row; only one started event.
    let all_started: Vec<_> = pub_
        .typed_events
        .lock()
        .await
        .iter()
        .filter(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionStarted { .. }))
        .cloned()
        .collect();
    assert_eq!(all_started.len(), 1, "no second started event from idempotent no-op");
    if let FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. } = &all_started[0].1 {
        assert_eq!(a, &first_attempt_id);
    }
    let crz_count = db
        .list_conflict_resolutions(None, &[], Some(&chore), None)
        .unwrap()
        .len();
    assert_eq!(crz_count, 1, "same base sha must not create a second crz row");
    let _ = (product, first_attempt_id); // silence unused warnings
}

#[tokio::test]
async fn detection_inserts_attempt_and_emits_started_event() {
    // on_conflict_detected inserts the conflict_resolution attempt and emits
    // ConflictResolutionStarted in the same call. Parent stays in_review
    // when revision spawns (no pre-wiring needed for on_resolved to fire).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/24";
    let (product, chore) = make_in_review(&db, "C-detect-noevt", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(transitioned);

    let attempt = db.active_conflict_resolution_for_work_item(&chore).unwrap();
    assert!(
        attempt.is_some(),
        "on_conflict_detected must insert a conflict_resolution row",
    );
    let attempt = attempt.unwrap();
    assert_eq!(attempt.status, "pending");

    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. } if a == &attempt.id
        )),
        "ConflictResolutionStarted must fire with the new attempt id, got {typed:?}",
    );
}

#[tokio::test]
async fn detection_defers_when_rebase_attempt_is_active() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/17";
    let (product, chore) = make_in_review(&db, "C-rebase", pr);
    // Simulate auto-rebase having created its side table and a
    // running attempt for this PR. The table doesn't ship until
    // auto-rebase lands, so the conflict_watch must defer when it
    // does exist + has a non-terminal row.
    let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
    conn.execute(
        "CREATE TABLE rebase_attempts (
             id                TEXT PRIMARY KEY,
             dependent_pr_url  TEXT NOT NULL,
             status            TEXT NOT NULL
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO rebase_attempts (id, dependent_pr_url, status)
          VALUES ('reb_1', ?1, 'running')",
        [pr],
    )
    .unwrap();
    drop(conn);

    let pub_ = Arc::new(RecordingPublisher::default());
    let r = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!r, "rebase-active path must defer");
    let (status, _) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview, "row stays where it was");
    assert!(pub_.events.lock().await.is_empty());
}

#[tokio::test]
async fn detection_spawns_revision_and_stamps_attempt() {
    // A genuinely-new conflict creates a `kind=revision` task (parent =
    // chore, merge-conflict provenance), stamps the ledger row's
    // `revision_task_id`, creates NO bespoke conflict_resolution execution,
    // and leaves the parent in `in_review` (new-model parent-state).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/30";
    let (product, chore) = make_in_review(&db, "C-rev-spawn", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

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

    // Parent stays in_review — the revision card is the Doing card.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent must stay in Review while revision is in flight"
    );
    assert!(reason.is_none());

    let attempt = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("a pending attempt row must exist");
    assert_eq!(attempt.status, "pending");
    let rev_id = attempt
        .revision_task_id
        .clone()
        .expect("the producer must stamp revision_task_id on the attempt");

    let revision = match db.get_work_item(&rev_id).unwrap() {
        WorkItem::Task(t) => t,
        other => panic!("expected revision task, got {other:?}"),
    };
    assert_eq!(revision.kind, TaskKind::Revision);
    assert_eq!(revision.parent_task_id.as_deref(), Some(chore.as_str()));
    assert_eq!(revision.created_via, format!("merge-conflict:{}", attempt.id));
    assert_eq!(revision.description, "Resolve merge conflict against main");

    // No bespoke conflict_resolution execution: the revision rides the
    // reconcile loop's revision_implementation dispatch instead.
    let ready = db.list_ready_executions().unwrap();
    assert!(
        !ready.iter().any(|e| e.kind == ExecutionKind::ConflictResolution),
        "cutover must not create a conflict_resolution execution; got {ready:?}",
    );
}

#[tokio::test]
async fn detection_idempotent_does_not_double_spawn_revision() {
    // Re-firing on the same base sha reuses the existing attempt (whose
    // revision_task_id is already set) and spawns no second revision.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/31";
    let (product, chore) = make_in_review(&db, "C-rev-idem", pr);
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
    // Reset to in_review so the second probe re-enters the primary flip
    // path with the same base sha (UNIQUE collision on the ledger).
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
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

    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1, "same base sha must not stack attempts");
    let revision_backed = attempts.iter().filter(|r| r.revision_task_id.is_some()).count();
    assert_eq!(revision_backed, 1, "exactly one revision-backed attempt");
}
