use super::helpers::*;

// ----- Back-to-back dequeue regression (PR #718 06:51Z miss) -----

/// Reproducer: a PR that was dequeued, manually re-queued,
/// and dequeued again must end up with a parked revision for the second
/// dequeue's SHA — without requiring the first dequeue's worker to have
/// completed.
///
/// This also pins the anti-flap contract: replaying an ALREADY-handled
/// dequeue SHA must NOT re-bounce the chore. The merge-queue dequeue event
/// stays in the PR timeline forever, so a resolved SHA would otherwise
/// re-block on every sweep (the blocked<->in_review flap). Only a genuinely
/// new failing merge SHA may flip an `in_review` chore back to blocked.
///
/// Sequence:
///   1. Chore in_review; first dequeue (SHA_1) detected → blocked, revision-1 spawned.
///   2. Worker marks SHA_1 succeeded_via_rebase (human re-queued the PR).
///   3. on_ci_resolved clears the block → chore back to in_review.
///   4. Next sweep sees both SHA_1 and SHA_2 in the timeline:
///      - SHA_1: INSERT IGNORED (key exists, row terminal) → per-sha
///               idempotency returns false; chore STAYS in_review (no re-bounce).
///      - SHA_2: INSERT succeeds → fresh attempt; chore is in_review so
///               mark_chore_blocked_ci_failure flips it to blocked and the
///               attempt gets its own revision immediately.
///   5. End state: chore blocked on SHA_2, exactly two revisions, nothing
///      stranded — and SHA_1's stale dequeue never caused a flap.
///
/// Detection must not require a live worker on the chore.
#[tokio::test]
async fn back_to_back_rebounce_parks_execution_for_second_dequeue() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/718";
    let (product, chore) = make_in_review(&db, "C-t628-backtoback", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Step 1: first dequeue (SHA_1) → chore flips to blocked, revision spawned.
    let first = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "sha-merge-1",
        &[],
        &[],
    )
    .await;
    assert!(first, "first rebounce must flip chore to ci_failure");
    // In the in_review model the spawned revision immediately unblocks the
    // parent back to `in_review`.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    {
        // Phase 5 cutover: no bespoke ci_remediation execution; a revision is
        // spawned instead.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM work_executions
                  WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "cutover: no ci_remediation execution after first dequeue");
        let r: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(r, 1, "exactly one revision after first dequeue");
    }

    // Step 2: mark SHA_1's ci_remediations row succeeded_via_rebase (PR re-queued
    // by human). In production a revision_implementation worker does the push and
    // the poller retires the ledger row; here we use the DB helper directly.
    let sha1_attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("sha1 attempt row");
    db.mark_ci_remediation_succeeded_via_rebase(&sha1_attempt.id, None)
        .unwrap()
        .expect("succeeded_via_rebase update");

    // Step 3: on_ci_resolved clears the block → chore in_review again.
    let cleared = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(cleared, "on_ci_resolved must clear the block after SHA_1 is terminal");
    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);

    // Step 4a: next sweep replays SHA_1 — INSERT is ignored (key exists, row
    // terminal). Per-sha idempotency: we already bounced (and resolved) this
    // failing merge SHA, so this is a no-op. The chore must NOT re-bounce — a
    // resolved dequeue event re-blocking on every sweep is the flap this fix
    // eliminates. The chore stays in_review.
    let sha1_replay = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "sha-merge-1",
        &[],
        &[],
    )
    .await;
    assert!(
        !sha1_replay,
        "sha1 replay must be an idempotent no-op (already bounced + resolved); no re-flip"
    );
    let (status, _reason) = chore_state(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "resolved SHA_1 replay must leave the chore in_review (no flap)"
    );
    // Still just the original revision from step 1.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let r: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(r, 1, "sha1 replay must not spawn a second revision");
    }

    // Step 4b: same sweep also sees SHA_2 — a genuinely NEW failing merge SHA.
    // INSERT succeeds (new key); the chore is in_review (SHA_1 replay was a
    // no-op), so mark_chore_blocked_ci_failure flips it to blocked and the
    // fresh attempt gets its own revision immediately.
    let sha2_detect = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "sha-merge-2",
        &[],
        &[],
    )
    .await;
    assert!(
        sha2_detect,
        "sha2 is a new failing merge SHA — it must bounce the in_review chore to blocked"
    );
    // SHA_2's ci_remediations row must exist as pending with revision_task_id stamped.
    let sha2_attempt = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ci_remediations
                  WHERE work_item_id = ?1 AND head_sha_at_trigger = 'sha-merge-2'
                    AND status = 'pending'",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1, "sha2 ci_remediations row must be pending");
        db.active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("sha2 attempt row")
    };
    assert!(
        sha2_attempt.revision_task_id.is_some(),
        "sha2 attempt must have a revision immediately — no stranding"
    );
    // Two revisions total: one for SHA_1, one for SHA_2.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let r: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(r, 2, "sha2 must have its own revision; total revisions must be 2");
    }
    // In the in_review model sha2's spawned revision immediately unblocks
    // the parent back to `in_review`.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // Sanity: no stranded ci_remediation attempts — sha2 has revision_task_id.
    let stranded = db.list_stranded_ci_remediation_attempts().unwrap();
    assert!(
        stranded.is_empty(),
        "no stranded attempts: sha2 has revision_task_id so it is excluded from rescue"
    );
}
