use super::helpers::*;

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

// ----- Phase 6 #18: opt-out gates conflict-watch flows -----

#[tokio::test]
async fn detection_skipped_when_product_opt_out_flag_disabled() {
    // Acceptance: an opted-out product's conflict-watch is a no-op.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/600";
    let (product, chore) = make_in_review(&db, "C-optout-prod", pr);
    set_product_auto_pr_maintenance(&db_path, &product, false);

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
    assert!(!r, "opted-out product must not flip to blocked");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    assert!(pub_.events.lock().await.is_empty());
}

#[tokio::test]
async fn detection_skipped_when_pr_has_opt_out_label() {
    // Per-PR label is the finer-grained opt-out — even on a
    // product with auto-maintenance enabled, a single labelled PR
    // is left alone.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/601";
    let (product, chore) = make_in_review(&db, "C-optout-label", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let r = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_labels(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            &["boss/no-auto-rebase"],
        ),
    )
    .await;
    assert!(!r, "labelled PR must not flip to blocked");
    let (status, _) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(pub_.events.lock().await.is_empty());
}

#[tokio::test]
async fn opt_out_label_match_is_case_insensitive() {
    // GitHub labels preserve case but the engine tolerates
    // BOSS/No-Auto-Rebase / etc. on the same gate so users don't
    // need to remember exact casing.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/602";
    let (product, chore) = make_in_review(&db, "C-optout-case", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let r = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_labels(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            &["Boss/No-Auto-Rebase"],
        ),
    )
    .await;
    assert!(!r);
}

#[tokio::test]
async fn resolution_skipped_when_product_opt_out_flag_disabled() {
    // Symmetric retire-path gate: an opted-out product's retire
    // is also a no-op so the engine doesn't undo a manual
    // intervention on a row it has stopped auto-managing.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/603";
    let (product, chore) = make_in_review(&db, "C-optout-retire", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Detect conflict with maintenance enabled: new model keeps parent
    // in_review (revision spawned). Then disable maintenance and assert
    // the retire path is a no-op.
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // New-model: parent stays in_review after detection (revision in flight).
    let (status_before, _) = chore_status(&db, &chore);
    assert_eq!(status_before, TaskStatus::InReview);
    let before = pub_.events.lock().await.len();
    set_product_auto_pr_maintenance(&db_path, &product, false);

    let r = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(!r, "opted-out product must not retire automatically");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    assert_eq!(pub_.events.lock().await.len(), before);
}
