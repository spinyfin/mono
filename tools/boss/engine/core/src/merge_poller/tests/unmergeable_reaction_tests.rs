use super::*;
use crate::merge_poller::TrunkQueueCheckFailure;

fn recent_completed_at() -> String {
    chrono::DateTime::from_timestamp(boss_engine_utils::epoch_time::now_epoch_secs() - 60, 0)
        .unwrap()
        .to_rfc3339()
}

fn trunk_check_failure() -> TrunkQueueCheckFailure {
    TrunkQueueCheckFailure {
        name: "Trunk Merge Queue (main)".to_owned(),
        conclusion: "FAILURE".to_owned(),
        details_url: "https://app.trunk.io/flunge/merge-queue/example/1508".to_owned(),
        completed_at: Some(recent_completed_at()),
    }
}

fn probe_with(
    pr: &str,
    mergeability: OpenPrMergeability,
    ci: OpenPrCiStatus,
    trunk_check: Option<TrunkQueueCheckFailure>,
) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr)
        .state(PrLifecycleState::Open(OpenPrStatus { mergeability, ci }))
        .base_ref_oid("base-1")
        .head_ref_oid("head-evicted")
        .head_ref_name("feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .maybe_trunk_queue_check_failure(trunk_check)
        .build()
}

/// Direct-mechanism product, PR-head CI clean, Trunk's own check failed:
/// the merge poller must mint a queue-rejection revision without waiting
/// for the Trunk API poller (which never enumerates Direct products).
#[tokio::test]
async fn failed_trunk_check_on_clean_mergeable_pr_mints_a_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1508";
    let (_product, chore) = make_chore_in_review(&db, "C-trunk-check", pr);

    let probe = StubProbe::new();
    probe.states.lock().unwrap().insert(
        pr.into(),
        Ok(probe_with(
            pr,
            OpenPrMergeability::Clean,
            OpenPrCiStatus::Clean,
            Some(trunk_check_failure()),
        )),
    );
    let publisher = Arc::new(RecordingPublisher::default());
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(outcome.ci_flagged, 1, "failed Trunk check must mint");
    assert_eq!(outcome.conflict_flagged, 0);

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("ci_remediations row");
    assert_eq!(attempt.failure_kind.as_deref(), Some("trunk_queue_eviction"));
    assert!(
        attempt.revision_task_id.is_some(),
        "queue-rejection revision must be stamped"
    );
}

/// Required checks still running is waiting, not stuck — even if
/// mergeStateStatus would read BLOCKED on GitHub. Do not mint.
#[tokio::test]
async fn in_flight_checks_do_not_mint_on_a_failed_trunk_check() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1509";
    let (_product, chore) = make_chore_in_review(&db, "C-trunk-inflight", pr);

    let probe = StubProbe::new();
    probe.states.lock().unwrap().insert(
        pr.into(),
        Ok(probe_with(
            pr,
            OpenPrMergeability::Clean,
            OpenPrCiStatus::InFlight,
            Some(trunk_check_failure()),
        )),
    );
    let publisher = Arc::new(RecordingPublisher::default());
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(
        outcome.ci_flagged, 0,
        "in-flight required checks are waiting, not stuck"
    );
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "no queue-rejection attempt while CI is still running"
    );
}

/// Conflict pre-empts the Trunk-check path. A CONFLICTING PR mints a
/// conflict-resolution revision, not a CI-fix, even when Trunk's check
/// also failed.
#[tokio::test]
async fn conflicting_pr_mints_conflict_revision_not_queue_rejection() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1510";
    let (_product, chore) = make_chore_in_review(&db, "C-conflict-preempts", pr);

    let probe = StubProbe::new();
    probe.states.lock().unwrap().insert(
        pr.into(),
        Ok(probe_with(
            pr,
            OpenPrMergeability::Conflict,
            OpenPrCiStatus::Clean,
            Some(trunk_check_failure()),
        )),
    );
    let publisher = Arc::new(RecordingPublisher::default());
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(outcome.conflict_flagged, 1);
    assert_eq!(outcome.ci_flagged, 0);
    assert!(db.active_conflict_resolution_for_work_item(&chore).unwrap().is_some());
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
}

/// A second sweep for the same failed Trunk check must not mint a
/// duplicate revision.
#[tokio::test]
async fn failed_trunk_check_is_idempotent_across_sweeps() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1511";
    let (_product, chore) = make_chore_in_review(&db, "C-trunk-idem", pr);

    let probe = StubProbe::new();
    let p = probe_with(
        pr,
        OpenPrMergeability::Clean,
        OpenPrCiStatus::Clean,
        Some(trunk_check_failure()),
    );
    probe.states.lock().unwrap().insert(pr.into(), Ok(p));
    let publisher = Arc::new(RecordingPublisher::default());
    let first = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    let second = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(first.ci_flagged, 1);
    assert_eq!(second.ci_flagged, 0);
    let conn = db.connect().unwrap();
    let rev_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rev_count, 1);
}

/// A `trunk_queue` product adopts the episode so the queue poller owns
/// remediation. The merge-poller head-check path must stand down rather
/// than mint a second revision against a different discriminator.
#[tokio::test]
async fn trunk_queue_product_adopts_and_does_not_double_mint() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1512";
    let (product, chore) = make_chore_in_review(&db, "C-trunk-adopt", pr);
    db.set_product_merge_mechanism(&product, Some("trunk_queue")).unwrap();

    let probe = StubProbe::new();
    probe.states.lock().unwrap().insert(
        pr.into(),
        Ok(probe_with(
            pr,
            OpenPrMergeability::Clean,
            OpenPrCiStatus::Clean,
            Some(trunk_check_failure()),
        )),
    );
    let publisher = Arc::new(RecordingPublisher::default());
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(outcome.trunk_episodes_adopted, 1);
    assert_eq!(
        outcome.ci_flagged, 0,
        "adopted episode is owned by the queue poller, not a second head-check mint"
    );
    assert!(db.get_active_trunk_merge_intent(&chore).unwrap().is_some());
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
}

/// End-to-end: a CONFLICTING PR whose chain already has in_review
/// revisions still mints a runnable conflict-resolution revision.
#[tokio::test]
async fn conflicting_pr_with_open_in_review_revisions_mints_a_runnable_fix() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1508";
    let (_product, chore) = make_chore_in_review(&db, "C-conflict-siblings", pr);
    let checker = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open);
    let prior = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.clone())
                .description("PR review: 8 finding(s)")
                .build(),
            &checker,
        )
        .unwrap();
    db.update_work_item(
        &prior.id,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();

    let probe = StubProbe::new();
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        "base-1",
        "head-1",
    );
    let publisher = Arc::new(RecordingPublisher::default());
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(outcome.conflict_flagged, 1);

    let attempt = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("conflict attempt");
    let rev_id = attempt.revision_task_id.expect("revision stamped");
    let conn = db.connect().unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            rusqlite::params![&rev_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(status, "blocked", "must not wait on in_review siblings");
}

/// A parent parked in `blocked: dependency` with a CONFLICTING PR is
/// still probed and still gets a conflict revision, without stealing
/// the dependency block.
#[tokio::test]
async fn blocked_dependency_parent_with_conflicting_pr_still_mints() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1862";
    let (_product, chore) = make_chore_in_review(&db, "C-dep-conflict", pr);
    db.update_work_item(
        &chore,
        crate::work::WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some("dependency".into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();

    let probe = StubProbe::new();
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        "base-1",
        "head-1",
    );
    let publisher = Arc::new(RecordingPublisher::default());
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(outcome.conflict_flagged, 1);
    match db.get_work_item(&chore).unwrap() {
        crate::work::WorkItem::Chore(t) => {
            assert_eq!(t.status, crate::work::TaskStatus::Blocked);
            assert_eq!(t.blocked_reason.as_deref(), Some("dependency"));
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(db.active_conflict_resolution_for_work_item(&chore).unwrap().is_some());
}
