use super::*;

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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
    assert_eq!(outcome.conflict_flagged, 1);
    assert_eq!(outcome.ci_flagged, 0);
    assert!(db.active_conflict_resolution_for_work_item(&chore).unwrap().is_some());
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
