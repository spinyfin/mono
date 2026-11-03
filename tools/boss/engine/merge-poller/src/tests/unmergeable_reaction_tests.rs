use super::*;

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
