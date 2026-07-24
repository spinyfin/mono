use super::helpers::*;

// ----- Merge-queue rebounce detection (T605 regression, PR #690) -----

/// A PR whose head-branch CI is all green but that was removed from
/// the merge queue with `reason=FAILED_CHECKS` must flip its owning
/// chore to `blocked: ci_failure` and park a `ci_remediation` execution.
///
/// This is the basic reproducer for the T604 missed-detection: the
/// engine must act on the `RemovedFromMergeQueueEvent` timeline signal,
/// not on the per-PR `statusCheckRollup` (which stays SUCCESS after a
/// dequeue).
#[tokio::test]
async fn rebounce_flips_in_review_to_blocked_ci_failure() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/500";
    let (product, chore) = make_in_review(&db, "C-rebounce-detect", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature-branch"),
        "synthetic-merge-sha-abc",
        &[],
        &[],
    )
    .await;
    assert!(flipped, "rebounce detection must flip chore to ci_failure");

    // T2381/PR#1861 fix: in the in_review model (mirrors on_ci_failure_detected
    // and on_conflict_detected) a spawned revision immediately unblocks the
    // parent back to `in_review`; `blocked: ci_failure` is transient. Before
    // this fix the parent stayed `blocked: ci_failure` forever, invisible to
    // conflict_watch's `status='in_review'` WHERE guard.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "ci_failure"),
        "in-flight ci_failure signal must stay armed while the revision runs",
    );

    // Phase 5 cutover: no bespoke ci_remediation execution — the fix
    // delivers via an engine-triggered revision instead.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let exec_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions
              WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exec_count, 0, "cutover: no bespoke ci_remediation execution");
    let rev_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rev_count, 1, "rebounce must spawn exactly one revision task");

    // The ci_remediations row must record the failure as a queue rebounce
    // and have its revision_task_id stamped.
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("active attempt row");
    assert_eq!(attempt.failure_kind.as_deref(), Some("merge_queue_rebounce"));
    assert_eq!(attempt.before_commit_sha.as_deref(), Some("synthetic-merge-sha-abc"));
    assert!(
        attempt.revision_task_id.is_some(),
        "attempt must have revision_task_id stamped"
    );
}

/// When reconciliation detects a merge-queue rebounce and is supplied with
/// the failing check data (from `fetch_failing_checks_for_commit`), those
/// checks must be stored on the `ci_remediations` row so the revision
/// directive can show the worker the exact build URL and job id — not just
/// generic "look for a failing build" instructions.
#[tokio::test]
async fn rebounce_stores_failing_checks_from_failures_slice() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/999";
    let (product, chore) = make_in_review(&db, "C-rebounce-checks", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let failures = vec![RequiredCheckFailure {
        name: "ci/build".into(),
        conclusion: "failure".into(),
        target_url: "https://buildkite.com/org/mono/builds/1666#job-abc-uuid".into(),
        provider: CiProvider::Buildkite,
        provider_job_id: Some("job-abc-uuid".into()),
    }];

    let flipped = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature-branch"),
        "synthetic-merge-sha-xyz",
        &[],
        &failures,
    )
    .await;
    assert!(flipped, "rebounce detection must flip chore to ci_failure");

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("active attempt row");

    // The failed_checks JSON must carry the Buildkite check so the revision
    // directive can show the worker a pre-filled `bk job log` command.
    let checks: Vec<serde_json::Value> = serde_json::from_str(&attempt.failed_checks).expect("valid JSON");
    assert!(
        !checks.is_empty(),
        "failed_checks must not be empty when failures were supplied"
    );
    assert_eq!(checks[0]["name"].as_str(), Some("ci/build"));
    assert_eq!(checks[0]["provider"].as_str(), Some("buildkite"));
    assert_eq!(checks[0]["provider_job_id"].as_str(), Some("job-abc-uuid"));
    assert!(
        checks[0]["target_url"]
            .as_str()
            .unwrap_or_default()
            .contains("builds/1666"),
        "target_url must contain the build number"
    );
}

/// THE REGRESSION (T604 / PR #690 04:44Z miss): a clean head-branch CI
/// probe must NOT clear a `merge_queue_rebounce` block.
///
/// Before the fix, `on_ci_resolved` treated "head-branch CI is green" as
/// a sufficient clearing signal for ALL ci_failure reasons.  For a
/// rebounce, the PR's own CI is *always* green (the failure is on the
/// synthetic merge commit), so every sweep immediately un-blocked the
/// chore, preventing detection from sticking.
#[tokio::test]
async fn rebounce_block_not_cleared_by_clean_head_branch_ci() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/501";
    let (product, chore) = make_in_review(&db, "C-rebounce-noclr", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Step 1: detect the rebounce — chore flips to blocked: ci_failure.
    let flipped = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature-branch"),
        "synthetic-sha-xyz",
        &[],
        &[],
    )
    .await;
    assert!(flipped);

    // Step 2: simulate the merge_poller's next sweep — the head-branch CI
    // probe returns Clean (statusCheckRollup is all SUCCESS), so sweep_one
    // calls on_ci_resolved.  This must NOT clear the rebounce block.
    let cleared = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(
        !cleared,
        "on_ci_resolved must not clear a merge_queue_rebounce block based on \
         head-branch CI; the PR's own CI is always green in this case"
    );

    // Parent stays in_review (in_review model) with the ci_failure signal
    // still armed — the clean head-branch probe must not clear it.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "ci_failure"),
        "ci_failure in-flight signal must remain armed — the PR's own CI is always \
         green for a rebounce",
    );
}

/// Defect #3 (the un-block side fighting the block): an InFlight head-branch
/// CI probe must NOT clear a `merge_queue_rebounce` block.
///
/// The PR's own branch CI is green for a queue failure, and the rebounce
/// attempt's `head_sha_at_trigger` is the synthetic merge commit — which never
/// equals the PR head — so `on_ci_in_flight_supersedes_failure`'s stale-head
/// heuristic would otherwise read "stale", abandon the attempt, and clear the
/// block. The next sweep's rebounce check would re-block it: the observed
/// blocked<->in_review flap. The block must stand and the attempt must stay
/// pending (so `on_ci_resolved`'s guard keeps holding too).
#[tokio::test]
async fn rebounce_block_not_cleared_by_inflight_head_branch_ci() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/504";
    let (product, chore) = make_in_review(&db, "C-rebounce-inflight", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature-branch"),
        "synthetic-sha-inflight",
        &[],
        &[],
    )
    .await;
    assert!(flipped);

    // Merge poller's next sweep probes the PR head and finds CI InFlight.
    // `current_head_sha` (PR head) differs from the synthetic merge SHA the
    // attempt was keyed on — exactly the condition that used to mis-fire.
    let cleared = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("pr-head-sha-different"),
    )
    .await;
    assert!(
        !cleared,
        "InFlight head-branch CI must not supersede a merge_queue_rebounce block",
    );

    // Parent stays in_review (in_review model) with the ci_failure signal
    // still armed — the InFlight probe must not supersede it.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "ci_failure"),
        "ci_failure in-flight signal must remain armed",
    );
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("rebounce attempt must still be active (not abandoned by the supersede path)");
    assert_eq!(attempt.failure_kind.as_deref(), Some("merge_queue_rebounce"));
}

/// End-to-end anti-flap reproducer: across repeated sweeps of an UNCHANGED
/// failing merge SHA — with both an InFlight supersede probe and a Clean
/// `on_ci_resolved` probe running between rebounce checks every cycle — the
/// chore bounces to blocked AT MOST ONCE and never oscillates back to
/// in_review. This is the operator-reported symptom (~once-a-minute flap)
/// pinned shut: defects #1 (per-sha idempotency) and #3 (sticky block) acting
/// together.
#[tokio::test]
async fn rebounce_does_not_flap_across_repeated_sweeps() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/505";
    let (product, chore) = make_in_review(&db, "C-rebounce-noflap", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);
    let sha = "synthetic-merge-noflap";

    let mut bounce_count = 0;
    for cycle in 0..5 {
        // The rebounce pass re-sees the same dequeue event on every sweep.
        if on_merge_queue_rebounce_detected(&db, pub_.as_ref(), &cand, Some("feature"), sha, &[], &[]).await {
            bounce_count += 1;
        }
        // The per-PR probe alternates between InFlight (supersede) and Clean
        // (on_ci_resolved) — both opposing un-block paths must decline.
        on_ci_in_flight_supersedes_failure(&db, pub_.as_ref(), &cand, &[], Some("pr-head")).await;
        on_ci_resolved(&db, pub_.as_ref(), &cand, &[]).await;

        // Invariant on every cycle after the first bounce: parent stays
        // in_review (in_review model) with the ci_failure signal still armed
        // — neither opposing un-block path may clear it.
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, TaskStatus::InReview, "cycle {cycle}");
        assert!(reason.is_none(), "cycle {cycle}");
        assert!(
            db.active_blocked_signals(&chore)
                .unwrap()
                .iter()
                .any(|s| s.reason == "ci_failure"),
            "ci_failure in-flight signal must stay armed on cycle {cycle} (no flap)",
        );
    }
    assert_eq!(
        bounce_count, 1,
        "an unchanged failing merge SHA must bounce exactly once across all sweeps"
    );
}

/// A second probe of the same dequeue event (same `before_commit_sha`)
/// is idempotent: the INSERT OR IGNORE is a no-op, but the chore stays
/// blocked and no new execution is created.
#[tokio::test]
async fn rebounce_detection_idempotent_on_same_sha() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/502";
    let (product, chore) = make_in_review(&db, "C-rebounce-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let first = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "sha-A",
        &[],
        &[],
    )
    .await;
    // Repeat for the same SHA (as would happen when the same dequeue event
    // appears in the timeline across consecutive sweeps).
    let second = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "sha-A",
        &[],
        &[],
    )
    .await;
    assert!(first, "first detection must flip the chore");
    assert!(!second, "second probe for same SHA must be a no-op");

    // In the in_review model the spawned revision immediately unblocks the
    // parent back to `in_review`.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // Phase 5 cutover: exactly one revision, no ci_remediation executions.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let exec_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions
              WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exec_count, 0, "cutover: no bespoke ci_remediation execution");
    let rev_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        rev_count, 1,
        "exactly one revision; duplicate probe must not spawn a second"
    );
}

/// After the worker marks the attempt succeeded, the next `on_ci_resolved`
/// call (with clean head-branch CI) should clear the rebounce block — that
/// is the correct terminal path.
#[tokio::test]
async fn rebounce_block_clears_after_worker_succeeds() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/503";
    let (product, chore) = make_in_review(&db, "C-rebounce-retire", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect.
    on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "sha-Q",
        &[],
        &[],
    )
    .await;

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt row");

    // 2. Worker marks attempt succeeded (re-enqueued the PR).
    db.mark_ci_remediation_succeeded(&attempt.id, None)
        .unwrap()
        .expect("succeeded update");

    // 3. Now on_ci_resolved fires (head-branch CI still clean) — no active
    //    attempt exists, so the rebounce guard does not fire and the block
    //    is cleared correctly.
    let cleared = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(cleared, "after worker succeeds, on_ci_resolved must clear the block");

    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
}
