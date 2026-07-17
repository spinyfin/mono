//! Phase 6 #16: the churn guard. Four conflict-resolve cycles inside the
//! rolling 1h window pre-abandon the fourth attempt; attempts that have aged
//! out of the window don't count; a pre-abandoned attempt spawns no revision.

use std::sync::Arc;

use tempfile::tempdir;

use super::super::*;
use super::helpers::*;
use crate::merge_poller::{OpenPrStatus, PrLifecycleState};
use crate::test_support::*;
use crate::work::WorkDb;

/// Re-open the SQLite file and back-date a `conflict_resolutions`
/// row's `created_at` so churn-guard tests can simulate "this
/// attempt is 30 minutes old without sleeping the test for 30
/// minutes." Pure plumbing — production code never touches
/// `created_at` after insert.
fn rewind_attempt_created_at(db_path: &std::path::Path, attempt_id: &str, secs_ago: i64) {
    let now_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    let new_ts = (now_secs - secs_ago).to_string();
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE conflict_resolutions SET created_at = ?2 WHERE id = ?1",
        rusqlite::params![attempt_id, new_ts],
    )
    .unwrap();
}

#[tokio::test]
async fn churn_guard_pre_abandons_fourth_attempt_in_window() {
    // Phase 6 #16 acceptance: 4 conflict-resolve cycles in <1h →
    // 4th attempt is abandoned with `churn_threshold_exceeded`.
    // We exercise the WorkDb insert path directly so the test
    // doesn't need to thread through a full worker-spawn cycle.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/700";
    let (product, chore) = make_in_review(&db, "C-churn", pr);
    // Move parent into blocked so the insert path's task-side
    // stamp matches its WHERE guard for the live attempts.
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();

    // First three attempts inside the window go live.
    let make_input = |sha: &str| crate::work::ConflictResolutionInsertInput {
        product_id: product.clone(),
        work_item_id: chore.clone(),
        pr_url: pr.into(),
        pr_number: 700,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some(sha.into()),
        head_sha_before: Some("head".into()),
    };
    let a1 = db.insert_conflict_resolution(make_input("sha-1")).unwrap().unwrap();
    let a2 = db.insert_conflict_resolution(make_input("sha-2")).unwrap().unwrap();
    let a3 = db.insert_conflict_resolution(make_input("sha-3")).unwrap().unwrap();
    for id in [&a1.id, &a2.id, &a3.id] {
        let row = db.get_conflict_resolution(id).unwrap().unwrap();
        assert_eq!(row.status, "pending", "first three attempts must be live");
        assert!(row.failure_reason.is_none());
    }

    // Fourth attempt — same hour — trips the guard.
    let a4 = db.insert_conflict_resolution(make_input("sha-4")).unwrap().unwrap();
    assert_eq!(
        a4.status, "abandoned",
        "fourth attempt inside the window must be pre-abandoned",
    );
    assert_eq!(
        a4.failure_reason.as_deref(),
        Some("churn_threshold_exceeded"),
        "failure_reason must record the guard",
    );
    assert!(
        a4.finished_at.is_some(),
        "pre-abandoned attempt must carry finished_at so it's terminal",
    );

    // Parent's `blocked_attempt_id` must still point at the
    // most-recent live attempt (a3), not the dead a4.
    match db.get_work_item(&chore).unwrap() {
        crate::work::WorkItem::Chore(t) => {
            assert_eq!(
                t.blocked_attempt_id.as_deref(),
                Some(a3.id.as_str()),
                "blocked_attempt_id must not retarget at the pre-abandoned row",
            );
            assert_eq!(t.status, TaskStatus::Blocked);
            assert_eq!(t.blocked_reason.as_deref(), Some("merge_conflict"));
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn churn_guard_does_not_count_attempts_older_than_window() {
    // The guard's window is rolling-1h. Back-date three attempts
    // to > 1h ago and a brand-new fourth must go live.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/701";
    let (product, chore) = make_in_review(&db, "C-churn-rollover", pr);
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let make_input = |sha: &str| crate::work::ConflictResolutionInsertInput {
        product_id: product.clone(),
        work_item_id: chore.clone(),
        pr_url: pr.into(),
        pr_number: 701,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some(sha.into()),
        head_sha_before: Some("head".into()),
    };
    let a1 = db.insert_conflict_resolution(make_input("sha-1")).unwrap().unwrap();
    let a2 = db.insert_conflict_resolution(make_input("sha-2")).unwrap().unwrap();
    let a3 = db.insert_conflict_resolution(make_input("sha-3")).unwrap().unwrap();
    // Push all three outside the 1h window (3700s > 3600s).
    for id in [&a1.id, &a2.id, &a3.id] {
        rewind_attempt_created_at(&db_path, id, 3_700);
    }

    let a4 = db.insert_conflict_resolution(make_input("sha-4")).unwrap().unwrap();
    assert_eq!(
        a4.status, "pending",
        "older-than-window attempts must not contribute to the guard",
    );
}

#[tokio::test]
async fn churn_abandoned_attempt_spawns_no_revision() {
    // The 4th conflict in the rolling window is pre-abandoned by the
    // churn guard; the producer's `status == 'pending'` guard means it
    // gets no revision (the cap is enforced before create).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/32";
    let (product, chore) = make_in_review(&db, "C-rev-churn", pr);

    // Three prior attempts in the window arm the guard. Plant them while
    // the chore is still `in_review` so the producer's primary flip path
    // (not the re-arm short-circuit) reaches the insert for the fourth.
    for sha in ["s1", "s2", "s3"] {
        db.insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 32,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some(sha.into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap();
    }

    let pub_ = Arc::new(RecordingPublisher::default());
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        // probe base is "abc123" — a fourth distinct sha in the window.
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    let fourth = db
        .list_conflict_resolutions(None, &[], Some(&chore), None)
        .unwrap()
        .into_iter()
        .find(|r| r.base_sha_at_trigger.as_deref() == Some("abc123"))
        .expect("fourth attempt row must exist");
    assert_eq!(fourth.status, "abandoned");
    assert_eq!(fourth.failure_reason.as_deref(), Some("churn_threshold_exceeded"),);
    assert!(
        fourth.revision_task_id.is_none(),
        "churn-abandoned attempt must spawn no revision",
    );
    // Churn cap = no fix vehicle → parent must be blocked (human-attention terminal).
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::Blocked,
        "churn cap exhausted: parent must be blocked"
    );
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
}
