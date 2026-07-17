use super::helpers::*;

/// T2381/PR#1861 regression: a row that another watcher flipped to
/// `blocked: ci_failure` and never returned to `in_review` (the exact
/// orphan the ci_watch merge-queue-rebounce gap used to produce before
/// its `unblock_for_revision` fix) must not be permanently invisible to
/// `conflict_watch`. When the live probe reports CONFLICTING, the
/// foreign-bucket takeover must re-bucket the row into
/// `blocked: merge_conflict`, supersede the stale `ci_remediations`
/// attempt (so it doesn't strand a "ci failing" badge forever), and
/// spawn a conflict-resolution revision like any other fresh detection.
#[tokio::test]
async fn foreign_bucket_takeover_rebuckets_stuck_ci_failure_row_to_conflict() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1861";
    let (product, chore) = make_in_review(&db, "C-t2381", pr);

    // Simulate the pre-fix orphan: the row is stuck `blocked: ci_failure`
    // with a still-active `ci_remediations` attempt, and NO merge_conflict
    // signal was ever recorded (conflict_watch has never touched this row).
    db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
    let stale_attempt = db
        .insert_ci_remediation(crate::work::CiRemediationInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.to_owned(),
            pr_number: 1861,
            head_branch: "feature".to_owned(),
            head_sha_at_trigger: "synthetic-merge-sha".to_owned(),
            attempt_kind: "fix".to_owned(),
            consumes_budget: 1,
            failed_checks: "[]".to_owned(),
            failure_kind: "merge_queue_rebounce".to_owned(),
            before_commit_sha: Some("synthetic-merge-sha".to_owned()),
        })
        .unwrap()
        .expect("fresh insert");
    assert_eq!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .map(|s| s.reason.clone())
            .collect::<Vec<_>>(),
        vec!["ci_failure".to_owned()],
        "precondition: only the ci_failure signal is active, no merge_conflict — \
         the orphan this fix targets (conflict_watch never touched this row)",
    );

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
    assert!(took_over, "conflict_watch must take the orphaned row over");

    // The stale ci_remediations attempt must be superseded, not left
    // dangling (it would otherwise strand a phantom "ci failing" badge
    // forever — no retire path ever marks a merge_queue_rebounce attempt
    // succeeded from a clean head-branch probe).
    let ci_attempt = db.active_ci_remediation_for_work_item(&chore).unwrap();
    assert!(
        ci_attempt.is_none(),
        "stale ci_remediations attempt must be superseded (abandoned), not left active"
    );
    let refreshed_stale = db
        .get_ci_remediation(&stale_attempt.id)
        .unwrap()
        .expect("row still exists");
    assert_eq!(refreshed_stale.status, "abandoned");

    // A conflict_resolutions attempt must now exist and the parent must
    // be either `blocked: merge_conflict` (no fix vehicle) or back in
    // `in_review` with the fix revision running — never still `ci_failure`.
    let crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(crz.len(), 1, "a fresh conflict_resolutions attempt must be created");
    let (status, reason) = chore_status(&db, &chore);
    assert!(
        reason.as_deref() != Some("ci_failure"),
        "row must no longer be stuck on the foreign ci_failure reason"
    );
    match status {
        TaskStatus::InReview => assert!(reason.is_none(), "in_review parent must have no blocked_reason"),
        TaskStatus::Blocked => assert_eq!(reason.as_deref(), Some("merge_conflict")),
        other => panic!("unexpected status after takeover: {other:?}"),
    }
}

/// A row blocked on a genuinely higher-priority foreign reason (design
/// §Q2: dependency > review_feedback > merge_conflict > ci_failure) must
/// NOT be taken over by conflict_watch even when the live probe reports
/// CONFLICTING — that reason's own watcher still owns the row.
#[tokio::test]
async fn foreign_bucket_takeover_declines_higher_priority_reason() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1862";
    let (product, chore) = make_in_review(&db, "C-higher-prio", pr);
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

    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(
        reason.as_deref(),
        Some("dependency"),
        "dependency block must be untouched"
    );
    let crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert!(crz.is_empty(), "no conflict_resolutions attempt must be created");
}
