//! Phase 6 #18: the opt-out gates. Both the detection and the retire side
//! must leave a row alone when its product has auto-PR-maintenance disabled
//! or its PR carries the opt-out label.

use std::sync::Arc;

use tempfile::tempdir;

use super::super::*;
use super::helpers::*;
use crate::merge_poller::{OpenPrStatus, PrLifecycleState};
use crate::test_support::*;
use crate::work::WorkDb;

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
