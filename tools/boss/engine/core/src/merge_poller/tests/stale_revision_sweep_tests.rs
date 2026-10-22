//! `run_one_pass`-level coverage for the stale-revision sweeps: engine-created
//! CI-fix / merge-conflict revisions whose spawning attempt ended are closed
//! once the PR recovers, independent of the mergeability arm the probe routes
//! through, and the board is told when that happens.

use super::*;

const REASON: &str = "moot_revisions_closed";

fn put(probe: &StubProbe, pr: &str, status: OpenPrStatus, head: &str, labels: &[&str]) {
    probe.states.lock().unwrap().insert(
        pr.to_owned(),
        Ok(PrLifecycleProbe::builder()
            .url(pr.to_owned())
            .state(PrLifecycleState::Open(status))
            .base_ref_oid(format!("base-{head}"))
            .head_ref_oid(head.to_owned())
            .head_ref_name("feature-branch")
            .base_ref_name("main")
            .labels(labels.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
            .review(PrReviewState::Unknown)
            .build()),
    );
}

fn park_blocked(db: &WorkDb, revision: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'spawning_attempt_retired_unsuccessfully' \
             WHERE id = ?1",
            [revision],
        )
        .unwrap();
}

fn status(db: &WorkDb, id: &str) -> TaskStatus {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t.status,
        other => panic!("expected task, got {other:?}"),
    }
}

async fn moot_events(publisher: &RecordingPublisher) -> usize {
    publisher
        .lifecycle_reasons()
        .await
        .iter()
        .filter(|r| *r == REASON)
        .count()
}

/// Flag a CI failure, then end the attempt and park its revision `blocked`.
async fn stale_ci_fix(
    db: &WorkDb,
    probe: &StubProbe,
    publisher: &RecordingPublisher,
    pr: &str,
    name: &str,
) -> (String, String) {
    let (_product, chore) = make_chore_in_review(db, name, pr);
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_failing(pr, "head-1")));
    assert_eq!(run_one_pass(db, probe, publisher, None, None, None).await.ci_flagged, 1);
    let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
    let revision = attempt.revision_task_id.clone().unwrap();
    db.mark_ci_remediation_failed(&attempt.id, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(db, &revision);
    (chore, revision)
}

/// Flag a conflict, then end the attempt and park its revision `blocked`.
async fn stale_conflict(
    db: &WorkDb,
    probe: &StubProbe,
    publisher: &RecordingPublisher,
    pr: &str,
    name: &str,
) -> (String, String) {
    let (_product, chore) = make_chore_in_review(db, name, pr);
    put(probe, pr, OpenPrStatus::conflict_only(), "head-1", &[]);
    assert_eq!(
        run_one_pass(db, probe, publisher, None, None, None)
            .await
            .conflict_flagged,
        1
    );
    let attempt = db.active_conflict_resolution_for_work_item(&chore).unwrap().unwrap();
    let revision = attempt.revision_task_id.clone().unwrap();
    db.mark_conflict_resolution_failed(&attempt.id, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(db, &revision);
    (chore, revision)
}

#[tokio::test]
async fn conflicting_pr_with_green_ci_closes_stale_ci_fix_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/8001";
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let (chore, revision) = stale_ci_fix(&db, &probe, &publisher, pr, "C-sweep-conflict-ci").await;

    // The head moved to a green commit, but the PR now conflicts with base.
    put(&probe, pr, OpenPrStatus::conflict_only(), "head-2", &[]);
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    assert_eq!(status(&db, &revision), TaskStatus::Archived);
    assert_eq!(moot_events(&publisher).await, 1);
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "merge_conflict"),
        "the CI sweep must not clear the merge-conflict signal",
    );

    // A repeat sweep finds nothing left to close and stays quiet.
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(moot_events(&publisher).await, 1);
}

#[tokio::test]
async fn clean_pr_closes_stale_conflict_revision_and_publishes_once() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/8002";
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let (_chore, revision) = stale_conflict(&db, &probe, &publisher, pr, "C-sweep-clean-conflict").await;
    assert_eq!(moot_events(&publisher).await, 0);

    put(&probe, pr, OpenPrStatus::clean(), "head-1", &[]);
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(status(&db, &revision), TaskStatus::Archived);
    assert_eq!(moot_events(&publisher).await, 1);

    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(moot_events(&publisher).await, 1, "a repeat sweep must not re-emit");
}

#[tokio::test]
async fn unknown_mergeability_leaves_conflict_revision_alone() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/8003";
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let (_chore, revision) = stale_conflict(&db, &probe, &publisher, pr, "C-sweep-unknown").await;

    put(&probe, pr, OpenPrStatus::unknown_mergeability(), "head-1", &[]);
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(status(&db, &revision), TaskStatus::Blocked);
    assert_eq!(moot_events(&publisher).await, 0);
}

#[tokio::test]
async fn pr_label_opt_out_skips_both_sweeps() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let ci_pr = "https://github.com/foo/bar/pull/8004";
    let conflict_pr = "https://github.com/foo/bar/pull/8005";
    let (_c1, ci_rev) = stale_ci_fix(&db, &probe, &publisher, ci_pr, "C-optout-label-ci").await;
    let (_c2, conflict_rev) = stale_conflict(&db, &probe, &publisher, conflict_pr, "C-optout-label-conflict").await;

    put(&probe, ci_pr, OpenPrStatus::clean(), "head-1", &["boss/no-auto-rebase"]);
    put(
        &probe,
        conflict_pr,
        OpenPrStatus::clean(),
        "head-1",
        &["boss/no-auto-rebase"],
    );
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    assert_eq!(status(&db, &ci_rev), TaskStatus::Blocked);
    assert_eq!(status(&db, &conflict_rev), TaskStatus::Blocked);
    assert_eq!(moot_events(&publisher).await, 0);
}

#[tokio::test]
async fn product_opt_out_skips_both_sweeps() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let ci_pr = "https://github.com/foo/bar/pull/8006";
    let conflict_pr = "https://github.com/foo/bar/pull/8007";
    let (_c1, ci_rev) = stale_ci_fix(&db, &probe, &publisher, ci_pr, "C-optout-product-ci").await;
    let (_c2, conflict_rev) = stale_conflict(&db, &probe, &publisher, conflict_pr, "C-optout-product-conflict").await;
    db.connect()
        .unwrap()
        .execute("UPDATE products SET auto_pr_maintenance_enabled = 0", [])
        .unwrap();

    put(&probe, ci_pr, OpenPrStatus::clean(), "head-1", &[]);
    put(&probe, conflict_pr, OpenPrStatus::clean(), "head-1", &[]);
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    assert_eq!(status(&db, &ci_rev), TaskStatus::Blocked);
    assert_eq!(status(&db, &conflict_rev), TaskStatus::Blocked);
    assert_eq!(moot_events(&publisher).await, 0);
}
